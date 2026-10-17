// 草稿与未发送消息（持久化到 IndexedDB `drafts`）+ 发送中的消息（内存）。
import { create } from 'zustand'
import * as db from '@/db'
import type { AttachmentRef } from '@/protocol'
import { storageFailed } from '@/lib/log'
import type { DraftRecord, UnsentMessage } from '@/db'

export interface Inflight {
  messageId: string
  sessionId: string
  content: string
  /** 随这条消息发出的附件引用（ADR-35），乐观气泡据此显示缩略图。 */
  attachments?: AttachmentRef[]
  createdAt: number
  /** daemon 说这条被排到下一轮了。 */
  queued: boolean
  /** 请求已被 daemon 接受，等事件流里出现回显。 */
  accepted: boolean
}

/** 输入区里一张附件的状态：上传中（含规整）/ 传完 / 失败。 */
export type AttachmentItemState = 'uploading' | 'ready' | 'error'

/**
 * 输入区里的一张附件（ADR-35）。只在内存里；传完的那些（`ready`）的引用同步到草稿的 `attachments` 持久化，
 * 上传中 / 失败的不存（刷新后没有意义）。按添加顺序排列，传完的先后不影响发送顺序。
 */
export interface AttachmentItem {
  /** 本地 id（不是 blob_id）。 */
  id: string
  name: string
  mime: string
  width?: number
  height?: number
  state: AttachmentItemState
  /** 0..1，`uploading` 时有意义。 */
  progress: number
  /** `ready` 时有：daemon 的记录。 */
  ref?: AttachmentRef
  /** 规整后（或 daemon 记录里）的字节数；一条消息的附件合计有上限，加图时据此判断。规整完成之前没有。 */
  bytes?: number
  /** 规整后字节生成的本地预览（`blob:`）；从草稿恢复的没有，缩略图改从图片缓存取。 */
  previewUrl?: string
  /** 刷新后从草稿恢复的引用，正在向节点确认 blob 还在；确认完之前不能发送（过期的会改走「从缓存重传」）。 */
  checking?: boolean
  /** `error` 时的原因（RpcCallError / ImageError …），界面据此给提示。 */
  error?: unknown
}

interface DraftsState {
  byKey: Record<string, DraftRecord>
  loaded: Record<string, boolean>
  inflight: Record<string, Inflight>
  /** 每个草稿 key 的附件（见 `AttachmentItem`）。undefined = 还没初始化过（草稿里有附件时由输入区恢复）。 */
  items: Record<string, AttachmentItem[]>
  load: (key: string) => Promise<void>
  /** 启动时把 IndexedDB 里所有草稿 / 未发送消息读进来（没打开过的会话里的未发送消息也要计数、重发）。幂等。 */
  loadAll: () => Promise<void>
  setText: (key: string, text: string) => void
  /** 改某个草稿的附件列表；传完的引用同步进草稿并持久化。 */
  updateItems: (key: string, f: (items: AttachmentItem[]) => AttachmentItem[]) => void
  /** 用草稿里存的引用初始化附件列表（都是 `ready`）；不动草稿本身。 */
  hydrateItems: (key: string, items: AttachmentItem[]) => void
  addUnsent: (key: string, msg: UnsentMessage) => void
  removeUnsent: (key: string, messageId: string) => void
  startInflight: (i: Inflight) => void
  patchInflight: (messageId: string, patch: Partial<Inflight>) => void
  endInflight: (messageId: string) => void
}

const timers = new Map<string, ReturnType<typeof setTimeout>>()
let allLoaded: Promise<void> | null = null

function schedulePersist(key: string, get: () => DraftsState): void {
  const prev = timers.get(key)
  if (prev) clearTimeout(prev)
  timers.set(
    key,
    setTimeout(() => {
      timers.delete(key)
      const rec = get().byKey[key]
      if (rec) db.putDraft(rec).catch(storageFailed('save the draft'))
    }, 300),
  )
}

const resetListeners = new Set<() => void>()
/** `resetDrafts` 时要做的清理（中止还在跑的上传）。返回取消订阅的函数。 */
export function onDraftsReset(fn: () => void): () => void {
  resetListeners.add(fn)
  return () => resetListeners.delete(fn)
}

/** 清空内存状态并取消还没落盘的定时器（退出并移除本设备：不能在库被删掉之后再写回去）。 */
export function resetDrafts(): void {
  for (const t of timers.values()) clearTimeout(t)
  timers.clear()
  allLoaded = null
  for (const fn of resetListeners) fn()
  for (const items of Object.values(useDrafts.getState().items)) for (const i of items) if (i.previewUrl) URL.revokeObjectURL(i.previewUrl)
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {}, items: {} })
}

const empty = (key: string): DraftRecord => ({ session_id: key, text: '', unsent: [] })

