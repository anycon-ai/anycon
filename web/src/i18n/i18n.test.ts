import { readdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'
import { EXPLAINED_ERROR_CODES } from '@/features/session/errors'
import type { AgentStatus, DiffFileStatus, EnrollMethod, ErrorCode, EnrollResult, FileChangeKind, GitAction, LinkBlock, NotifyCategory, PullRequestState, SecurityEventKind, SessionStatus, SyncOutcome, TurnDoneReason, WorktreeState } from '@/protocol'
import type { PushErrorCode } from '@/features/push/actions'
import type { UnsupportedReason } from '@/features/push/browser'
import type { PushStatus } from '@/features/push/usePush'
import { unlockErrorKeys } from '@/features/unlock/errors'
import type { PairErrorCode } from '@/transport/client'
import type { NodeConnState } from '@/transport/connection'
import en from './en.json'
import zh from './zh.json'

type Json = { [k: string]: string | Json }

function leaves(obj: Json, prefix = ''): string[] {
  return Object.entries(obj).flatMap(([k, v]) => (typeof v === 'string' ? [prefix + k] : leaves(v, `${prefix}${k}.`)))
}
/** i18next 复数后缀 `_one` / `_other` 归一成基础 key。 */
const base = (k: string): string => k.replace(/_(one|other|zero|two|few|many)$/, '')

const enKeys = new Set(leaves(en as Json).map(base))
const zhKeys = new Set(leaves(zh as Json).map(base))

const srcDir = resolve(dirname(fileURLToPath(import.meta.url)), '..')
function sourceFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((f) => {
    const p = join(dir, f)
    if (statSync(p).isDirectory()) return sourceFiles(p)
    return /\.(ts|tsx)$/.test(f) && !/\.test\.|test\//.test(p) ? [p] : []
  })
}

