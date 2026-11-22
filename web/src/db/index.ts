// IndexedDB（idb）。schema 见 spec/web.md § 6.2。按 origin 隔离，这是 ADR-23 接受的代价。
import { deleteShareDb } from '@/sw/shareStore'
import { deleteDB, openDB, type DBSchema, type IDBPDatabase } from 'idb'
import type { AgentInfo, AttachmentRef, NodeInfo, Project, SessionEvent, SessionSummary, Todo } from '@/protocol'
import { isClosed, type OutboxRecord } from '@/features/todos/types'
import type { RuleSyncRecord } from '@/features/rules/syncTypes'
import { bytesEqual } from '@/lib/bytes'

export interface IdentityRecord {
  id: 'self'
  /** 配对时 hub 签发的正式设备 id；还没配对过是 undefined。 */
  device_id?: string
  device_token?: string
  /**
   * `device_token` 的签发时间（Unix 毫秒）：配对 / 接入拿到它的时刻，或最近一次轮换的时刻。没有 = 未知，按「很旧」处理，
   * 下次连上 hub 就轮换一次（spec/web.md § 6.4）。
   */
  device_token_issued_at_ms?: number
  /** 本设备 Noise 静态密钥。 */
  static_keypair: { public: Uint8Array; secret: Uint8Array }
  device_name: string
  /** 创建这份身份时登录的账号；换账号登录时整库清空。 */
  account_id?: string
}

export interface NodeRecord {
  node_id: string
  name: string
  /** 配对时从握手学到的 daemon 静态公钥。 */
  public_key: Uint8Array
  info?: NodeInfo
  agents?: AgentInfo[]
  agents_etag?: string
  /** 项目列表缓存：节点离线 / 刚重载时先显示它，而不是「没有项目」。 */
  projects?: Project[]
  workspace_root?: string
  /** 本机镜像：最近一次 node.set_agent_defaults 写进去的值（daemon 不提供读接口）。 */
  agent_defaults?: Record<string, NodeAgentDefaults>
  paired_at: number
  last_seen: number
}

export interface NodeAgentDefaults {
  model?: string | null
  thinking?: string | null
  mode?: string | null
  options?: Record<string, unknown> | null
}

export interface SessionRecord {
  session_id: string
  node_id: string
  project: string | null
  agent: string
  title: string
  status: SessionSummary['status']
  options: SessionSummary['settings']
  last_seq: number
  updated_at: number
  /** 本地缓存的事件区间下界（向上翻页后会变小）；缓存区间是连续的 [low_seq, last_seq]。 */
  low_seq: number
  summary: SessionSummary
}

export interface EventRecord {
  session_id: string
  seq: number
  event: SessionEvent
}

export interface UnsentMessage {
  message_id: string
  content: string
  created_at: number
  /** unsent：没发出去（节点离线 / undeliverable）；failed：daemon 拒绝了。 */
  state: 'unsent' | 'failed'
  error?: string
  /** 随消息一起留在本地的附件引用（ADR-35）；重发时原样再发。 */
  attachments?: AttachmentRef[]
}

export interface DraftRecord {
  session_id: string
  text: string
  /** 输入区里已经传完的附件（ADR-35）；刷新后还在。上传中 / 失败的不存。 */
  attachments?: AttachmentRef[]
  unsent: UnsentMessage[]
}

/** 附件缓存（ADR-35，LRU 上限 200 MiB，见 features/attachments/cache.ts）。读到缺字段的旧记录当缓存未命中。 */
export interface BlobRecord {
  blob_id: string
  bytes: Uint8Array
  mime: string
  last_used_ms: number
}

/** 清单的本地缓存：daemon 最近一次给的那一份（`Todo` 原样），不含还没同步的本地改动（那些在 `todo_outbox`）。明文，和会话缓存同样的暴露面。 */
export interface TodoRecord {
  /** `${node_id}:${todo.id}` */
  key: string
  node_id: string
  todo: Todo
}

