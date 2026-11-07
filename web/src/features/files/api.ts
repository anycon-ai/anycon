// `fs.list` / `fs.read` 的封装（docs/protocol.md § 5.3）。节点上的路径限制由 daemon 保证；这里只管发请求和整理结果。
import type { FsEntry, FsReadResult } from '@/protocol'
import { base64ToBytes } from '@/lib/base64'
import { rpc } from '@/transport/instance'
import { sortEntries } from './paths'

/** 文本预览读多少字节：和 daemon 的默认值一致，超出的部分不显示并提示（`truncated`）。 */
export const TEXT_MAX_BYTES = 1024 * 1024
/** 图片预览最多取多少字节：超过的不显示（读到第一块就知道总长，不用把它传完）。 */
export const IMAGE_MAX_BYTES = 10 * 1024 * 1024
/** 「下载」最多取多少字节：整个文件要拼在内存里，一块 512 KiB 顺序读，再大的文件在浏览器里既慢又占内存。 */
export const DOWNLOAD_MAX_BYTES = 50 * 1024 * 1024
/**
 * 一块读多少字节，等于 daemon 在 base64 模式下一次最多回的（和 `blob.get` 一样）。设备到 daemon 只有一条有序的传输：
 * 十几 MiB 的单个响应会让事件、审批请求、「停止」的回复都排在它后面，分块读每次只占一小段。
 */
export const IMAGE_CHUNK_BYTES = 512 * 1024

/** daemon 回了结果、但内容不能用。 */
export class FsDataError extends Error {
  constructor(
    /** `image_unsupported`：旧 daemon（不认 `encoding`，或不认 `offset` 而不能分块读）；`bad_data`：数据对不上（base64 坏了、
     * 块的位置或长度不对）；`changed`：读的过程中文件被改写了（总长变了）。 */
    readonly code: 'image_unsupported' | 'bad_data' | 'changed',
  ) {
    super(code)
    this.name = 'FsDataError'
  }
}

/** 列目录，已排好序（目录在前，再按名字）。 */
export async function listDir(nodeId: string, path: string): Promise<FsEntry[]> {
  const res = await rpc().request<{ entries: FsEntry[] }>(nodeId, 'fs.list', { path })
  return sortEntries(res.entries)
}

/** 读文本：最多 1 MiB。二进制文件 `binary = true` 且没有内容。 */
export function readText(nodeId: string, path: string): Promise<FsReadResult> {
  return rpc().request<FsReadResult>(nodeId, 'fs.read', { path, max_bytes: TEXT_MAX_BYTES })
}

export interface ImageBytes {
  /** 完整的文件内容；`truncated` 时是空的。 */
  bytes: Uint8Array
  /** 文件超过 `maxBytes`，没有读（`bytes` 是空的）。 */
  truncated: boolean
  /** 文件总字节数。 */
  total: number
}

export interface ReadImageOptions {
  /** 最多读多少字节，超过的不读、返回 `truncated`。缺省 [`IMAGE_MAX_BYTES`]。 */
  maxBytes?: number
  /** 中止：下一块不再发（预览关掉、换了文件）。 */
  signal?: AbortSignal
  /** 每读完一块调用：已读字节数、文件总长。 */
  onProgress?: (loaded: number, total: number) => void
}

const aborted = (): Error => new DOMException('aborted', 'AbortError')

/**
 * 读文件的全部字节（图片预览、下载共用）：base64 模式、一块 [`IMAGE_CHUNK_BYTES`]，按 `offset` 一块一块**顺序**读
 * （同时只有一个请求在途），直到读完。第一块就带着文件总长：超过 `maxBytes` 就不再读，返回 `truncated`。
 * 读的过程中总长变了（文件被改写）：从头重来一次，再变就报 `changed`。
 * 旧 daemon 会忽略它不认识的 `offset`（回应里没有 `offset`）：这时只有整个文件在第一块里读完才能用，否则 `image_unsupported`——
 * 不然会把同一块重复拼起来，得到一张坏图。
 */
export async function readFileBytes(nodeId: string, path: string, opts: ReadImageOptions = {}): Promise<ImageBytes> {
  for (let attempt = 0; ; attempt++) {
    try {
      return await readFileBytesOnce(nodeId, path, opts)
    } catch (e) {
      if (!(e instanceof FsDataError && e.code === 'changed' && attempt === 0)) throw e
    }
  }
}

async function readFileBytesOnce(nodeId: string, path: string, { signal, onProgress, maxBytes = IMAGE_MAX_BYTES }: ReadImageOptions): Promise<ImageBytes> {
  let out: Uint8Array | null = null
  let total = 0
  let offset = 0
  for (;;) {
    if (signal?.aborted) throw aborted()
    const res = await rpc().request<FsReadResult>(nodeId, 'fs.read', { path, max_bytes: IMAGE_CHUNK_BYTES, encoding: 'base64', offset })
    if (signal?.aborted) throw aborted()
    // 旧 daemon 不认识 `encoding`，当成文本读了：没有 `data_base64`
    if (typeof res.data_base64 !== 'string') throw new FsDataError('image_unsupported')
    if (out && res.bytes !== total) throw new FsDataError('changed')
    total = res.bytes
    if (res.offset === undefined || res.offset === null) {
      // 不认识 `offset`：回的永远是文件开头那一块。整个文件就在里面时没关系
      if (offset > 0 || res.truncated) throw new FsDataError('image_unsupported')
    } else if (res.offset !== offset) throw new FsDataError('bad_data')
    if (!out) {
      if (total > maxBytes) return { bytes: new Uint8Array(0), truncated: true, total }
      out = new Uint8Array(total)
    }
    let chunk: Uint8Array
    try {
      chunk = base64ToBytes(res.data_base64)
    } catch {
      throw new FsDataError('bad_data')
    }
    // daemon 回的恰好是 min(一块, 剩下的)；对不上说明数据有问题（空块也不行，否则会一直读下去）
    if (chunk.length !== Math.min(IMAGE_CHUNK_BYTES, total - offset)) throw new FsDataError('bad_data')
    out.set(chunk, offset)
    offset += chunk.length
    onProgress?.(offset, total)
    if (offset >= total) return { bytes: out, truncated: false, total }
    if (!res.truncated) throw new FsDataError('bad_data') // daemon 说读完了，但还差一截
  }
}

/** 读图片预览要的全部字节，最多 [`IMAGE_MAX_BYTES`]。 */
export function readImage(nodeId: string, path: string, opts: ReadImageOptions = {}): Promise<ImageBytes> {
  return readFileBytes(nodeId, path, { ...opts, maxBytes: IMAGE_MAX_BYTES })
}