describe('i18n', () => {
  it('English and Chinese have exactly the same keys', () => {
    expect([...enKeys].filter((k) => !zhKeys.has(k))).toEqual([])
    expect([...zhKeys].filter((k) => !enKeys.has(k))).toEqual([])
  })

  it('every static t("…") key used in the source exists in both languages', () => {
    const used = new Set<string>()
    for (const f of sourceFiles(srcDir)) for (const m of readFileSync(f, 'utf8').matchAll(/\bt\(\s*'([A-Za-z0-9_.]+)'/g)) used.add(m[1]!)
    const missing = [...used].filter((k) => !enKeys.has(k) || !zhKeys.has(k))
    expect(missing).toEqual([])
    expect(used.size).toBeGreaterThan(200)
  })

  it('has no empty strings', () => {
    expect(leaves(en as Json).filter((k) => !k)).toEqual([])
    const flat = (o: Json): string[] => Object.values(o).flatMap((v) => (typeof v === 'string' ? [v] : flat(v)))
    expect(flat(en as Json).filter((v) => v.trim() === '')).toEqual([])
    expect(flat(zh as Json).filter((v) => v.trim() === '')).toEqual([])
  })

  // 动态拼出来的 key：用 Record<联合类型, true> 让编译器保证枚举完整
  it('covers every value of the enums used to build dynamic keys', () => {
    const agentStatus: Record<AgentStatus, true> = { ready: true, loading: true, disabled: true, not_installed: true, not_authenticated: true, unsupported_version: true, error: true }
    const sessionStatus: Record<SessionStatus | 'offline', true> = { queued: true, starting: true, running: true, waiting_permission: true, idle: true, ended: true, error: true, interrupted: true, offline: true }
    const nodeConn: Record<NodeConnState, true> = { hub_disconnected: true, hub_connecting: true, hub_authed: true, peer_offline: true, noise_handshaking: true, ready: true, noise_failed: true }
    const fileKind: Record<FileChangeKind, true> = { created: true, modified: true, deleted: true }
    const errCode: Record<ErrorCode, true> = { invalid: true, invalid_settings: true, agent_unavailable: true, node_busy: true, busy: true, already_resolved: true, exists: true, not_found: true, forbidden: true, unsupported: true, protocol_mismatch: true, internal: true }
    const turnReason: Record<TurnDoneReason, true> = { completed: true, cancelled: true, error: true, max_turns: true, budget: true }
    const pairErr: Record<PairErrorCode | 'retry' | 'generic', true> = {
      hub_unavailable: true, invalid_code: true, not_found: true, timeout: true, peer_gone: true, rate_limited: true, quota: true, forbidden: true,
      handshake_failed: true, no_identity: true, cancelled: true, ack_invalid: true, key_changed: true, retry: true, generic: true,
    }
    const linkBlock: Record<LinkBlock, true> = { hidden: true, sensitive: true, workspace: true, data_dir: true, filesystem_root: true }
    // ADR-34：worktree / git 的枚举，都用来拼 key
    const worktreeState: Record<WorktreeState, true> = { creating: true, active: true, kept: true, removed: true, missing: true, failed: true }
    const gitAction: Record<GitAction, true> = {
      worktree_created: true, worktree_failed: true, worktree_removed: true, worktree_kept: true, worktree_restored: true, worktree_missing: true,
      sync_up_to_date: true, sync_merged: true, sync_conflicts: true, sync_aborted: true,
    }
    const syncOutcome: Record<SyncOutcome, true> = { up_to_date: true, merged: true, conflicts: true, aborted: true }
    const diffFileStatus: Record<DiffFileStatus, true> = { added: true, modified: true, deleted: true, renamed: true }
    const prState: Record<PullRequestState, true> = { open: true, draft: true, merged: true, closed: true }
    // `data.reason`（daemon.md § 10.1）：同步 base / 建会话 / 查状态 / 删 worktree 的原因，按原因查专门的说法
    const gitReasons = [
      'not_worktree_ready', 'bad_base', 'uncommitted_changes', 'merge_in_progress', 'fetch_failed', 'git_failed', 'worktree_missing', 'not_git',
      // daemon 实现里还有的原因
      'worktree_creating', 'worktree_failed', 'not_worktree', 'worktree_not_active', 'no_merge_in_progress', 'no_commits', 'worktree_remove_failed',
      // 已归档的会话不能 send / sync_base
      'archived', 'unarchive_failed',
    ]
    // `SessionGitStatus.pr_error`
    const prErrors = ['no_pr', 'gh_not_installed', 'gh_not_authenticated', 'no_remote', 'error']
    const apiCodes = ['unauthorized', 'invalid_token', 'forbidden', 'csrf', 'not_found', 'invite_invalid', 'invite_expired', 'invite_used', 'bad_request', 'unsupported_media_type', 'payload_too_large', 'rate_limited', 'internal', 'network', 'email_taken', 'invalid_code', 'last_login_method', 'push_endpoint_not_allowed', 'device_not_found', 'subscription_not_found', 'invalid_email']
    // Web Push / 邮箱通知（ADR-37）：浏览器不能推送的原因、推送动作的错误、六个通知类别、推送卡片的状态、推送文案
    const pushUnsupported: Record<UnsupportedReason, true> = { insecure: true, no_service_worker: true, no_push: true, ios_install: true }
    const pushErrors: Record<PushErrorCode, true> = { dismissed: true, denied: true, no_registration: true, subscribe_failed: true, unsubscribe_failed: true }
    const notifyCategories: Record<NotifyCategory, true> = { permission_requested: true, turn_finished: true, error: true, session_idle: true, enroll_requested: true, security: true }
    const pushStatus: Record<PushStatus, true> = { loading: true, unsupported: true, unavailable: true, denied: true, off: true, on: true }
    const sessionPushKeys = ['permissionRequested', 'turnFinished', 'error', 'sessionIdle']
    // 账号内接入（ADR-36）：失败原因、安全事件、审计动作、状态，全是拼 key 用的枚举
    const enrollMethod: Record<EnrollMethod, true> = { scan: true, code: true, password: true }
    const securityKind: Record<SecurityEventKind, true> = { device_enrolled: true, password_failures: true, password_locked: true, password_changed: true, settings_changed: true }
    const enrollResult: Record<EnrollResult, true> = { approved: true, denied: true, completed: true, failed: true, expired: true }
    const passwordState: Record<'unset' | 'enabled' | 'disabled' | 'locked', true> = { unset: true, enabled: true, disabled: true, locked: true }
    const unlockEntries = ['scan', 'approve', 'code', 'password']
    const unlockSteps = ['connecting', 'starting', 'waitingScan', 'waitingApproval', 'enterCode', 'deriving', 'verifying', 'handshaking', 'confirming']
    const auditActions = ['requested', 'approved', 'denied', 'code', 'completed', 'failed', 'password_set', 'password_cleared', 'password_locked', 'settings']
    const want: string[] = [
      ...unlockErrorKeys(),
      ...unlockEntries.flatMap((k) => [`unlock.entries.${k}.title`, `unlock.entries.${k}.hint`]),
      ...unlockSteps.map((k) => `unlock.steps.${k}`),
      ...Object.keys(enrollMethod).map((k) => `security.method.${k}`),
      ...Object.keys(securityKind).map((k) => `security.${{ device_enrolled: 'deviceEnrolled', password_failures: 'passwordFailures', password_locked: 'passwordLocked', password_changed: 'passwordChanged', settings_changed: 'settingsChanged' }[k as SecurityEventKind]}`),
      'security.passwordCleared',
      ...Object.keys(enrollResult).map((k) => `approve.${{ approved: 'approvedBy', denied: 'deniedBy', completed: 'completed', failed: 'failed', expired: 'expired' }[k as EnrollResult]}`),
      ...Object.keys(passwordState).map((k) => `enrollTab.state.${k}`),
      ...auditActions.map((k) => `enrollTab.audit.${k}`),
      ...Object.keys(agentStatus).map((k) => `agent.status.${k}`),
      ...Object.keys(sessionStatus).map((k) => `status.session.${k}`),
      ...Object.keys(nodeConn).map((k) => `status.node.${k}`),
      ...Object.keys(fileKind).map((k) => `info.file.${k}`),
      ...Object.keys(errCode).map((k) => `errors.rpc.${k}`),
      ...Object.keys(turnReason).map((k) => `turn.reason.${k}`),
      ...Object.keys(pairErr).map((k) => `pair.errors.${k}`),
      ...apiCodes.map((k) => `errors.api.${k}`),
      ...Object.keys(pushUnsupported).map((k) => `notifications.push.unsupported.${k}`),
      ...Object.keys(pushErrors).map((k) => `notifications.push.errors.${k}`),
      ...Object.keys(notifyCategories).map((k) => `notifications.categories.label.${k}`),
      ...Object.keys(pushStatus).map((k) => `notifications.push.status.${k}`),
      ...sessionPushKeys.map((k) => `push.${k}`),
      // `error` 事件里有本地化说明的 code（features/session/errors.ts）
      ...EXPLAINED_ERROR_CODES.map((k) => `agentError.${k}`),
      // 关联已有目录（ADR-33）：按错误码查专门的说法（`describeErrorIn`）
      ...['forbidden', 'exists', 'invalid', 'not_found'].map((k) => `project.linkErrors.${k}`),
      ...['busy', 'invalid', 'not_found'].map((k) => `project.removeErrors.${k}`),
      ...['forbidden', 'invalid', 'not_found', 'protected_sensitive', 'protected_data_dir'].map((k) => `folderPicker.errors.${k}`),
      // `LinkBlock`（workspace.browse 的 blocked）：Record 让编译器保证枚举完整
      ...Object.keys(linkBlock).map((k) => `folderPicker.blocked.${k}`),
      ...Object.keys(worktreeState).map((k) => `worktree.state.${k}`),
      // 改动标签里 worktree 不可用时的说明（active / kept 以外的状态）
      ...Object.keys(worktreeState).filter((k) => k !== 'active' && k !== 'kept').map((k) => `git.tab.${k}`),
      ...Object.keys(gitAction).map((k) => `git.event.${k}`),
      ...Object.keys(diffFileStatus).map((k) => `git.file.${k}`),
      ...Object.keys(prState).map((k) => `git.pr.state.${k}`),
      ...gitReasons.map((k) => `git.errors.${k}`),
      ...prErrors.map((k) => `git.pr.error.${k}`),
      ...Object.keys(syncOutcome).map((k) => `git.sync_result.${k}`),
      // 已归档会话的输入区说明、新建会话「工作位置」读 git 信息时的占位
      'session.archivedHint', 'workspace.reading', 'workspace.readingBlocked',
      // 运行 / 等审批时不能归档，base 是提交时的 PR 消息
      'info.archiveBusy', 'git.messages.prDefault',
      'login.invite_invalid', 'login.invite_expired', 'login.invite_used',
      'setup.step1', 'setup.step2', 'setup.step3',
      'pair.steps.connecting', 'pair.steps.deriving', 'pair.steps.matching', 'pair.steps.handshaking', 'pair.steps.confirming', 'pair.steps.verifying',
      'permission.allowedBy', 'permission.deniedBy', 'system.thinking', 'system.current',
    ]
    expect(want.filter((k) => !enKeys.has(k) || !zhKeys.has(k))).toEqual([])
  })
})
