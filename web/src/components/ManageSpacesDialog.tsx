import { useId, useState } from 'react'
import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import { renameSpace, setNodeGroup, type GroupResult } from '@/features/spaces/manage'
import { cleanHintText } from '@/lib/hint'
import { useNodeFilter } from '@/lib/nodeFilter'
import { displayName, groupError, groupOf } from '@/lib/spaces'
import { useManageSpaces } from '@/stores/manageSpaces'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
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
  const [renameNote, setRenameNote] = useState<string | null>(null)
  const [mergeConfirmed, setMergeConfirmed] = useState(false)
  const draftOf = (id: string): string => drafts[id] ?? groupOf(byId[id]?.info)
  const nodeName = (id: string): string => cleanHintText(byId[id]?.name) || id.slice(0, 8)

  const saveNode = async (id: string): Promise<void> => {
    setBusy(id)
    const r = await setNodeGroup(id, draftOf(id))
    setBusy(null)
    setResults((x) => ({ ...x, [id]: r }))
    if (r.status === 'changed' || r.status === 'unchanged') setDrafts((d) => Object.fromEntries(Object.entries(d).filter(([k]) => k !== id)))
  }

  const target = renameTo.trim()
  const fromSpace = spaces.spaces.find((s) => s.key === renameKey)
  // 改成一个已经存在的、别的空间的名字：两个空间会合并，先说清楚谁会进哪里，让用户确认
  const mergeInto = fromSpace && target !== '' && target !== renameKey ? spaces.spaces.find((s) => s.key === target) : undefined
  // 看起来和一个已有的空间一样、原文却不一样（不可见字符、NFD、多余的空格）：改完会多出一个长得一样的空间
  const lookalike = fromSpace && target !== '' && !mergeInto && target !== renameKey ? spaces.spaces.find((s) => s.key !== fromSpace.key && displayName(s.key) === displayName(target)) : undefined

  const doRename = async (): Promise<void> => {
    if (!fromSpace) {
      // 打开对话框之后这个空间没了（别的设备把它的节点都改了分组）：别悄悄什么都不做
      setRenameNote(t('spaces.renameGone'))
      return
    }
    if (mergeInto && !mergeConfirmed) {
      setMergeConfirmed(true)
      return
    }
    setBusy('rename')
    setRenameNote(null)
    const results = await renameSpace(fromSpace.nodeIds, renameTo)
    setBusy(null)
    setMergeConfirmed(false)
    setRenameResults(results)
    const done = results.filter((r) => r.result.status === 'changed' || r.result.status === 'unchanged')
    if (done.length > 0) {
      // 选中的正是这个空间：选择跟着改名走（不然偏好里还是旧名字，这个空间已经没有节点，选择会退回「全部」）
      if (usePrefs.getState().sidebarSpace === fromSpace.key) usePrefs.getState().set({ sidebarSpace: target })
      const left = results.filter((r) => !done.includes(r))
      if (left.length > 0) setRenameNote(t('spaces.stillOld', { old: fromSpace.label, nodes: left.map((r) => nodeName(r.nodeId)).join(', ') }))
    }
  }
  const renameProblem = renameTo.trim() === '' ? null : groupError(renameTo)

  return (
    <Dialog open={open} onOpenChange={(o) => !o && hide()} title={t('spaces.manageTitle')} description={t('spaces.manageHint')} closeLabel={t('common.close')} testId="manage-spaces" size="lg">
      <div className="space-y-6 text-sm">
        <datalist id={listId}>
          {spaces.spaces.map((sp) => (
            // 选项的值是原文（选了就是这个名字），显示的是清理过的文字
            <option key={sp.key} value={sp.key} label={sp.label} />
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
          <section className="space-y-2" aria-label={t('spaces.overview')}>
            <h3 className="text-xs font-semibold tracking-wide text-fg-muted uppercase">{t('spaces.overview')}</h3>
            <ul className="space-y-1" data-testid="space-overview">
              {spaces.spaces.map((sp) => (
                <li key={sp.key} data-testid="space-overview-item">
                  <span className="font-medium">{sp.label}</span>: {sp.nodeIds.map(nodeName).join(', ')}
                  {displayName(sp.key) !== sp.key ? (
                    <span className="ml-2 text-xs text-warning" data-testid="space-overview-odd">
                      {t('spaces.oddName')}
                    </span>
                  ) : null}
                </li>
              ))}
            </ul>
          </section>
        ) : null}
        {spaces.hasGroups ? (
          <section className="space-y-2" aria-label={t('spaces.renameSection')}>
            <h3 className="text-xs font-semibold tracking-wide text-fg-muted uppercase">{t('spaces.renameSection')}</h3>
            <div className="flex flex-wrap items-center gap-2">
              <Select value={renameKey} onChange={(e) => { setRenameKey(e.target.value); setRenameResults([]); setRenameNote(null); setMergeConfirmed(false) }} aria-label={t('spaces.title')} className="w-auto max-w-48" data-testid="rename-space">
                <option value="">{t('spaces.pickSpace')}</option>
                {spaces.spaces.map((sp) => (
                  <option key={sp.key} value={sp.key}>
                    {sp.label}
                  </option>
                ))}
              </Select>
              <Input value={renameTo} onChange={(e) => { setRenameTo(e.target.value); setMergeConfirmed(false) }} aria-label={t('spaces.newName')} placeholder={t('spaces.newName')} className="min-w-40 flex-1" aria-invalid={renameProblem ? true : undefined} data-testid="rename-to" />
              <Button size="sm" variant="primary" disabled={busy !== null || renameKey === '' || renameTo.trim() === '' || !!renameProblem} onClick={() => void doRename()} data-testid="rename-apply">
                {mergeInto && mergeConfirmed ? t('spaces.mergeAction') : t('spaces.rename')}
              </Button>
            </div>
            {lookalike ? (
              <div className="flex flex-wrap items-center gap-2 rounded-md border border-warning/40 bg-warning-subtle p-2 text-xs" role="alert" data-testid="lookalike-warning">
                <span className="min-w-0 flex-1">{t('spaces.lookalike', { name: displayName(target), existing: lookalike.label })}</span>
                <Button size="sm" onClick={() => { setRenameTo(lookalike.key); setMergeConfirmed(false) }} data-testid="lookalike-use-existing">
                  {t('spaces.useExisting')}
                </Button>
              </div>
            ) : null}
            {mergeInto && mergeConfirmed && fromSpace ? (
              <p role="alert" className="rounded-md border border-warning/40 bg-warning-subtle p-2" data-testid="merge-confirm">
                {t('spaces.mergeConfirm', { from: fromSpace.label, to: mergeInto.label, nodes: [...fromSpace.nodeIds, ...mergeInto.nodeIds].map(nodeName).join(', ') })}
              </p>
            ) : null}
            {renameNote ? (
              <p role="status" className="text-xs text-danger" data-testid="rename-note">
                {renameNote}
              </p>
            ) : null}
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
