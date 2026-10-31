// `fs.list` / `fs.read` 的封装（docs/protocol.md § 5.3）。节点上的路径限制由 daemon 保证；这里只管发请求和整理结果。
import type { FsEntry, FsReadResult } from '@/protocol'
import { base64ToBytes } from '@/lib/base64'
import { rpc } from '@/transport/instance'
import { sortEntries } from './paths'

/** 文本预览读多少字节：和 daemon 的默认值一致，超出的部分不显示并提示（`truncated`）。 */
export const TEXT_MAX_BYTES = 1024 * 1024
/** 图片最多取多少字节：daemon 的上限。超出的图片不显示。 */
export const IMAGE_MAX_BYTES = 10 * 1024 * 1024
/** 10 MiB 的图片 base64 之后约 13.4 MiB，要经 hub 分片传回来：给的时间比普通请求长。 */
const IMAGE_TIMEOUT_MS = 90_000

/** daemon 回了结果、但内容不能用（旧 daemon 不认 `encoding`，或 base64 坏了）。 */
export class FsDataError extends Error {
  constructor(readonly code: 'image_unsupported' | 'bad_data') {
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
  bytes: Uint8Array
  /** 文件比能取的更大（`bytes` 只是前一段，不能显示）。 */
  truncated: boolean
  /** 文件总字节数。 */
  total: number
}

/** 读图片的字节（base64 模式）。 */
export async function readImage(nodeId: string, path: string): Promise<ImageBytes> {
  const res = await rpc().request<FsReadResult>(nodeId, 'fs.read', { path, max_bytes: IMAGE_MAX_BYTES, encoding: 'base64' }, IMAGE_TIMEOUT_MS)
  // 旧 daemon 不认识 `encoding`，当成文本读了：没有 `data_base64`
  if (typeof res.data_base64 !== 'string') throw new FsDataError('image_unsupported')
  try {
    return { bytes: base64ToBytes(res.data_base64), truncated: res.truncated, total: res.bytes }
  } catch {
    throw new FsDataError('bad_data')
  }
}