/** 每个节点上一次同步清单的时间（界面上「缓存于 …」）。 */
export interface TodoMetaRecord {
  node_id: string
  synced_at: number
}

interface AnyconDB extends DBSchema {
  rule_outbox: { key: number; value: RuleSyncRecord; indexes: { by_node: string; by_rule: [string, string] } }
  keys: { key: string; value: IdentityRecord }
  nodes: { key: string; value: NodeRecord }
  sessions: { key: string; value: SessionRecord; indexes: { by_node: string } }
  events: { key: [string, number]; value: EventRecord }
  drafts: { key: string; value: DraftRecord }
  blobs: { key: string; value: BlobRecord }
  todos: { key: string; value: TodoRecord; indexes: { by_node: string } }
  todo_meta: { key: string; value: TodoMetaRecord }
  todo_outbox: { key: number; value: OutboxRecord; indexes: { by_node: string } }
}

export const DB_NAME = 'anycon'
/** 3：同组规则期望状态（`rule_outbox`）。升级只新增仓，不动已有的。 */
export const DB_VERSION = 3

let dbPromise: Promise<IDBPDatabase<AnyconDB>> | null = null

/**
 * 本地库已被另一个标签页清掉（「退出并移除本设备」）或要升级：这个标签页不能再碰它——
 * 再 `openDB` 会把刚删掉的库（连同私钥 / token）又建出来写回去。
 */
let disabled = false
let blockedHandler: (() => void) | null = null

export class DbDisabledError extends Error {
  constructor() {
    super('the local database was closed because it was wiped (or upgraded) in another tab')
    this.name = 'DbDisabledError'
  }
}

/** 之后所有读写都以 `DbDisabledError` 失败，并关掉已打开的连接。 */
export function disableDb(): void {
  disabled = true
  const closing = dbPromise
  dbPromise = null
  void closing?.then((d) => d.close()).catch((e: unknown) => console.warn('anycon: closing the local database failed', e))
}

/** 别的标签页要删库 / 升级库时的回调（此时本标签页的库已经被禁用）。 */
export function setBlockedHandler(fn: (() => void) | null): void {
  blockedHandler = fn
}

export function getDb(): Promise<IDBPDatabase<AnyconDB>> {
  if (disabled) return Promise.reject(new DbDisabledError())
  dbPromise ??= openDB<AnyconDB>(DB_NAME, DB_VERSION, {
    upgrade(db, oldVersion) {
      if (oldVersion < 1) {
        db.createObjectStore('keys', { keyPath: 'id' })
        db.createObjectStore('nodes', { keyPath: 'node_id' })
        const sessions = db.createObjectStore('sessions', { keyPath: 'session_id' })
        sessions.createIndex('by_node', 'node_id')
        db.createObjectStore('events', { keyPath: ['session_id', 'seq'] })
        db.createObjectStore('drafts', { keyPath: 'session_id' })
        db.createObjectStore('blobs', { keyPath: 'blob_id' })
      }
      if (oldVersion < 3) {
        const rules = db.createObjectStore('rule_outbox', { keyPath: 'seq', autoIncrement: true })
        rules.createIndex('by_node', 'node')
        rules.createIndex('by_rule', ['node', 'rule_id'], { unique: true })
      }
      if (oldVersion < 2) {
        const todos = db.createObjectStore('todos', { keyPath: 'key' })
        todos.createIndex('by_node', 'node_id')
        db.createObjectStore('todo_meta', { keyPath: 'node_id' })
        const outbox = db.createObjectStore('todo_outbox', { keyPath: 'seq', autoIncrement: true })
        outbox.createIndex('by_node', 'node_id')
      }
    },
    terminated() {
      dbPromise = null
    },
    // 别的标签页要删库（退出并移除本设备）时，让出连接，否则 deleteDB 会一直等
    // 要删库 / 升级的只会是「退出并移除本设备」或新版本：这个标签页从这一刻起不再写它
    blocking() {
      disableDb()
      blockedHandler?.()
    },
  }).catch((e: unknown) => {
    dbPromise = null
    throw e
  })
  return dbPromise
}

