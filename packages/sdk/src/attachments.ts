/**
 * 附件（ADR-35，docs/protocol.md § 5.6）：经 blob 通道把文件传到 daemon，消息只带 `AttachmentRef`。
 *
 * - 两个入口共用同一套分块上传：`uploadImage` 只传图片（`image/png|jpeg|gif|webp`，认不出抛 `unsupported_image`），
 *   `uploadFile` 传任意文件（mime 缺省时先按文件头认图片，再按扩展名猜，猜不出用 `application/octet-stream`）。
 *   四种图片由 daemon 核对文件头、读宽高，作为图片交给模型；其它文件 daemon 不看内容，以它所在机器上的路径交给 agent，
 *   由 agent 自己读（agent 收不收看 `capabilities.attachments`，通配项 `ATTACHMENT_ANY_FILE` = 任意文件）。
 * - 上传：`blob.put_begin` → 每块 `chunk_size`（daemon 给，目前 512 KiB）的 `blob.put_chunk`，最多 2 块在途 →
 *   `blob.put_end`，返回 daemon 记录的 `AttachmentRef`（图片含宽高）。每块都有答复：超时 / 掉线的块自己重试，不整个重传；
 *   daemon 明确拒绝的不重试（块答 `finalizing` = `put_end` 已经接手，直接 `put_end` 拿结果）。失败或中止时尽力 `blob.discard`。
 * - 同一个 `Node` 上的上传排队、一个一个传（ADR-35「2026-10 修订」：每个节点 1 个在上传、2 块在途），
 *   免得「允许」「停止」之类的请求排在几 MB 的文件后面。
 * - SDK 不缩放、不重编码图片，也不替 daemon 校验（daemon 是唯一的校验点；SDK 只挡掉明显不成形的 mime）：调用方自己把图控制在
 *   长边 ≤ 2000 px、单张 ≤ 约 1.5 MiB（硬上限：daemon 每个文件默认 10 MiB、每条消息合计 20 MiB；Claude 单张 base64 后约 5 MB）。
 * - 下载：`blob.get` 按偏移分页拉（每段 ≤ 512 KiB），图片和其它文件一样。
 */
import {
  ATTACHMENT_ANY_FILE,
  RpcTimeoutError,
  type AgentCapabilities,
  type AttachmentRef,
  type BlobPutBeginResult,
} from '@anycon/protocol'

import { AnyconError, AttachmentError, NodeOffline } from './errors.js'
import type { Node } from './node.js'

/** daemon 当作图片处理（核对文件头、读宽高、作为图片交给模型）的四种类型。 */
export const IMAGE_MIME_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'] as const
export type ImageMime = (typeof IMAGE_MIME_TYPES)[number]

/** 不知道类型时用的 mime（daemon 也这么约定）。 */
export const DEFAULT_FILE_MIME = 'application/octet-stream'

/**
 * `uploadFile` 缺省 mime 时按扩展名（小写，不带点）猜的表。故意不含四种图片：它们只按文件头认，
 * 扩展名是 `.png` 但内容不是 PNG 的文件按 `application/octet-stream` 传，免得在 `put_end` 撞上 `mime_mismatch`。
 */
const MIME_BY_EXTENSION: Readonly<Record<string, string>> = {
  pdf: 'application/pdf',
  txt: 'text/plain',
  log: 'text/plain',
  md: 'text/markdown',
  markdown: 'text/markdown',
  csv: 'text/csv',
  tsv: 'text/tab-separated-values',
  json: 'application/json',
  html: 'text/html',
  htm: 'text/html',
  css: 'text/css',
  xml: 'application/xml',
  yaml: 'application/yaml',
  yml: 'application/yaml',
  toml: 'application/toml',
  svg: 'image/svg+xml',
  zip: 'application/zip',
  gz: 'application/gzip',
  tgz: 'application/gzip',
  tar: 'application/x-tar',
  js: 'text/javascript',
  mjs: 'text/javascript',
  cjs: 'text/javascript',
  ts: 'text/x-typescript',
  tsx: 'text/x-typescript',
  jsx: 'text/javascript',
  py: 'text/x-python',
  rs: 'text/x-rust',
  go: 'text/x-go',
  java: 'text/x-java',
  c: 'text/x-c',
  h: 'text/x-c',
  cpp: 'text/x-c++',
  hpp: 'text/x-c++',
  sh: 'application/x-sh',
  sql: 'application/sql',
  diff: 'text/x-diff',
  patch: 'text/x-diff',
}

