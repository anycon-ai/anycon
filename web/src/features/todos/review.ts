// 验收页要用的数据和动作（spec/web.md § 4.12、docs/spec/daemon.md § 22）：最近一个还在的关联会话、它的改动统计、agent 的审查器拒绝过几次、
// 通过之后「提交、推送并开 PR」（要先把这个会话的「不 push」关掉，必须由用户明确确认）。
import type { TFunction } from 'i18next'
import type { DiffAgainst, DiffFile } from '@/protocol'
import * as db from '@/db'
import { sendMessage, setSessionSettings } from '@/features/session/actions'
import { loadDiffList } from '@/features/worktree/diff'
import { rpc } from '@/transport/instance'
import { archiveSession } from '@/features/session/actions'
import { useGit } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { useSessions, type SessionItem } from '@/stores/sessions'

export interface LinkedSession {
  session?: SessionItem
  /** 最近派发的那个会话本地列表里还没有，而节点这一轮同步还没做完：不知道它是「被删掉了」还是「还没读到」，先不替用户选一个旧的。 */
  resolving: boolean
}

/**
 * 关联会话里最近派发的那个还存在的（和 daemon 算 `activity` 时「说了算」的是同一个）。
 * 本地列表里没有 ≠ 被删掉：只有节点这一轮同步做完了（`nodeSynced`）、它仍然不在，才退到更早的一个；
 * 还没同步完时返回 `resolving`，不退。一个都不在了（同步完之后）是没有会话。
 */
export function resolveLinkedSession(sessions: readonly string[] | undefined, list: Record<string, SessionItem>, nodeSynced: boolean): LinkedSession {
  const ids = sessions ?? []
  for (let i = ids.length - 1; i >= 0; i--) {
    const s = list[ids[i]!]
    if (s) return { session: s, resolving: false }
    if (!nodeSynced) return { resolving: true }
  }
  return { resolving: false }
}

/** 订阅版：节点是否同步完、会话列表一起读。返回的会话对象只在那个会话变了时才变。 */
export function useLinkedSession(nodeId: string, sessions: readonly string[] | undefined): LinkedSession {
  const synced = useNodes((s) => s.byId[nodeId]?.conn === 'ready' && s.byId[nodeId]?.synced === true)
  const session = useSessions((s) => resolveLinkedSession(sessions, s.list, synced).session)
  const resolving = useSessions((s) => resolveLinkedSession(sessions, s.list, synced).resolving)
  return { session, resolving }
}

/** 这个会话的 agent 这会儿在干活（或排队、启动中、等审批）：这时「再改改」不发给它，要等这一轮结束。 */
export const isSessionActive = (s: Pick<SessionItem, 'status'>): boolean => s.status === 'running' || s.status === 'starting' || s.status === 'queued' || s.status === 'waiting_permission'

/** worktree 的会话比较它和 base 的差，在项目目录里干的比较没提交的改动。 */
export const diffAgainstFor = (s: Pick<SessionItem, 'worktree'>): DiffAgainst => (s.worktree ? 'base' : 'uncommitted')

export interface ChangeSummary {
  files: DiffFile[]
  additions: number
  deletions: number
  /** 文件超过 1000 个，只给了前 1000 个。 */
  truncated: boolean
}

/** 文件列表和行数（`session.diff`，只读，不带 patch）。 */
export async function loadChangeSummary(nodeId: string, session: Pick<SessionItem, 'id' | 'worktree'>): Promise<ChangeSummary> {
  const res = await loadDiffList(nodeId, session.id, diffAgainstFor(session))
  return { files: res.files, additions: res.files.reduce((n, f) => n + f.additions, 0), deletions: res.files.reduce((n, f) => n + f.deletions, 0), truncated: res.truncated === true }
}

const DENIAL_CODES = new Set(['permission_denied', 'auto_review'])

export interface DenialCount {
  count: number
  /** 本地缓存的事件覆盖了 `[1, last_seq]` 的全部：数字是准的。否则是「至少」。 */
  complete: boolean
}

/**
 * 这个会话里审查器 / 权限规则拒绝过几次（`notice { permission_denied | auto_review }`）。只数本地已经缓存的事件（这个浏览器打开过的会话）：
 * 不为了一个数字去翻整段历史。缓存没有覆盖 `[1, lastSeq]` 时 `complete` 为假，界面写「至少」；一个事件都没有缓存返回 `null`，界面改成链接到会话并说明。
 */
