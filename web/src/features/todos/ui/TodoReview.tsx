import { lazy, Suspense, useEffect, useMemo, useRef, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { ChevronLeft, ChevronRight, ExternalLink } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { DiffFile } from '@/protocol'
import { Markdown } from '@/components/Markdown'
import { archiveSession, deleteSession, sendMessage } from '@/features/session/actions'
import { WorktreeDisposePicker, effectiveChoice, type DisposeChoice, type DisposeInfo } from '@/features/worktree/DisposeDialog'
import { loadDiffPatch } from '@/features/worktree/diff'
import { describeGitError } from '@/features/worktree/errors'
import { formatDateTime } from '@/lib/format'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { useGitEntry } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Textarea } from '@/ui/Field'
import { Chip, Spinner } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'
import { setTodoState } from '../actions'
import { activityOf } from '../activity'
import { latestResult, loadHistory, useHistoryEntry } from '../history'
import { commitPushAndOpenPr, countDenials, diffAgainstFor, hasNoPush, latestLinkedSession, loadChangeSummary, type ChangeSummary } from '../review'
import type { ViewTodo } from '../types'
import { cleanOneLine } from '../validate'
import { SessionRef } from './TodoHistory'

const DiffView = lazy(() => import('@/features/session/ui/DiffView').then((m) => ({ default: m.DiffView })))

/**
 * 验收页的内容（`/todos/:node/:todo/review`，spec/web.md § 4.12）：先给结论（agent 最后一段话、改了几个文件和行数、审查器拒绝过几次），
 * 然后是文件列表，点开看 diff；三个动作固定在底部：通过、再改改…、不要了。通过之后可以让 agent 提交、推送并开 PR——那一步要先把这个会话的
 * 「不 push」关掉，必须由用户明确确认。手机上一个文件一屏，左右切换。
 */
