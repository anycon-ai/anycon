import { lazy, Suspense, useEffect, useRef, useState, type ReactNode } from 'react'
import { AlertTriangle, Check, ChevronDown, ChevronRight, Clock, ExternalLink, GitBranch, GitMerge, GitPullRequest, MoreHorizontal, RefreshCw, Sparkles, X } from 'lucide-react'
import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import type { DiffAgainst, DiffFile, PullRequestInfo, SessionSyncBaseResult, SessionWorktree } from '@/protocol'
import { copyText } from '@/lib/clipboard'
import { formatBytes } from '@/lib/format'
import { useGitEntry } from '@/stores/git'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Menu, MenuContent, MenuItem, MenuTrigger } from '@/ui/Menu'
import { Card, Chip, Segmented, Spinner } from '@/ui/Misc'
import { loadDiffList, loadDiffPatch } from './diff'
import { describeGitError, gitErrorOutput, gitErrorReason } from './errors'
import { lookupPullRequest, refreshGitStatus, syncBase } from './git'
import { commitMessage, pullRequestMessage, resolveConflictsMessage } from './messages'
import { sendQuickMessage } from './quick'

const DiffView = lazy(() => import('@/features/session/ui/DiffView').then((m) => ({ default: m.DiffView })))

/** 单个文件 patch 的上限（daemon 截断在 256 KiB）。 */
const PATCH_LIMIT_BYTES = 256 * 1024

export interface ChangesTabProps {
  nodeId: string
  sessionId: string
  worktree?: SessionWorktree | null
  /** turn 在跑：同步 base 禁用（daemon 会回 `busy`）；快捷消息照常发，进入排队。 */
  running: boolean
  offline: boolean
  /** 会话已归档：不能再给 agent 发消息，快捷消息和同步 base 禁用（先取消归档）。 */
  archived?: boolean
  /** 消息流里最近一次 `sync_conflicts` 事件的文件，store 里没有冲突文件时用它。 */
  eventConflictFiles?: readonly string[]
}

type SyncUi = { kind: 'ok'; res: SessionSyncBaseResult; base: string } | { kind: 'error'; error: unknown }

