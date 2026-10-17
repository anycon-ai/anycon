import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AttachmentRef } from '@/protocol'
import { resetDb } from '@/db'
import { installBlobDaemon, blobId } from '@/test/blobDaemon'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { resetDrafts, useDrafts } from '@/stores/drafts'
import { readCached, resetBlobCache, writeCached } from './cache'
import { addFiles, clearItems, discardRefs, hydrateFromDraft, MAX_ATTACHMENTS, removeItem, restoreAttachments, retryItem, type AttachContext, type AttachNotice } from './composerUploads'
import type { ImageCodec } from './normalize'

const NODE = 'n'.repeat(32)
const KEY = 'sess_a'
const PNG_HEAD = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
let rpc: FakeRpc
let notices: AttachNotice[]
let decodes: number

const png = (name: string, size = 2000): File => {
  const bytes = new Uint8Array(size)
  bytes.set(PNG_HEAD)
  return new File([bytes], name, { type: 'image/png' })
}

const codec: ImageCodec = {
  async decode() {
    decodes += 1
    return { width: 40, height: 30, close: () => {} }
  },
  async encode() {
    return null
  },
}

const ctx = (over: Partial<AttachContext> = {}): AttachContext => ({
  draftKey: KEY,
  nodeId: NODE,
  accepted: ['image/png', 'image/jpeg'],
  notify: (n) => notices.push(n),
  codec,
  upload: { retryDelayMs: 0 },
  ...over,
})

const items = () => useDrafts.getState().items[KEY] ?? []
const allSettled = (state: string) => () => {
  expect(items().length).toBeGreaterThan(0)
  expect(items().every((i) => i.state === state)).toBe(true)
}

