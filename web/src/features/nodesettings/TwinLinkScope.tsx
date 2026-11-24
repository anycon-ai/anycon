// 设备列表里一台 Bot 设备（别的节点的 Bot）的「范围」：这条连接的 `twin.get` / `twin.set`（`requester` = 设备 id）。
// 表单和 Bot 页的一样；缺省什么都不开放。
import { useCallback, useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { PeerDevice, TwinGetResult, TwinSetParams } from '@/protocol'
import { describeError } from '@/lib/errors'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { useTodos } from '@/features/todos/store'
import { ScopeForm } from '@/features/twin/ScopeForm'
import { rpc } from '@/transport/instance'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'

export function TwinLinkScope({ nodeId, device, onClose }: { nodeId: string; device: PeerDevice | null; onClose: () => void }) {
  const { t } = useTranslation()
  const node = useNodes((s) => s.byId[nodeId])
  const order = useNodes((s) => s.order)
  const todos = useTodos((s) => s.nodes[nodeId]?.items)
  const [value, setValue] = useState<TwinGetResult | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const id = device?.device_id
  useEffect(() => {
    setValue(null)
    setError('')
    if (!id) return
    let live = true
    rpc()
      .request<TwinGetResult>(nodeId, 'twin.get', { requester: id })
      .then(
        (v) => live && setValue(v),
        (e: unknown) => live && setError(describeError(e, t)),
      )
    return () => {
      live = false
    }
  }, [nodeId, id, t])
  const save = useCallback(
    async (params: TwinSetParams): Promise<void> => {
      if (!id) return
      setBusy(true)
      setError('')
      try {
        setValue(await rpc().request<TwinGetResult>(nodeId, 'twin.set', { ...params, requester: id }))
      } catch (e) {
        setError(describeError(e, t))
      } finally {
        setBusy(false)
      }
    },
    [nodeId, id, t],
  )
  return (
    <Dialog open={!!device} onOpenChange={(o) => !o && onClose()} title={t('twinScope.title', { name: device?.name ?? '' })} description={t('twinScope.hint')} closeLabel={t('common.close')} size="lg" testId="twin-scope-dialog">
      <div className="space-y-3">
        {!value && !error ? <p className="text-sm text-fg-subtle">{t('twin.loading')}</p> : null}
        {value && node ? (
          <>
            <p className="text-sm" data-testid="twin-scope-state">
              {t(`twin.state.${value.state}`)}
            </p>
            {!value.enabled ? <p className="text-sm">{t('twinScope.acceptOff')}</p> : null}
            {!value.scope.projects.length ? <p className="text-sm">{t('twinScope.nothingOpen')}</p> : null}
            <p className="text-sm text-fg-subtle">{t('twinScope.usage', { dispatches: value.today.dispatches, budget: value.scope.daily_dispatches })}</p>
            {value.working.length ? (
              <section>
                <h3 className="text-sm font-semibold">{t('twinScope.working', { count: value.working.length })}</h3>
                <ul>
                  {value.working.map((todo) => (
                    <li key={todo}>
                      <Link to="/todos/$node/$todo" params={{ node: shortNodeId(nodeId, order), todo }} className="text-accent-text">
                        {todos?.[todo]?.title ?? todo}
                      </Link>
                    </li>
                  ))}
                </ul>
              </section>
            ) : null}
            <Button size="sm" disabled={busy || node.conn !== 'ready'} onClick={() => void save({ paused: !value.scope.paused })} data-testid="twin-scope-pause">
              {value.scope.paused ? t('twinScope.resume') : t('twinScope.pause')}
            </Button>
            <ScopeForm key={JSON.stringify(value.scope)} node={node} value={value} busy={busy} onSave={save} formId={`scope-${nodeId}-${id}`} hint={t('twinScope.formHint')} />
          </>
        ) : null}
        {error ? (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        ) : null}
      </div>
    </Dialog>
  )
}