/** 和 daemon 一样：`type/subtype`，不带参数，≤ 127 字节。字符按 RFC 6838 的 restricted-name。 */
const MIME_PATTERN = /^[a-z0-9][a-z0-9!#$&^_.+-]*\/[a-z0-9][a-z0-9!#$&^_.+-]*$/
const MAX_MIME_BYTES = 127

/** 同一个节点同时在途的块数（一块 512 KiB，base64 后约 700 KB）。 */
export const MAX_CHUNKS_IN_FLIGHT = 2
/** 超时 / 掉线的块（和 `put_end`）再试几次。 */
const CHUNK_RETRIES = 2
const RETRY_DELAY_MS = 400
/** 块和 `put_end` 的请求超时：慢速上行一块就要十几秒，默认的 30 秒会在数据还在路上时就判超时、白白重发。 */
const UPLOAD_REQUEST_TIMEOUT_MS = 90_000
/** 掉线的块重试之前最多等节点重新上线多久。 */
const OFFLINE_WAIT_MS = 30_000
/** daemon 给的 `chunk_size` 超出这个范围当作协议错误，不照着切。 */
const MAX_CHUNK_BYTES = 4 * 1024 * 1024
/** `blob.get` 每段的上限（daemon 也封在 512 KiB）。 */
const GET_PAGE_BYTES = 512 * 1024

export interface UploadFileOptions {
  /** 展示用的文件名（1–255 字节，不含 `/` `\` 和控制字符）。daemon 不用它拼路径；扩展名和 mime 对不上时 daemon 会补上。 */
  name: string
  /**
   * `type/subtype`（不带参数，大小写不敏感，发出去前转小写）。缺省时：文件头是四种图片之一就用它，否则按 `name` 的扩展名猜，
   * 猜不出用 `application/octet-stream`。不成形的抛 `AnyconError('invalid_mime')`，什么都不发。
   */
  mime?: string
  /** 中止上传：不再发块、尽力 `blob.discard`，以 `signal.reason` 失败。 */
  signal?: AbortSignal
  /** 已确认的字节数 / 总字节数。从 `(0, total)` 开始，成功时以 `(total, total)` 结束。 */
  onProgress?: (sent: number, total: number) => void
  /** 每块（和 `put_end`）的请求超时，默认 90 秒。 */
  requestTimeoutMs?: number
}

export interface UploadImageOptions extends UploadFileOptions {
  /**
   * `image/png` / `image/jpeg` / `image/gif` / `image/webp`（大小写不敏感）。缺省按文件头（magic bytes）判断；
   * 认不出、或者给的不是这四种之一，抛 `AnyconError('unsupported_image')`，什么都不发。
   */
  mime?: ImageMime | (string & {})
}

export interface DownloadOptions {
  signal?: AbortSignal
}

export interface DownloadedAttachment {
  /** daemon 记录的元数据（和 `user_message.attachments` 里的一致）。 */
  ref: AttachmentRef
  bytes: Uint8Array
}

const ascii = (s: string): number[] => Array.from(s, (c) => c.charCodeAt(0))

/** 按文件头判断图片类型（和 daemon 一样只比前缀，不解码）。认不出返回 `undefined`。 */
export function detectImageMime(bytes: Uint8Array): ImageMime | undefined {
  const starts = (prefix: readonly number[], at = 0) =>
    bytes.length >= at + prefix.length && prefix.every((b, i) => bytes[at + i] === b)
  if (starts([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) return 'image/png'
  if (starts([0xff, 0xd8, 0xff])) return 'image/jpeg'
  if (starts(ascii('GIF87a')) || starts(ascii('GIF89a'))) return 'image/gif'
  if (starts(ascii('RIFF')) && starts(ascii('WEBP'), 8)) return 'image/webp'
  return undefined
}

/** 规范化一个 mime：成形（`type/subtype`、不带参数、≤ 127 字节）就返回小写形式，否则 `undefined`。 */
export function normalizeMime(mime: string): string | undefined {
  const lower = mime.toLowerCase()
  return lower.length <= MAX_MIME_BYTES && MIME_PATTERN.test(lower) ? lower : undefined
}

/**
 * `uploadFile` 缺省 mime 时的猜法：文件头是四种图片之一 → 那种图片；否则按 `name` 的扩展名查一张常见类型的小表；
 * 都不行 → `application/octet-stream`。只是猜，daemon 只核对四种图片的文件头。
 */
export function guessMime(bytes: Uint8Array, name: string): string {
  const image = detectImageMime(bytes)
  if (image) return image
  const dot = name.lastIndexOf('.')
  const ext = dot > 0 ? name.slice(dot + 1).toLowerCase() : ''
  return MIME_BY_EXTENSION[ext] ?? DEFAULT_FILE_MIME
}

/**
 * agent 收不收 `mime` 类型的附件：`capabilities.attachments` 点名了它，或者含通配项 `ATTACHMENT_ANY_FILE`（任意文件）。
 * 没有 `capabilities` / 列表为空 = 不收附件。和 daemon 在 `session.send` / `session.create` 的核对一致。
 */
export function acceptsAttachment(
  agent: { capabilities?: Pick<AgentCapabilities, 'attachments'> | null },
  mime: string,
): boolean {
  const want = mime.toLowerCase()
  return (agent.capabilities?.attachments ?? []).some(
    (m) => m === ATTACHMENT_ANY_FILE || m.toLowerCase() === want,
  )
}

/** @internal `uploadImage` 的 mime：给了就必须是四种图片之一，没给按文件头认。 */
export function resolveImageMime(bytes: Uint8Array, options: UploadImageOptions): ImageMime {
  const accepted = `only ${IMAGE_MIME_TYPES.join(', ')} are accepted; use uploadFile() for other files`
  if (options.mime === undefined) {
    const detected = detectImageMime(bytes)
    if (detected) return detected
    throw new AnyconError(
      'unsupported_image',
      `cannot tell the image type of ${options.name} from its first bytes; pass mime (${accepted})`,
    )
  }
  const mime = options.mime.toLowerCase()
  if (!(IMAGE_MIME_TYPES as readonly string[]).includes(mime)) {
    throw new AnyconError('unsupported_image', `${options.mime} is not an image type (${accepted})`)
  }
  return mime as ImageMime
}

/** @internal `uploadFile` 的 mime：给了就校验成形并转小写，没给按 `guessMime` 猜。 */
export function resolveFileMime(bytes: Uint8Array, options: UploadFileOptions): string {
  if (options.mime === undefined) return guessMime(bytes, options.name)
  const mime = normalizeMime(options.mime)
  if (mime === undefined) {
    throw new AnyconError(
      'invalid_mime',
      `${JSON.stringify(options.mime)} is not a valid mime type: expected type/subtype without parameters, at most ${MAX_MIME_BYTES} bytes (application/octet-stream if unknown)`,
    )
  }
  return mime
}

/** 标准 base64（带 padding）。 */
export function bytesToBase64(bytes: Uint8Array): string {
  const B = (globalThis as { Buffer?: NodeBuffer }).Buffer
  if (B) return B.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString('base64')
  let binary = ''
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000))
  }
  return btoa(binary)
}

