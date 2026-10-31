// 输入区附件的流水线（ADR-35）：选中 → 规整（图片）/ 读出字节（其它文件）→ 上传 → 传完。状态放在 drafts store 的 `items` 里（组件卸载 / 切换会话不会打断上传），
// 这里只管把 File 推过流水线、处理移除 / 重试 / 发送后清理 / 失败后恢复。组件和测试都调这些函数。
import type { AttachmentRef } from '@/protocol'
import { uuid } from '@/lib/ids'
import { useDrafts, onDraftsReset, type AttachmentItem } from '@/stores/drafts'
import { RpcCallError } from '@/transport/rpcClient'
import { createLimiter, isAborted, type Limiter } from './async'
import { readCached } from './cache'
import { blobCall } from './rpc'
import { discardBlob, uploadBlob, type UploadHooks, type UploadSource } from './upload'
import { fileMime, ImageError, isImageMime, nameForMime, normalizeImage, OCTET_STREAM, readBytes, sanitizeName, sniffImageMime, type ImageCodec, type ImageErrorCode } from './normalize'

/** 一条消息最多带几个附件（图片和文件一起算；daemon 的上限是 20，输入区取 10）。 */
export const MAX_ATTACHMENTS = 10
/**
 * 一条消息的附件（图片按规整后、文件按原大小，合在一起算）合计字节上限：15 MiB。Claude 单次请求的上限是 32 MB，
 * 图片 base64 后每张约 ×1.33，再留出文字和历史的余地；daemon 另有单个附件（`sessions.max_attachment_bytes`）和
 * 每条消息 20 MiB 的兜底（`invalid` / `too_large`）。再加的附件提示后忽略。
 */
export const MAX_TOTAL_BYTES = 15 * 1024 * 1024
/**
 * 同一节点同时上传几个：1。上传和这台设备的其它请求（允许 / 停止 / 发消息）共用一条有序的加密通道，
 * 一个附件在传时在途的只有 2 块（base64 后约 1.4 MB），排在后面的交互请求最多等一两秒；多个同时传只会让它们等得更久。
 * 排队的附件要等轮到才 `put_begin`（它不幂等，daemon 每台设备同时进行中的上传也只收 8 个）。
 */
export const MAX_CONCURRENT_UPLOADS = 1
/**
 * 同一节点上，规整（图片的解码、缩小、重编码；文件的读出字节）可以先于上传做几个：2。准备好的占着名额等上传轮到自己，
 * 名额放掉之后下一个才开始准备——内存里最多是「1 个在传 + 2 个准备好 / 正在准备」，不会一次解码 10 张大图。
 */
export const MAX_PREPARED_AHEAD = 2

/** 添加过程中要告诉用户的事（界面给提示，这里不碰 i18n）。 */
export type AttachNotice =
  /** 这个 agent 只收图片，`name` 不是图片：被忽略。 */
  | { kind: 'not_image'; name: string }
  | { kind: 'max'; max: number }
  | { kind: ImageErrorCode; name: string }
  /** 加上这个，一条消息的附件合计超过 `max` 字节：这个被忽略。 */
  | { kind: 'total'; name: string; max: number }
  /** 草稿 / 未发送消息里的附件在节点上没了、本地缓存里也没有：`count` 个被去掉。 */
  | { kind: 'lost'; count: number }

export interface AttachContext {
  /** drafts store 的 key。 */
  draftKey: string
  nodeId: string
  /** 这个 agent 作为图片收的 mime（已经和四种图片格式取过交集，见 `AttachmentSupport.images`）。 */
  accepted: readonly string[]
  /** agent 收任意文件（`AttachmentSupport.files`）：不是图片的文件原样上传，规整不了的图也改按文件上传。 */
  files?: boolean
  notify: (notice: AttachNotice) => void
  /** 测试注入。 */
  codec?: ImageCodec
  upload?: Pick<UploadHooks, 'retryDelayMs'>
}