export async function countDenials(sessionId: string, lastSeq: number): Promise<DenialCount | null> {
  let events: Awaited<ReturnType<typeof db.getEvents>>
  try {
    events = await db.getEvents(sessionId)
  } catch {
    return null
  }
  if (events.length === 0) return null
  const seqs = events.map((e) => e.seq)
  const complete = Math.min(...seqs) <= 1 && Math.max(...seqs) >= lastSeq && new Set(seqs).size >= lastSeq
  return { count: events.filter((e) => e.event.type === 'notice' && DENIAL_CODES.has(e.event.code)).length, complete }
}

/** 这个会话现在有没有「不 push」（`settings.options.no_push === true`）。 */
export const hasNoPush = (s: Pick<SessionItem, 'settings'>): boolean => s.settings.options?.no_push === true

/** 「提交、推送并开 PR」发给 agent 的固定消息（按界面语言）。分支名取 worktree 的，没有就取 git 状态里的。 */
export function pushMessage(t: TFunction, session: SessionItem): string {
  const branch = session.worktree?.branch ?? useGit.getState().bySession[session.id]?.status?.branch ?? undefined
  return branch ? t('todos.reviewPage.pushMessage', { branch }) : t('todos.reviewPage.pushMessageNoBranch')
}

export type PushResult = { ok: true } | { ok: false; step: 'settings' | 'send'; error?: unknown; reason?: string }

/**
 * 通过之后的「提交、推送并开 PR」：会话带着「不 push」时先把它关掉（`session.set_settings`，改它要重启 agent 进程），再发固定消息。
 * **调用方必须先让用户明确确认「这会让 agent 能 push 和开 PR」**：这里不替用户决定，也不会在没有确认的情况下关掉它。
 * `confirmedAllowPush`：用户已经确认。会话带着「不 push」而它是 `false` 时什么都不做，返回 `settings` 失败。
 */
export async function commitPushAndOpenPr(nodeId: string, session: SessionItem, t: TFunction, confirmedAllowPush: boolean): Promise<PushResult> {
  if (hasNoPush(session)) {
    if (!confirmedAllowPush) return { ok: false, step: 'settings', reason: 'unconfirmed' }
    try {
      const settings = await setSessionSettings(nodeId, session.id, { settings: { options: { no_push: false } }, clear: [] })
      if (settings.options?.no_push === true) return { ok: false, step: 'settings', reason: 'still_on' }
    } catch (error) {
      return { ok: false, step: 'settings', error }
    }
  }
  const r = await sendMessage(nodeId, session.id, pushMessage(t, useSessions.getState().list[session.id] ?? session))
  return r.state === 'failed' || r.state === 'unsent' ? { ok: false, step: 'send', reason: r.state === 'failed' ? r.reason : 'unsent' } : { ok: true }
}

// ---- 「不要了」之后清理 worktree ----

/**
 * 一个 worktree 的清理方式。**会话和它的记录永远留着**（只归档，不删除）：
 * - `auto`：归档会话，daemon 决定——没有未提交的改动才删 worktree，有就保留；分支上没有自己的提交才连分支一起删。
 * - `keep`：只归档会话，worktree 和分支都留着。
 * - `remove`：归档会话，再 `workspace.remove_worktree`。`force` 才会丢掉未提交的改动（界面上必须逐个 worktree 明确勾选、写明丢什么），
 *   `deleteBranch` 才会 `git branch -D`（分支上没合并 / 没推送的提交会丢，界面同样写明）。
 */
export type CleanupChoice = { kind: 'auto' } | { kind: 'keep' } | { kind: 'remove'; deleteBranch: boolean; force: boolean }

export const SAFE_CLEANUP: CleanupChoice = { kind: 'auto' }

export async function cleanUpWorktree(nodeId: string, session: Pick<SessionItem, 'id' | 'archived' | 'worktree'>, choice: CleanupChoice): Promise<void> {
  if (!session.worktree) return
  // 归档要先于删除（daemon 对没归档的会话回 `busy`）；要自己删的时候归档只是把会话收起来，别让 daemon 顺手决定 worktree 的去留
  if (session.archived !== true) await archiveSession(nodeId, session.id, true, { worktree: choice.kind === 'auto' ? 'auto' : 'keep' })
  if (choice.kind !== 'remove') return
  await rpc().request(nodeId, 'workspace.remove_worktree', { id: session.worktree.id, ...(choice.force ? { force: true } : {}), ...(choice.deleteBranch ? { delete_branch: true } : {}) })
}