export function TodoReview({ item, online }: { item: ViewTodo; online: boolean }) {
  const { t, i18n } = useTranslation()
  const nodeId = item.node_id
  const order = useNodes((s) => s.order)
  const session = useSessions((s) => latestLinkedSession(item.sessions, s.list))
  const history = useHistoryEntry(nodeId, item.id)
  const result = latestResult(history.events)
  const activity = activityOf(item)
  const closed = item.state === 'done' || item.state === 'dropped'

  // ---- 结论 ----
  useEffect(() => {
    if (online) void loadHistory(nodeId, item.id)
  }, [online, nodeId, item.id])

  // ---- 改动统计 ----
  const [changes, setChanges] = useState<{ phase: 'loading' | 'ready' | 'error'; summary?: ChangeSummary; error?: unknown }>({ phase: 'loading' })
  const sessionId = session?.id
  const lastSeq = session?.last_seq
  const worktree = session?.worktree
  useEffect(() => {
    if (!session || !online) return
    let live = true
    setChanges((c) => ({ ...c, phase: c.summary ? 'ready' : 'loading' }))
    loadChangeSummary(nodeId, session).then(
      (summary) => live && setChanges({ phase: 'ready', summary }),
      (error) => live && setChanges({ phase: 'error', error }),
    )
    return () => {
      live = false
    }
    // 会话有新事件（agent 又改了东西）就重新读；对象本身每次都是新的，用 id / last_seq / worktree 状态当依赖
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [online, nodeId, sessionId, lastSeq, worktree?.state])

  // ---- 审查器拒绝过几次：只数本地缓存的事件，没有就链接到会话 ----
  const [denials, setDenials] = useState<number | null | undefined>(undefined)
  useEffect(() => {
    if (!sessionId) return
    let live = true
    void countDenials(sessionId).then((n) => live && setDenials(n))
    return () => {
      live = false
    }
  }, [sessionId, lastSeq])

  // ---- 文件：一次一个 ----
  const files = changes.summary?.files ?? []
  const [fileIdx, setFileIdx] = useState(0)
  const idx = Math.min(fileIdx, Math.max(0, files.length - 1))
  const file = files[idx]
  const [patches, setPatches] = useState<Record<string, DiffFile | null | 'loading' | 'error'>>({})
  const against = session ? diffAgainstFor(session) : 'uncommitted'
  useEffect(() => {
    if (!session || !file || !online) return
    const path = file.path
    setPatches((p) => ({ ...p, [path]: 'loading' }))
    let live = true
    loadDiffPatch(nodeId, session.id, against, path).then(
      (f) => live && setPatches((p) => ({ ...p, [path]: f })),
      () => live && setPatches((p) => ({ ...p, [path]: 'error' })),
    )
    return () => {
      live = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [online, nodeId, sessionId, lastSeq, file?.path, against])
  const patch = file ? patches[file.path] : undefined
  const touch = useRef<number | null>(null)
  const go = (d: 1 | -1): void => setFileIdx(Math.min(files.length - 1, Math.max(0, idx + d)))

  // ---- 动作 ----
  const [reworking, setReworking] = useState(false)
  const [text, setText] = useState('')
  const [sending, setSending] = useState(false)
  const archived = session?.archived === true
  const canRework = !!session && !archived && online && !closed
  const rework = async (): Promise<void> => {
    if (!session || text.trim() === '' || sending) return
    setSending(true)
    try {
      const r = await sendMessage(nodeId, session.id, text.trim())
      if (r.state === 'failed') toast.error(r.reason === 'archived' ? t('session.archivedHint') : t('todos.reviewPage.reworkFailed'))
      else {
        toast.info(r.state === 'queued' ? t('todos.reviewPage.reworkQueued') : t('todos.reviewPage.reworkSent'))
        setText('')
        setReworking(false)
      }
    } finally {
      setSending(false)
    }
  }

  const [dropCleanup, setDropCleanup] = useState(false)
  const drop = async (): Promise<void> => {
    await setTodoState(item, 'dropped')
    // 干活的 worktree 还在：问一句要不要连它（和分支）一起清掉
    if (session?.worktree && (session.worktree.state === 'active' || session.worktree.state === 'kept')) setDropCleanup(true)
  }

  const [pushing, setPushing] = useState(false)
  const [confirmPush, setConfirmPush] = useState(false)
  const runPush = async (confirmed: boolean): Promise<void> => {
    if (!session || pushing) return
    setPushing(true)
    try {
      const r = await commitPushAndOpenPr(nodeId, session, t, confirmed)
      if (r.ok) toast.info(t('todos.reviewPage.pushSent'))
      else toast.error(r.step === 'settings' ? t('todos.reviewPage.pushSettingsFailed') : t('todos.reviewPage.pushSendFailed'))
    } finally {
      setPushing(false)
      setConfirmPush(false)
    }
  }
  const startPush = (): void => {
    if (!session) return
    // 带着「不 push」的会话：先问清楚，用户确认之后才去关它
    if (hasNoPush(session)) setConfirmPush(true)
    else void runPush(false)
  }

  const node = shortNodeId(nodeId, order)
  const title = cleanOneLine(item.title)

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="todo-review">
      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto p-4">
        <div className="mx-auto max-w-3xl space-y-5">
          <header className="space-y-1">
            <h2 className="text-lg font-semibold [overflow-wrap:anywhere]" data-testid="review-title">
              {title}
            </h2>
            <div className="flex flex-wrap items-center gap-2 text-sm">
              {activity ? <Chip tone={activity === 'review' ? 'success' : activity === 'waiting' ? 'warning' : 'info'} testId="review-activity" dataState={activity}>{t(`todos.activity.${activity}`)}</Chip> : null}
              {closed ? <Chip testId="review-closed" dataState={item.state}>{t(`todos.states.${item.state}`)}</Chip> : null}
              <Link to="/todos/$node/$todo" params={{ node, todo: item.id }} className="text-accent-text hover:underline" data-testid="review-back-to-item">
                {t('todos.reviewPage.backToItem')}
              </Link>
            </div>
            {!activity && !closed ? <p className="text-sm text-fg-muted" data-testid="review-not-ready">{t('todos.reviewPage.notReady')}</p> : null}
            {activity && activity !== 'review' ? <p className="text-sm text-fg-muted" data-testid="review-not-ready">{t('todos.reviewPage.notReady')}</p> : null}
          </header>

          <section className="space-y-2" aria-labelledby="review-conclusion" data-testid="review-conclusion">
            <h3 id="review-conclusion" className="text-sm font-semibold">
              {t('todos.reviewPage.conclusion')}
            </h3>
            {result ? (
              <div className="space-y-1">
                <p className="text-xs text-fg-subtle">{formatDateTime(result.ts_ms, i18n.language)}</p>
                {result.text ? (
                  <div className="rounded-md border border-border p-3" data-testid="review-result">
                    <Markdown text={result.text} />
                  </div>
                ) : (
                  <p className="text-sm text-fg-muted" data-testid="review-no-text">{t('todos.history.noText')}</p>
                )}
                {result.truncated ? <p className="text-xs text-fg-subtle">{t('todos.history.truncated')}</p> : null}
              </div>
            ) : history.loading || !history.loaded ? (
              <Spinner className="size-5" />
            ) : (
              <p className="text-sm text-fg-muted" data-testid="review-no-result">{t('todos.reviewPage.noResult')}</p>
            )}
            {session ? (
              <div className="flex flex-wrap items-center gap-3 text-sm" data-testid="review-summary">
                {changes.phase === 'ready' && changes.summary ? (
                  <span data-testid="review-changes">
                    {changes.summary.files.length === 0 ? t('todos.reviewPage.noChanges') : t('todos.reviewPage.changes', { count: changes.summary.files.length })}
                    {changes.summary.files.length > 0 ? (
                      <>
                        {' '}
                        <span className="text-success">+{changes.summary.additions}</span> <span className="text-danger">−{changes.summary.deletions}</span>
                      </>
                    ) : null}
                    {changes.summary.truncated ? ` ${t('todos.reviewPage.truncatedList')}` : ''}
                  </span>
                ) : changes.phase === 'error' ? (
                  <span className="text-danger" data-testid="review-changes-error">{describeGitError(changes.error, t)}</span>
                ) : (
                  <Spinner className="size-4" />
                )}
                {denials === undefined ? null : denials === null ? (
                  <span className="text-fg-muted" data-testid="review-denials-unknown">
                    {t('todos.reviewPage.denialsUnknown')}
                  </span>
                ) : (
                  <span className={denials > 0 ? 'text-warning' : 'text-fg-muted'} data-testid="review-denials">
                    {t('todos.reviewPage.denials', { count: denials })}
                  </span>
                )}
                <SessionRef nodeId={nodeId} sessionId={session.id} label={t('todos.reviewPage.openSession')} />
              </div>
            ) : (
              <p className="text-sm text-fg-muted" data-testid="review-no-session">{t('todos.reviewPage.noSession')}</p>
            )}
          </section>

          {session && changes.phase === 'ready' && files.length > 0 ? (
            <section className="space-y-2" aria-labelledby="review-files" data-testid="review-files">
              <h3 id="review-files" className="text-sm font-semibold">
                {t('todos.reviewPage.files')}
              </h3>
              <div className="grid gap-3 md:grid-cols-[14rem_1fr]">
                <ul className="max-h-60 space-y-0.5 overflow-y-auto text-sm md:max-h-96 max-md:hidden" aria-label={t('todos.reviewPage.files')}>
                  {files.map((f, i) => (
                    <li key={f.path}>
                      <button type="button" onClick={() => setFileIdx(i)} aria-current={i === idx ? 'true' : undefined} className={`flex w-full items-center gap-1.5 rounded px-2 py-1 text-left font-mono text-xs hover:bg-bg-hover ${i === idx ? 'bg-accent-subtle' : ''}`} data-testid="review-file" data-path={f.path}>
                        <span className="min-w-0 flex-1 truncate" title={f.path}>
                          {f.path}
                        </span>
                        <span className="shrink-0 text-success">+{f.additions}</span>
                        <span className="shrink-0 text-danger">−{f.deletions}</span>
                      </button>
                    </li>
                  ))}
                </ul>
                <div
                  className="min-w-0 space-y-2"
                  onTouchStart={(e) => (touch.current = e.touches[0]?.clientX ?? null)}
                  onTouchEnd={(e) => {
                    const x0 = touch.current
                    touch.current = null
                    const x1 = e.changedTouches[0]?.clientX
                    if (x0 === null || x1 === undefined) return
                    if (x1 - x0 > 60) go(-1)
                    else if (x0 - x1 > 60) go(1)
                  }}
                  data-testid="review-viewer"
                >
                  <div className="flex items-center gap-2">
                    <Button size="sm" variant="outline" disabled={idx === 0} onClick={() => go(-1)} aria-label={t('todos.reviewPage.prevFile')} icon={<ChevronLeft className="size-4" aria-hidden />} data-testid="review-prev" />
                    <p className="min-w-0 flex-1 truncate text-center font-mono text-xs" data-testid="review-file-name">
                      {file?.path} <span className="text-fg-subtle">({idx + 1} / {files.length})</span>
                    </p>
                    <Button size="sm" variant="outline" disabled={idx >= files.length - 1} onClick={() => go(1)} aria-label={t('todos.reviewPage.nextFile')} icon={<ChevronRight className="size-4" aria-hidden />} data-testid="review-next" />
                  </div>
                  <div className="rounded-md border border-border" data-testid="review-diff">
                    {patch === 'loading' || patch === undefined ? (
                      <div className="p-3"><Spinner className="size-5" /></div>
                    ) : patch === 'error' ? (
                      <p className="p-3 text-sm text-danger">{t('todos.reviewPage.patchFailed')}</p>
                    ) : patch === null || (patch.patch ?? '') === '' ? (
                      <p className="p-3 text-sm text-fg-muted">{file?.status === 'deleted' || patch?.binary ? t('todos.reviewPage.noPatchBinary') : t('todos.reviewPage.noPatch')}</p>
                    ) : (
                      <Suspense fallback={<div className="p-3"><Spinner className="size-5" /></div>}>
                        <DiffView diff={patch.patch ?? ''} />
                        {patch.patch_truncated ? <p className="border-t border-border p-2 text-xs text-fg-subtle">{t('todos.reviewPage.patchTruncated')}</p> : null}
                      </Suspense>
                    )}
                  </div>
                </div>
              </div>
              {session ? (
                <Link to="/n/$node/s/$session" params={{ node, session: session.id }} search={{ view: 'changes' } as never} className="inline-flex items-center gap-1 text-sm text-accent-text hover:underline" data-testid="review-open-changes">
                  <ExternalLink className="size-3.5" aria-hidden />
                  {t('todos.reviewPage.openChanges')}
                </Link>
              ) : null}
            </section>
          ) : null}
        </div>
      </div>

      {/* 三个动作固定在底部，手机上一直在 */}
      <div className="pb-safe shrink-0 border-t border-border bg-bg px-3 py-2" data-testid="review-actions">
        <div className="mx-auto max-w-3xl space-y-2">
          {reworking ? (
            <div className="space-y-2" data-testid="review-rework-box">
              <Textarea rows={3} value={text} onChange={(e) => setText(e.target.value)} placeholder={t('todos.reviewPage.reworkPlaceholder')} aria-label={t('todos.reviewPage.reworkPlaceholder')} data-testid="review-rework-input" />
              <div className="flex justify-end gap-2">
                <Button onClick={() => setReworking(false)} data-testid="review-rework-cancel">
                  {t('common.cancel')}
                </Button>
                <Button variant="primary" disabled={sending || text.trim() === ''} onClick={() => void rework()} data-testid="review-rework-send">
                  {t('todos.reviewPage.reworkSend')}
                </Button>
              </div>
            </div>
          ) : null}
          {closed && item.state === 'done' && session && !archived ? (
            <div className="flex flex-wrap items-center gap-2 text-sm" data-testid="review-after">
              <span className="text-fg-muted">{t('todos.reviewPage.approved')}</span>
              <Button size="sm" disabled={pushing || !online} onClick={startPush} data-testid="review-push">
                {t('todos.reviewPage.pushAction')}
              </Button>
            </div>
          ) : null}
          <div className="flex flex-wrap items-center justify-end gap-2">
            <Button variant="primary" disabled={closed} onClick={() => void setTodoState(item, 'done')} data-testid="review-approve">
              {t('todos.reviewPage.approve')}
            </Button>
            <Button disabled={!canRework} onClick={() => setReworking((r) => !r)} aria-expanded={reworking} data-testid="review-rework">
              {t('todos.reviewPage.rework')}
            </Button>
            <Button variant="outline-danger" disabled={closed} onClick={() => void drop()} data-testid="review-drop">
              {t('todos.reviewPage.drop')}
            </Button>
          </div>
        </div>
      </div>

      <Dialog open={confirmPush} onOpenChange={(o) => !pushing && setConfirmPush(o)} title={t('todos.reviewPage.pushConfirmTitle')} description={t('todos.reviewPage.pushConfirmBody')} closeLabel={t('common.close')} testId="review-push-dialog">
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirmPush(false)} disabled={pushing} data-testid="review-push-cancel">
            {t('common.cancel')}
          </Button>
          <Button variant="danger" disabled={pushing} onClick={() => void runPush(true)} data-testid="review-push-confirm">
            {t('todos.reviewPage.pushConfirm')}
          </Button>
        </div>
      </Dialog>

      {session?.worktree ? <DropCleanupDialog open={dropCleanup} onOpenChange={setDropCleanup} nodeId={nodeId} sessionId={session.id} branch={session.worktree.branch} /> : null}
    </div>
  )
}

/** 「不要了」之后：干活的 worktree（和分支）要不要一起清掉。复用归档 / 删除会话时的 worktree 处理选项。 */
function DropCleanupDialog({ open, onOpenChange, nodeId, sessionId, branch }: { open: boolean; onOpenChange: (o: boolean) => void; nodeId: string; sessionId: string; branch: string }) {
  const { t } = useTranslation()
  const git = useGitEntry(sessionId)
  const info: DisposeInfo = useMemo(() => ({ branch, uncommitted: git.status?.uncommitted ?? 0, ahead: git.status?.ahead ?? 0, unpushed: git.status?.unpushed ?? 0 }), [branch, git.status])
  const [choice, setChoice] = useState<DisposeChoice>({ worktree: 'remove', deleteBranch: false })
  const [busy, setBusy] = useState(false)
  useEffect(() => {
    if (open) setChoice({ worktree: 'remove', deleteBranch: false })
  }, [open])
  const confirm = async (): Promise<void> => {
    const eff = effectiveChoice(choice, info)
    setBusy(true)
    try {
      // 删分支只有删除会话时才能一起做；只清 worktree 就归档会话（会话和它的记录留着）
      if (eff.deleteBranch) await deleteSession(nodeId, sessionId, { worktree: eff.worktree, deleteBranch: true })
      else await archiveSession(nodeId, sessionId, true, { worktree: eff.worktree })
      onOpenChange(false)
    } catch (e) {
      toast.error(describeGitError(e, t))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open={open} onOpenChange={(o) => !busy && onOpenChange(o)} title={t('todos.reviewPage.cleanupTitle')} description={t('todos.reviewPage.cleanupBody', { branch: cleanHintText(branch) })} closeLabel={t('common.close')} testId="review-cleanup-dialog">
      <div className="space-y-4">
        <WorktreeDisposePicker idPrefix="review-cleanup" info={info} value={choice} onChange={setChoice} withBranch />
        <div className="flex justify-end gap-2">
          <Button onClick={() => onOpenChange(false)} disabled={busy} data-testid="review-cleanup-keep">
            {t('todos.reviewPage.cleanupKeep')}
          </Button>
          <Button variant="danger" disabled={busy || choice.worktree === 'keep'} onClick={() => void confirm()} data-testid="review-cleanup-confirm">
            {t('todos.reviewPage.cleanupDo')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