/**
 * 关闭并删掉整个 IndexedDB（身份私钥、device token、节点、会话缓存、草稿都在里面）。
 * 「退出并移除本设备」用它；测试也用它隔离。
 */
export async function resetDb(): Promise<void> {
  disabled = false // 发起删库的这一方（或测试）自己可以重新打开
  if (dbPromise) {
    const db = await dbPromise.catch(() => null)
    db?.close()
  }
  dbPromise = null
  await deleteDB(DB_NAME)
}

/** 清空全部本地数据（换账号登录时）。 */
export async function wipeAll(): Promise<void> {
  const db = await getDb()
  const tx = db.transaction(['keys', 'nodes', 'sessions', 'events', 'drafts', 'blobs', 'todos', 'todo_meta', 'todo_outbox', 'rule_outbox'], 'readwrite')
  await Promise.all([
    tx.objectStore('todos').clear(),
    tx.objectStore('todo_meta').clear(),
    tx.objectStore('todo_outbox').clear(),
    tx.objectStore('rule_outbox').clear(),
    tx.objectStore('keys').clear(),
    tx.objectStore('nodes').clear(),
    tx.objectStore('sessions').clear(),
    tx.objectStore('events').clear(),
    tx.objectStore('drafts').clear(),
    tx.objectStore('blobs').clear(),
    tx.done,
  ])
  // 换账号：账号 A 分享进来、还没被取走的内容不能在账号 B 的输入条里冒出来
  await deleteShareDb().catch((e: unknown) => console.warn('anycon: could not delete the share stash', e))
}

// ---- keys ----

export async function getIdentity(): Promise<IdentityRecord | undefined> {
  return (await getDb()).get('keys', 'self')
}

/**
 * 写身份时和库里已有的那份合并。同一个浏览器的几个标签页各持一份内存里的身份：一个标签页轮换了 device token 并落盘之后，
 * 另一个还拿着旧 token 的标签页再写身份（改设备名、补 device_id……）不能把新 token 盖回旧的。
 *
 * 规则：库里和写入的都有 token、token 不同、两份是同一个设备身份（Noise 公钥相同），且库里的签发时间严格更晚
 * （未知按 0）→ token 连同签发时间和设备 id 取库里的，其余字段取写入的。其他情况原样写入：
 * 写入的没有 token（显式清除）、签发时间不更旧（配对 / 轮换拿到的新 token）、换了身份。
 */
export function mergeIdentity(stored: IdentityRecord | undefined, incoming: IdentityRecord): IdentityRecord {
  if (!stored?.device_token || !incoming.device_token || stored.device_token === incoming.device_token) return incoming
  if (!bytesEqual(stored.static_keypair.public, incoming.static_keypair.public)) return incoming
  if ((stored.device_token_issued_at_ms ?? 0) <= (incoming.device_token_issued_at_ms ?? 0)) return incoming
  const { device_id: _d, device_token_issued_at_ms: _i, ...rest } = incoming
  return {
    ...rest,
    device_token: stored.device_token,
    ...(stored.device_token_issued_at_ms !== undefined ? { device_token_issued_at_ms: stored.device_token_issued_at_ms } : {}),
    ...(stored.device_id !== undefined ? { device_id: stored.device_id } : {}),
  }
}

/** 一个读写事务里「读 → 合并 → 写」（见 [`mergeIdentity`]）。 */
export async function putIdentity(rec: IdentityRecord): Promise<void> {
  const db = await getDb()
  const tx = db.transaction('keys', 'readwrite')
  const store = tx.objectStore('keys')
  const stored = await store.get(rec.id)
  await store.put(mergeIdentity(stored, rec))
  await tx.done
}

/**
 * 清掉库里的 device token（和它的设备 id、签发时间），**仅当库里的 token 还是 `expected`**：hub 拒绝的是我们手里的这一份，
 * 别的标签页可能已经轮换并存了新的，那份是有效的，不能抹掉。返回是否清了。
 */