export function base64ToBytes(data: string): Uint8Array {
  const B = (globalThis as { Buffer?: NodeBuffer }).Buffer
  if (B) return new Uint8Array(B.from(data, 'base64'))
  const binary = atob(data)
  const out = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i)
  return out
}

interface NodeBuffer {
  from(data: ArrayBufferLike, offset: number, length: number): { toString(enc: 'base64'): string }
  from(data: string, enc: 'base64'): Uint8Array
}

/** 64 位小写 hex 的 SHA-256（`blob.put_begin` 要整个文件的摘要，daemon 在 `put_end` 校验）。 */
export async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const subtle = globalThis.crypto?.subtle
  if (!subtle) {
    throw new AnyconError(
      'unsupported_runtime',
      'crypto.subtle is not available (a browser page served over plain http?); it is needed to hash attachments',
    )
  }
  const digest = new Uint8Array(await subtle.digest('SHA-256', bytes as Uint8Array<ArrayBuffer>))
  return Array.from(digest, (b) => b.toString(16).padStart(2, '0')).join('')
}

function abortReason(signal: AbortSignal): unknown {
  return signal.reason ?? new AnyconError('aborted', 'the upload was aborted')
}

/** 等 `p`，但 `signal` 中止时立刻放弃（在途的请求还会跑完，结果不再有人要）。 */
export function raceAbort<T>(p: Promise<T>, signal: AbortSignal | undefined): Promise<T> {
  if (!signal) return p
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(abortReason(signal))
    if (signal.aborted) return onAbort()
    signal.addEventListener('abort', onAbort, { once: true })
    p.then(resolve, reject).finally(() => signal.removeEventListener('abort', onAbort))
  })
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

