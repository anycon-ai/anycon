// 会话相关的网络操作：打开 / 追赶历史 / 观察、发送与重发、取消、权限答复、改设置。
// 状态都落在 zustand store；这里只编排 RPC 与 store 变更。
import type {
  AttachmentRef,
  PermissionRespondParams,
  SessionArchiveParams,
  SessionCreateParams,
  SessionDeleteParams,
  SessionEvent,
  SessionHistoryResult,
  SessionSendParams,
  SessionSendResult,
  SessionSetSettingsParams,
  SessionSetSettingsResult,
  SessionSettings,
  SessionSummary,
  SessionWorkspace,
  WorktreeDisposal,
} from '@/protocol'
import * as db from '@/db'
import type { SettingsUpdate } from '@/features/settings/resolve'
import { isRecord } from '@/lib/guards'
import { uuid } from '@/lib/ids'
import { mark, shortId, timed } from '@/lib/timing'
import { warnOnError, storageFailed } from '@/lib/log'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { onDraftsReset, useDrafts } from '@/stores/drafts'
import { useSessions } from '@/stores/sessions'

const HISTORY_PAGE = 200
const INCREMENTAL_PAGE = 500
/** 缓存落后太多时不逐页补，直接丢掉缓存重新拉最近一屏。 */
const MAX_INCREMENTAL_GAP = 2000
const MAX_SEQ = Number.MAX_SAFE_INTEGER

// ---- 打开 / 同步 ----

export async function openSession(nodeId: string, sessionId: string): Promise<void> {
  let cached: SessionEvent[] = []
  try {
    cached = await db.getEvents(sessionId)
  } catch (e) {
    console.warn('anycon: could not read the cached events; loading the session from the node instead', e)
  }
  // 刚创建的会话（这个浏览器见证了创建）：用创建时的设置做基线，settings_changed 才能说清「从什么变成什么」
  const item = useSessions.getState().list[sessionId]
  const baseline = cached.length === 0 && item && item.last_seq <= 3 ? item.settings : null
  useSessions.getState().openStart(nodeId, sessionId, cached, baseline)
  if (rpc().isReady(nodeId)) await syncOpenSession(nodeId, sessionId)
}

export async function closeSession(nodeId: string, sessionId: string): Promise<void> {
  const o = useSessions.getState().open[sessionId]
  useSessions.getState().openClose(sessionId)
  if (o?.watching && rpc().isReady(nodeId)) {
    await rpc()
      .request(nodeId, 'session.unwatch', { session_id: sessionId })
      .catch(warnOnError('unwatch the session'))
  }
}

function eventsOf(res: SessionHistoryResult): SessionEvent[] {
  return res.events.slice().sort((a, b) => a.seq - b.seq)
}

/**
 * 当前打开的会话：先 watch（之后的实时事件进 held 缓冲），再拉历史补齐，最后把缓冲接上。
 * watch 和第一次 history 一起发、不等 watch 的响应：daemon 对同一客户端的请求按到达顺序处理（protocol.md § 7.3），
 * watch 仍在 history 之前生效，watch 与历史之间不会漏事件，又省一次往返。
 */
export async function syncOpenSession(nodeId: string, sessionId: string): Promise<void> {
  const st = useSessions.getState()
  const o = st.open[sessionId]
  if (!o || o.catchingUp) return
  st.openPatch(sessionId, { catchingUp: true })
  try {
    const watch = timed('rpc session.watch', () => rpc().request(nodeId, 'session.watch', { session_id: sessionId }))
      .then(() => useSessions.getState().openPatch(sessionId, { watching: true }))
      .catch((e: unknown) => {
        if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found') throw e
        // 观察失败（比如超过 5 个 watcher）不阻止查看历史
      })
    // 两个都等完再看结果：watch 的 not_found（会话已不存在）优先于 history 的错误
    const [w, h] = await Promise.allSettled([watch, timed('rpc session.history', () => pullHistory(nodeId, sessionId))])
    if (w.status === 'rejected') throw w.reason
    if (h.status === 'rejected') throw h.reason
    mark(`session.ready ${shortId(sessionId)}`)
    useSessions.getState().openPatch(sessionId, { phase: 'ready', error: undefined })
  } catch (e) {
    const cur = useSessions.getState().open[sessionId]
    if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found') {
      useSessions.getState().openPatch(sessionId, { phase: 'error', error: 'not_found' })
    } else if (cur && cur.events.length === 0) {
      useSessions.getState().openPatch(sessionId, { phase: 'error', error: e instanceof Error ? e.message : String(e) })
    }
  } finally {
    useSessions.getState().openPatch(sessionId, { catchingUp: false })
    flushHeld(sessionId)
    reapEchoed(sessionId)
  }
}