beforeEach(async () => {
  await resetDb()
  resetBlobCache()
  resetDrafts()
  rpc = new FakeRpc()
  setRpc(rpc)
  notices = []
  decodes = 0
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => {
  setRpc(null)
  vi.useRealTimers()
})

describe('adding images', () => {
  it('shows an uploading item with a local preview straight away, then turns it into a ready item with the daemon record', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    addFiles([png('shot.png')], ctx())
    expect(items()).toMatchObject([{ name: 'shot.png', state: 'uploading', progress: 0 }])
    await vi.waitFor(allSettled('ready'))
    const [item] = items()
    expect(item).toMatchObject({ state: 'ready', progress: 1, name: 'shot.png', mime: 'image/png', width: 40, height: 30 })
    expect(item!.ref).toMatchObject({ blob_id: blobId(1), bytes: 2000 })
    expect(item!.previewUrl).toMatch(/^blob:/)
  })

  it('mirrors the ready references into the draft (for persistence) in the order the images were added, not the order they finished', async () => {
    // 第一张的第一次上传失败（要用户点重试），第二张先传完
    let failFirst = true
    const d = installBlobDaemon(rpc, {
      chunkSize: 1024,
      onChunk: (p) => {
        if (failFirst && d.uploads.get(p.blob_id)?.begin.name === 'first.png') throw rpcError('invalid')
      },
    })
    addFiles([png('first.png'), png('second.png')], ctx())
    await vi.waitFor(() => expect(items().map((i) => i.state)).toEqual(['error', 'ready']))
    expect(useDrafts.getState().byKey[KEY]?.attachments?.map((r) => r.name)).toEqual(['second.png'])
    failFirst = false
    retryItem(KEY, items()[0]!.id)
    await vi.waitFor(allSettled('ready'))
    expect(items().map((i) => i.name)).toEqual(['first.png', 'second.png'])
    expect(useDrafts.getState().byKey[KEY]?.attachments?.map((r) => r.name)).toEqual(['first.png', 'second.png'])
  })

  it('takes at most 10 images per message and says so', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    addFiles(Array.from({ length: 12 }, (_, i) => png(`${i}.png`)), ctx())
    expect(items()).toHaveLength(MAX_ATTACHMENTS)
    expect(notices).toEqual([{ kind: 'max', max: 10 }])
    await vi.waitFor(allSettled('ready'))
    addFiles([png('extra.png')], ctx())
    expect(items()).toHaveLength(MAX_ATTACHMENTS)
    expect(notices).toHaveLength(2)
  })

  it('uploads one image at a time per node with two chunks in flight — and the fake daemon would refuse a 9th concurrent upload', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024, onChunk: () => new Promise((r) => setTimeout(r, 5)) })
    addFiles(Array.from({ length: MAX_ATTACHMENTS }, (_, i) => png(`${i}.png`, 4096)), ctx())
    expect(items().every((i) => i.state === 'uploading')).toBe(true)
    await vi.waitFor(allSettled('ready'))
    // 写死的数字：不是 `MAX_*` 常量跟自己比
    expect(d.peak.uploads).toBe(1)
    expect(d.peak.chunks).toBe(2)
    expect(d.uploads.size).toBe(10)
    expect(rpc.calls.filter((c) => c.method === 'blob.put_begin')).toHaveLength(10)
  })

  it('normalises ahead while an image uploads — at most two images wait, prepared, for the upload turn — and none of them holds the upload turn', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((r) => (release = r))
    const d = installBlobDaemon(rpc, { chunkSize: 1024, onChunk: () => gate })
    addFiles(Array.from({ length: 6 }, (_, i) => png(`${i}.png`, 4096)), ctx())
    // 第 1 张在传（卡在第一块上）；第 2、3 张规整好了在等；第 4 张起连规整都还没开始
    await vi.waitFor(() => expect(decodes).toBe(3))
    await new Promise((r) => setTimeout(r, 40))
    expect(decodes).toBe(3)
    expect(d.uploads.size).toBe(1)
    expect(rpc.calls.filter((c) => c.method === 'blob.put_begin')).toHaveLength(1)
    // 规整好等着的那两张已经有本地预览，别的还没有
    expect(items().map((i) => !!i.previewUrl)).toEqual([true, true, true, false, false, false])
    release()
    await vi.waitFor(allSettled('ready'))
    expect(decodes).toBe(6)
    expect(d.peak.uploads).toBe(1)
    expect(d.peak.chunks).toBe(2)
  })

  it('removing an image that waits prepared for its upload turn frees its place at once, so the next one starts to be prepared', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024, onChunk: () => new Promise(() => {}) })
    addFiles(Array.from({ length: 4 }, (_, i) => png(`${i}.png`, 4096)), ctx())
    await vi.waitFor(() => expect(decodes).toBe(3))
    await new Promise((r) => setTimeout(r, 20))
    expect(decodes).toBe(3)
    removeItem(KEY, NODE, items().find((i) => i.name === '1.png')!.id)
    await vi.waitFor(() => expect(decodes).toBe(4))
    expect(d.uploads.size).toBe(1)
  })

  it('removing the image that is uploading lets the next one start right away, even if the request it was waiting on never answers', async () => {
    const d = installBlobDaemon(rpc, {
      chunkSize: 1024,
      onChunk: (p) => (d.uploads.get(p.blob_id)?.begin.name === 'stuck.png' ? new Promise(() => {}) : undefined),
    })
    addFiles([png('stuck.png', 4096), png('next.png', 4096)], ctx())
    await vi.waitFor(() => expect(d.chunksInFlight()).toBe(2))
    removeItem(KEY, NODE, items().find((i) => i.name === 'stuck.png')!.id)
    await vi.waitFor(() => expect(items().map((i) => [i.name, i.state])).toEqual([['next.png', 'ready']]))
    expect(d.discarded).toEqual([blobId(1)])
  })

  it('ignores an image that would take one message over 15 MiB, with a notice, and never uploads it', async () => {
    const MiB = 1024 * 1024
    // 每张规整后 3 MiB（假编码器）：5 张刚好 15 MiB，第 6 张超了
    const big: ImageCodec = {
      decode: async () => ({ width: 40, height: 30, close: () => {} }),
      encode: async (_i, _w, _h, mime) => new Blob([new Uint8Array(3 * MiB)], { type: mime === 'image/png' ? 'image/webp' : mime }),
    }
    const d = installBlobDaemon(rpc, { chunkSize: 512 * 1024 })
    addFiles(Array.from({ length: 6 }, (_, i) => png(`${i}.png`, 2 * MiB)), ctx({ codec: big, accepted: ['image/png', 'image/webp'] }))
    await vi.waitFor(() => expect(notices).toHaveLength(1))
    expect(notices[0]).toMatchObject({ kind: 'total', max: 15 * MiB })
    await vi.waitFor(allSettled('ready'))
    expect(items()).toHaveLength(5)
    expect(d.uploads.size).toBe(5)
    expect(items().reduce((n, i) => n + (i.ref?.bytes ?? 0), 0)).toBe(15 * MiB)
  })

  it('an image removed while it waits in the queue never reaches the node', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024, onChunk: () => new Promise((r) => setTimeout(r, 20)) })
    addFiles([png('a.png'), png('b.png'), png('queued.png')], ctx())
    const queued = items().find((i) => i.name === 'queued.png')!
    removeItem(KEY, NODE, queued.id)
    await vi.waitFor(allSettled('ready'))
    expect(items().map((i) => i.name)).toEqual(['a.png', 'b.png'])
    expect([...d.uploads.values()].map((u) => u.begin.name)).toEqual(['a.png', 'b.png'])
  })

  it('skips files that are not images, with a notice', () => {
    installBlobDaemon(rpc)
    addFiles([new File(['x'], 'notes.txt', { type: 'text/plain' })], ctx())
    expect(items()).toEqual([])
    expect(notices).toEqual([{ kind: 'not_image', name: 'notes.txt' }])
    expect(rpc.calls).toEqual([])
  })

  it('drops an image the browser cannot decode (no placeholder is left behind) and tells the user', async () => {
    installBlobDaemon(rpc)
    const broken: ImageCodec = {
      decode: () => Promise.reject(new Error('bad')),
      encode: async () => null,
    }
    addFiles([png('broken.png')], ctx({ codec: broken }))
    await vi.waitFor(() => expect(notices).toEqual([{ kind: 'undecodable', name: 'broken.png' }]))
    expect(items()).toEqual([])
    expect(rpc.calls).toEqual([])
  })
})