interface Runtime {
  ctl: AbortController
  /** 还没准备的原始文件。 */
  file?: File
  /** 按文件原样上传（不规整）。 */
  asFile?: boolean
  /** 准备好、可以上传的字节（规整后的图，或文件本身）。 */
  prepared?: UploadSource
  ctx: AttachContext
}
const runtime = new Map<string, Runtime>()
interface Queues {
  /** 规整（含规整好、等上传名额的）。 */
  prepare: Limiter
  upload: Limiter
}
const queues = new Map<string, Queues>()
const queuesFor = (nodeId: string): Queues => {
  let q = queues.get(nodeId)
  if (!q) queues.set(nodeId, (q = { prepare: createLimiter(MAX_PREPARED_AHEAD), upload: createLimiter(MAX_CONCURRENT_UPLOADS) }))
  return q
}

onDraftsReset(() => {
  for (const rt of runtime.values()) rt.ctl.abort()
  runtime.clear()
  // 名额也一起重置：被中止的上传可能还卡在一个迟迟不返回的请求上，不能让它占着新一轮的名额
  queues.clear()
})

const items = (key: string): AttachmentItem[] => useDrafts.getState().items[key] ?? []
const hasItem = (key: string, id: string): boolean => items(key).some((i) => i.id === id)
/** 改一个附件的状态。它已经被移除（或整个草稿被重置）了就什么都不做——在途请求的回调可能晚到。 */
function patchItem(key: string, id: string, patch: Partial<AttachmentItem>): void {
  if (!hasItem(key, id)) return
  useDrafts.getState().updateItems(key, (list) => list.map((i) => (i.id === id ? { ...i, ...patch } : i)))
}

/** 浏览器没报类型的文件（有的平台上 HEIC 就是这样）也试着当图片解码，解不开再提示（或改按文件上传）。 */
export const isImageFile = (f: File): boolean => f.type === '' || f.type.toLowerCase().startsWith('image/')

/** 规整失败时，agent 收任意文件就改按文件原样上传的原因：不是图片 / 浏览器解不开 / 编不出 agent 收的图片格式。 */
const FILE_FALLBACK: readonly ImageErrorCode[] = ['not_image', 'undecodable', 'unsupported_format']

/** 一条消息里已经准备好 / 传完的附件合计多少字节（`exceptId` 那个不算）。 */
const usedBytes = (key: string, exceptId: string): number => items(key).reduce((sum, i) => (i.id === exceptId ? sum : sum + (i.ref?.bytes ?? i.bytes ?? 0)), 0)

/**
 * 一个附件的流水线：排规整名额 → 规整 / 读出字节 → 排上传名额（拿到后才放掉规整名额）→ 上传。
 * 每个节点同时只有 1 个在上传、最多 2 个在准备或准备好等着；轮到时已经被移除 / 中止的直接跳过。
 */
async function run(key: string, id: string): Promise<void> {
  const rt = runtime.get(id)
  if (!rt) return
  const { ctx } = rt
  const signal = rt.ctl.signal
  const q = queuesFor(ctx.nodeId)
  const alive = (): boolean => runtime.get(id) === rt && !signal.aborted && hasItem(key, id)
  let releasePrepare: (() => void) | null = null
  try {
    if (!rt.prepared) {
      releasePrepare = await q.prepare.acquire(signal)
      if (!alive()) return
      if (!(await prepare(key, id, rt))) return
    }
    const releaseUpload = await q.upload.acquire(signal)
    releasePrepare?.()
    releasePrepare = null
    try {
      if (!alive()) return
      await upload(key, id, rt)
    } finally {
      releaseUpload()
    }
  } catch (e) {
    if (isAborted(e) || signal.aborted) return
    console.warn('anycon: uploading the attachment failed', e)
    patchItem(key, id, { state: 'error', progress: 0, error: e })
  } finally {
    releasePrepare?.()
  }
}