/** 「改动」视图（web.md § 4.6）：状态行、PR、同步 base、快捷消息、相对 base / 未提交的文件列表和 patch。 */
export function ChangesTab({ nodeId, sessionId, worktree, running, offline, archived = false, eventConflictFiles }: ChangesTabProps) {
  const { t } = useTranslation()
  const entry = useGitEntry(sessionId)
  const { status } = entry
  const hasStatus = status !== null
  const [against, setAgainst] = useState<DiffAgainst>(worktree ? 'base' : 'uncommitted')
  const [syncing, setSyncing] = useState<'sync' | 'abort' | null>(null)
  const [sync, setSync] = useState<SyncUi | null>(null)
  const asked = useRef({ status: false, pr: false })
  // worktree 还没建好 / 已经删了 / 目录不见了 / 没建成：cwd 不可用，不去拉，只说明情况
  const unusable = !!worktree && worktree.state !== 'active' && worktree.state !== 'kept'

  // 进入标签：页面那边的第一次拉取还没做（单独渲染这个标签时）就先拉一次状态；有了状态再查一次 PR。
  // PR 只在这里和手动刷新时查，其余刷新不查；这次查询不动 `rev`，文件列表不会因它重新加载。
  useEffect(() => {
    if (offline || unusable) return
    if (entry.status) {
      if (!asked.current.pr) {
        asked.current.pr = true
        void lookupPullRequest(nodeId, sessionId)
      }
    } else if (!asked.current.status && !entry.loading && !entry.error) {
      asked.current.status = true
      void refreshGitStatus(nodeId, sessionId)
    }
  }, [offline, unusable, entry.status, entry.error, entry.loading, nodeId, sessionId])

  const base = worktree?.base ?? status?.base ?? undefined
  const baseSha = worktree?.base_sha ?? status?.base_sha ?? undefined
  const branch = worktree?.branch ?? status?.branch ?? undefined
  const merging = !!status?.merging || (status?.conflicts ?? 0) > 0
  const storedConflicts = entry.conflictFiles.length > 0 ? entry.conflictFiles : (eventConflictFiles ?? [])
  // 状态已知时以状态为准（合并结束了就不再显示冲突）；还不知道时用同步结果 / 事件里的
  const showMerge = status ? merging : storedConflicts.length > 0
  const conflictFiles = showMerge ? storedConflicts : []
  const canSync = !!worktree && worktree.state === 'active' && !archived
  const cannotAsk = offline || archived

  const refresh = (): void => {
    asked.current = { status: true, pr: true }
    void refreshGitStatus(nodeId, sessionId, { includePr: true })
  }

  const ask = (text: string): void => {
    void sendQuickMessage(nodeId, sessionId, text, t)
  }
  const askCommit = (): void => {
    if (branch) ask(commitMessage(t, branch))
  }
  const askPr = (): void => {
    if (branch && base) ask(pullRequestMessage(t, branch, base, baseSha))
  }
  const askResolve = (): void => {
    if (branch) ask(resolveConflictsMessage(t, branch, base ?? ''))
  }

  const runSync = async (abort: boolean): Promise<void> => {
    setSyncing(abort ? 'abort' : 'sync')
    setSync(null)
    try {
      const res = await syncBase(nodeId, sessionId, { abort })
      setSync({ kind: 'ok', res, base: base ?? '' })
    } catch (e) {
      setSync({ kind: 'error', error: e })
    } finally {
      setSyncing(null)
    }
  }

  if (worktree && unusable)
    return (
      <p className="text-sm text-fg-muted" data-testid="changes-unavailable" data-state={worktree.state}>
        {t(`git.tab.${worktree.state}`, { branch: worktree.branch })}
      </p>
    )

  return (
    <div className="space-y-4" data-testid="changes-tab">
      {entry.error && !status ? (
        <div className="space-y-2" data-testid="git-error" data-reason={entry.error.reason}>
          <p className="text-sm text-danger">{gitFetchErrorText(entry.error.reason, entry.error.code, t)}</p>
          <Button size="md" variant="outline" icon={<RefreshCw className="size-4" />} onClick={refresh} disabled={offline} data-testid="git-retry">
            {t('git.retry')}
          </Button>
        </div>
      ) : null}

      {!status && !entry.error ? (
        <div className="flex items-center gap-2 text-sm text-fg-subtle" data-testid="git-loading">
          <Spinner /> {t('git.loading')}
        </div>
      ) : null}

      {status ? (
        <section className="space-y-2" data-testid="git-status">
          <div className="flex min-w-0 items-center gap-1.5 text-sm">
            <GitBranch className="size-4 shrink-0 text-fg-subtle" aria-hidden />
            <span className="min-w-0 truncate font-mono text-xs" title={branch}>
              {branch ?? t('git.detached', { sha: status.head_sha.slice(0, 7) })}
            </span>
            {base ? (
              <span className="shrink-0 font-mono text-xs text-fg-muted" data-testid="git-base">
                ← {base}
              </span>
            ) : null}
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            {worktree && (status.ahead ?? 0) > 0 ? <Chip tone="success" testId="git-ahead">{t('git.ahead', { count: status.ahead ?? 0 })}</Chip> : null}
            {worktree && (status.behind ?? 0) > 0 ? <Chip tone="warning" testId="git-behind">{t('git.behind', { count: status.behind ?? 0 })}</Chip> : null}
            {status.uncommitted > 0 ? <Chip tone="warning" testId="git-uncommitted">{t('git.uncommitted', { count: status.uncommitted })}</Chip> : null}
            {status.conflicts > 0 ? <Chip tone="danger" testId="git-conflicts">{t('git.conflicts', { count: status.conflicts })}</Chip> : null}
            {status.merging ? <Chip tone="warning" testId="git-merging">{t('git.chipMerging')}</Chip> : null}
            {worktree && !status.ahead && !status.behind && status.uncommitted === 0 && !status.merging ? <Chip>{t('git.clean')}</Chip> : null}
            {!worktree && status.uncommitted === 0 && !status.merging ? <Chip>{t('git.clean')}</Chip> : null}
          </div>
          <p className="text-xs text-fg-subtle" data-testid="git-upstream">
            {status.upstream
              ? status.unpushed
                ? t('git.upstreamUnpushed', { upstream: status.upstream, count: status.unpushed })
                : t('git.upstream', { upstream: status.upstream })
              : t('git.notPushed')}
          </p>
          <PullRequestRow pr={entry.pr} prError={entry.prError} prChecked={entry.prChecked} loading={entry.prLoading} />
        </section>
      ) : null}

      {status && entry.error ? <p className="text-xs text-warning" data-testid="git-stale">{t('git.stale')}</p> : null}

      {showMerge ? (
        <Card tone="warning" testId="git-merge-banner" className="space-y-2 p-3">
          <p className="flex items-start gap-1.5 text-sm font-medium">
            <AlertTriangle className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
            {t('git.mergeStopped', { base: base ?? '' })}
          </p>
          {conflictFiles.length > 0 ? (
            <ul className="space-y-0.5 font-mono text-xs" data-testid="git-conflict-files">
              {conflictFiles.map((f) => (
                <li key={f} className="truncate" title={f}>
                  {f}
                </li>
              ))}
            </ul>
          ) : null}
          <div className="flex flex-wrap gap-2">
            <Button variant="primary" icon={<Sparkles className="size-4" />} onClick={askResolve} disabled={cannotAsk} data-testid="git-resolve">
              {t('git.resolveWithAgent')}
            </Button>
            <Button variant="outline-danger" onClick={() => void runSync(true)} loading={syncing === 'abort'} disabled={offline || syncing !== null} data-testid="git-abort">
              {t('git.abortMerge')}
            </Button>
          </div>
        </Card>
      ) : null}

      <section className="flex flex-wrap items-center gap-2" data-testid="git-actions">
        {canSync && !showMerge ? (
          <Button
            variant={(status?.behind ?? 0) > 0 ? 'primary' : 'outline'}
            icon={<GitMerge className="size-4" />}
            onClick={() => void runSync(false)}
            loading={syncing === 'sync'}
            disabled={offline || running || syncing !== null || !hasStatus}
            title={running ? t('git.syncRunning') : undefined}
            data-testid="git-sync"
            data-highlight={(status?.behind ?? 0) > 0}
          >
            {t('git.sync')}
          </Button>
        ) : null}
        <Button variant="outline" icon={<Sparkles className="size-4" />} onClick={askCommit} disabled={cannotAsk || !branch} title={archived ? t('git.archivedNoAsk') : undefined} data-testid="git-commit">
          {t('git.commit')}
        </Button>
        {worktree ? (
          <Button variant="outline" icon={<GitPullRequest className="size-4" />} onClick={askPr} disabled={cannotAsk || !branch || !base} title={archived ? t('git.archivedNoAsk') : undefined} data-testid="git-create-pr">
            {t('git.createPr')}
          </Button>
        ) : null}
        <Button variant="ghost" size="icon" aria-label={t('git.refresh')} title={t('git.refresh')} onClick={refresh} disabled={offline} loading={entry.loading} data-testid="git-refresh">
          <RefreshCw className="size-4" />
        </Button>
        <Menu>
          <MenuTrigger asChild>
            <Button variant="ghost" size="icon" aria-label={t('git.more')} data-testid="git-more">
              <MoreHorizontal className="size-4" />
            </Button>
          </MenuTrigger>
          <MenuContent align="end">
            <MenuItem icon={<Sparkles className="size-4" />} onSelect={askResolve} disabled={cannotAsk || !branch || !showMerge} testId="git-more-resolve">
              {t('git.resolveWithAgent')}
            </MenuItem>
            <MenuItem
              icon={<GitBranch className="size-4" />}
              disabled={!branch}
              testId="git-more-copy"
              onSelect={() => {
                if (!branch) return
                copyText(branch).then(
                  () => toast.success(t('common.copied')),
                  () => toast.error(t('common.copyFailed')),
                )
              }}
            >
              {t('git.copyBranch')}
            </MenuItem>
          </MenuContent>
        </Menu>
      </section>

      {sync ? <SyncResult sync={sync} showMerge={showMerge} onCommit={askCommit} onDismiss={() => setSync(null)} /> : null}

      <DiffSection
        nodeId={nodeId}
        sessionId={sessionId}
        hasWorktree={!!worktree}
        against={against}
        onAgainst={setAgainst}
        rev={entry.rev}
        ready={hasStatus && !offline}
        base={base}
      />
    </div>
  )
}