/**
 * 跑 `fn`，超时 / 掉线时最多再试 `CHUNK_RETRIES` 次（掉线的先等节点重新上线）。daemon 的明确答复（`RpcError`）不重试。
 * `put_chunk` 按 `index` 定位、`put_end` 对已完成的 blob 返回同一个结果，所以重发是安全的。
 */
async function withRetry<T>(
  node: Node,
  fn: () => Promise<T>,
  signal: AbortSignal | undefined,
): Promise<T> {
  for (let attempt = 0; ; attempt++) {
    if (signal?.aborted) throw abortReason(signal)
    try {
      return await fn()
    } catch (err) {
      const transient = err instanceof RpcTimeoutError || err instanceof NodeOffline
      if (!transient || attempt >= CHUNK_RETRIES) throw err
      if (err instanceof NodeOffline && !node.online) {
        await raceAbort(node.waitOnline(OFFLINE_WAIT_MS), signal).catch((waitErr: unknown) => {
          throw signal?.aborted ? waitErr : err
        })
      } else {
        await raceAbort(sleep(RETRY_DELAY_MS * (attempt + 1)), signal)
      }
    }
  }
}

/** 丢掉一个进行中的上传或还没被引用的 blob；失败不管（daemon 自己回收：上传 10 分钟、blob 24 小时）。 */
function discardQuietly(node: Node, blobId: string): void {
  node.call('blob.discard', { blob_id: blobId }).catch(() => undefined)
}

/**
 * @internal `Node.uploadImage` / `Node.uploadFile` 共用的分块上传（排队由 `Node` 负责）。
 * `mime` 已经由 `resolveImageMime` / `resolveFileMime` 定好。
 */
