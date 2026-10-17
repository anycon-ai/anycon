// 假 daemon 本身的行为：它得像真 daemon 一样限流，否则靠它的并发用例什么都证明不了。
import { beforeEach, describe, expect, it } from 'vitest'
import { FakeRpc } from './fakeRpc'
import { blobId, DAEMON_MAX_UPLOADS, installBlobDaemon } from './blobDaemon'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

const begin = (i: number) => rpc.request(NODE, 'blob.put_begin', { name: `${i}.png`, mime: 'image/png', size: 10, sha256: 'a'.repeat(64) })

beforeEach(() => {
  rpc = new FakeRpc()
})

describe('installBlobDaemon', () => {
  it('refuses the 9th concurrent in-progress put_begin with busy, like the real daemon, and counts the peak', async () => {
    const d = installBlobDaemon(rpc)
    expect(DAEMON_MAX_UPLOADS).toBe(8)
    for (let i = 1; i <= 8; i++) await begin(i)
    expect(d.inProgress()).toBe(8)
    await expect(begin(9)).rejects.toMatchObject({ kind: 'rpc', code: 'busy' })
    expect(d.uploads.size).toBe(8)
    expect(d.peak.uploads).toBe(8)
  })

  it('finishing or discarding an upload frees a place', async () => {
    const d = installBlobDaemon(rpc)
    for (let i = 1; i <= 8; i++) await begin(i)
    await rpc.request(NODE, 'blob.put_end', { blob_id: blobId(1) })
    await begin(9)
    expect(d.inProgress()).toBe(8)
    await expect(begin(10)).rejects.toMatchObject({ code: 'busy' })
    await rpc.request(NODE, 'blob.discard', { blob_id: blobId(2) })
    await begin(10)
    expect(d.inProgress()).toBe(8)
    expect(d.peak.uploads).toBe(8)
  })

  it('tracks how many put_chunk requests are in flight at once', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((r) => (release = r))
    const d = installBlobDaemon(rpc, { chunkSize: 4, onChunk: () => gate })
    const { blob_id } = (await begin(1)) as { blob_id: string }
    const sends = [0, 1, 2].map((index) => rpc.request(NODE, 'blob.put_chunk', { blob_id, index, data: 'AAAA' }))
    expect(d.chunksInFlight()).toBe(3)
    release()
    await Promise.all(sends)
    expect(d.chunksInFlight()).toBe(0)
    expect(d.peak.chunks).toBe(3)
  })
})