describe('failures, retry and removal', () => {
  it('a failed upload becomes an error item that retries without normalising the image again', async () => {
    let fail = true
    installBlobDaemon(rpc, {
      chunkSize: 1024,
      onChunk: () => {
        if (fail) throw new RpcCallError('offline', 'node link not ready')
      },
    })
    addFiles([png('a.png')], ctx())
    await vi.waitFor(allSettled('error'))
    expect(items()[0]!.error).toBeInstanceOf(RpcCallError)
    expect(useDrafts.getState().byKey[KEY]?.attachments ?? []).toEqual([])
    expect(decodes).toBe(1)

    fail = false
    retryItem(KEY, items()[0]!.id)
    expect(items()[0]).toMatchObject({ state: 'uploading', progress: 0 })
    await vi.waitFor(allSettled('ready'))
    expect(decodes).toBe(1)
    expect(useDrafts.getState().byKey[KEY]?.attachments).toHaveLength(1)
  })

  it('removing an item that is still uploading aborts it and discards the upload on the node', async () => {
    const d = installBlobDaemon(rpc, {
      chunkSize: 100,
      onChunk: () => new Promise((r) => setTimeout(r, 10)),
    })
    addFiles([png('a.png', 3000)], ctx())
    await vi.waitFor(() => expect(d.uploads.size).toBe(1))
    removeItem(KEY, NODE, items()[0]!.id)
    expect(items()).toEqual([])
    await vi.waitFor(() => expect(d.discarded).toEqual([blobId(1)]))
    expect(d.uploads.get(blobId(1))!.ended).toBe(false)
  })

  it('removing a finished item calls blob.discard and drops it from the draft', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    addFiles([png('a.png')], ctx())
    await vi.waitFor(allSettled('ready'))
    const id = items()[0]!.id
    const url = items()[0]!.previewUrl!
    const revoke = vi.spyOn(URL, 'revokeObjectURL')
    removeItem(KEY, NODE, id)
    expect(items()).toEqual([])
    expect(useDrafts.getState().byKey[KEY]?.attachments).toEqual([])
    expect(d.discarded).toEqual([blobId(1)])
    expect(revoke).toHaveBeenCalledWith(url)
  })

  it('clearItems (the message was handed to onSend) empties the list and the draft without discarding the blobs', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    addFiles([png('a.png'), png('b.png')], ctx())
    await vi.waitFor(allSettled('ready'))
    const revoke = vi.spyOn(URL, 'revokeObjectURL')
    clearItems(KEY)
    expect(items()).toEqual([])
    expect(useDrafts.getState().byKey[KEY]?.attachments).toEqual([])
    expect(d.discarded).toEqual([])
    expect(revoke).toHaveBeenCalledTimes(2)
  })

  it('resetting the drafts (sign out and remove this device) aborts uploads that are still running', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 100, onChunk: () => new Promise((r) => setTimeout(r, 10)) })
    addFiles([png('a.png', 3000)], ctx())
    await vi.waitFor(() => expect(d.uploads.size).toBe(1))
    resetDrafts()
    await vi.waitFor(() => expect(d.discarded).toEqual([blobId(1)]))
    expect(useDrafts.getState().items).toEqual({})
  })
})