function gitFetchErrorText(reason: string, code: string, t: TFunction): string {
  const text = t(`git.errors.${reason}`, { defaultValue: '' })
  if (text) return text
  if (code === 'offline' || code === 'undeliverable' || reason === 'offline' || reason === 'undeliverable') return t('errors.nodeOffline')
  if (reason === 'timeout') return t('errors.timeout')
  return t('git.errors.generic')
}

// ---- PR ----

/** PR 的链接只在是 http(s) 时才渲染成链接（URL 来自 `gh` 的输出）。 */
export function safeHttpUrl(url: string): string | null {
  try {
    const u = new URL(url)
    return u.protocol === 'https:' || u.protocol === 'http:' ? u.href : null
  } catch {
    return null
  }
}

function PullRequestRow({ pr, prError, prChecked, loading }: { pr: PullRequestInfo | null; prError: string | null; prChecked: boolean; loading: boolean }) {
  const { t } = useTranslation()
  if (!pr) {
    if (!prChecked) return loading ? <p className="text-xs text-fg-subtle" data-testid="git-pr-loading">{t('git.pr.loading')}</p> : null
    return (
      <p className="text-xs text-fg-subtle" data-testid="git-pr-none" data-reason={prError ?? 'none'}>
        {t(`git.pr.error.${prError ?? 'no_pr'}`, { defaultValue: t('git.pr.error.error') })}
      </p>
    )
  }
  const href = safeHttpUrl(pr.url)
  const label = t('git.pr.label', { number: pr.number })
  const checksTotal = pr.checks_passed + pr.checks_failed + pr.checks_pending
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-sm" data-testid="git-pr" data-state={pr.state}>
      {href ? (
        <a href={href} target="_blank" rel="noopener noreferrer" className="inline-flex min-h-9 items-center gap-1 font-medium text-accent-text hover:underline" title={pr.title} data-testid="git-pr-link">
          <GitPullRequest className="size-4 shrink-0" aria-hidden />
          {label}
          <ExternalLink className="size-3 shrink-0" aria-hidden />
          <span className="sr-only">{t('git.pr.newTab')}</span>
        </a>
      ) : (
        <span className="inline-flex items-center gap-1 font-medium" title={pr.title}>
          <GitPullRequest className="size-4 shrink-0" aria-hidden />
          {label}
        </span>
      )}
      <Chip tone={pr.state === 'merged' ? 'accent' : pr.state === 'open' ? 'success' : pr.state === 'closed' ? 'danger' : 'neutral'} testId="git-pr-state">
        {t(`git.pr.state.${pr.state}`)}
      </Chip>
      {checksTotal > 0 ? (
        <span className="inline-flex items-center gap-2 text-xs" data-testid="git-pr-checks" aria-label={t('git.pr.checks', { passed: pr.checks_passed, failed: pr.checks_failed, pending: pr.checks_pending })} title={t('git.pr.checks', { passed: pr.checks_passed, failed: pr.checks_failed, pending: pr.checks_pending })}>
          <span className="inline-flex items-center gap-0.5 text-success">
            <Check className="size-3" aria-hidden />
            {pr.checks_passed}
          </span>
          <span className="inline-flex items-center gap-0.5 text-danger">
            <X className="size-3" aria-hidden />
            {pr.checks_failed}
          </span>
          <span className="inline-flex items-center gap-0.5 text-fg-muted">
            <Clock className="size-3" aria-hidden />
            {pr.checks_pending}
          </span>
        </span>
      ) : null}
      <p className="w-full truncate text-xs text-fg-subtle" title={pr.title}>
        {pr.title}
      </p>
    </div>
  )
}