const readyRefs = (items: readonly AttachmentItem[]): AttachmentRef[] => items.flatMap((i) => (i.state === 'ready' && i.ref ? [i.ref] : []))
const sameRefs = (a: readonly AttachmentRef[], b: readonly AttachmentRef[]): boolean => a.length === b.length && a.every((r, i) => r.blob_id === b[i]!.blob_id)
/** 内存里的草稿和库里读出来的合并：内存里已经有内容的优先。 */
function mergeDraft(rec: DraftRecord, mem: DraftRecord): DraftRecord {
  return { ...rec, text: mem.text || rec.text, attachments: mem.attachments?.length ? mem.attachments : rec.attachments, unsent: mergeUnsent(rec.unsent, mem.unsent) }
}

export const useDrafts = create<DraftsState>()((set, get) => {
  const mutate = (key: string, f: (d: DraftRecord) => DraftRecord): void => {
    set((s) => ({ byKey: { ...s.byKey, [key]: f(s.byKey[key] ?? empty(key)) } }))
    schedulePersist(key, get)
  }
  return {
    byKey: {},
    loaded: {},
    inflight: {},
    items: {},
    load: async (key) => {
      if (get().loaded[key]) return
      set((s) => ({ loaded: { ...s.loaded, [key]: true } }))
      try {
        const rec = await db.getDraft(key)
        if (rec) set((s) => ({ byKey: { ...s.byKey, [key]: s.byKey[key] ? mergeDraft(rec, s.byKey[key]!) : rec } }))
      } catch (e) {
        console.warn('anycon: could not read the saved draft; only the in-memory one is available', e)
      }
    },
    loadAll: () => {
      allLoaded ??= (async () => {
        let recs: DraftRecord[]
        try {
          recs = await db.listDrafts()
        } catch (e) {
          console.warn('anycon: could not read the saved drafts; unsent messages from earlier visits are not restored', e)
          return
        }
        set((s) => {
          const byKey = { ...s.byKey }
          const loaded = { ...s.loaded }
          for (const rec of recs) {
            const mem = byKey[rec.session_id]
            byKey[rec.session_id] = mem ? mergeDraft(rec, mem) : rec
            loaded[rec.session_id] = true
          }
          return { byKey, loaded }
        })
      })()
      return allLoaded
    },
    setText: (key, text) => mutate(key, (d) => ({ ...d, text })),
    updateItems: (key, f) => {
      const next = f(get().items[key] ?? [])
      set((s) => ({ items: { ...s.items, [key]: next } }))
      const refs = readyRefs(next)
      if (!sameRefs(get().byKey[key]?.attachments ?? [], refs)) mutate(key, (d) => ({ ...d, attachments: refs }))
    },
    hydrateItems: (key, items) => set((s) => (s.items[key] ? s : { items: { ...s.items, [key]: items } })),
    addUnsent: (key, msg) =>
      mutate(key, (d) => ({ ...d, unsent: d.unsent.some((u) => u.message_id === msg.message_id) ? d.unsent : [...d.unsent, msg] })),
    removeUnsent: (key, messageId) => mutate(key, (d) => ({ ...d, unsent: d.unsent.filter((u) => u.message_id !== messageId) })),
    startInflight: (i) => set((s) => ({ inflight: { ...s.inflight, [i.messageId]: i } })),
    patchInflight: (messageId, patch) =>
      set((s) => {
        const cur = s.inflight[messageId]
        return cur ? { inflight: { ...s.inflight, [messageId]: { ...cur, ...patch } } } : s
      }),
    endInflight: (messageId) =>
      set((s) => {
        if (!s.inflight[messageId]) return s
        const { [messageId]: _gone, ...rest } = s.inflight
        return { inflight: rest }
      }),
  }
})

function mergeUnsent(a: UnsentMessage[], b: UnsentMessage[]): UnsentMessage[] {
  const seen = new Set(a.map((u) => u.message_id))
  return [...a, ...b.filter((u) => !seen.has(u.message_id))]
}

/** 会话页「待发 / 发送中」列表的一项。 */
export interface PendingMessage {
  messageId: string
  content: string
  /** 带的图片附件（ADR-35）；没有就是 undefined 或空。 */
  attachments?: AttachmentRef[]
  createdAt: number
  state: 'sending' | 'queued' | 'unsent' | 'failed' | 'accepted'
  error?: string
}

/** 合并持久化的未发送和内存里的发送中，去掉事件流里已经有回显的。 */
export function pendingMessages(draft: DraftRecord | undefined, inflight: Record<string, Inflight>, sessionId: string, seenMessageIds: ReadonlySet<string>): PendingMessage[] {
  const out: PendingMessage[] = []
  const inflightIds = new Set<string>()
  for (const i of Object.values(inflight)) {
    if (i.sessionId !== sessionId || seenMessageIds.has(i.messageId)) continue
    inflightIds.add(i.messageId)
    out.push({ messageId: i.messageId, content: i.content, attachments: i.attachments ?? [], createdAt: i.createdAt, state: i.queued ? 'queued' : i.accepted ? 'accepted' : 'sending' })
  }
  for (const u of draft?.unsent ?? []) {
    if (seenMessageIds.has(u.message_id) || inflightIds.has(u.message_id)) continue
    out.push({ messageId: u.message_id, content: u.content, attachments: u.attachments ?? [], createdAt: u.created_at, state: u.state, error: u.error })
  }
  return out.sort((a, b) => a.createdAt - b.createdAt)
}
