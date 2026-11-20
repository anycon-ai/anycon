import { useId, useMemo, useState } from 'react'
import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import { renameSpace, setNodeGroup, type GroupResult } from '@/features/spaces/manage'
import { cleanHintText } from '@/lib/hint'
import { useNodeFilter } from '@/lib/nodeFilter'
import { groupError, groupOf } from '@/lib/spaces'
import { useManageSpaces } from '@/stores/manageSpaces'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Input, Select } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { Dot } from '@/ui/Misc'
import { connectionTone } from './useConnectionLabel'

/** 一次设置 / 改名的结果 → 一句话（en / zh 都走 i18n）。 */
function resultText(t: TFunction, r: GroupResult): string {
  switch (r.status) {
    case 'changed':
      return t('spaces.result.changed')
    case 'unchanged':
      return t('spaces.result.unchanged')
    case 'offline':
      return t('spaces.result.offline')
    case 'invalid':
      return t(`spaces.errors.${r.reason}`)
    case 'rejected':
      return r.code === 'forbidden' && r.reason === 'env_override' ? t('spaces.errors.envOverride') : r.code === 'invalid' ? t('spaces.errors.invalid') : t('spaces.errors.other')
  }
}

/**
 * 管理空间（spec/web.md § 4.13）：给每个已配对的节点设分组（选已有的名字或输入新的，空 = 没有分组），给一个空间改名。
 * 改名是对这个空间里每个在线的节点各发一次；不在线的列为「没改」，等它回来可以再试。写的是节点上的 `hub.group`，所以别的设备也会看到。
 */
export function ManageSpacesDialog() {
  const { t } = useTranslation()
  const open = useManageSpaces((s) => s.open)
  const hide = useManageSpaces((s) => s.hide)
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const { spaces } = useNodeFilter()
  const listId = useId()
  const [drafts, setDrafts] = useState<Record<string, string>>({})
  const [results, setResults] = useState<Record<string, GroupResult>>({})
  const [busy, setBusy] = useState<string | null>(null)
  const [renameKey, setRenameKey] = useState('')
  const [renameTo, setRenameTo] = useState('')
  const [renameResults, setRenameResults] = useState<Array<{ nodeId: string; result: GroupResult }>>([])
  const names = useMemo(() => spaces.spaces.map((s) => s.key), [spaces])
  const draftOf = (id: string): string => drafts[id] ?? groupOf(byId[id]?.info)
  const nodeName = (id: string): string => cleanHintText(byId[id]?.name) || id.slice(0, 8)

  const saveNode = async (id: string): Promise<void> => {
    setBusy(id)
    const r = await setNodeGroup(id, draftOf(id))
    setBusy(null)
    setResults((x) => ({ ...x, [id]: r }))
    if (r.status === 'changed' || r.status === 'unchanged') setDrafts((d) => Object.fromEntries(Object.entries(d).filter(([k]) => k !== id)))
  }

  const doRename = async (): Promise<void> => {
    const space = spaces.spaces.find((s) => s.key === renameKey)
    if (!space) return
    setBusy('rename')
    setRenameResults(await renameSpace(space.nodeIds, renameTo))
    setBusy(null)
  }
  const renameProblem = renameTo.trim() === '' ? null : groupError(renameTo)

  return (
    <Dialog open={open} onOpenChange={(o) => !o && hide()} title={t('spaces.manageTitle')} description={t('spaces.manageHint')} closeLabel={t('common.close')} testId="manage-spaces" size="lg">
      <div className="space-y-6 text-sm">
        <datalist id={listId}>
          {names.map((n) => (
            <option key={n} value={n} />
          ))}
        </datalist>
        <section className="space-y-2" aria-label={t('spaces.nodesSection')}>
          <h3 className="text-xs font-semibold tracking-wide text-fg-muted uppercase">{t('spaces.nodesSection')}</h3>
          <ul className="space-y-2">
            {order.map((id) => {
              const n = byId[id]
              if (!n) return null
              const v = draftOf(id)
              const problem = groupError(v)
              const r = results[id]
              return (
                <li key={id} className="flex flex-wrap items-center gap-2" data-testid="manage-node" data-node={id}>
                  <Dot tone={connectionTone(n.conn)} />
                  <span className="w-40 min-w-0 truncate font-medium">{nodeName(id)}</span>
                  <Input value={v} onChange={(e) => setDrafts((d) => ({ ...d, [id]: e.target.value }))} list={listId} aria-label={t('spaces.groupOf', { node: nodeName(id) })} placeholder={t('spaces.noGroup')} className="min-w-40 flex-1" aria-invalid={problem ? true : undefined} data-testid="manage-group-input" />
                  <Button size="sm" disabled={busy !== null || !!problem || n.conn !== 'ready' || v.trim() === groupOf(n.info)} onClick={() => void saveNode(id)} data-testid="manage-group-save">
                    {t('common.save')}
                  </Button>
                  {n.conn !== 'ready' ? <span className="w-full text-xs text-fg-subtle">{t('spaces.result.offline')}</span> : null}
                  {problem ? (
                    <span className="w-full text-xs text-danger" role="alert">
                      {t(`spaces.errors.${problem}`)}
                    </span>
                  ) : r ? (
                    <span className={r.status === 'changed' || r.status === 'unchanged' ? 'w-full text-xs text-fg-subtle' : 'w-full text-xs text-danger'} role={r.status === 'changed' || r.status === 'unchanged' ? 'status' : 'alert'} data-testid="manage-result">
                      {resultText(t, r)}
                    </span>
                  ) : null}
                </li>
              )
            })}
          </ul>
        </section>
        {spaces.hasGroups ? (
          <section className="space-y-2" aria-label={t('spaces.renameSection')}>
            <h3 className="text-xs font-semibold tracking-wide text-fg-muted uppercase">{t('spaces.renameSection')}</h3>
            <div className="flex flex-wrap items-center gap-2">
              <Select value={renameKey} onChange={(e) => { setRenameKey(e.target.value); setRenameResults([]) }} aria-label={t('spaces.title')} className="w-auto max-w-48" data-testid="rename-space">
                <option value="">{t('spaces.pickSpace')}</option>
                {spaces.spaces.map((sp) => (
                  <option key={sp.key} value={sp.key}>
                    {sp.label}
                  </option>
                ))}
              </Select>
              <Input value={renameTo} onChange={(e) => setRenameTo(e.target.value)} aria-label={t('spaces.newName')} placeholder={t('spaces.newName')} className="min-w-40 flex-1" aria-invalid={renameProblem ? true : undefined} data-testid="rename-to" />
              <Button size="sm" variant="primary" disabled={busy !== null || renameKey === '' || renameTo.trim() === '' || !!renameProblem} onClick={() => void doRename()} data-testid="rename-apply">
                {t('spaces.rename')}
              </Button>
            </div>
            {renameProblem ? (
              <p className="text-xs text-danger" role="alert">
                {t(`spaces.errors.${renameProblem}`)}
              </p>
            ) : null}
            {renameResults.length > 0 ? (
              <ul className="space-y-1" data-testid="rename-results">
                {renameResults.map((r) => (
                  <li key={r.nodeId} className={r.result.status === 'changed' || r.result.status === 'unchanged' ? 'text-fg-muted' : 'text-danger'} data-status={r.result.status}>
                    {nodeName(r.nodeId)}: {resultText(t, r.result)}
                  </li>
                ))}
              </ul>
            ) : null}
          </section>
        ) : null}
      </div>
    </Dialog>
  )
}
