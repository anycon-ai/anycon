// 会话 git 状态的拉取与同步 base（ADR-34）。状态在 stores/git.ts；这里只编排 RPC 和防抖。
// 刷新时机（不轮询）：进入会话页、`turn_done`、`file_changed`（防抖 2 秒）、`git` 事件、手动刷新。
import type { SessionGitStatus, SessionSyncBaseResult } from '@/protocol'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useGit, type GitFetchError } from '@/stores/git'

/** `file_changed` 之后等多久再刷新：一个 turn 里常常连着改很多文件。 */
export const FILE_CHANGED_DEBOUNCE_MS = 2000

interface Job {
  /** 在途的这个请求带了 `include_pr`。 */
  sentPr: boolean
  /** 在途期间又有人要求刷新：这一轮结束后再来一轮（在途的结果可能已经旧了）。 */
  again: boolean
  /** 下一轮要带 `include_pr`。 */
  wantPr: boolean
  /** 请求刚发出、还没过一个微任务：同一批 effect 里的重复调用并进它，不再多发一轮。 */
  fresh: boolean
  promise: Promise<void>
}
const jobs = new Map<string, Job>()
const timers = new Map<string, ReturnType<typeof setTimeout>>()
/** 每次 `dropGit` +1：离开会话页时还在途的请求，结果回来也不再写进 store。 */
const generation = new Map<string, number>()

function toFetchError(e: unknown): GitFetchError {
  if (e instanceof RpcCallError && e.kind === 'rpc') return { code: e.code ?? 'internal', reason: e.reason ?? e.code ?? 'internal', message: e.message }
  return { code: e instanceof RpcCallError ? e.kind : 'internal', reason: e instanceof RpcCallError ? e.kind : 'internal', message: e instanceof Error ? e.message : String(e) }
}

async function fetchOnce(nodeId: string, sessionId: string, includePr: boolean): Promise<void> {
  const patch = useGit.getState().patch
  const gen = generation.get(sessionId) ?? 0
  const stale = (): boolean => (generation.get(sessionId) ?? 0) !== gen
  patch(sessionId, () => ({ loading: true }))
  try {
    const res = await rpc().request<SessionGitStatus>(nodeId, 'session.git_status', { session_id: sessionId, ...(includePr ? { include_pr: true } : {}) })
    if (stale()) return
    patch(sessionId, (e) => ({
      status: res,
      error: null,
      loading: false,
      rev: e.rev + 1,
      ...(includePr ? { pr: res.pr ?? null, prError: res.pr ? null : (res.pr_error ?? null), prChecked: true } : {}),
    }))
  } catch (e) {
    if (stale()) return
    const err = toFetchError(e)
    // 不在 git 仓库里是确定的结论，丢掉旧状态；其它失败（离线、超时…）保留上一次的结果，只记下原因
    patch(sessionId, () => ({ loading: false, error: err, ...(err.reason === 'not_git' ? { status: null, pr: null, prError: null } : {}) }))
  }
}

/**
 * 拉一次 git 状态。同一个会话同时只有一个请求在途；在途时再来的请求并进它的下一轮，返回的 promise 等到最新一轮结束。
 * `includePr`：同时用 `gh` 查 PR（要联网，1–3 秒），只在进入「改动」标签和手动刷新时给。
 */
export function refreshGitStatus(nodeId: string, sessionId: string, opts: { includePr?: boolean } = {}): Promise<void> {
  const includePr = !!opts.includePr
  const running = jobs.get(sessionId)
  if (running) {
    if (running.fresh && (running.sentPr || !includePr)) return running.promise
    running.again = true
    running.wantPr ||= includePr
    return running.promise
  }
  const job: Job = { sentPr: includePr, again: false, wantPr: false, fresh: true, promise: Promise.resolve() }
  jobs.set(sessionId, job)
  queueMicrotask(() => {
    job.fresh = false
  })
  job.promise = (async () => {
    try {
      for (;;) {
        await fetchOnce(nodeId, sessionId, job.sentPr)
        if (!job.again) break
        job.again = false
        job.sentPr = job.wantPr
        job.wantPr = false
      }
    } finally {
      jobs.delete(sessionId)
    }
  })()
  return job.promise
}

const prLookups = new Set<string>()

/**
 * 进入「改动」标签时查一次 PR（`include_pr`，要联网，1–3 秒）。只更新 PR 相关的字段，不动 `rev`——
 * 刚刚拉过的状态和文件列表没有变，不用因为这次查询再重新加载。
 */
export async function lookupPullRequest(nodeId: string, sessionId: string): Promise<void> {
  if (prLookups.has(sessionId)) return
  prLookups.add(sessionId)
  const gen = generation.get(sessionId) ?? 0
  const patch = useGit.getState().patch
  patch(sessionId, () => ({ prLoading: true }))
  try {
    const res = await rpc().request<SessionGitStatus>(nodeId, 'session.git_status', { session_id: sessionId, include_pr: true })
    if ((generation.get(sessionId) ?? 0) === gen) patch(sessionId, () => ({ pr: res.pr ?? null, prError: res.pr ? null : (res.pr_error ?? null), prChecked: true, prLoading: false }))
  } catch (e) {
    // 查不到 PR 不是大事：状态行照常显示，PR 一行说「查询失败」；手动刷新可以再试
    console.warn('anycon: could not look up the pull request', e)
    if ((generation.get(sessionId) ?? 0) === gen) patch(sessionId, () => ({ prError: 'error', prChecked: true, prLoading: false }))
  } finally {
    prLookups.delete(sessionId)
  }
}

/** 防抖刷新（`file_changed`）。 */
export function scheduleGitRefresh(nodeId: string, sessionId: string, delayMs: number = FILE_CHANGED_DEBOUNCE_MS): void {
  const t = timers.get(sessionId)
  if (t) clearTimeout(t)
  timers.set(
    sessionId,
    setTimeout(() => {
      timers.delete(sessionId)
      void refreshGitStatus(nodeId, sessionId)
    }, delayMs),
  )
}

/** 离开会话页：取消待办的防抖，丢掉这个会话的 git 状态。 */
export function dropGit(sessionId: string): void {
  const t = timers.get(sessionId)
  if (t) clearTimeout(t)
  timers.delete(sessionId)
  generation.set(sessionId, (generation.get(sessionId) ?? 0) + 1)
  useGit.getState().clear(sessionId)
}

/**
 * 把 base 合并进 worktree 分支（`abort` 放弃一次有冲突的合并）。结果里的冲突文件记进 store，
 * 之后刷新一次状态。错误原样抛出，由调用方按 `data.reason` 说明。
 */
export async function syncBase(nodeId: string, sessionId: string, opts: { abort?: boolean } = {}): Promise<SessionSyncBaseResult> {
  const gen = generation.get(sessionId) ?? 0
  try {
    const res = await rpc().request<SessionSyncBaseResult>(nodeId, 'session.sync_base', { session_id: sessionId, ...(opts.abort ? { abort: true } : {}) })
    if ((generation.get(sessionId) ?? 0) === gen) useGit.getState().patch(sessionId, () => ({ conflictFiles: res.outcome === 'conflicts' ? (res.conflicts ?? []) : [] }))
    return res
  } finally {
    // 成功、冲突、失败（比如已经有合并停在中途）之后状态都可能变了
    void refreshGitStatus(nodeId, sessionId)
  }
}
