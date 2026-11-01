// IndexedDB（idb）。schema 见 spec/web.md § 6.2。按 origin 隔离，这是 ADR-23 接受的代价。
import { deleteDB, openDB, type DBSchema, type IDBPDatabase } from 'idb'
import type { AgentInfo, AttachmentRef, NodeInfo, Project, SessionEvent, SessionSummary } from '@/protocol'
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

interface AnyconDB extends DBSchema {
  keys: { key: string; value: IdentityRecord }
  nodes: { key: string; value: NodeRecord }
  sessions: { key: string; value: SessionRecord; indexes: { by_node: string } }
  events: { key: [string, number]; value: EventRecord }
  drafts: { key: string; value: DraftRecord }
  blobs: { key: string; value: BlobRecord }
}

export const DB_NAME = 'anycon'
export const DB_VERSION = 1

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
    upgrade(db) {
      db.createObjectStore('keys', { keyPath: 'id' })
      db.createObjectStore('nodes', { keyPath: 'node_id' })
      const sessions = db.createObjectStore('sessions', { keyPath: 'session_id' })
      sessions.createIndex('by_node', 'node_id')
      db.createObjectStore('events', { keyPath: ['session_id', 'seq'] })
      db.createObjectStore('drafts', { keyPath: 'session_id' })
      db.createObjectStore('blobs', { keyPath: 'blob_id' })
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
  const tx = db.transaction(['keys', 'nodes', 'sessions', 'events', 'drafts', 'blobs'], 'readwrite')
  await Promise.all([
    tx.objectStore('keys').clear(),
    tx.objectStore('nodes').clear(),
    tx.objectStore('sessions').clear(),
    tx.objectStore('events').clear(),
    tx.objectStore('drafts').clear(),
    tx.objectStore('blobs').clear(),
    tx.done,
  ])
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
  const tx = db.transaction(['nodes', 'sessions', 'events', 'drafts'], 'readwrite')
  await tx.objectStore('nodes').delete(nodeId)
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