async function pullHistory(nodeId: string, sessionId: string): Promise<void> {
  const get = useSessions.getState
  const o = get().open[sessionId]
  if (!o) return
  const remoteLast = get().list[sessionId]?.last_seq ?? 0
  let last = o.view.lastSeq
  if (last > 0 && remoteLast - last > MAX_INCREMENTAL_GAP) {
    await db.clearEvents(sessionId).catch(storageFailed('clear the stale event cache'))
    last = 0
  }
  if (last === 0) {
    const res = await rpc().request<SessionHistoryResult>(nodeId, 'session.history', { session_id: sessionId, before_seq: MAX_SEQ, limit: HISTORY_PAGE })
    const events = eventsOf(res)
    get().replaceAll(sessionId, events)
    get().openPatch(sessionId, { hasMoreOlder: res.has_more })
    db.putEvents(sessionId, events).catch(storageFailed('cache the session events'))
    const item = get().list[sessionId]
    if (item) useSessions.setState((s) => ({ list: { ...s.list, [sessionId]: { ...item, cached_seq: events.at(-1)?.seq ?? 0 } } }))
    return
  }
  for (let guard = 0; guard < 50; guard++) {
    const cur = get().open[sessionId]
    if (!cur) return
    const res = await rpc().request<SessionHistoryResult>(nodeId, 'session.history', { session_id: sessionId, after_seq: cur.view.lastSeq, limit: INCREMENTAL_PAGE })
    const events = eventsOf(res)
    get().ingest(sessionId, events)
    if (!res.has_more || events.length === 0) break
  }
}

/**
 * 追赶期间攒下的实时事件，追赶结束后接上。
 * `allowGap`：这轮追赶没有任何进展，说明剩下的缺口补不上了（被合并掉的 delta），直接接受，避免无限追赶。
 */
function flushHeld(sessionId: string, allowGap = false): void {
  const st = useSessions.getState()
  const o = st.open[sessionId]
  if (!o || o.held.length === 0) return
  const held = o.held
  st.openPatch(sessionId, { held: [] })
  const r = useSessions.getState().ingest(sessionId, held, { allowGap })
  if (r.gap) void catchUp(o.nodeId, sessionId)
}

/** 实时事件之间出现缺口（含被合并的 delta）：按 seq 补历史。 */
export async function catchUp(nodeId: string, sessionId: string): Promise<void> {
  const st = useSessions.getState()
  const o = st.open[sessionId]
  if (!o || o.catchingUp) return
  st.openPatch(sessionId, { catchingUp: true })
  const before = o.view.lastSeq
  let ok = false
  try {
    await pullHistory(nodeId, sessionId)
    ok = true
  } catch (e) {
    // 下一个事件或重连时会再触发
    console.warn('anycon: catching up on the session history failed; will retry on the next event or reconnect', e)
  } finally {
    useSessions.getState().openPatch(sessionId, { catchingUp: false })
    const progressed = (useSessions.getState().open[sessionId]?.view.lastSeq ?? before) > before
    flushHeld(sessionId, ok && !progressed)
  }
}

export async function loadOlder(sessionId: string): Promise<void> {
  const o = useSessions.getState().open[sessionId]
  if (!o || !o.hasMoreOlder || o.lowSeq <= 1) return
  const res = await rpc().request<SessionHistoryResult>(o.nodeId, 'session.history', { session_id: sessionId, before_seq: o.lowSeq, limit: HISTORY_PAGE })
  useSessions.getState().prepend(sessionId, eventsOf(res), res.has_more)
}

// ---- 实时事件（rAF 合并） ----

const liveBuffer = new Map<string, SessionEvent[]>()
let flushScheduled = false

export function enqueueLive(event: SessionEvent): void {
  const list = liveBuffer.get(event.session_id)
  if (list) list.push(event)
  else liveBuffer.set(event.session_id, [event])
  if (flushScheduled) return
  flushScheduled = true
  const run = (): void => {
    flushScheduled = false
    flushLive()
  }
  if (typeof requestAnimationFrame === 'function') requestAnimationFrame(run)
  else setTimeout(run, 16)
}

