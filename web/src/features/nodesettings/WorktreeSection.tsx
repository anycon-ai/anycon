import { useCallback, useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { GitBranch, RefreshCw, Trash2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { WorktreeInfo, WorkspaceWorktreesResult } from '@/protocol'
import { describeGitError, gitErrorReason } from '@/features/worktree/errors'
import { formatBytes, formatRelative } from '@/lib/format'
import { useNodeParam } from '@/lib/nav'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { CheckboxField } from '@/ui/Choice'
import { Card, Chip, SectionTitle, Spinner, type Tone } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

const STATE_TONE: Record<WorktreeInfo['state'], Tone> = { creating: 'info', active: 'success', kept: 'warning', removed: 'neutral', missing: 'danger', failed: 'danger' }

type Load = { status: 'loading' } | { status: 'ready'; worktrees: WorktreeInfo[] } | { status: 'error'; error: unknown }

/**
 * 节点设置 → Workspace → Worktree（web.md § 4.8，ADR-34）：anycon 建的全部 worktree，含已保留的、会话已删除的。
 * 删除（`workspace.remove_worktree`）：会话还没归档的禁用并提示先归档；有未提交的改动要勾「丢弃未提交的改动」（`force`）；
 * 另可同时删分支（`delete_branch`，分支上有自己的提交时警告）。
 */
export function WorktreeSection({ nodeId }: { nodeId: string }) {
  const { t, i18n } = useTranslation()
  const entry = useNodes((s) => s.byId[nodeId])
  const sessions = useSessions((s) => s.list)
  const nodeParam = useNodeParam(nodeId)
  const offline = entry?.conn !== 'ready'
  const supported = !!entry?.info?.capabilities.worktree
  const [load, setLoad] = useState<Load>({ status: 'loading' })
  const [reload, setReload] = useState(0)
  const [target, setTarget] = useState<WorktreeInfo | null>(null)
  const [force, setForce] = useState(false)
  const [deleteBranch, setDeleteBranch] = useState(false)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    if (offline || !supported) return
    let alive = true
    rpc()
      .request<WorkspaceWorktreesResult>(nodeId, 'workspace.worktrees', {})
      .then((res) => alive && setLoad({ status: 'ready', worktrees: res.worktrees }))
      .catch((error: unknown) => {
        console.warn('anycon: could not list the worktrees', error)
        if (alive) setLoad({ status: 'error', error })
      })
    return () => {
      alive = false
    }
  }, [nodeId, offline, supported, reload])

  const refresh = useCallback(() => setReload((n) => n + 1), [])

  const ask = (w: WorktreeInfo): void => {
    setTarget(w)
    setForce(false)
    setDeleteBranch(false)
  }

  const remove = async (): Promise<void> => {
    if (!target) return
    setBusy(true)
    try {
      await rpc().request(nodeId, 'workspace.remove_worktree', { id: target.id, ...(force ? { force: true } : {}), ...(deleteBranch ? { delete_branch: true } : {}) })
      setTarget(null)
      toast.success(t('worktree.removed', { branch: target.branch }))
      refresh()
    } catch (e) {
      toast.error(describeGitError(e, t))
      // 状态变了（比如这期间又有了未提交的改动、会话被取消归档）：重新列一次，让界面和 daemon 一致
      if (e instanceof RpcCallError && e.kind === 'rpc' && (e.code === 'busy' || gitErrorReason(e) === 'uncommitted_changes')) {
        setTarget(null)
        refresh()
      }
    } finally {
      setBusy(false)
    }
  }

  if (!supported) return null
  const worktrees = load.status === 'ready' ? load.worktrees : []
  const lost = target ? target.ahead : 0

  return (
    <section data-testid="worktree-section">
      <SectionTitle
        action={
          <Button size="icon" variant="ghost" aria-label={t('git.refresh')} title={t('git.refresh')} onClick={refresh} disabled={offline} data-testid="worktree-refresh">
            <RefreshCw className="size-4" />
          </Button>
        }
      >
        {t('worktree.title')}
      </SectionTitle>
      <p className="mb-3 text-sm text-fg-muted">{t('worktree.hint')}</p>

      {load.status === 'loading' && !offline ? (
        <div className="flex items-center gap-2 text-sm text-fg-subtle">
          <Spinner /> {t('git.loading')}
        </div>
      ) : null}
      {load.status === 'error' ? (
        <p className="text-sm text-danger" data-testid="worktree-error">
          {describeGitError(load.error, t)}
        </p>
      ) : null}
      {load.status === 'ready' && worktrees.length === 0 ? (
        <p className="text-sm text-fg-subtle" data-testid="worktree-empty">
          {t('worktree.empty')}
        </p>
      ) : null}

      <ul className="space-y-2">
        {worktrees.map((w) => {
          const session = w.session_id ? sessions[w.session_id] : undefined
          // 所属会话还没归档：daemon 会拒绝（busy），这里先禁用并说明
          const blocked = !!session && !session.archived
          return (
            <li key={w.id} data-testid="worktree-row" data-id={w.id} data-state={w.state}>
              <Card className="space-y-2 p-3">
                <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                  <span className="min-w-0 truncate font-medium" data-testid="worktree-project">
                    {w.project}
                  </span>
                  <span className="inline-flex min-w-0 items-center gap-1 font-mono text-xs text-fg-muted" data-testid="worktree-branch">
                    <GitBranch className="size-3 shrink-0" aria-hidden />
                    <span className="truncate">{w.branch}</span>
                    <span className="shrink-0">← {w.base}</span>
                  </span>
                  <Chip tone={STATE_TONE[w.state]} testId="worktree-state" dataState={w.state}>
                    {t(`worktree.state.${w.state}`)}
                  </Chip>
                </div>
                <p className="text-xs text-fg-subtle">
                  {w.session_id ? (
                    <Link to="/n/$node/s/$session" params={{ node: nodeParam, session: w.session_id }} className="text-accent-text hover:underline" data-testid="worktree-session">
                      {session?.title || w.session_title || w.session_id}
                    </Link>
                  ) : (
                    <span data-testid="worktree-no-session">{t('worktree.noSession')}</span>
                  )}
                  {' · '}
                  <span data-testid="worktree-uncommitted">{t('git.uncommitted', { count: w.uncommitted })}</span>
                  {' · '}
                  <span data-testid="worktree-ahead">{t('worktree.ownCommits', { count: w.ahead })}</span>
                  {w.bytes != null ? ` · ${formatBytes(w.bytes)}` : ''}
                  {' · '}
                  {formatRelative(w.created_at_ms, i18n.language)}
                </p>
                <div className="flex flex-wrap items-center gap-2">
                  <Button variant="outline-danger" icon={<Trash2 className="size-4" />} disabled={offline || blocked || busy} onClick={() => ask(w)} data-testid="worktree-remove">
                    {t('worktree.remove')}
                  </Button>
                  {blocked ? (
                    <span className="text-xs text-fg-subtle" data-testid="worktree-archive-first">
                      {t('worktree.archiveFirst')}
                    </span>
                  ) : null}
                </div>
              </Card>
            </li>
          )
        })}
      </ul>

      <Dialog
        open={target !== null}
        onOpenChange={(open) => !open && !busy && setTarget(null)}
        title={t('worktree.removeTitle', { branch: target?.branch ?? '' })}
        description={target ? t('worktree.removeBody', { project: target.project }) : undefined}
        closeLabel={t('common.close')}
        testId="worktree-remove-dialog"
      >
        {target ? (
          <div className="space-y-3">
            {target.uncommitted > 0 ? (
              <CheckboxField
                id="worktree-remove-force"
                testId="worktree-remove-force"
                checked={force}
                onCheckedChange={setForce}
                label={t('worktree.discardChanges', { count: target.uncommitted })}
                hint={t('worktree.discardHint')}
              />
            ) : null}
            <div>
              <CheckboxField id="worktree-remove-delete-branch" testId="worktree-remove-delete-branch" checked={deleteBranch} onCheckedChange={setDeleteBranch} label={t('worktree.alsoDeleteBranch', { branch: target.branch })} />
              {lost > 0 ? (
                <p className={deleteBranch ? 'ml-7 text-xs font-medium text-danger' : 'ml-7 text-xs text-warning'} role={deleteBranch ? 'alert' : undefined} data-testid="worktree-remove-commits-lost">
                  {t('worktree.dispose.commitsLost', { count: lost })}
                </p>
              ) : null}
            </div>
            <div className="flex justify-end gap-2">
              <Button variant="outline" onClick={() => setTarget(null)} disabled={busy}>
                {t('common.cancel')}
              </Button>
              <Button variant="danger" loading={busy} disabled={target.uncommitted > 0 && !force} onClick={() => void remove()} data-testid="worktree-remove-confirm">
                {t('worktree.remove')}
              </Button>
            </div>
          </div>
        ) : null}
      </Dialog>
    </section>
  )
}