export async function clearDeviceToken(expected: string): Promise<boolean> {
  const db = await getDb()
  const tx = db.transaction('keys', 'readwrite')
  const store = tx.objectStore('keys')
  const stored = await store.get('self')
  if (!stored || stored.device_token !== expected) {
    await tx.done
    return false
  }
  const { device_token: _t, device_id: _d, device_token_issued_at_ms: _i, ...rest } = stored
  await store.put(rest)
  await tx.done
  return true
}

// ---- nodes ----

export async function listNodes(): Promise<NodeRecord[]> {
  return (await getDb()).getAll('nodes')
}

export async function putNode(rec: NodeRecord): Promise<void> {
  await (await getDb()).put('nodes', rec)
}

export async function deleteNode(nodeId: string): Promise<void> {
  const db = await getDb()
  const sessions = await db.getAllFromIndex('sessions', 'by_node', nodeId)
  const tx = db.transaction(['nodes', 'sessions', 'events', 'drafts', 'todos', 'todo_meta', 'todo_outbox', 'rule_outbox'], 'readwrite')
  await tx.objectStore('nodes').delete(nodeId)
  for (const k of await tx.objectStore('rule_outbox').index('by_node').getAllKeys(nodeId)) await tx.objectStore('rule_outbox').delete(k)
  // 这台节点的清单缓存和还没同步的改动一起删（节点被忘记 / 取消配对之后它们没有地方可发了）
  await tx.objectStore('todo_meta').delete(nodeId)
  for (const k of await tx.objectStore('todos').index('by_node').getAllKeys(nodeId)) await tx.objectStore('todos').delete(k)
  for (const k of await tx.objectStore('todo_outbox').index('by_node').getAllKeys(nodeId)) await tx.objectStore('todo_outbox').delete(k)
  for (const s of sessions) {
    await tx.objectStore('sessions').delete(s.session_id)
    await tx.objectStore('events').delete(IDBKeyRange.bound([s.session_id, 0], [s.session_id, Number.MAX_SAFE_INTEGER]))
    await tx.objectStore('drafts').delete(s.session_id)
  }
  await tx.done
}

// ---- sessions ----

export async function listSessions(nodeId: string): Promise<SessionRecord[]> {
  return (await getDb()).getAllFromIndex('sessions', 'by_node', nodeId)
}

export async function putSessions(recs: SessionRecord[]): Promise<void> {
  if (recs.length === 0) return
  const tx = (await getDb()).transaction('sessions', 'readwrite')
  await Promise.all([...recs.map((r) => tx.store.put(r)), tx.done])
}

export async function deleteSession(sessionId: string): Promise<void> {
  const db = await getDb()
  const tx = db.transaction(['sessions', 'events', 'drafts'], 'readwrite')
  await Promise.all([
    tx.objectStore('sessions').delete(sessionId),
    tx.objectStore('events').delete(IDBKeyRange.bound([sessionId, 0], [sessionId, Number.MAX_SAFE_INTEGER])),
    tx.objectStore('drafts').delete(sessionId),
    tx.done,
  ])
}

// ---- events ----

export async function getEvents(sessionId: string): Promise<SessionEvent[]> {
  const range = IDBKeyRange.bound([sessionId, 0], [sessionId, Number.MAX_SAFE_INTEGER])
  const recs = await (await getDb()).getAll('events', range)
  return recs.map((r) => r.event)
}

export async function putEvents(sessionId: string, events: SessionEvent[]): Promise<void> {
  if (events.length === 0) return
  const tx = (await getDb()).transaction('events', 'readwrite')
  await Promise.all([...events.map((event) => tx.store.put({ session_id: sessionId, seq: event.seq, event })), tx.done])
}

export async function clearEvents(sessionId: string): Promise<void> {
  await (await getDb()).delete('events', IDBKeyRange.bound([sessionId, 0], [sessionId, Number.MAX_SAFE_INTEGER]))
}