export function flushLive(): void {
  for (const [sessionId, events] of liveBuffer) {
    liveBuffer.delete(sessionId)
    const st = useSessions.getState()
    const o = st.open[sessionId]
    if (!o) continue
    if (o.catchingUp) {
      st.openPatch(sessionId, { held: o.held.concat(events) })
      continue
    }
    const r = st.ingest(sessionId, o.held.length > 0 ? o.held.concat(events) : events)
    if (o.held.length > 0) st.openPatch(sessionId, { held: [] })
    if (r.applied.length > 0) reapEchoed(sessionId)
    if (r.gap) void catchUp(o.nodeId, sessionId)
  }
}

// ---- 发送 ----

/** daemon 对已归档会话的 `session.send` / `sync_base` 回 `invalid`，`data.reason` 是它（daemon.md § 10.1「错误原因」）。 */
export const ARCHIVED_REASON = 'archived'

/**
 * 「附件已过期」：daemon 回 `not_found` 且 `data.reason = "blob"`（blob 被回收了，或从来没传完），protocol.md § 5.6。
 * 存进 `UnsentMessage.error`，失败气泡据此说明「附件已过期，请重新添加」。
 */
export const BLOB_EXPIRED = 'blob_expired'

export function isBlobExpired(e: unknown): boolean {
  return e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found' && e.reason === 'blob'
}

/**
 * 「附件已经属于别的会话」：daemon 回 `invalid` 且 `data.reason = "blob_in_use"`（protocol.md § 5.6）。
 * 比如新建会话请求超时但其实建好了，用户又换了个 `message_id` 重发。从本地缓存重传一份就能发出去。
 */
export const BLOB_IN_USE = 'blob_in_use'

export function isBlobInUse(e: unknown): boolean {
  return e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'invalid' && e.reason === BLOB_IN_USE
}

/** 一条消息的附件合计超过节点的上限：daemon 回 `invalid` 且 `data.reason = "too_large"`（单张超限在 `blob.put_begin` 就被拒了）。 */
export const ATTACHMENTS_TOO_LARGE = 'attachments_too_large'

export function isAttachmentsTooLarge(e: unknown): boolean {
  return e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'invalid' && e.reason === 'too_large'
}

/** 发送 / 新建被 daemon 因附件拒绝时的原因（界面各自给说明）；不是附件的问题返回 undefined。 */
export type AttachmentFailure = typeof BLOB_EXPIRED | typeof BLOB_IN_USE | typeof ATTACHMENTS_TOO_LARGE

export function attachmentFailure(e: unknown): AttachmentFailure | undefined {
  if (isBlobExpired(e)) return BLOB_EXPIRED
  if (isBlobInUse(e)) return BLOB_IN_USE
  if (isAttachmentsTooLarge(e)) return ATTACHMENTS_TOO_LARGE
  return undefined
}

/** 这种失败可以靠「用本地缓存的字节重新上传」解决（blob 在节点上没了 / 已经被别的会话占了）。 */
export const needsReupload = (reason: string | undefined): boolean => reason === BLOB_EXPIRED || reason === BLOB_IN_USE

export interface SendOutcome {
  messageId: string
  state: 'accepted' | 'queued' | 'unsent' | 'failed'
  /** `failed` 的原因里调用方要区别对待的那几种：会话已归档（先取消归档）、附件的问题（ADR-35）。 */
  reason?: typeof ARCHIVED_REASON | AttachmentFailure
}

export function isArchivedRejection(e: unknown): boolean {
  return e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'invalid' && e.reason === ARCHIVED_REASON
}

/** 只改本地列表里的 `archived` 标志（daemon 的 `session.changed` 随后会给出同样的结果）。 */
function patchArchived(nodeId: string, sessionId: string, archived: boolean): void {
  useSessions.setState((s) => {
    const item = s.list[sessionId]
    if (!item || item.node_id !== nodeId || !!item.archived === archived) return s
    return { list: { ...s.list, [sessionId]: { ...item, archived } } }
  })
}

/** 发送用户消息：永远先落「待发」，成功后等事件流回显。节点离线 / undeliverable → 标未发送。`attachments` 是 `blob.put_end` 的结果（ADR-35）。 */
export async function sendMessage(nodeId: string, sessionId: string, content: string, attachments: readonly AttachmentRef[] = []): Promise<SendOutcome> {
  const messageId = uuid()
  useDrafts.getState().startInflight({ messageId, sessionId, content, attachments: [...attachments], createdAt: Date.now(), queued: false, accepted: false })
  return deliver(nodeId, sessionId, messageId, content, Date.now(), attachments)
}