const ref = (n: number, over: Partial<AttachmentRef> = {}): AttachmentRef => ({ blob_id: blobId(n), name: `r${n}.png`, mime: 'image/png', bytes: 100, width: 10, height: 8, ...over })

describe('restoring attachments', () => {
  it('hydrateFromDraft turns the persisted references into ready items, once', () => {
    hydrateFromDraft(KEY, [ref(1), ref(2)])
    expect(items().map((i) => [i.state, i.ref?.blob_id])).toEqual([['ready', blobId(1)], ['ready', blobId(2)]])
    hydrateFromDraft(KEY, [ref(3)])
    expect(items()).toHaveLength(2)
    // 只恢复内存列表，不动草稿本身
    expect(useDrafts.getState().byKey[KEY]).toBeUndefined()
  })

  it('with a context, asks the node whether each restored blob is still there (blob.get with max_bytes 1) and holds sending until it knows', async () => {
    const present = { blob: ref(1), data: 'AA==', eof: false }
    rpc.handlers['blob.get'] = () => present
    hydrateFromDraft(KEY, [ref(1), ref(2)], ctx())
    expect(items().map((i) => [i.state, !!i.checking])).toEqual([['ready', true], ['ready', true]])
    await vi.waitFor(() => expect(items().every((i) => !i.checking)).toBe(true))
    expect(rpc.calls.map((c) => [c.method, c.params])).toEqual([
      ['blob.get', { blob_id: blobId(1), offset: 0, max_bytes: 1 }],
      ['blob.get', { blob_id: blobId(2), offset: 0, max_bytes: 1 }],
    ])
    expect(items().map((i) => [i.state, i.ref?.blob_id])).toEqual([['ready', blobId(1)], ['ready', blobId(2)]])
  })

  it('a restored blob the node no longer has is re-uploaded from the local image cache, in place; one the cache lacks is dropped with a notice', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    rpc.handlers['blob.get'] = (p) => {
      const id = (p as { blob_id: string }).blob_id
      if (id === blobId(91) || id === blobId(93)) throw rpcError('not_found', undefined, 'blob')
      return { blob: ref(92), data: 'AA==', eof: false }
    }
    await writeCached(blobId(91), new Uint8Array(500).fill(7), 'image/png')
    // 92 还在；91 回收了、缓存里有；93 回收了、缓存里也没有
    hydrateFromDraft(KEY, [ref(91), ref(92), ref(93)], ctx())
    await vi.waitFor(() => expect(notices).toEqual([{ kind: 'lost', count: 1 }]))
    await vi.waitFor(() => expect(items().every((i) => i.state === 'ready' && !i.checking)).toBe(true))
    // 位置不变；91 换成了新上传的 blob，93 被去掉了
    expect(items().map((i) => i.name)).toEqual(['r91.png', 'r92.png'])
    expect(items().map((i) => i.ref?.blob_id)).toEqual([blobId(1), blobId(92)])
    expect(Array.from(d.uploads.get(blobId(1))!.chunks.get(0)!)).toEqual(Array.from(new Uint8Array(500).fill(7)))
    expect(d.uploads.get(blobId(1))!.begin).toMatchObject({ name: 'r91.png', mime: 'image/png', size: 500 })
    // 草稿里存的是新的引用，不是已经回收了的旧的
    expect(useDrafts.getState().byKey[KEY]?.attachments?.map((r) => r.blob_id)).toEqual([blobId(1), blobId(92)])
  })

  it('when the node cannot be asked (offline, timeout) the restored images are taken as still there — the send path has its own safety net', async () => {
    rpc.handlers['blob.get'] = () => {
      throw new RpcCallError('timeout', 'blob.get: timed out')
    }
    hydrateFromDraft(KEY, [ref(1)], ctx())
    await vi.waitFor(() => expect(items()[0]!.checking).toBe(false))
    expect(items()[0]).toMatchObject({ state: 'ready', ref: { blob_id: blobId(1) } })
    expect(notices).toEqual([])
  })

  it('without a context nothing is asked (the node is offline or unknown)', () => {
    hydrateFromDraft(KEY, [ref(1)])
    expect(items()[0]).toMatchObject({ state: 'ready' })
    expect(items()[0]!.checking).toBeFalsy()
    expect(rpc.calls).toEqual([])
  })

  it('puts the references of an unsent message back as ready items (not duplicating ones already there)', async () => {
    hydrateFromDraft(KEY, [ref(1)])
    const r = await restoreAttachments(ctx(), [ref(1), ref(2)])
    expect(r).toEqual({ restored: 1, lost: 0 })
    expect(items().map((i) => i.ref?.blob_id)).toEqual([blobId(1), blobId(2)])
    expect(useDrafts.getState().byKey[KEY]?.attachments?.map((x) => x.blob_id)).toEqual([blobId(1), blobId(2)])
  })

  it('re-uploads from the local image cache when the node has dropped the blobs, and reports the ones the cache lacks', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    await writeCached(blobId(7), new Uint8Array(500).fill(9), 'image/png')
    const r = await restoreAttachments(ctx(), [ref(7), ref(8)], { expired: true })
    expect(r).toEqual({ restored: 1, lost: 1 })
    await vi.waitFor(allSettled('ready'))
    // 新上传得到新的 blob_id，内容是缓存里的字节
    const fresh = items()[0]!.ref!
    expect(fresh.blob_id).toBe(blobId(1))
    expect(Array.from(d.uploads.get(blobId(1))!.chunks.get(0)!)).toEqual(Array.from(new Uint8Array(500).fill(9)))
    expect(d.uploads.get(blobId(1))!.begin).toMatchObject({ name: 'r7.png', mime: 'image/png', size: 500 })
    expect(await readCached(fresh.blob_id)).not.toBeNull()
    expect(decodes).toBe(0)
  })

  it('discardRefs asks the node to drop each blob', () => {
    const d = installBlobDaemon(rpc)
    discardRefs(NODE, [ref(1), ref(2)])
    expect(rpc.methods()).toEqual(['blob.discard', 'blob.discard'])
    expect(d.discarded).toEqual([blobId(1), blobId(2)])
  })
})
