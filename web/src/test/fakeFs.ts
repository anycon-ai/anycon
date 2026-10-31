// 假的节点文件系统：给 `FakeRpc` 装上 `fs.list` / `fs.read`，行为照 docs/protocol.md § 5.3（路径不存在 `not_found`、
// 越界 `forbidden`、目录当文件读 `invalid`；文本默认读 1 MiB，`encoding: "base64"` 返回原始字节）。
import type { FsEntry, FsReadParams, FsReadResult } from '@/protocol'
import { bytesToBase64 } from '@/lib/base64'
import type { FakeRpc } from './fakeRpc'
import { rpcError } from './fakeRpc'

export type FakeFile = string | Uint8Array

export interface FakeFsOptions {
  /** 没有文件的空目录。 */
  emptyDirs?: string[]
  /** 这些路径（和它们下面的）一律 `forbidden`。 */
  forbidden?: string[]
  /** 读这些路径（和它们下面的）一律抛这个错（比如离线 / 超时）。 */
  failWith?: (path: string, method: 'fs.list' | 'fs.read') => unknown
}

const enc = new TextEncoder()
const dec = new TextDecoder()
const bytesOf = (f: FakeFile): Uint8Array => (typeof f === 'string' ? enc.encode(f) : f)

export function serveFs(rpc: FakeRpc, files: Record<string, FakeFile>, opts: FakeFsOptions = {}): { files: Record<string, FakeFile> } {
  const norm = (p: string): string => p.split('/').filter((s) => s && s !== '.').join('/')
  const isForbidden = (p: string): boolean => (opts.forbidden ?? []).some((f) => p === f || p.startsWith(`${f}/`))
  const dirExists = (p: string): boolean => p === '' || (opts.emptyDirs ?? []).some((d) => d === p || d.startsWith(`${p}/`)) || Object.keys(files).some((f) => f.startsWith(`${p}/`))

  rpc.handlers['fs.list'] = (params) => {
    const path = norm((params as { path: string }).path)
    const failure = opts.failWith?.(path, 'fs.list')
    if (failure) throw failure
    if (isForbidden(path)) throw rpcError('forbidden')
    if (path in files) throw rpcError('invalid')
    if (!dirExists(path)) throw rpcError('not_found')
    const prefix = path ? `${path}/` : ''
    const names = new Map<string, FsEntry>()
    for (const f of [...Object.keys(files), ...(opts.emptyDirs ?? [])]) {
      if (!f.startsWith(prefix) || f === path) continue
      const rest = f.slice(prefix.length)
      const name = rest.split('/')[0]!
      const isDir = rest.includes('/') || (opts.emptyDirs ?? []).includes(f)
      names.set(name, { name, is_dir: isDir, bytes: isDir ? 0 : bytesOf(files[f]!).length, modified_at_ms: 1_700_000_000_000 })
    }
    // 故意不排序（倒序）：目录在前、按名字排是客户端的事
    return { entries: [...names.values()].reverse() }
  }

  rpc.handlers['fs.read'] = (params): FsReadResult => {
    const p = params as FsReadParams
    const path = norm(p.path)
    const failure = opts.failWith?.(path, 'fs.read')
    if (failure) throw failure
    if (isForbidden(path)) throw rpcError('forbidden')
    if (!(path in files)) throw rpcError(dirExists(path) ? 'invalid' : 'not_found')
    const all = bytesOf(files[path]!)
    const max = Math.min(p.max_bytes ?? 1024 * 1024, 10 * 1024 * 1024)
    const got = all.slice(0, max)
    const truncated = all.length > got.length
    const binary = got.subarray(0, 8192).includes(0)
    if (p.encoding === 'base64') return { text: '', binary, truncated, bytes: all.length, data_base64: bytesToBase64(got) }
    return binary ? { text: '', binary: true, truncated, bytes: all.length } : { text: dec.decode(got), binary: false, truncated, bytes: all.length }
  }
  return { files }
}