// ---- drafts ----

export async function getDraft(sessionId: string): Promise<DraftRecord | undefined> {
  return (await getDb()).get('drafts', sessionId)
}

export async function putDraft(rec: DraftRecord): Promise<void> {
  const db = await getDb()
  if (rec.text === '' && rec.unsent.length === 0 && !rec.attachments?.length) await db.delete('drafts', rec.session_id)
  else await db.put('drafts', rec)
}

export async function listDrafts(): Promise<DraftRecord[]> {
  return (await getDb()).getAll('drafts')
}

// ---- blobs（附件缓存） ----

/** 记录形状对得上（旧版本只有 `blob_id` 和 `bytes`，缺 `mime` / `last_used_ms`）。 */
export function isBlobRecord(rec: unknown): rec is BlobRecord {
  if (typeof rec !== 'object' || rec === null) return false
  const r = rec as Partial<Record<keyof BlobRecord, unknown>>
  return typeof r.blob_id === 'string' && typeof r.mime === 'string' && r.mime !== '' && typeof r.last_used_ms === 'number' && ArrayBuffer.isView(r.bytes)
}

/** 命中返回记录，没有或形状不对（旧记录）返回 undefined。 */
export async function getBlobRecord(blobId: string): Promise<BlobRecord | undefined> {
  const rec: unknown = await (await getDb()).get('blobs', blobId)
  return isBlobRecord(rec) ? rec : undefined
}

export async function putBlobRecord(rec: BlobRecord): Promise<void> {
  await (await getDb()).put('blobs', rec)
}

export async function deleteBlobRecords(blobIds: readonly string[]): Promise<void> {
  if (blobIds.length === 0) return
  const tx = (await getDb()).transaction('blobs', 'readwrite')
  await Promise.all([...blobIds.map((id) => tx.store.delete(id)), tx.done])
}

export interface BlobScan {
  /** 形状对的记录的大小和最近使用时间。 */
  entries: Array<{ blob_id: string; size: number; last_used_ms: number }>
  /** 旧形状的记录（读不了，该删）。 */
  invalid: string[]
}

/** 扫一遍缓存（要读出每条记录；只在第一次写缓存时做一次）。 */
export async function scanBlobRecords(): Promise<BlobScan> {
  const out: BlobScan = { entries: [], invalid: [] }
  const tx = (await getDb()).transaction('blobs', 'readonly')
  let cursor = await tx.store.openCursor()
  while (cursor) {
    const rec: unknown = cursor.value
    if (isBlobRecord(rec)) out.entries.push({ blob_id: rec.blob_id, size: rec.bytes.byteLength, last_used_ms: rec.last_used_ms })
    else out.invalid.push(String(cursor.key))
    cursor = await cursor.continue()
  }
  await tx.done
  return out
}

// ---- todos（清单，docs/spec/daemon.md § 21） ----

export async function listTodoRecords(nodeId: string): Promise<{ todos: Todo[]; synced_at?: number }> {
  const db = await getDb()
  const [recs, meta] = await Promise.all([db.getAllFromIndex('todos', 'by_node', nodeId), db.get('todo_meta', nodeId)])
  return { todos: recs.map((r) => r.todo), ...(meta ? { synced_at: meta.synced_at } : {}) }
}

/** 写入 / 删除一批缓存的项；`syncedAt` 给了就同时记下同步时间。一个事务。 */
export async function applyTodoCache(nodeId: string, change: { put?: readonly Todo[]; remove?: readonly string[]; syncedAt?: number }): Promise<void> {
  const db = await getDb()
  const tx = db.transaction(['todos', 'todo_meta'], 'readwrite')
  const ops: Promise<unknown>[] = [tx.done]
  for (const t of change.put ?? []) ops.push(tx.objectStore('todos').put({ key: `${nodeId}:${t.id}`, node_id: nodeId, todo: t }))
  for (const id of change.remove ?? []) ops.push(tx.objectStore('todos').delete(`${nodeId}:${id}`))
  if (change.syncedAt !== undefined) ops.push(tx.objectStore('todo_meta').put({ node_id: nodeId, synced_at: change.syncedAt }))
  await Promise.all(ops)
}

