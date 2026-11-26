// Bot 页的状态头：节点名、状态行和操作。配置（范围、预算、判断后端、Bot 连接）在节点设置 →「Bot」。
// 状态头是卡片的卡头：带底色和状态点，节点名是收起 / 展开的开关（外层是 `TwinPage` 里的 `Collapsible.Root`），操作按钮在收起时也在。
// 恢复要先试跑再确认：只有试跑结果是 `done` 或 `idle` 才允许「就这么办」。
import { useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { Collapsible } from 'radix-ui'
import { ChevronRight } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { describeError } from '@/lib/errors'
import { shortNodeId } from '@/lib/ids'
import { useNodes, type NodeEntry } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'
import { Dot, Spinner, type Tone } from '@/ui/Misc'
import { Decisions } from './Parts'
import { setTwin, syncTwin, wakeTwin } from './service'
import { missingTriage, twinHead, twinHeadLabel, type TwinHead } from './state'
import { useTwin } from './store'
import { usePlan } from './usePlan'

/** 卡头状态点的语气：状态不只靠颜色，旁边总有状态文字。 */
const headTone: Record<TwinHead, Tone> = {
  offline: 'neutral',
  loading: 'neutral',
  failed: 'danger',
  disabled: 'neutral',
  not_configured: 'warning',
  paused: 'warning',
  budget: 'warning',
  running: 'accent',
  idle: 'success',
  unknown: 'neutral',
}

/** `resume` 是暂停后的恢复路径（带「就这么办」）；`dry` 只看计划。 */
type DialogMode = 'resume' | 'dry'

export function StatusHead({ node, onError }: { node: NodeEntry; onError: (message: string) => void }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const entry = useTwin((s) => s.nodes[node.id])
  const [busy, setBusy] = useState(false)
  // 点「现在看看」时最新一次运行的标记；之后有新的运行就和它对不上，提示随之消失
  const [looked, setLooked] = useState<string | null>(null)
  const [dialog, setDialog] = useState<DialogMode | null>(null)
  const [doItError, setDoItError] = useState('')
  const { plan, canProceed, start, reset } = usePlan(node.id)
  const value = entry?.value
  const online = node.conn === 'ready'
  const head = twinHead(node, entry)
  const configured = !!value?.configured
  const paused = !!value?.scope.paused
  const enabled = !!value?.enabled
  const runMarker = [value?.last_run?.id ?? '', entry?.runs.reduce((m, r) => Math.max(m, r.started_ms), 0) ?? 0].join(':')
  const lookedShown = looked !== null && looked === runMarker && online && !paused
  // 离线或暂停 / 恢复后，「稍等片刻」不再成立
  useEffect(() => {
    if (!online || paused) setLooked(null)
  }, [online, paused])
  const act = async (task: () => Promise<void>): Promise<void> => {
    setBusy(true)
    onError('')
    try {
      await task()
    } catch (e) {
      onError(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }
  const openPlan = (mode: DialogMode): void => {
    setDialog(mode)
    setDoItError('')
    void start()
  }
  const close = (): void => {
    reset()
    setDialog(null)
  }
  const doIt = async (): Promise<void> => {
    setBusy(true)
    setDoItError('')
    onError('')
    try {
      await setTwin(node.id, { paused: false })
    } catch (e) {
      setDoItError(describeError(e, t))
      setBusy(false)
      return
    }
    // 已经恢复：对话框先关。接着唤醒失败不影响恢复，在卡片上单独说明
    close()
    setLooked(null)
    try {
      await wakeTwin(node.id)
    } catch (e) {
      onError(t('twin.resumedWakeFailed', { error: describeError(e, t) }))
    } finally {
      setBusy(false)
    }
  }
  return (
    <div className="space-y-2 bg-bg-subtle px-4 py-3" data-testid="twin-head">
      <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2">
        <div className="flex min-w-0 items-center gap-2">
          <h2 className="min-w-0 font-semibold">
            <Collapsible.Trigger className="group flex min-h-9 min-w-0 items-center gap-1.5 rounded-md text-left" data-testid="twin-toggle">
              <ChevronRight className="size-4 shrink-0 text-fg-muted transition-transform group-data-[state=open]:rotate-90" aria-hidden />
              <span className="truncate">{node.name}</span>
            </Collapsible.Trigger>
          </h2>
          <span className="flex shrink-0 items-center gap-1.5 text-sm text-fg-muted">
            <Dot tone={headTone[head]} pulse={head === 'running'} />
            <span data-testid="twin-state">{twinHeadLabel(t, head)}</span>
          </span>
        </div>
        {value && configured ? (
          <div className="flex flex-wrap gap-2">
            {paused ? (
              <Button variant="primary" disabled={!online || busy || !enabled} onClick={() => openPlan('resume')}>
                {t('twin.resume')}
              </Button>
            ) : (
              <Button
                variant="primary"
                disabled={!online || busy}
                onClick={() => {
                  setLooked(null)
                  void act(() => setTwin(node.id, { paused: true }))
                }}
              >
                {t('twin.pause')}
              </Button>
            )}
            {!paused && enabled ? (
              <Button
                variant="outline"
                className="bg-surface"
                disabled={!online || busy}
                onClick={() =>
                  void act(async () => {
                    const marker = runMarker
                    await wakeTwin(node.id)
                    setLooked(marker)
                  })
                }
              >
                {t('twin.lookNow')}
              </Button>
            ) : null}
            {enabled ? (
              <Button variant="outline" className="bg-surface" disabled={!online || busy} onClick={() => openPlan('dry')}>
                {t('twin.dryRun')}
              </Button>
            ) : null}
          </div>
        ) : null}
      </div>
      {entry?.failed && online ? (
        <div role="alert">
          {t('twin.loadFailed')}{' '}
          <Button disabled={busy} onClick={() => void act(() => syncTwin(node.id))}>
            {t('todos.retry')}
          </Button>
        </div>
      ) : null}
      {lookedShown ? (
        <p role="status" className="text-sm text-fg-muted">
          {t('twin.lookNowQueued')}
        </p>
      ) : null}
      {head === 'disabled' ? null : value && !configured ? (
        <div className="space-y-2">
          {!value.scope.projects.length ? <p>{t('twin.noProjectsYet')}</p> : null}
          <MissingTriage node={node} />
          <Link to="/n/$node/settings" params={{ node: shortNodeId(node.id, order) }} search={{ tab: 'twin' }} className="inline-flex h-9 items-center rounded-md bg-accent px-4 text-sm font-medium text-accent-fg hover:bg-accent-hover" data-testid="twin-setup">
            {t('twin.setUp')}
          </Link>
        </div>
      ) : null}
      <Dialog
        open={dialog !== null}
        onOpenChange={(o) => !o && close()}
        title={dialog === 'resume' ? t('twin.resumeTitle') : t('twin.dryRunTitle')}
        description={dialog === 'resume' ? t('twin.resumeHint') : t('twin.dryRunHint')}
        closeLabel={t('common.close')}
        size="lg"
        testId="twin-plan-dialog"
      >
        <div className="space-y-3">
          {plan?.kind === 'running' ? (
            <p role="status" className="flex items-center gap-2 text-sm">
              <Spinner /> {t('twin.dryRunning')}
            </p>
          ) : null}
          {plan?.kind === 'failed' ? (
            <p role="alert" className="text-danger">
              {plan.message}
            </p>
          ) : null}
          {plan?.kind === 'ready' ? (
            <>
              {dialog === 'resume' && !canProceed ? <p role="alert">{plan.run.result === 'budget' ? t('twin.resumeBlockedBudget') : t('twin.resumeBlocked', { result: t(`twin.result.${plan.run.result}`) })}</p> : null}
              <Decisions nodeId={node.id} run={plan.run} />
            </>
          ) : null}
          {dialog === 'resume' && plan && plan.kind !== 'running' && !canProceed ? (
            <div className="space-y-1">
              <p className="text-sm text-fg-muted">{t('twin.resumeAnywayHint')}</p>
              <Button disabled={!online || busy || !enabled} onClick={() => void doIt()} data-testid="twin-resume-anyway">
                {t('twin.resumeAnyway')}
              </Button>
            </div>
          ) : null}
          {doItError ? (
            <p role="alert" className="text-danger">
              {doItError}
            </p>
          ) : null}
          <div className="flex justify-end gap-2">
            <Button onClick={close}>{dialog === 'resume' ? t('common.cancel') : t('common.close')}</Button>
            {dialog === 'resume' ? (
              <Button variant="primary" disabled={!online || busy || !enabled || !canProceed} onClick={() => void doIt()}>
                {t('twin.doIt')}
              </Button>
            ) : null}
          </div>
        </div>
      </Dialog>
    </div>
  )
}

/** 没有可用的分流后端时说明缺什么（和节点设置里的状态行同一个判断）。 */
function MissingTriage({ node }: { node: NodeEntry }) {
  const { t } = useTranslation()
  return missingTriage(node) ? <p>{t('twin.noTriage')}</p> : null
}