async function deliver(nodeId: string, sessionId: string, messageId: string, content: string, createdAt: number, attachments: readonly AttachmentRef[]): Promise<SendOutcome> {
  const drafts = useDrafts.getState()
  try {
    const params: SessionSendParams = { session_id: sessionId, message_id: messageId, content, ...(attachments.length > 0 ? { attachments: [...attachments] } : {}) }
    const res = await rpc().request<SessionSendResult>(nodeId, 'session.send', params)
    drafts.removeUnsent(sessionId, messageId)
    useDrafts.getState().patchInflight(messageId, { accepted: true, queued: res.queued })
    scheduleEchoCheck(nodeId, sessionId, messageId)
    return { messageId, state: res.queued ? 'queued' : 'accepted' }
  } catch (e) {
    useDrafts.getState().endInflight(messageId)
    const definitive = e instanceof RpcCallError && e.kind === 'rpc'
    // 已归档：daemon 说了算。本地列表还没收到 `session.changed`（或根本没收到）时，输入区也要换成「已归档 · 取消归档」，
    // 消息留在「失败」里，取消归档之后可以点重试
    const archived = isArchivedRejection(e)
    // 附件的问题（已被 daemon 回收 / 已被别的会话占了 / 合计太大）：消息和附件引用都留着，说明原因，
    // 用户「编辑」时把图放回输入区（前两种从本地缓存重新上传一份）
    const attachment = attachmentFailure(e)
    drafts.addUnsent(sessionId, {
      message_id: messageId,
      content,
      created_at: createdAt,
      state: definitive ? 'failed' : 'unsent',
      error: definitive ? (archived ? ARCHIVED_REASON : (attachment ?? e.code ?? 'internal')) : undefined,
      ...(attachments.length > 0 ? { attachments: [...attachments] } : {}),
    })
    if (archived) {
      patchArchived(nodeId, sessionId, true)
      return { messageId, state: 'failed', reason: ARCHIVED_REASON }
    }
    if (attachment) return { messageId, state: 'failed', reason: attachment }
    if (definitive) return { messageId, state: 'failed' }
    return { messageId, state: 'unsent' }
  }
}

function scheduleEchoCheck(nodeId: string, sessionId: string, messageId: string): void {
  const seen = (): boolean => !!useSessions.getState().open[sessionId]?.view.blocks.some((b) => b.type === 'user' && b.messageId === messageId)
  setTimeout(() => {
    if (!useDrafts.getState().inflight[messageId]) return
    if (seen()) return useDrafts.getState().endInflight(messageId)
    if (useSessions.getState().open[sessionId]) void catchUp(nodeId, sessionId)
  }, 3000)
  setTimeout(() => useDrafts.getState().endInflight(messageId), 20_000)
}

/** 事件流里出现回显后清理发送中记录。 */
export function reapEchoed(sessionId: string): void {
  const o = useSessions.getState().open[sessionId]
  if (!o) return
  const ids = new Set<string>()
  for (const b of o.view.blocks) if (b.type === 'user') ids.add(b.messageId)
  const d = useDrafts.getState()
  for (const i of Object.values(d.inflight)) if (i.sessionId === sessionId && ids.has(i.messageId)) d.endInflight(i.messageId)
  for (const u of d.byKey[sessionId]?.unsent ?? []) if (ids.has(u.message_id)) d.removeUnsent(sessionId, u.message_id)
}

export async function resendUnsent(nodeId: string, sessionId: string, messageId: string): Promise<SendOutcome | null> {
  const u = useDrafts.getState().byKey[sessionId]?.unsent.find((m) => m.message_id === messageId)
  if (!u) return null
  const attachments = u.attachments ?? []
  useDrafts.getState().startInflight({ messageId, sessionId, content: u.content, attachments, createdAt: u.created_at, queued: false, accepted: false })
  return deliver(nodeId, sessionId, messageId, u.content, u.created_at, attachments)
}

/** 节点重新就绪后，按顺序重发本地所有「未发送」。 */
export async function flushUnsent(nodeId: string): Promise<number> {
  await useDrafts.getState().loadAll() // 重载后还没打开过的会话里的未发送消息也要发
  const drafts = useDrafts.getState()
  let n = 0
  for (const [key, d] of Object.entries(drafts.byKey)) {
    const item = useSessions.getState().list[key]
    if (!item || item.node_id !== nodeId) continue
    for (const u of d.unsent.filter((m) => m.state === 'unsent').sort((a, b) => a.created_at - b.created_at)) {
      const r = await resendUnsent(nodeId, key, u.message_id)
      if (r?.state === 'unsent') return n // 又断了，别继续
      n += 1
    }
  }
  return n
}

