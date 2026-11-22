// 清空缓存：只清会话历史和附件缓存；草稿和未发送消息（落盘的和只在内存里的）引用的附件留下；
// 身份、节点、会话摘要、草稿本身不动；没打开的会话 cached_seq 归零；存储失败的横幅收起。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import * as db from '@/db'
import { attachmentCacheStats, readCached, resetBlobCache, writeCached } from '@/features/attachments/cache'
import type { AttachmentRef } from '@/protocol'
import { useDrafts } from '@/stores/drafts'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { ev, summary } from '@/test/factories'
import { blobIdsInUse, clearLocalCaches, readCacheStats, readPersist, readUsage, requestPersist } from './localData'

const ref = (blob_id: string): AttachmentRef => ({ blob_id, bytes: 3, mime: 'image/png', name: `${blob_id}.png` })
const bytes = new Uint8Array([1, 2, 3])

function record(id: string, lowSeq: number, lastSeq: number): db.SessionRecord {
  const s = summary({ id, last_seq: lastSeq })
  return { session_id: id, node_id: 'n1', project: null, agent: s.agent, title: s.title, status: s.status, options: s.settings, last_seq: lastSeq, updated_at: 2, low_seq: lowSeq, summary: s }
}

beforeEach(async () => {
  await db.resetDb()
  resetBlobCache()
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {}, items: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.getState().setStorageError(false)
})
afterEach(() => vi.unstubAllGlobals())

describe('clearLocalCaches', () => {
  it('removes cached events and attachments, keeping the ones drafts and unsent messages still need', async () => {
    await db.putSessions([record('s1', 1, 2)])
    await db.putEvents('s1', [ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }, { session_id: 's1', seq: 1 }), ev({ type: 'turn_done', turn_id: 1, reason: 'completed' }, { session_id: 's1', seq: 2 })])
    for (const id of ['old', 'saved_draft', 'saved_unsent', 'memory_draft', 'composer', 'inflight']) await writeCached(id, bytes, 'image/png')
    // 落盘的草稿：输入区附件 + 未发送消息的附件
    await db.putDraft({ session_id: 's1', text: '', attachments: [ref('saved_draft')], unsent: [{ message_id: 'u1', content: 'x', created_at: 1, state: 'unsent', attachments: [ref('saved_unsent')] }] })
    // 只在内存里的：还没落盘的草稿、输入区里传完的附件、发送中的消息
    useDrafts.setState({
      byKey: { s2: { session_id: 's2', text: 'hi', attachments: [ref('memory_draft')], unsent: [] } },
      items: { s2: [{ id: 'i1', name: 'c.png', mime: 'image/png', state: 'ready', progress: 1, ref: ref('composer') }] },
      inflight: { m9: { messageId: 'm9', sessionId: 's2', content: 'x', attachments: [ref('inflight')], createdAt: 1, queued: false, accepted: false } },
    })
    useUi.getState().setStorageError(true)

    expect(await clearLocalCaches()).toEqual({ events: 2, blobs: 1 })

    expect(await db.getEvents('s1')).toEqual([])
    expect(await readCached('old')).toBeNull()
    for (const id of ['saved_draft', 'saved_unsent', 'memory_draft', 'composer', 'inflight']) expect(await readCached(id), id).not.toBeNull()
    // 摘要和草稿还在；摘要的 low_seq 归零（本地已经没有它的事件）
    const [s1] = await db.listSessions('n1')
    expect(s1?.low_seq).toBe(0)
    expect((await db.getDraft('s1'))?.unsent).toHaveLength(1)
    expect(useUi.getState().storageError).toBe(false)
    // 内存索引重新扫过：统计只剩留下的那些
    expect(await attachmentCacheStats()).toEqual({ count: 5, bytes: 15 })
  })

  it('zeroes cached_seq of sessions that are not open and leaves an open one alone', async () => {
    useSessions.setState({
      list: { a: { ...summary({ id: 'a', last_seq: 9 }), node_id: 'n1', cached_seq: 9 }, b: { ...summary({ id: 'b', last_seq: 5 }), node_id: 'n1', cached_seq: 5 } },
    })
    useSessions.getState().openStart('n1', 'b', [])
    await clearLocalCaches()
    expect(useSessions.getState().list.a?.cached_seq).toBe(0)
    expect(useSessions.getState().list.b?.cached_seq).toBe(5)
  })

  it('leaves identity, nodes and the to-do list alone', async () => {
    await db.putIdentity({ id: 'self', device_name: 'phone', static_keypair: { public: new Uint8Array([1]), secret: new Uint8Array([2]) } })
    await db.putNode({ node_id: 'n1', name: 'box', public_key: new Uint8Array([3]), paired_at: 1, last_seen: 1 })
    await clearLocalCaches()
    expect((await db.getIdentity())?.device_name).toBe('phone')
    expect(await db.listNodes()).toHaveLength(1)
  })
})

describe('blobIdsInUse', () => {
  it('collects attachments of drafts, unsent and in-flight messages and the composer', () => {
    useDrafts.setState({
      byKey: { s: { session_id: 's', text: '', attachments: [ref('a')], unsent: [{ message_id: 'u', content: '', created_at: 1, state: 'failed', attachments: [ref('b')] }] } },
      items: { s: [{ id: 'i', name: 'n', mime: 'image/png', state: 'uploading', progress: 0.5 }, { id: 'j', name: 'n', mime: 'image/png', state: 'ready', progress: 1, ref: ref('c') }] },
      inflight: { m: { messageId: 'm', sessionId: 's', content: '', attachments: [ref('d')], createdAt: 1, queued: false, accepted: false } },
    })
    expect([...blobIdsInUse()].sort()).toEqual(['a', 'b', 'c', 'd'])
  })
})

describe('readCacheStats', () => {
  it('counts cached events and the attachment cache', async () => {
    await db.putEvents('s1', [ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }, { session_id: 's1', seq: 1 })])
    await writeCached('x', new Uint8Array(10), 'image/png')
    expect(await readCacheStats()).toEqual({ events: 1, attachments: { count: 1, bytes: 10 } })
  })
})

describe('the browser storage API', () => {
  it('reports null usage and an unsupported persist state without navigator.storage', async () => {
    vi.stubGlobal('navigator', { ...navigator, storage: undefined })
    expect(await readUsage()).toBeNull()
    expect(await readPersist()).toBe('unsupported')
    expect(await requestPersist()).toBe('unsupported')
  })

  it('reads the estimate and the persist state, and a refused request stays best-effort', async () => {
    const persist = vi.fn().mockResolvedValue(false)
    vi.stubGlobal('navigator', { ...navigator, storage: { estimate: () => Promise.resolve({ usage: 50, quota: 200 }), persisted: () => Promise.resolve(false), persist } })
    expect(await readUsage()).toEqual({ usage: 50, quota: 200 })
    expect(await readPersist()).toBe('best_effort')
    expect(await requestPersist()).toBe('best_effort')
    expect(persist).toHaveBeenCalledOnce()
  })

  it('treats an estimate without a quota as unknown', async () => {
    vi.stubGlobal('navigator', { ...navigator, storage: { estimate: () => Promise.resolve({ usage: 5 }) } })
    expect(await readUsage()).toBeNull()
  })
})