export async function uploadBlob(
  node: Node,
  bytes: Uint8Array,
  mime: string,
  options: UploadFileOptions,
): Promise<AttachmentRef> {
  const { signal, onProgress } = options
  const timeoutMs = options.requestTimeoutMs ?? UPLOAD_REQUEST_TIMEOUT_MS
  if (signal?.aborted) throw abortReason(signal)
  const size = bytes.length
  const sha256 = await sha256Hex(bytes)
  if (signal?.aborted) throw abortReason(signal)

  const beginReq = node.call('blob.put_begin', { name: options.name, mime, size, sha256 })
  let begin: BlobPutBeginResult
  try {
    begin = await raceAbort(beginReq, signal)
  } catch (err) {
    // 等 put_begin 的时候被中止了：它迟早会答复，到时候把这个刚开的上传丢掉
    if (signal?.aborted)
      beginReq.then(
        (b) => discardQuietly(node, b.blob_id),
        () => undefined,
      )
    throw err
  }

  const blobId = begin.blob_id
  try {
    const chunkSize = begin.chunk_size
    if (!Number.isInteger(chunkSize) || chunkSize <= 0 || chunkSize > MAX_CHUNK_BYTES) {
      throw new AnyconError(
        'protocol',
        `blob.put_begin returned an unusable chunk_size (${String(chunkSize)})`,
      )
    }
    const total = Math.ceil(size / chunkSize)
    let next = 0
    let sent = 0
    let failed = false
    let finalizing = false
    onProgress?.(0, size)

    // 两个 worker 各自取下一块；任何一块最终失败，整个上传立刻失败（另一个 worker 发完手上这块就停）
    const worker = async (): Promise<void> => {
      for (;;) {
        if (failed || finalizing || signal?.aborted) return
        const index = next++
        if (index >= total) return
        const start = index * chunkSize
        const slice = bytes.subarray(start, Math.min(size, start + chunkSize))
        try {
          await withRetry(
            node,
            () =>
              node.call(
                'blob.put_chunk',
                { blob_id: blobId, index, data: bytesToBase64(slice) },
                { timeoutMs },
              ),
            signal,
          )
        } catch (err) {
          // `put_end` 已经接手了这个上传（重连后旧传输上的 put_end 和这里的重发重叠）：不再发块，下面 put_end 拿它的结果
          if (err instanceof AttachmentError && err.reason === 'finalizing') {
            finalizing = true
            return
          }
          failed = true
          throw err
        }
        sent += slice.length
        onProgress?.(sent, size)
      }
    }
    await raceAbort(
      Promise.all(Array.from({ length: Math.min(MAX_CHUNKS_IN_FLIGHT, total) }, worker)),
      signal,
    )
    if (signal?.aborted) throw abortReason(signal)

    const ref = await raceAbort(
      withRetry(node, () => node.call('blob.put_end', { blob_id: blobId }, { timeoutMs }), signal),
      signal,
    )
    // 传完的同时被中止了：没人会引用它，下面丢掉
    if (signal?.aborted) throw abortReason(signal)
    return ref
  } catch (err) {
    discardQuietly(node, blobId)
    throw err
  }
}

/** @internal `Node.downloadAttachment` 的实现。 */
export async function downloadAttachment(
  node: Node,
  blobId: string,
  options: DownloadOptions = {},
): Promise<DownloadedAttachment> {
  const { signal } = options
  const parts: Uint8Array[] = []
  let ref: AttachmentRef | undefined
  let offset = 0
  for (;;) {
    if (signal?.aborted) throw abortReason(signal)
    const page = await raceAbort(
      node.call('blob.get', { blob_id: blobId, offset, max_bytes: GET_PAGE_BYTES }),
      signal,
    )
    ref ??= page.blob
    const data = base64ToBytes(page.data)
    parts.push(data)
    offset += data.length
    if (page.eof) break
    if (data.length === 0) {
      throw new AnyconError(
        'protocol',
        `blob.get returned an empty page before the end of ${blobId}`,
      )
    }
    if (offset > ref.bytes) {
      throw new AnyconError(
        'protocol',
        `blob.get returned more than ${ref.bytes} bytes for ${blobId}`,
      )
    }
  }
  const bytes = new Uint8Array(offset)
  let at = 0
  for (const part of parts) {
    bytes.set(part, at)
    at += part.length
  }
  return { ref, bytes }
}
