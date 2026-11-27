// 「Twin」标签里的范围与预算：`ScopeForm` 加首次流程——没配置过时保存成暂停，接着试跑并展示计划，确认后才取消暂停并唤醒。
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { describeError } from '@/lib/errors'
import type { NodeEntry } from '@/stores/nodes'
import { Decisions } from '@/features/twin/Parts'
import { ScopeForm } from '@/features/twin/ScopeForm'
import { setTwin, wakeTwin } from '@/features/twin/service'
import { useTwin } from '@/features/twin/store'
import { usePlan } from '@/features/twin/usePlan'
import { Button } from '@/ui/Button'
import { Card, Spinner } from '@/ui/Misc'

export function TwinScopeSection({ node }: { node: NodeEntry }) {
  const { t } = useTranslation()
  const value = useTwin((s) => s.nodes[node.id]?.value)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const { plan, canProceed, start, reset } = usePlan(node.id)
  const online = node.conn === 'ready'
  if (!value) return null

  const act = async (task: () => Promise<void>): Promise<void> => {
    setBusy(true)
    setError('')
    try {
      await task()
    } catch (e) {
      setError(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }
  const canRun = online && !busy && value.enabled && value.configured
  return (
    <Card className="space-y-4 p-4">
      <ScopeForm
        key={`${node.id}:${JSON.stringify(value.scope)}`}
        node={node}
        value={value}
        busy={busy}
        hint={value.configured ? undefined : t('nodeSettings.twin.firstSaveHint')}
        onSave={(params) =>
          act(async () => {
            reset()
            // 首次保存一律暂停：计划没看过之前 Twin 不会自己开始干活
            const first = !value.configured
            await setTwin(node.id, { ...params, ...(first ? { paused: true } : {}) })
            if (first && params.projects?.length && value.enabled) await start()
          })
        }
      />
      <section className="space-y-2 border-t border-border pt-4" data-testid="twin-tab-plan">
        <h3 className="font-semibold">{t('twin.what')}</h3>
        <p className="text-sm text-fg-subtle">{t('twin.trialHint')}</p>
        <Button disabled={!canRun} onClick={() => void start()} data-testid="twin-tab-dry-run">
          {t('twin.dryRun')}
        </Button>
        {plan?.kind === 'running' ? (
          <p role="status" className="flex items-center gap-2 text-sm">
            <Spinner /> {t('twin.dryRunning')}
          </p>
        ) : null}
        {plan?.kind === 'failed' ? (
          <p role="alert" className="text-sm text-danger">
            {plan.message}
          </p>
        ) : null}
        {plan?.kind === 'ready' ? (
          <>
            <Decisions nodeId={node.id} run={plan.run} />
            <Button
              variant="primary"
              disabled={!canRun || !canProceed}
              data-testid="twin-tab-do-it"
              onClick={() =>
                void act(async () => {
                  await setTwin(node.id, { paused: false })
                  await wakeTwin(node.id)
                  reset()
                })
              }
            >
              {t('twin.doIt')}
            </Button>
          </>
        ) : null}
      </section>
      {error ? (
        <p role="alert" className="text-sm text-danger" data-testid="twin-tab-error">
          {error}
        </p>
      ) : null}
    </Card>
  )
}
