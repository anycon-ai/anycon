// 图片分块读：一块 512 KiB、顺序、到 10 MiB 为止；文件在读的过程中变了；旧 daemon（不认 `offset`）不能拼出坏图；中止。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { FsReadParams, FsReadResult } from '@/protocol'
import { FAKE_FS_CHUNK, serveFs, type FakeFile } from '@/test/fakeFs'
import { FakeRpc } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { FsDataError, IMAGE_CHUNK_BYTES, IMAGE_MAX_BYTES, readImage } from './api'

// 10 MiB 的那条要转 base64、再解码 20 遍；机器忙的时候 5 秒的默认超时不够
vi.setConfig({ testTimeout: 30_000 })

const NODE = 'n'.repeat(32)
const KiB = 1024
let rpc: FakeRpc
let files: Record<string, FakeFile>

/** 有规律、每一块都不同的字节：拼错了位置一定对不上。 */
const pattern = (n: number, seed = 0): Uint8Array => {
  const a = new Uint8Array(n)
  for (let i = 0; i < n; i++) a[i] = (i * 7 + (i >> 9) + seed) & 0xff
  return a
}
const reads = (): FsReadParams[] => rpc.calls.filter((c) => c.method === 'fs.read').map((c) => c.params as FsReadParams)

beforeEach(() => {
  rpc = new FakeRpc()
  files = {}
  serveFs(rpc, files)
  setRpc(rpc)
})
afterEach(() => setRpc(null))

describe('readImage', () => {
  it('uses the chunk size of the daemon, and that is 512 KiB', () => {
    expect(IMAGE_CHUNK_BYTES).toBe(512 * KiB)
    expect(IMAGE_CHUNK_BYTES).toBe(FAKE_FS_CHUNK)
  })

  it('reads a file that fits in one chunk with a single request', async () => {
    files['a.png'] = pattern(1000)
    const img = await readImage(NODE, 'a.png')
    expect(img).toEqual({ bytes: pattern(1000), truncated: false, total: 1000 })
    expect(reads()).toEqual([{ path: 'a.png', max_bytes: 512 * KiB, encoding: 'base64', offset: 0 }])
  })

  it('assembles a bigger file from consecutive chunks, in order, one request at a time', async () => {
    const data = pattern(2 * 512 * KiB + 4321)
    files['big.png'] = data
    let inFlight = 0
    let maxInFlight = 0
    const serve = rpc.handlers['fs.read']!
    rpc.handlers['fs.read'] = async (p, n) => {
      inFlight++
      maxInFlight = Math.max(maxInFlight, inFlight)
      await new Promise((r) => setTimeout(r, 1))
      inFlight--
      return serve(p, n)
    }
    const progress: Array<[number, number]> = []
    const img = await readImage(NODE, 'big.png', { onProgress: (l, t) => progress.push([l, t]) })
    expect(img.truncated).toBe(false)
    expect(img.total).toBe(data.length)
    expect(img.bytes.length).toBe(data.length)
    expect(img.bytes.every((b, i) => b === data[i])).toBe(true)
    expect(reads().map((r) => r.offset)).toEqual([0, 512 * KiB, 1024 * KiB])
    expect(reads().every((r) => r.max_bytes === 512 * KiB && r.encoding === 'base64')).toBe(true)
    expect(maxInFlight).toBe(1)
    expect(progress).toEqual([
      [512 * KiB, data.length],
      [1024 * KiB, data.length],
      [data.length, data.length],
    ])
  })

  it('reads a file of exactly the preview limit, and a file exactly one chunk long takes one request', async () => {
    files['limit.png'] = new Uint8Array(IMAGE_MAX_BYTES).fill(5)
    const img = await readImage(NODE, 'limit.png')
    expect(img.truncated).toBe(false)
    expect(img.bytes.length).toBe(IMAGE_MAX_BYTES)
    expect(reads()).toHaveLength(IMAGE_MAX_BYTES / IMAGE_CHUNK_BYTES)
    rpc.calls.length = 0
    files['one.png'] = pattern(IMAGE_CHUNK_BYTES)
    expect((await readImage(NODE, 'one.png')).bytes.length).toBe(IMAGE_CHUNK_BYTES)
    expect(reads()).toHaveLength(1)
  })

  it('stops after the first chunk when the file is over the limit: the total is known, the rest is not transferred', async () => {
    files['huge.png'] = new Uint8Array(IMAGE_MAX_BYTES + 1)
    const img = await readImage(NODE, 'huge.png')
    expect(img).toEqual({ bytes: new Uint8Array(0), truncated: true, total: IMAGE_MAX_BYTES + 1 })
    expect(reads()).toHaveLength(1)
  })

  it('reads an empty file as zero bytes', async () => {
    files['empty.png'] = new Uint8Array(0)
    expect(await readImage(NODE, 'empty.png')).toEqual({ bytes: new Uint8Array(0), truncated: false, total: 0 })
    expect(reads()).toHaveLength(1)
  })
})