/**
 * 一次同步（翻完没关闭的项）的结果落盘：在**一个**读写事务里读出这个节点缓存里没关闭的项、删掉不在 `keep` 里的、写入 `keep`。
 * 事务一开始就建（前面没有 await），`todo.changed` 自己的写入排在它后面，不会被这里较旧的 `keep` 覆盖。
 */
export async function commitOpenList(nodeId: string, keepNow: () => readonly Todo[], syncedAt: number): Promise<void> {
  const db = await getDb()
  const tx = db.transaction(['todos', 'todo_meta'], 'readwrite')
  // 事务建好的这一刻才算要写的内容：在这之前到的变更已经并进来，在这之后到的，它们自己的写入排在这个事务后面
  const keep = keepNow()
  const store = tx.objectStore('todos')
  const keepIds = new Set(keep.map((t) => t.id))
  const existing = await store.index('by_node').getAll(nodeId)
  const ops: Promise<unknown>[] = [tx.done]
  for (const r of existing) if (!isClosed(r.todo.state) && !keepIds.has(r.todo.id)) ops.push(store.delete(r.key))
  for (const t of keep) ops.push(store.put({ key: `${nodeId}:${t.id}`, node_id: nodeId, todo: t }))
  ops.push(tx.objectStore('todo_meta').put({ node_id: nodeId, synced_at: syncedAt }))
  await Promise.all(ops)
}

/** 「已关闭」从头翻到尾之后：缓存里已关闭、这一趟没翻到、期间也没变过的项（别处删了）删掉。 */
export async function pruneClosedCache(nodeId: string, seen: ReadonlySet<string>, touched: ReadonlyMap<string, unknown>): Promise<void> {
  const db = await getDb()
  const tx = db.transaction('todos', 'readwrite')
  const existing = await tx.store.index('by_node').getAll(nodeId)
  const ops: Promise<unknown>[] = [tx.done]
  for (const r of existing) if (isClosed(r.todo.state) && !seen.has(r.todo.id) && !touched.has(r.todo.id)) ops.push(tx.store.delete(r.key))
  await Promise.all(ops)
}

/** 追加一条删除；这一项已有的失败记录和排在它们后面的记录一起丢掉（发不出去的记录不能留着、同时又把项藏起来）。 */
export async function appendDelete(rec: OutboxRecord): Promise<number> {
  const tx = (await getDb()).transaction('todo_outbox', 'readwrite')
  const mine = (await tx.store.index('by_node').getAll(rec.node_id)).filter((r) => r.id === rec.id).sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))
  const firstFailed = mine.find((r) => r.state === 'failed')
  if (firstFailed) for (const r of mine) if ((r.seq ?? 0) >= (firstFailed.seq ?? 0)) await tx.store.delete(r.seq!)
  const seq = Number(await tx.store.add(rec))
  await tx.done
  return seq
}

export async function listOutbox(nodeId?: string): Promise<OutboxRecord[]> {
  const db = await getDb()
  const all = nodeId ? await db.getAllFromIndex('todo_outbox', 'by_node', nodeId) : await db.getAll('todo_outbox')
  return all.sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))
}

/** 追加一条待发改动（排在最后）。不合并、不取消：重放就是按这个日志的顺序一条条发。返回 `seq`。 */
export async function appendOutbox(rec: OutboxRecord): Promise<number> {
  return Number(await (await getDb()).add('todo_outbox', rec))
}

export async function deleteOutbox(seqs: readonly number[]): Promise<void> {
  if (seqs.length === 0) return
  const tx = (await getDb()).transaction('todo_outbox', 'readwrite')
  await Promise.all([...seqs.map((q) => tx.store.delete(q)), tx.done])
}