export function countUnsent(nodeId: string | null): number {
  const { byKey } = useDrafts.getState()
  const { list } = useSessions.getState()
  let n = 0
  for (const [key, d] of Object.entries(byKey)) {
    const item = list[key]
    if (!item || (nodeId && item.node_id !== nodeId)) continue
    n += d.unsent.filter((m) => m.state === 'unsent').length
  }
  return n
}

// ---- 其它会话操作 ----

export async function cancelTurn(nodeId: string, sessionId: string): Promise<void> {
  await rpc().request(nodeId, 'session.cancel', { session_id: sessionId })
}

/**
 * `answered`：daemon 接受了答复。
 * `denied_folder_changed`：daemon 回 `not_found` 且 `data.reason === "project_dir_changed"`——关联项目的目录在 turn 进行中被换掉了，
 * 请求已被 daemon 消耗并按**拒绝**答复了 agent，会话随后以 `error { not_found }` / `turn_done { error }` 收尾
 * （docs/protocol.md § 5.5）。这不是失败：不能重试同一个请求，也不该弹通用的错误提示。
 * `request_gone`：别的原因的 `not_found`（未知的 `request_id`、找不到会话）——请求已经不存在，同样不重试；
 * 只能说「请求已不存在」，**不能**说目录变了或本轮已中止。
 * 两种都把请求从待审批里去掉，调用方把卡片定格成对应的说明。其余错误照常抛出。
 */
export type RespondOutcome = 'answered' | 'denied_folder_changed' | 'request_gone'

/** daemon 在 `permission.respond` 的 `not_found` 上带的原因：关联项目的目录在 turn 进行中变了。 */
export const PROJECT_DIR_CHANGED = 'project_dir_changed'

export async function respondPermission(nodeId: string, params: PermissionRespondParams): Promise<RespondOutcome> {
  try {
    await rpc().request(nodeId, 'permission.respond', params)
  } catch (e) {
    if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found') {
      useSessions.getState().resolvePending(nodeId, params.request_id)
      return e.reason === PROJECT_DIR_CHANGED ? 'denied_folder_changed' : 'request_gone'
    }
    throw e
  }
  useSessions.getState().resolvePending(nodeId, params.request_id)
  return 'answered'
}

/** 改会话设置。`update.clear` 里点名的字段被清回「未设置」；返回并采用 daemon 给出的、生效后的完整设置。 */
export async function setSessionSettings(nodeId: string, sessionId: string, update: SettingsUpdate): Promise<SessionSettings> {
  const params: SessionSetSettingsParams = { session_id: sessionId, settings: update.settings, ...(update.clear.length > 0 ? { clear: update.clear } : {}) }
  const res = await rpc().request<SessionSetSettingsResult>(nodeId, 'session.set_settings', params)
  const item = useSessions.getState().list[sessionId]
  if (item) useSessions.setState((s) => ({ list: { ...s.list, [sessionId]: { ...item, settings: res.settings } } }))
  return res.settings
}

export async function renameSession(nodeId: string, sessionId: string, title: string): Promise<void> {
  await rpc().request(nodeId, 'session.rename', { session_id: sessionId, title })
}
/**
 * 归档 / 取消归档。`opts.worktree` 只在有 worktree 的会话归档时给（ADR-34）；没有 worktree 的会话照旧只发 `session_id`。
 */
export async function archiveSession(nodeId: string, sessionId: string, archive: boolean, opts: { worktree?: WorktreeDisposal } = {}): Promise<void> {
  if (!archive) {
    await unarchiveSession(nodeId, sessionId)
    return
  }
  const params: SessionArchiveParams = { session_id: sessionId, ...(opts.worktree ? { worktree: opts.worktree } : {}) }
  await rpc().request(nodeId, 'session.archive', params)
}
/**
 * 取消归档。成功之后立刻把本地的 `archived` 清掉（会话页的输入区据此换回输入框），不等 `session.changed`；
 * 失败（比如 worktree 重建失败，`git_failed`）会话仍然是归档的，原样抛出。
 */