// ---- 同步结果 ----

function SyncResult({ sync, showMerge, onCommit, onDismiss }: { sync: SyncUi; showMerge: boolean; onCommit: () => void; onDismiss: () => void }) {
  const { t } = useTranslation()
  let tone: 'info' | 'success' | 'warning' | 'danger' = 'info'
  let outcome: string
  let body: ReactNode
  if (sync.kind === 'ok') {
    const { res, base } = sync
    outcome = res.outcome
    if (res.outcome === 'up_to_date') body = t('git.sync_result.up_to_date', { base })
    else if (res.outcome === 'merged') {
      tone = 'success'
      body = t('git.sync_result.merged', { base, count: res.commits })
    } else if (res.outcome === 'conflicts') {
      tone = 'warning'
      body = t('git.sync_result.conflicts', { base, count: res.conflicts?.length ?? 0 })
    } else body = t('git.sync_result.aborted')
  } else {
    const reason = gitErrorReason(sync.error)
    outcome = `error:${reason ?? 'unknown'}`
    tone = reason === 'uncommitted_changes' || reason === 'merge_in_progress' ? 'warning' : 'danger'
    const output = gitErrorOutput(sync.error)
    body = (
      <>
        <span>{describeGitError(sync.error, t)}</span>
        {reason === 'uncommitted_changes' ? (
          <Button variant="outline" icon={<Sparkles className="size-4" />} onClick={onCommit} className="mt-2" data-testid="git-sync-commit">
            {t('git.commit')}
          </Button>
        ) : null}
        {reason === 'merge_in_progress' && !showMerge ? <span className="mt-1 block text-xs text-fg-subtle">{t('git.errors.merge_in_progress_hint')}</span> : null}
        {output ? (
          <pre className="scroll-thin mt-2 max-h-40 overflow-auto rounded-md bg-bg-muted p-2 font-mono text-xs whitespace-pre-wrap" data-testid="git-sync-output">
            {output}
          </pre>
        ) : null}
      </>
    )
  }
  const toneCls = { info: 'border-info/40 bg-info-subtle', success: 'border-success/40 bg-success-subtle', warning: 'border-warning/40 bg-warning-subtle', danger: 'border-danger/40 bg-danger-subtle' }[tone]
  return (
    <div className={`flex items-start gap-2 rounded-md border p-2.5 text-sm ${toneCls}`} role="status" data-testid="sync-result" data-outcome={outcome}>
      <div className="min-w-0 flex-1">{body}</div>
      <Button variant="ghost" size="icon" aria-label={t('common.close')} onClick={onDismiss} className="-my-1 -mr-1">
        <X className="size-4" />
      </Button>
    </div>
  )
}