/** 准备。返回 false = 这个不再往下走了（被移除，或不能用 / 合计超限而被丢掉，已经提示过）。 */
async function prepare(key: string, id: string, rt: Runtime): Promise<boolean> {
  if (rt.asFile) return prepareFile(key, id, rt)
  const { ctx, ctl } = rt
  try {
    const image = await normalizeImage(rt.file!, { accepted: ctx.accepted, codec: ctx.codec })
    if (ctl.signal.aborted || !hasItem(key, id)) return false
    if (!withinTotal(key, id, rt.file?.name ?? image.name, image.bytes.length, ctx)) return false
    rt.prepared = image
    rt.file = undefined
    // 只有四种栅格图片会做本地预览（规整的结果一定是其中之一）
    const previewUrl = isImageMime(image.mime) ? URL.createObjectURL(new Blob([image.bytes as Uint8Array<ArrayBuffer>], { type: image.mime })) : undefined
    patchItem(key, id, { name: image.name, mime: image.mime, width: image.width, height: image.height, bytes: image.bytes.length, previewUrl })
    return true
  } catch (e) {
    if (ctl.signal.aborted || !hasItem(key, id)) return false
    if (!(e instanceof ImageError)) throw e
    // agent 收任意文件：当图片用不了的，改按文件原样上传
    if (ctx.files && FILE_FALLBACK.includes(e.code)) {
      rt.asFile = true
      return prepareFile(key, id, rt)
    }
    // 压根不能用的图：不留占位，只提示
    const name = rt.file?.name ?? ''
    dropItem(key, id)
    ctx.notify({ kind: e.code, name })
    return false
  }
}

/** 加上 `bytes` 一条消息的附件合计还在上限内；超了就丢掉这个并提示，返回 false。 */
function withinTotal(key: string, id: string, name: string, bytes: number, ctx: AttachContext): boolean {
  if (usedBytes(key, id) + bytes <= MAX_TOTAL_BYTES) return true
  dropItem(key, id)
  ctx.notify({ kind: 'total', name, max: MAX_TOTAL_BYTES })
  return false
}

/**
 * 按文件上传时的 mime：浏览器报的类型（规整成 daemon 收的写法）。daemon 会核对四种图片类型的文件头，
 * 所以报成这四种、文件头却对不上的（坏图、改了扩展名的文件）按文件头认，认不出就当 `application/octet-stream`。
 */
function uploadMime(declared: string, bytes: Uint8Array): string {
  const mime = fileMime(declared)
  if (!isImageMime(mime)) return mime
  const sniffed = sniffImageMime(bytes)
  return sniffed ?? OCTET_STREAM
}

/** 文件不规整：先按 `File.size` 查合计上限（不必读出来），再读出字节原样上传。不做本地预览。 */
async function prepareFile(key: string, id: string, rt: Runtime): Promise<boolean> {
  const { ctx, ctl } = rt
  const file = rt.file!
  if (!withinTotal(key, id, file.name, file.size, ctx)) return false
  const bytes = await readBytes(file)
  if (ctl.signal.aborted || !hasItem(key, id)) return false
  const mime = uploadMime(file.type, bytes)
  // 图片的名字照 daemon 的规则补对扩展名（它记录的名字会这样改），别的文件原样（去掉 daemon 不收的字符）
  const name = isImageMime(mime) ? nameForMime(file.name, mime) : sanitizeName(file.name, 'file')
  rt.prepared = { bytes, mime, name }
  rt.file = undefined
  patchItem(key, id, { kind: 'file', name, mime, bytes: bytes.length })
  return true
}

async function upload(key: string, id: string, rt: Runtime): Promise<void> {
  const { ctx, ctl } = rt
  const ref = await uploadBlob(ctx.nodeId, rt.prepared!, {
    ...ctx.upload,
    signal: ctl.signal,
    onProgress: (progress) => patchItem(key, id, { progress }),
  })
  // 上传期间被移除了（abort 已经让 uploadBlob 丢弃了 blob，走不到这里；双保险）
  if (!hasItem(key, id)) return discardBlob(ctx.nodeId, ref.blob_id)
  patchItem(key, id, { state: 'ready', progress: 1, ref, error: undefined, name: ref.name, mime: ref.mime, width: ref.width ?? undefined, height: ref.height ?? undefined, bytes: ref.bytes })
  runtime.delete(id)
}

function dropItem(key: string, id: string): void {
  const item = items(key).find((i) => i.id === id)
  if (item?.previewUrl) URL.revokeObjectURL(item.previewUrl)
  runtime.delete(id)
  useDrafts.getState().updateItems(key, (list) => list.filter((i) => i.id !== id))
}