describe('readImage: the file changes while it is being read', () => {
  it('starts over once when the total changes between two chunks, and returns the new content', async () => {
    files['x.png'] = pattern(3 * 512 * KiB, 1)
    const fresh = pattern(2 * 512 * KiB + 10, 2)
    const serve = rpc.handlers['fs.read']!
    let calls = 0
    rpc.handlers['fs.read'] = (p, n) => {
      // 第二块请求到达之前文件被改写成另一个长度
      if (++calls === 2) files['x.png'] = fresh
      return serve(p, n)
    }
    const img = await readImage(NODE, 'x.png')
    expect(img.total).toBe(fresh.length)
    expect(img.bytes.every((b, i) => b === fresh[i])).toBe(true)
    // 第一轮 0 / 512K（第二块发现总长变了），第二轮从头：0 / 512K / 1M
    expect(reads().map((r) => r.offset)).toEqual([0, 512 * KiB, 0, 512 * KiB, 1024 * KiB])
  })

  it('gives up with `changed` when it changes again during the second attempt', async () => {
    files['x.png'] = pattern(3 * 512 * KiB)
    const serve = rpc.handlers['fs.read']!
    let calls = 0
    rpc.handlers['fs.read'] = (p, n) => {
      // 每读一块文件就变长一点
      files['x.png'] = pattern(3 * 512 * KiB + ++calls)
      return serve(p, n)
    }
    const e = await readImage(NODE, 'x.png').catch((x: unknown) => x)
    expect(e).toBeInstanceOf(FsDataError)
    expect((e as FsDataError).code).toBe('changed')
    expect(reads().length).toBe(4) // 两轮，每轮两个请求（第二个发现变了）
  })

  it('a file that shrinks below the next offset counts as a change: it starts over and reads the new, shorter file', async () => {
    files['x.png'] = pattern(2 * 512 * KiB)
    const serve = rpc.handlers['fs.read']!
    let calls = 0
    rpc.handlers['fs.read'] = (p, n) => {
      if (++calls === 2) files['x.png'] = pattern(100) // 比下一块的偏移还短：daemon 回空块，`bytes` 变成 100
      return serve(p, n)
    }
    const img = await readImage(NODE, 'x.png') // 第一轮发现总长变了，第二轮读到的是 100 字节的新文件
    expect(img.total).toBe(100)
    expect(img.bytes.length).toBe(100)
  })
})