// ---- 改动列表 ----

interface DiffList {
  against: DiffAgainst
  files: DiffFile[]
  truncated: boolean
}

function DiffSection({ nodeId, sessionId, hasWorktree, against, onAgainst, rev, ready, base }: { nodeId: string; sessionId: string; hasWorktree: boolean; against: DiffAgainst; onAgainst: (a: DiffAgainst) => void; rev: number; ready: boolean; base?: string }) {
  const { t } = useTranslation()
  const [list, setList] = useState<DiffList | null>(null)
  const [error, setError] = useState<unknown>(null)
  const [loading, setLoading] = useState(false)
  const req = useRef(0)

  // 状态每刷新一次（rev 变了）就重新取列表；上一份列表留着直到新的回来，避免闪烁
  useEffect(() => {
    if (!ready) return
    const id = ++req.current
    let alive = true
    setLoading(true)
    loadDiffList(nodeId, sessionId, against)
      .then((res) => {
        if (!alive || id !== req.current) return
        setList({ against, files: res.files, truncated: !!res.truncated })
        setError(null)
      })
      .catch((e: unknown) => {
        if (!alive || id !== req.current) return
        setError(e)
      })
      .finally(() => {
        if (alive && id === req.current) setLoading(false)
      })
    return () => {
      alive = false
    }
  }, [ready, nodeId, sessionId, against, rev])

  const shown = list && list.against === against ? list : null
  return (
    <section className="space-y-2" data-testid="changes-diff">
      {hasWorktree ? (
        <Segmented
          ariaLabel={t('git.view')}
          value={against}
          onChange={onAgainst}
          fill
          testId="diff-against"
          options={[
            { value: 'base', label: base ? t('git.viewBaseNamed', { base }) : t('git.viewBase'), testId: 'diff-against-base' },
            { value: 'uncommitted', label: t('git.viewUncommitted'), testId: 'diff-against-uncommitted' },
          ]}
        />
      ) : (
        <h3 className="text-xs font-semibold tracking-wide text-fg-subtle uppercase" data-testid="diff-uncommitted-only">
          {t('git.viewUncommitted')}
        </h3>
      )}

      {error ? (
        <p className="text-sm text-danger" data-testid="diff-error">
          {describeGitError(error, t)}
        </p>
      ) : null}

      {!shown && loading ? (
        <div className="flex items-center gap-2 text-sm text-fg-subtle">
          <Spinner /> {t('git.loading')}
        </div>
      ) : null}

      {shown && shown.files.length === 0 ? (
        <p className="text-sm text-fg-subtle" data-testid="diff-empty">
          {against === 'base' ? t('git.noChangesBase', { base: base ?? '' }) : t('git.noChangesUncommitted')}
        </p>
      ) : null}

      {shown && shown.files.length > 0 ? (
        <ul className="divide-y divide-border rounded-md border border-border" data-testid="diff-files" aria-busy={loading}>
          {shown.files.map((f) => (
            <FileRow key={`${f.path}`} nodeId={nodeId} sessionId={sessionId} against={against} file={f} rev={rev} />
          ))}
        </ul>
      ) : null}

      {shown?.truncated ? (
        <p className="text-xs text-fg-subtle" data-testid="diff-truncated">
          {t('git.filesTruncated')}
        </p>
      ) : null}
    </section>
  )
}

