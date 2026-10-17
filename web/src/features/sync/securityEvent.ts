// `node.security_event`（ADR-36）：新设备经接入加入、节点口令连续输错、口令接入被锁定、口令 / 接入设置被改动。
// 在前台时的呈现是 toast（点「查看」去节点设置的「接入」标签）；不在前台时是 Web Push（`notify { category: security }`，
// 文案见 `./pushText.ts`）。自己做的改动不弹（只刷新状态）。
import type { TFunction } from 'i18next'
import type { EnrollMethod, NodeDevicesResult, NodeSecurityEvent, SecurityEventKind } from '@/protocol'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { navigateTo } from '@/lib/navigate'
import { useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { rpc } from '@/transport/instance'
import i18n from '@/i18n'

const KINDS: ReadonlySet<string> = new Set<SecurityEventKind>(['device_enrolled', 'password_failures', 'password_locked', 'password_changed', 'settings_changed'])

export function parseSecurityEvent(v: unknown): NodeSecurityEvent | null {
  if (typeof v !== 'object' || v === null) return null
  const o = v as Record<string, unknown>
  if (typeof o.kind !== 'string' || !KINDS.has(o.kind) || typeof o.ts_ms !== 'number') return null
  return v as NodeSecurityEvent
}

export interface SecurityTextContext {
  /** 节点名。 */
  node: string
  /** 设备 id → 显示名；认不出的返回 undefined。 */
  deviceName: (id: string) => string | undefined
}

/** 设备显示名：`local` 是节点本机的 CLI，认不出的设备显示成一个短 id，没有就是「某台设备」。 */
function who(t: TFunction, id: string | null | undefined, ctx: SecurityTextContext): string {
  if (!id) return t('security.someone')
  if (id === 'local') return t('security.local')
  return cleanHintText(ctx.deviceName(id)) || id.slice(0, 8)
}

const METHODS: Record<EnrollMethod, string> = { scan: 'security.method.scan', code: 'security.method.code', password: 'security.method.password' }

/** 事件 → 给人看的一句话。 */
export function securityText(t: TFunction, ev: NodeSecurityEvent, ctx: SecurityTextContext): string {
  const node = cleanHintText(ctx.node) || t('security.thisNode')
  switch (ev.kind) {
    case 'device_enrolled':
      return t('security.deviceEnrolled', {
        device: cleanHintText(ev.device_name) || t('security.unknownDevice'),
        node,
        via: ev.method ? ` (${t(METHODS[ev.method])})` : '',
      })
    case 'password_failures':
      return t('security.passwordFailures', { node, count: ev.failures ?? 3 })
    case 'password_locked':
      return t('security.passwordLocked', { node })
    case 'password_changed':
      return ev.password_set === false ? t('security.passwordCleared', { node, who: who(t, ev.by_device, ctx) }) : t('security.passwordChanged', { node, who: who(t, ev.by_device, ctx) })
    case 'settings_changed':
      return t('security.settingsChanged', { node, who: who(t, ev.by_device, ctx) })
  }
}

export function handleSecurityEvent(nodeId: string, params: unknown): void {
  const ev = parseSecurityEvent(params)
  if (!ev) {
    console.warn('anycon: ignored a malformed node.security_event')
    return
  }
  useEnroll.getState().bumpSecurity(nodeId) // 「接入」标签打开着的话重新读状态
  const nodes = useNodes.getState()
  const entry = nodes.byId[nodeId]
  const self = useHub.getState().snapshot.deviceId
  // 新设备加入后，设备列表要补一次
  if (ev.kind === 'device_enrolled')
    rpc()
      .request<NodeDevicesResult>(nodeId, 'node.devices', {})
      .then((r) => useNodes.getState().setDevices(nodeId, r.devices))
      .catch((e: unknown) => console.warn('anycon: could not refresh the device list after a new device enrolled', e))
  // 自己改的不用再提醒自己
  if ((ev.kind === 'password_changed' || ev.kind === 'settings_changed') && ev.by_device && ev.by_device === self) return
  const text = securityText(i18n.t.bind(i18n), ev, {
    node: entry?.name ?? '',
    deviceName: (id) => entry?.devices.find((d) => d.device_id === id)?.name,
  })
  const order = nodes.order
  toast.info(text, { label: i18n.t('security.review'), run: () => navigateTo(`/n/${shortNodeId(nodeId, order)}/settings?tab=enroll`) })
}