/**
 * 认领下一条要发的：在**一个**读写事务里选出来并写上认领（只改库里现在的这条记录，不写回之前读到的快照，
 * 所以事务之间入队 / 重试的改动不会被盖掉）。
 *   - 每一项只有它最前面的那一条有资格（同一项内保持顺序）；最前面的是 `failed`，这一项就等着，别的项照常；
 *   - 被别的实例认领、而且认领还没过期（`takeoverMs` 内）的跳过；`takeoverMs = 0`：持有 Web Lock 的实例独占，库里留的认领都是死掉的实例留下的，
 *     不论年龄一律接手（时钟回拨留下的「未来」时间戳也一样）；只有租约（没有 Web Locks）才用下面那条保守的年龄规则。
 */
export async function claimNextOutbox(nodeId: string, owner: string, now: number, takeoverMs: number): Promise<OutboxRecord | null> {
  const tx = (await getDb()).transaction('todo_outbox', 'readwrite')
  const recs = (await tx.store.index('by_node').getAll(nodeId)).sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))
  const seen = new Set<string>()
  for (const r of recs) {
    if (seen.has(r.id)) continue
    seen.add(r.id)
    if (r.state !== 'pending') continue
    // 时钟往回拨过（认领的时刻在「现在」之后）时差是负数，同样不接手：宁可多等一会儿，也不把别人正在发的那条抢过来发两遍
    // `takeoverMs = 0`（持有 Web Lock）：别的实例的认领一律是死掉的，不管它盖的是什么时刻
    if (r.claimed_at !== undefined && r.claimed_by !== owner && takeoverMs > 0 && now - r.claimed_at < takeoverMs) continue
    const claimed: OutboxRecord = { ...r, claimed_at: now, claimed_by: owner }
    await tx.store.put(claimed)
    await tx.done
    return claimed
  }
  await tx.done
  return null
}

/**
 * 把一条记录标成失败（`f` 拿到库里现在的样子，返回标了失败的新样子；返回的不是 `failed` 就原样写回）。同一个事务里：这一项排在它后面的
 * 已经有一条删除时，失败的这条和夹在中间的记录一起丢掉，让删除能发出去（不能留着一条发不出去的失败记录挡在删除前面）。
 * 返回丢掉了几条记录（0 = 只是标了失败）。
 */
export async function failOutbox(seq: number, f: (stored: OutboxRecord) => OutboxRecord): Promise<number> {
  const tx = (await getDb()).transaction('todo_outbox', 'readwrite')
  const stored = await tx.store.get(seq)
  if (!stored) {
    await tx.done
    return 0
  }
  const next = f(stored)
  let dropped = 0
  if (next.state === 'failed') {
    const mine = (await tx.store.index('by_node').getAll(stored.node_id)).filter((r) => r.id === stored.id).sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))
    const del = mine.find((r) => (r.seq ?? 0) > seq && r.op.kind === 'delete')
    if (del) {
      for (const r of mine) {
        if ((r.seq ?? 0) >= seq && (r.seq ?? 0) < (del.seq ?? 0)) {
          await tx.store.delete(r.seq!)
          dropped++
        }
      }
      await tx.done
      return dropped
    }
  }
  await tx.store.put(next)
  await tx.done
  return 0
}

/** 读—改—写一条记录（`f` 拿到库里现在的样子；返回 `null` 删掉）。记录已经不在了（被丢弃 / 别处发完了）返回 `false`。 */
export async function patchOutbox(seq: number, f: (stored: OutboxRecord) => OutboxRecord | null): Promise<boolean> {
  const tx = (await getDb()).transaction('todo_outbox', 'readwrite')
  const stored = await tx.store.get(seq)
  if (!stored) {
    await tx.done
    return false
  }
  const next = f(stored)
  if (next) await tx.store.put(next)
  else await tx.store.delete(seq)
  await tx.done
  return true
}