function start(ctx: AttachContext, item: AttachmentItem, source: { file?: File; asFile?: boolean; prepared?: UploadSource }): void {
  runtime.set(item.id, { ctl: new AbortController(), ctx, ...source })
  useDrafts.getState().updateItems(ctx.draftKey, (list) => [...list, item])
  void run(ctx.draftKey, item.id)
}

/**
 * 选择 / 粘贴 / 拖放进来的文件。agent 收图片时，图片（含浏览器没报类型的）先规整；agent 收任意文件时，其余文件原样上传，
 * 否则提示后忽略。超过 10 个的提示后忽略。
 */
export function addFiles(files: Iterable<File>, ctx: AttachContext): void {
  const picked: { file: File; asFile: boolean }[] = []
  for (const file of files) {
    if (ctx.accepted.length > 0 && isImageFile(file)) picked.push({ file, asFile: false })
    else if (ctx.files) picked.push({ file, asFile: true })
    else ctx.notify({ kind: 'not_image', name: file.name })
  }
  const room = Math.max(0, MAX_ATTACHMENTS - items(ctx.draftKey).length)
  if (picked.length > room) ctx.notify({ kind: 'max', max: MAX_ATTACHMENTS })
  for (const { file, asFile } of picked.slice(0, room)) {
    const item: AttachmentItem = asFile
      ? { id: uuid(), kind: 'file', name: sanitizeName(file.name, 'file'), mime: fileMime(file.type), bytes: file.size, state: 'uploading', progress: 0 }
      : { id: uuid(), name: file.name || 'image', mime: file.type, state: 'uploading', progress: 0 }
    start(ctx, item, { file, asFile })
  }
}

/** 重试失败的那个（已经准备好的不重做）。 */
export function retryItem(draftKey: string, id: string): void {
  const rt = runtime.get(id)
  const item = items(draftKey).find((i) => i.id === id)
  if (!rt || !item || item.state !== 'error') return
  rt.ctl = new AbortController()
  patchItem(draftKey, id, { state: 'uploading', progress: 0, error: undefined })
  void run(draftKey, id)
}

/** 用户移除一个：中止上传；传完的（daemon 上已有）`blob.discard`。 */
export function removeItem(draftKey: string, nodeId: string, id: string): void {
  const item = items(draftKey).find((i) => i.id === id)
  if (!item) return
  runtime.get(id)?.ctl.abort()
  if (item.state === 'ready' && item.ref) discardBlob(nodeId, item.ref.blob_id)
  dropItem(draftKey, id)
}

/** 消息交给 `onSend` 之后清空输入区的附件（blob 已经跟着消息走了，不 discard）。 */
export function clearItems(draftKey: string): void {
  for (const i of items(draftKey)) {
    if (i.previewUrl) URL.revokeObjectURL(i.previewUrl)
    runtime.delete(i.id)
  }
  useDrafts.getState().updateItems(draftKey, () => [])
}

const readyItem = (ref: AttachmentRef): AttachmentItem => ({
  id: uuid(),
  ...(isImageMime(ref.mime) ? {} : { kind: 'file' as const }),
  name: ref.name,
  mime: ref.mime,
  width: ref.width ?? undefined,
  height: ref.height ?? undefined,
  bytes: ref.bytes,
  state: 'ready',
  progress: 1,
  ref,
})

/** 节点说这个 blob 已经不在了（被回收，或从来没传完）。 */
const isGone = (e: unknown): boolean => e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'not_found'

/**
 * 草稿里存的引用 → 输入区的附件（刷新后恢复）。草稿可能放了很久：给了 `ctx` 就顺便向节点探一下每个 blob 还在不在
 * （`blob.get { max_bytes: 1 }`），不在的（daemon 24 小时回收没被引用的 blob）改走「从缓存重传」，缓存里也没有就去掉并提示。
 * 探的过程中这些附件标 `checking`，发送要等它结束。节点答不上来（离线、超时）按「在」处理，真发不出去时有 `blob_expired` 兜底。
 */
export function hydrateFromDraft(draftKey: string, refs: readonly AttachmentRef[], ctx?: AttachContext): void {
  if (refs.length === 0) return
  // 已经恢复过（Composer 的 effect 可能跑两遍）
  if (useDrafts.getState().items[draftKey] !== undefined) return
  const list = refs.map((r) => ({ ...readyItem(r), ...(ctx ? { checking: true } : {}) }))
  useDrafts.getState().hydrateItems(draftKey, list)
  if (ctx) void verifyRestored(ctx, list)
}

