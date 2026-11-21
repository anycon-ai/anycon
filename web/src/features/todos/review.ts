// 验收页要用的数据和动作（spec/web.md § 4.12、docs/spec/daemon.md § 22）：最近一个还在的关联会话、它的改动统计、agent 的审查器拒绝过几次、
// 通过之后「提交、推送并开 PR」（要先把这个会话的「不 push」关掉，必须由用户明确确认）。
import type { TFunction } from 'i18next'
import type { DiffAgainst, DiffFile } from '@/protocol'
import * as db from '@/db'
import { sendMessage, setSessionSettings } from '@/features/session/actions'
import { loadDiffList } from '@/features/worktree/diff'
import { useGit } from '@/stores/git'
import { useSessions, type SessionItem } from '@/stores/sessions'

/** 关联会话里最近派发的那个还存在的（和 daemon 算 `activity` 时「说了算」的是同一个）；都不在了是 `undefined`。 */
export function latestLinkedSession(sessions: readonly string[] | undefined, list: Record<string, SessionItem>): SessionItem | undefined {
  const ids = sessions ?? []
  for (let i = ids.length - 1; i >= 0; i--) {
    const s = list[ids[i]!]
    if (s) return s
  }
  return undefined
}

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

/**
 * 这个会话里审查器 / 权限规则拒绝过几次（`notice { permission_denied | auto_review }`）。只数本地已经缓存的事件（这个浏览器打开过的会话）：
 * 不为了一个数字去翻整段历史。没有缓存（没打开过）返回 `null`，界面改成链接到会话并说明。
 */
export async function countDenials(sessionId: string): Promise<number | null> {
  let events: Awaited<ReturnType<typeof db.getEvents>>
  try {
    events = await db.getEvents(sessionId)
  } catch {
    return null
  }
  if (events.length === 0) return null
  return events.filter((e) => e.event.type === 'notice' && DENIAL_CODES.has(e.event.code)).length
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
