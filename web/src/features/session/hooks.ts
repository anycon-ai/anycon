import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { AgentInfo, PermissionRespondParams, SessionSettings, SessionStatus } from '@/protocol'
import { describeError, isDangerousModeRejection } from '@/lib/errors'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { applyUpdate, buildSettingsUpdate, isEmptyUpdate, mergeUpdates, pendingFields, selectionFromSettings, selectionKey, type SettingsField, type SettingsUpdate, type Selection } from '@/features/settings/resolve'
import { RpcCallError } from '@/transport/rpcClient'
import { beginAnswer, endAnswer, useInflight } from './inflight'
import { respondPermission, setSessionSettings } from './actions'
import type { SettledReason } from './ui/PermissionCard'

const NO_PENDING: ReadonlySet<SettingsField> = new Set()

export const RUNNING_STATES: readonly SessionStatus[] = ['running', 'starting', 'waiting_permission', 'queued']
export const isRunning = (s: SessionStatus | null | undefined): boolean => !!s && RUNNING_STATES.includes(s)

/** 把 device id 翻成显示名：本机 → 「你」，daemon 自己（`by_device: "daemon"`，如目录变了自动拒绝、超时、取消）→ 「anycon」，其余查 node.devices，查不到用短 id。 */
export function useDeviceLabel(nodeId: string | undefined): (deviceId: string) => string {
  const { t } = useTranslation()
  const self = useHub((s) => s.snapshot.deviceId)
  const devices = useNodes((s) => (nodeId ? s.byId[nodeId]?.devices : undefined))
  return useCallback(
    (id: string) => {
      if (self && id === self) return t('permission.you')
      if (id === 'daemon') return t('permission.anycon')
      return devices?.find((d) => d.device_id === id)?.name ?? id.slice(0, 8)
    },
    [self, devices, t],
  )
}

/**
 * 已有会话的模型 / 思考 / 模式控件的取值与修改（spec/web.md § 5.3）：
 * 空闲时立即 `session.set_settings`；运行中先排队，本轮结束后应用。
 * 上一次 `set_settings` 还在途中时的新改动也排队（合并进同一个待应用的更新），等它返回再发——不能丢。
 */
export function useSessionSettings(opts: { nodeId: string; sessionId: string; agent: AgentInfo | undefined; running: boolean; baseSettings: SessionSettings | null | undefined; offline: boolean }) {
  const { t } = useTranslation()
  const { nodeId, sessionId, agent, running, baseSettings, offline } = opts
  const [queued, setQueued] = useState<SettingsUpdate | null>(null)
  const setLastUsed = usePrefs((s) => s.setLastUsed)
  /** 有 `set_settings` 在途。ref 供同一个 tick 里的连续调用判断，state 让在途结束时重新渲染、触发下面排队改动的 effect。 */
  const applying = useRef(false)
  const [inFlight, setInFlight] = useState(false)

  const current = useMemo<Selection | null>(() => (agent ? selectionFromSettings(agent, baseSettings) : null), [agent, baseSettings])
  const shown = useMemo<Selection | null>(() => (current && queued ? applyUpdate(current, queued) : current), [current, queued])
  // 排队中、和会话实际设置不同的那几格：控件上换成浅色，表示还没生效
  const pending = useMemo(() => (current && shown && queued ? pendingFields(current, shown) : NO_PENDING), [current, shown, queued])

  const apply = useCallback(
    async (update: SettingsUpdate): Promise<void> => {
      if (applying.current) {
        // 上一个还没返回：排到它后面，不要丢
        setQueued((q) => mergeUpdates(q, update))
        return
      }
      applying.current = true
      setInFlight(true)
      try {
        await setSessionSettings(nodeId, sessionId, update)
      } catch (e) {
        const dangerous = !!update.settings.mode && (agent?.modes ?? []).some((m) => m.id === update.settings.mode && m.risk === 'dangerous')
        if (isDangerousModeRejection(e, dangerous)) useNodes.getState().patch(nodeId, { dangerousForbidden: true })
        toast.error(describeError(e, t))
      } finally {
        applying.current = false
        setInFlight(false)
      }
    },
    [nodeId, sessionId, agent, t],
  )

  // 本轮结束后、上一次 set_settings 返回后，应用排队中的改动
  useEffect(() => {
    if (running || !queued || offline || inFlight) return
    const patch = queued
    setQueued(null)
    void apply(patch)
  }, [running, queued, offline, inFlight, apply])

  const onChange = useCallback(
    (next: Selection, info?: { thinkingAdjusted?: boolean }) => {
      if (!agent || !shown) return
      const update = buildSettingsUpdate(shown, next)
      if (isEmptyUpdate(update)) return
      setLastUsed(selectionKey(nodeId, agent.id), { model: next.model, thinking: next.thinking, mode: next.mode, options: next.options })
      if (info?.thinkingAdjusted && next.thinking) toast.info(t('options.thinkingAdjusted', { value: next.thinking }))
      if (running || offline || applying.current) setQueued((q) => mergeUpdates(q, update))
      else void apply(update)
    },
    [agent, shown, nodeId, running, offline, apply, setLastUsed, t],
  )

  return { selection: shown, onChange, queued: queued !== null, pending }
}

/** 订阅某个会话的摘要 + 打开状态。 */
export function useSessionData(sessionId: string) {
  const summary = useSessions((s) => s.list[sessionId])
  const open = useSessions((s) => s.open[sessionId])
  return { summary, open }
}

/**
 * 答复权限请求（会话页和「待处理」页共用，所以两处的卡片行为一致）：同一个请求已经在答复时第二次答复直接丢掉（`inflight.ts`，
 * 和 ⌘⇧A / ⌘⇧D 共用）；daemon 回 `not_found` 的请求定格成说明（`settled`：目录变了 / 请求已不存在，不再给按钮、不重试），失败弹 toast。
 */
export function useRespondPermission(nodeId: string | undefined, opts: { handledHere?: boolean } = {}): {
  respond: (params: PermissionRespondParams) => void
  responding: ReadonlySet<string>
  settled: ReadonlyMap<string, SettledReason>
} {
  const { t } = useTranslation()
  const handledHere = !!opts.handledHere
  const responding = useInflight((s) => s.ids)
  // 原因是目录变了（`folder_changed`）时随后会来 permission_resolved（by_device: daemon），卡片换成「已被 anycon 自动拒绝」；
  // 别的 not_found（`gone`：请求已不存在）就停在说明上
  const [settled, setSettled] = useState<ReadonlyMap<string, SettledReason>>(new Map())
  const respond = useCallback(
    (params: PermissionRespondParams) => {
      if (!nodeId || !beginAnswer(params.request_id, handledHere)) return
      let ok = false
      respondPermission(nodeId, params)
        .then((outcome) => {
          ok = true
          if (outcome === 'answered') return
          const reason: SettledReason = outcome === 'denied_folder_changed' ? 'folder_changed' : 'gone'
          setSettled((s) => new Map(s).set(params.request_id, reason))
          toast.info(t(outcome === 'denied_folder_changed' ? 'permission.deniedFolderChanged' : 'permission.requestGone'))
        })
        .catch((e: unknown) => {
          if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'already_resolved') {
            ok = true
            toast.info(t('permission.alreadyResolved'))
          } else toast.error(describeError(e, t))
        })
        .finally(() => endAnswer(params.request_id, ok))
    },
    [nodeId, t, handledHere],
  )
  return { respond, responding, settled }
}