async function verifyRestored(ctx: AttachContext, list: readonly AttachmentItem[]): Promise<void> {
  let lost = 0
  await Promise.all(
    list.map(async (item) => {
      const ref = item.ref!
      let gone = false
      try {
        await blobCall(ctx.nodeId, 'blob.get', { blob_id: ref.blob_id, offset: 0, max_bytes: 1 })
      } catch (e) {
        gone = isGone(e)
      }
      if (!hasItem(ctx.draftKey, item.id)) return
      if (!gone) return patchItem(ctx.draftKey, item.id, { checking: false })
      if (!(await reuploadFromCache(ctx, item.id, ref))) lost += 1
    }),
  )
  if (lost > 0) ctx.notify({ kind: 'lost', count: lost })
}

/** 缓存里的字节 → 重新上传用的来源（不再规整：字节就是当初上传的那份）和本地预览（只有四种栅格图片才有）。 */
function fromCache(ref: AttachmentRef, hit: { bytes: Uint8Array; mime: string }): { prepared: UploadSource; previewUrl?: string } {
  const prepared: UploadSource = { bytes: hit.bytes, mime: hit.mime, name: ref.name }
  const previewUrl = isImageMime(hit.mime) ? URL.createObjectURL(new Blob([hit.bytes as Uint8Array<ArrayBuffer>], { type: hit.mime })) : undefined
  return { prepared, previewUrl }
}

/** 这个附件在节点上没了：用本地缓存里的字节原地重新上传（位置不变）。缓存里也没有就去掉，返回 false。 */
async function reuploadFromCache(ctx: AttachContext, id: string, ref: AttachmentRef): Promise<boolean> {
  const hit = await readCached(ref.blob_id)
  if (!hasItem(ctx.draftKey, id)) return true // 用户已经把它移掉了，不算丢
  if (!hit) {
    dropItem(ctx.draftKey, id)
    return false
  }
  const { prepared, previewUrl } = fromCache(ref, hit)
  runtime.set(id, { ctl: new AbortController(), ctx, prepared })
  // 旧引用在节点上已经没了：换成上传中的状态（草稿里存的引用随之去掉，传完再存新的）
  patchItem(ctx.draftKey, id, { state: 'uploading', progress: 0, ref: undefined, checking: false, previewUrl, mime: hit.mime })
  void run(ctx.draftKey, id)
  return true
}

/**
 * 把一条没发出去的消息的附件放回输入区（「编辑」、新建会话失败）。
 * `expired`：daemon 已经回收了这些 blob——从附件缓存里取回字节重新上传，缓存里也没有的就丢了（返回 `lost`）。
 */
export async function restoreAttachments(ctx: AttachContext, refs: readonly AttachmentRef[], opts: { expired?: boolean } = {}): Promise<{ restored: number; lost: number }> {
  let restored = 0
  let lost = 0
  for (const ref of refs) {
    if (items(ctx.draftKey).some((i) => i.ref?.blob_id === ref.blob_id)) continue
    if (!opts.expired) {
      useDrafts.getState().updateItems(ctx.draftKey, (list) => [...list, readyItem(ref)])
      restored += 1
      continue
    }
    const hit = await readCached(ref.blob_id)
    if (!hit) {
      lost += 1
      continue
    }
    const { prepared, previewUrl } = fromCache(ref, hit)
    const kind = isImageMime(hit.mime) ? {} : { kind: 'file' as const }
    start(ctx, { id: uuid(), ...kind, name: ref.name, mime: hit.mime, width: ref.width ?? undefined, height: ref.height ?? undefined, bytes: hit.bytes.length, state: 'uploading', progress: 0, previewUrl }, { prepared })
    restored += 1
  }
  return { restored, lost }
}

/** 丢掉一批还没被消息引用的 blob（删除「未发送」消息时）。已被引用的会得到 `in_use`，只记 warn。 */
export function discardRefs(nodeId: string, refs: readonly AttachmentRef[]): void {
  for (const r of refs) discardBlob(nodeId, r.blob_id)
}