/** 丢掉这一项从 `fromSeq` 起（含）的全部待发改动，返回丢掉了几条。 */
export async function discardOutboxFrom(nodeId: string, id: string, fromSeq: number): Promise<number> {
  const tx = (await getDb()).transaction('todo_outbox', 'readwrite')
  const mine = (await tx.store.index('by_node').getAll(nodeId)).filter((r) => r.id === id && (r.seq ?? 0) >= fromSeq)
  for (const r of mine) await tx.store.delete(r.seq!)
  await tx.done
  return mine.length
}

/** 这台节点上的待发改动和缓存一起清掉（节点被忘记）。返回丢掉的待发改动数。 */
export async function dropNodeTodos(nodeId: string): Promise<number> {
  const db = await getDb()
  const tx = db.transaction(['todos', 'todo_meta', 'todo_outbox'], 'readwrite')
  const pending = await tx.objectStore('todo_outbox').index('by_node').getAllKeys(nodeId)
  for (const k of pending) await tx.objectStore('todo_outbox').delete(k)
  for (const k of await tx.objectStore('todos').index('by_node').getAllKeys(nodeId)) await tx.objectStore('todos').delete(k)
  await tx.objectStore('todo_meta').delete(nodeId)
  await tx.done
  return pending.length
}

// ---- 同组规则期望状态 ----

/** 同一个目标节点和规则只保留最后写下的状态；多个目标在一个事务里持久化。 */
export async function putRuleSync(records: readonly RuleSyncRecord[]): Promise<void> {
  if (records.length === 0) return
  const tx = (await getDb()).transaction('rule_outbox', 'readwrite')
  for (const rec of records) {
    const key = await tx.store.index('by_rule').getKey([rec.node, rec.rule_id])
    const old = key === undefined ? undefined : await tx.store.get(key)
    if (key !== undefined) await tx.store.delete(key)
    await tx.store.add({ ...rec, ...(old?.claimed_at !== undefined ? { claimed_at: old.claimed_at, claimed_by: old.claimed_by } : {}) })
  }
  await tx.done
}

export async function listRuleSync(): Promise<RuleSyncRecord[]> {
  return (await getDb()).getAll('rule_outbox')
}

/** 按顺序认领；无 Web Locks 时，事务里的租约避免多个标签页同时发送。 */
export async function claimRuleSync(node: string, leaseMs: number, owner: string): Promise<RuleSyncRecord | null> {
  const tx = (await getDb()).transaction('rule_outbox', 'readwrite')
  const all = await tx.store.index('by_node').getAll(node)
  const held = all.some((r) => r.claimed_by !== owner && r.claimed_at !== undefined && r.claimed_at + leaseMs > Date.now())
  const rec = all.find((r) => r.state === 'pending')
  if (!rec || held) {
    await tx.done
    return null
  }
  rec.claimed_at = Date.now()
  rec.claimed_by = owner
  await tx.store.put(rec)
  await tx.done
  return rec
}

/** 回执只处理被发送的那个序号，不会删掉它在途时写下的新期望。 */
export async function finishRuleSync(seq: number, error?: RuleSyncRecord['error'], retain = false, owner?: string, counts: Pick<RuleSyncRecord, 'attempts' | 'drops'> = {}): Promise<void> {
  const tx = (await getDb()).transaction('rule_outbox', 'readwrite')
  const rec = await tx.store.get(seq)
  if (rec && (owner === undefined || rec.claimed_by === undefined || rec.claimed_by === owner)) {
    if (error || retain) {
      Object.assign(rec, counts)
      delete rec.claimed_at
      delete rec.claimed_by
      if (error) { rec.state = 'failed'; rec.error = error }
      await tx.store.put(rec)
    } else await tx.store.delete(seq)
  }
  await tx.done
}

export async function retryRuleSync(seq: number): Promise<void> {
  const tx = (await getDb()).transaction('rule_outbox', 'readwrite')
  const rec = await tx.store.get(seq)
  if (rec) {
    delete rec.attempts
    delete rec.drops
    delete rec.error
    delete rec.claimed_at
    delete rec.claimed_by
    rec.state = 'pending'
    await tx.store.put(rec)
  }
  await tx.done
}