export async function unarchiveSession(nodeId: string, sessionId: string): Promise<void> {
  await rpc().request(nodeId, 'session.unarchive', { session_id: sessionId })
  patchArchived(nodeId, sessionId, false)
}
/** 删除会话。`opts` 只在有 worktree 的会话上给：worktree 怎么处理、要不要同时删分支。 */
export async function deleteSession(nodeId: string, sessionId: string, opts: { worktree?: WorktreeDisposal; deleteBranch?: boolean } = {}): Promise<void> {
  const params: SessionDeleteParams = {
    session_id: sessionId,
    ...(opts.worktree ? { worktree: opts.worktree } : {}),
    ...(opts.deleteBranch ? { delete_branch: true } : {}),
  }
  await rpc().request(nodeId, 'session.delete', params)
  useSessions.getState().removeSessions([sessionId], nodeId)
}
export async function restartSession(nodeId: string, sessionId: string): Promise<void> {
  await rpc().request(nodeId, 'session.restart', { session_id: sessionId })
}
export async function promoteSession(nodeId: string, sessionId: string, name: string): Promise<void> {
  await rpc().request(nodeId, 'session.promote', { session_id: sessionId, name })
}

/** session.create 的结果形状没在协议里定义：兼容 SessionSummary / { session } / { session_id } / { id }。 */
function extractSummary(res: unknown): { id: string; summary?: SessionSummary } {
  const o = isRecord(res) ? res : {}
  const nested = o.session
  if (isRecord(nested) && typeof nested.id === 'string') return { id: nested.id, summary: nested as unknown as SessionSummary }
  if (typeof o.id === 'string' && 'agent' in o) return { id: o.id, summary: o as unknown as SessionSummary }
  const id = typeof o.session_id === 'string' ? o.session_id : typeof o.id === 'string' ? o.id : undefined
  if (!id) throw new Error('session.create returned no session id')
  return { id }
}

export interface CreateSessionInput {
  project?: string | null
  agent: string
  prompt?: string
  /** 第一条消息的附件（`blob.put_end` 的结果，ADR-35）。`prompt` 为空但有附件时也发第一条消息。 */
  attachments?: readonly AttachmentRef[]
  settings: SessionSettings
  /** 在哪里工作（ADR-34）。缺省不带，daemon 按 `local` 处理——线路上不猜默认值。 */
  workspace?: SessionWorkspace
}

/**
 * 没成功的 `session.create` 用过的 `message_id`，按请求内容（节点、项目、agent、文字、图片、设置、工作位置）记着。
 * 超时但 daemon 其实建好了的情况下，用户原样重发：沿用同一个 `message_id`，daemon 按它幂等，返回第一次建好的会话
 * （附件这时已经绑在那个会话上了，换新的 id 只会得到 `blob_in_use`）。内容变了就是另一次请求，用新的 id。成功后忘掉。
 */
const failedCreates = new Map<string, string>()
const MAX_FAILED_CREATES = 20
onDraftsReset(() => failedCreates.clear())

function createFingerprint(nodeId: string, input: CreateSessionInput, attachments: readonly AttachmentRef[]): string {
  return JSON.stringify([nodeId, input.project ?? null, input.agent, input.prompt ?? '', attachments.map((a) => a.blob_id), input.settings, input.workspace ?? null])
}

export async function createSession(nodeId: string, input: CreateSessionInput): Promise<string> {
  const attachments = input.attachments ?? []
  const hasMessage = !!input.prompt || attachments.length > 0
  const fingerprint = hasMessage ? createFingerprint(nodeId, input, attachments) : null
  const messageId = fingerprint ? (failedCreates.get(fingerprint) ?? uuid()) : undefined
  if (fingerprint && messageId) {
    failedCreates.delete(fingerprint) // 重新放到最后（最近用过）
    failedCreates.set(fingerprint, messageId)
    if (failedCreates.size > MAX_FAILED_CREATES) failedCreates.delete(failedCreates.keys().next().value!)
  }
  const params: SessionCreateParams = {
    project: input.project ?? undefined,
    agent: input.agent,
    prompt: input.prompt || undefined,
    ...(attachments.length > 0 ? { attachments: [...attachments] } : {}),
    message_id: messageId,
    settings: input.settings,
    ...(input.workspace ? { workspace: input.workspace } : {}),
  }
  const res = await rpc().request<unknown>(nodeId, 'session.create', params)
  if (fingerprint) failedCreates.delete(fingerprint)
  const { id, summary } = extractSummary(res)
  if (summary) useSessions.getState().upsertSummaries(nodeId, [summary])
  else
    await rpc()
      .request<SessionSummary>(nodeId, 'session.get', { session_id: id })
      .then((s) => useSessions.getState().upsertSummaries(nodeId, [s]))
      .catch(warnOnError('fetch the new session’s details'))
  return id
}