describe('readImage: an old daemon', () => {
  /** 不认识 `offset`：忽略它、回应里也没有 `offset`（`encoding` 和一块的上限它是有的）。 */
  function oldDaemon(): void {
    const serve = rpc.handlers['fs.read']!
    rpc.handlers['fs.read'] = async (p, n) => {
      const { offset: _offset, ...rest } = p as FsReadParams
      const { offset: _echo, ...res } = (await serve(rest, n)) as FsReadResult
      return res
    }
  }

  it('does not glue the first chunk together over and over: a file bigger than one chunk is unsupported after the first request', async () => {
    files['big.png'] = pattern(2 * 512 * KiB)
    oldDaemon()
    const e = await readImage(NODE, 'big.png').catch((x: unknown) => x)
    expect(e).toBeInstanceOf(FsDataError)
    expect((e as FsDataError).code).toBe('image_unsupported')
    expect(reads()).toHaveLength(1)
  })

  it('still shows a small image, which the first chunk holds completely', async () => {
    files['small.png'] = pattern(5000)
    oldDaemon()
    expect((await readImage(NODE, 'small.png')).bytes).toEqual(pattern(5000))
    expect(reads()).toHaveLength(1)
  })

  it('a daemon that does not know `encoding` either (no data_base64) is unsupported', async () => {
    rpc.handlers['fs.read'] = () => ({ text: '', binary: true, truncated: false, bytes: 16 })
    const e = await readImage(NODE, 'a.png').catch((x: unknown) => x)
    expect((e as FsDataError).code).toBe('image_unsupported')
  })
})

describe('readImage: data that does not add up', () => {
  const failsWith = async (code: string): Promise<void> => {
    const e = await readImage(NODE, 'a.png').catch((x: unknown) => x)
    expect(e).toBeInstanceOf(FsDataError)
    expect((e as FsDataError).code).toBe(code)
  }

  it('a chunk at a different offset than requested is bad data', async () => {
    files['a.png'] = pattern(2 * 512 * KiB)
    const serve = rpc.handlers['fs.read']!
    rpc.handlers['fs.read'] = async (p, n) => ({ ...((await serve(p, n)) as FsReadResult), offset: 7 })
    await failsWith('bad_data')
  })

  it('a chunk of the wrong length is bad data', async () => {
    files['a.png'] = pattern(2 * 512 * KiB)
    const serve = rpc.handlers['fs.read']!
    rpc.handlers['fs.read'] = async (p, n) => {
      const r = (await serve(p, n)) as FsReadResult
      return { ...r, data_base64: r.data_base64!.slice(0, 400) }
    }
    await failsWith('bad_data')
  })

  it('an empty chunk before the end is bad data (and does not loop forever)', async () => {
    files['a.png'] = pattern(2 * 512 * KiB)
    const serve = rpc.handlers['fs.read']!
    rpc.handlers['fs.read'] = async (p, n) => ({ ...((await serve(p, n)) as FsReadResult), data_base64: '' })
    await failsWith('bad_data')
    expect(reads()).toHaveLength(1)
  })

  it('invalid base64 is bad data', async () => {
    files['a.png'] = pattern(100)
    const serve = rpc.handlers['fs.read']!
    rpc.handlers['fs.read'] = async (p, n) => ({ ...((await serve(p, n)) as FsReadResult), data_base64: '!!!' })
    await failsWith('bad_data')
  })

  it('a daemon that says "done" while bytes are still missing is bad data', async () => {
    files['a.png'] = pattern(2 * 512 * KiB)
    const serve = rpc.handlers['fs.read']!
    rpc.handlers['fs.read'] = async (p, n) => ({ ...((await serve(p, n)) as FsReadResult), truncated: false })
    await failsWith('bad_data')
  })
})

describe('readImage: abort', () => {
  it('stops requesting further chunks once aborted', async () => {
    files['big.png'] = pattern(4 * 512 * KiB)
    const ctl = new AbortController()
    const e = await readImage(NODE, 'big.png', { signal: ctl.signal, onProgress: () => ctl.abort() }).catch((x: unknown) => x)
    expect((e as Error).name).toBe('AbortError')
    expect(reads()).toHaveLength(1)
  })

  it('does not even start when already aborted', async () => {
    files['a.png'] = pattern(10)
    const ctl = new AbortController()
    ctl.abort()
    await expect(readImage(NODE, 'a.png', { signal: ctl.signal })).rejects.toMatchObject({ name: 'AbortError' })
    expect(reads()).toHaveLength(0)
  })
})