/** 路径：目录部分在窄屏上先被截断，文件名始终看得见。 */
function PathLabel({ path, title }: { path: string; title: string }) {
  const i = path.lastIndexOf('/')
  const dir = i >= 0 ? path.slice(0, i + 1) : ''
  const name = i >= 0 ? path.slice(i + 1) : path
  return (
    <span className="flex min-w-0 font-mono text-xs" title={title} data-testid="diff-file-path">
      {dir ? <span className="min-w-0 truncate text-fg-subtle">{dir}</span> : null}
      <span className="max-w-full shrink-0 truncate">{name}</span>
    </span>
  )
}

type Patch = { state: 'loading' } | { state: 'ok'; text: string; truncated: boolean } | { state: 'error'; error: unknown }

function FileRow({ nodeId, sessionId, against, file, rev }: { nodeId: string; sessionId: string; against: DiffAgainst; file: DiffFile; rev: number }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  const [patch, setPatch] = useState<Patch>({ state: 'loading' })

  useEffect(() => {
    if (!open || file.binary) return
    let alive = true
    loadDiffPatch(nodeId, sessionId, against, file.path)
      .then((f) => {
        if (alive) setPatch({ state: 'ok', text: f?.patch ?? '', truncated: !!f?.patch_truncated })
      })
      .catch((e: unknown) => {
        if (alive) setPatch({ state: 'error', error: e })
      })
    return () => {
      alive = false
    }
  }, [open, file.binary, file.path, nodeId, sessionId, against, rev])

  const statusLabel = file.untracked ? t('git.file.untracked') : t(`git.file.${file.status}`)
  const tone = file.status === 'added' ? 'success' : file.status === 'deleted' ? 'danger' : file.status === 'renamed' ? 'info' : 'warning'
  return (
    <li data-testid="diff-file-row" data-path={file.path} data-status={file.status} data-untracked={file.untracked ? 'true' : 'false'}>
      <button type="button" className="flex min-h-9 w-full items-center gap-1.5 px-2 py-1.5 text-left hover:bg-bg-hover" aria-expanded={open} onClick={() => setOpen((v) => !v)} data-testid="diff-file-toggle">
        {open ? <ChevronDown className="size-4 shrink-0 text-fg-subtle" aria-hidden /> : <ChevronRight className="size-4 shrink-0 text-fg-subtle" aria-hidden />}
        <span className="min-w-0 flex-1">
          <PathLabel path={file.path} title={file.old_path ? `${file.old_path} → ${file.path}` : file.path} />
          {file.old_path ? <span className="block truncate font-mono text-[0.6875rem] text-fg-subtle">← {file.old_path}</span> : null}
        </span>
        <Chip tone={tone} testId="diff-file-status">
          {statusLabel}
        </Chip>
        {file.binary ? (
          <span className="shrink-0 text-xs text-fg-subtle">{t('git.file.binary')}</span>
        ) : (
          <span className="shrink-0 font-mono text-xs" data-testid="diff-file-counts">
            <span className="text-success">+{file.additions}</span> <span className="text-danger">−{file.deletions}</span>
          </span>
        )}
      </button>
      {open ? (
        <div className="border-t border-border bg-bg-subtle" data-testid="diff-patch-panel">
          {file.binary ? (
            <p className="p-3 text-xs text-fg-subtle">{t('git.binaryNoPatch')}</p>
          ) : patch.state === 'loading' ? (
            <div className="p-3">
              <Spinner />
            </div>
          ) : patch.state === 'error' ? (
            <p className="p-3 text-xs text-danger" data-testid="diff-patch-error">
              {describeGitError(patch.error, t)}
            </p>
          ) : patch.text.trim() === '' ? (
            <p className="p-3 text-xs text-fg-subtle">{t('git.emptyPatch')}</p>
          ) : (
            <>
              <div className="scroll-thin max-h-[70dvh] overflow-auto text-xs" data-testid="diff-patch">
                <Suspense
                  fallback={
                    <div className="p-3">
                      <Spinner />
                    </div>
                  }
                >
                  <DiffView diff={patch.text} />
                </Suspense>
              </div>
              {patch.truncated ? (
                <p className="border-t border-border px-3 py-1.5 text-xs text-fg-subtle" data-testid="diff-patch-truncated">
                  {t('git.patchTruncated', { size: formatBytes(PATCH_LIMIT_BYTES) })}
                </p>
              ) : null}
            </>
          )}
        </div>
      ) : null}
    </li>
  )
}
