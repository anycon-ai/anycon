// 上传前规整图片（spec/web.md § 4.6「图片附件」、ADR-35 第 7 条及「2026-10 修订」）。
//
// - agent 收的格式、长边 ≤ 2 000 px、≤ 1.5 MiB、不带元数据的：原样上传，一个字节都不动
// - 长边 > 2 000 px 一律缩到 2 000（一次请求里超过 20 张图时 Anthropic 要求每张 ≤ 2 000 px）
// - 缩完（或原图）> 1.5 MiB 就有损重编码：agent 收 WebP 且浏览器编得出 → WebP 0.9，否则 JPEG 0.9（JPEG 没有透明通道，先铺白底）
// - 带 EXIF 的 JPEG（APP1 段）一律重编码：画布按方向转正后重画，GPS 等元数据随之去掉
// - 单张的硬上限仍是 3.75 MiB（base64 后约 5 MB，Claude 的单图上限）：重编码后还超就降质量、再一路缩小尺寸
// - 浏览器能解码而 agent 不收的格式（HEIC、BMP …）：转成 PNG，太大就有损重编码
// - 解不开的图抛 `ImageError`
//
// 解码和编码走可注入的 `ImageCodec`：生产用浏览器的 `createImageBitmap` + `OffscreenCanvas`（没有就退回 `<img>` + `<canvas>`），
// 测试注入假的（jsdom 两样都没有）。规则本身是纯函数。

export const UPLOAD_MIMES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'] as const
/** 单张图的硬上限：3.75 MiB。 */
export const MAX_IMAGE_BYTES = 3.75 * 1024 * 1024
/** 缩完（或原图）超过这个大小就有损重编码：1.5 MiB。 */
export const REENCODE_BYTES = 1.5 * 1024 * 1024
/** 长边上限，超过一律缩到它。 */
export const MAX_LONG_EDGE = 2000
export const JPEG_QUALITY = 0.9
/** 有损格式还超限时逐级降低的质量。 */
const QUALITY_STEPS = [0.8, 0.7, 0.6]
/** 还不行就把长边一路缩小到这个值为止。 */
const MIN_LONG_EDGE = 512

export interface NormalizedImage {
  bytes: Uint8Array
  mime: string
  /** 展示名；格式变了就换了扩展名。daemon 不用它拼路径。 */
  name: string
  width: number
  height: number
}

export type ImageErrorCode =
  /** 不是图片（MIME 类型不是 `image/*`）。 */
  | 'not_image'
  /** 浏览器解不开（损坏，或者不认识的格式）。 */
  | 'undecodable'
  /** 缩小之后仍然超限。 */
  | 'too_large'
  /** 这个 agent 收的格式里没有一种可以编码出来。 */
  | 'unsupported_format'

export class ImageError extends Error {
  constructor(
    public readonly code: ImageErrorCode,
    message?: string,
  ) {
    super(message ?? code)
    this.name = 'ImageError'
  }
}

/** 解码后的图片。`close` 释放位图。 */
export interface DecodedImage {
  width: number
  height: number
  close(): void
}

export interface ImageCodec {
  /** 解码失败抛错。 */
  decode(file: Blob): Promise<DecodedImage>
  /** 缩放到 `width × height` 后编码；这个格式编不出来时可以返回别的格式的 Blob（canvas 就是退回 PNG），由调用方看 `blob.type`。 */
  encode(image: DecodedImage, width: number, height: number, mime: string, quality: number): Promise<Blob | null>
}

// ---- 文件头 / 文件名 ----

/** 看前几个字节认出 PNG / JPEG / GIF / WebP；认不出返回 null。 */
export function sniffImageMime(b: Uint8Array): string | null {
  if (b.length >= 8 && b[0] === 0x89 && b[1] === 0x50 && b[2] === 0x4e && b[3] === 0x47 && b[4] === 0x0d && b[5] === 0x0a && b[6] === 0x1a && b[7] === 0x0a) return 'image/png'
  if (b.length >= 3 && b[0] === 0xff && b[1] === 0xd8 && b[2] === 0xff) return 'image/jpeg'
  if (b.length >= 6 && b[0] === 0x47 && b[1] === 0x49 && b[2] === 0x46 && b[3] === 0x38 && (b[4] === 0x37 || b[4] === 0x39) && b[5] === 0x61) return 'image/gif'
  if (b.length >= 12 && b[0] === 0x52 && b[1] === 0x49 && b[2] === 0x46 && b[3] === 0x46 && b[8] === 0x57 && b[9] === 0x45 && b[10] === 0x42 && b[11] === 0x50) return 'image/webp'
  return null
}

const EXTENSIONS: Record<string, readonly string[]> = {
  'image/png': ['png'],
  'image/jpeg': ['jpg', 'jpeg'],
  'image/gif': ['gif'],
  'image/webp': ['webp'],
}

/**
 * daemon 不接受的字符（`blobs.rs::validate_name`）：`/` `\`、Unicode Cc（C0、DEL、C1）、双向控制符
 * （U+200E U+200F U+202A–U+202E U+2066–U+2069，能把 `invoice\u202Egnp.exe` 显示成别的样子）、行 / 段分隔符 U+2028 U+2029。
 */
const BAD_NAME_CHARS = /[/\\\p{Cc}\u200e\u200f\u202a-\u202e\u2066-\u2069\u2028\u2029]/gu

/** 展示名要满足 blob.put_begin：1–255 字节，不含 `/` `\` 和上面那些字符（换成 `_`）。 */
export function sanitizeName(name: string, fallback = 'image'): string {
  let out = name.replace(BAD_NAME_CHARS, '_').trim()
  if (out === '' || out === '.' || out === '..') out = fallback
  const enc = new TextEncoder()
  if (enc.encode(out).length <= 255) return out
  // 按字节截断，保留扩展名
  const dot = out.lastIndexOf('.')
  const ext = dot > 0 && out.length - dot <= 10 ? out.slice(dot) : ''
  let stem = dot > 0 && ext ? out.slice(0, dot) : out
  while (enc.encode(stem + ext).length > 255) stem = stem.slice(0, -1)
  return stem + ext
}

/** 扩展名和 `mime` 对不上（或没有）就换成对的；对得上原样保留。 */
export function nameForMime(name: string, mime: string): string {
  const exts = EXTENSIONS[mime]
  const base = sanitizeName(name)
  if (!exts) return base
  const dot = base.lastIndexOf('.')
  const ext = dot > 0 ? base.slice(dot + 1).toLowerCase() : ''
  if (exts.includes(ext)) return base
  const stem = dot > 0 ? base.slice(0, dot) : base
  return sanitizeName(`${stem}.${exts[0]}`)
}

/**
 * 能当图片显示（`<img>`、大图）、作为图片交给模型的四种栅格格式。别的类型（含 `image/svg+xml`）一律按文件处理：
 * 不内联显示，只能下载。
 */
export function isImageMime(mime: string): boolean {
  return (UPLOAD_MIMES as readonly string[]).includes(mime)
}

/** daemon 收的 mime 写法（`blobs.rs::normalize_mime`）：`type/subtype`，两段都是 RFC 6838 的 restricted-name（字母数字开头），≤ 127 字节。 */
const MIME_RE = /^[a-z0-9][a-z0-9!#$&^_.+-]*\/[a-z0-9][a-z0-9!#$&^_.+-]*$/
const MIME_MAX = 127
/** 浏览器给不出（或给的不合格）类型时用的 mime。 */
export const OCTET_STREAM = 'application/octet-stream'

/** 按文件上传时的 mime：浏览器报的类型转小写、去掉参数；空的或不合格的用 `application/octet-stream`。 */
export function fileMime(type: string): string {
  const m = (type.split(';')[0] ?? '').trim().toLowerCase()
  return m.length <= MIME_MAX && MIME_RE.test(m) ? m : OCTET_STREAM
}

/** agent 收的 mime 和 daemon 上传白名单的交集（顺序跟 agent 的）。 */
export function uploadableMimes(agentMimes: readonly string[]): string[] {
  return agentMimes.filter((m) => (UPLOAD_MIMES as readonly string[]).includes(m))
}

// ---- 规整 ----

export async function readBytes(blob: Blob): Promise<Uint8Array> {
  if (typeof blob.arrayBuffer === 'function') return new Uint8Array(await blob.arrayBuffer())
  return new Promise((resolve, reject) => {
    const r = new FileReader()
    r.onload = () => resolve(new Uint8Array(r.result as ArrayBuffer))
    r.onerror = () => reject(r.error ?? new Error('could not read the file'))
    r.readAsArrayBuffer(blob)
  })
}

/** canvas 能编出来的格式（GIF 编不出来；WebP 取决于浏览器，编不出来时 codec 会退回 PNG，我们看 `blob.type` 判断）。 */
const ENCODABLE = ['image/png', 'image/jpeg', 'image/webp']
/** 有损重编码按这个顺序选格式：agent 收 WebP 且浏览器编得出就用 WebP（同样质量更小），否则 JPEG。 */
const LOSSY = ['image/webp', 'image/jpeg']

/** 第一步用的格式（按顺序试）：原格式优先（编得出来的话），GIF / agent 不收的格式当 PNG；只保留 agent 收的。 */
function naturalFormats(source: string, accepted: readonly string[]): string[] {
  const wanted = source === 'image/jpeg' ? ['image/jpeg'] : source === 'image/webp' ? ['image/webp', 'image/jpeg'] : ['image/png']
  const out = wanted.filter((m) => accepted.includes(m))
  if (out.length > 0) return out
  return ENCODABLE.filter((m) => accepted.includes(m))
}

/**
 * JPEG 里有没有 APP1 段（Exif 和 XMP 都放在这里：方向、拍摄时间、GPS …）。只扫文件头部的段，扫到图像数据（SOS）为止；
 * 结构读不下去就当没有。
 */
export function jpegHasExif(b: Uint8Array): boolean {
  if (b.length < 4 || b[0] !== 0xff || b[1] !== 0xd8) return false
  let i = 2
  while (i + 3 < b.length) {
    if (b[i] !== 0xff) return false
    const marker = b[i + 1]!
    if (marker === 0xff) {
      i += 1 // 填充字节
      continue
    }
    if (marker === 0xe1) return true
    if (marker === 0xda || marker === 0xd9) return false // 图像数据 / 结束
    if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd8)) {
      i += 2 // 没有长度的标记
      continue
    }
    const len = (b[i + 2]! << 8) | b[i + 3]!
    if (len < 2) return false
    i += 2 + len
  }
  return false
}

function fit(width: number, height: number, longEdge: number): { w: number; h: number } {
  const long = Math.max(width, height)
  if (long <= longEdge) return { w: width, h: height }
  const scale = longEdge / long
  return { w: Math.max(1, Math.round(width * scale)), h: Math.max(1, Math.round(height * scale)) }
}

let codecOverride: ImageCodec | null = null
/** 测试用：整个应用换一套编解码（jsdom 里没有 createImageBitmap / canvas）。传 null 恢复。 */
export function setImageCodec(codec: ImageCodec | null): void {
  codecOverride = codec
}

export interface NormalizeOptions {
  /** 这个会话的 agent 收的 mime（`capabilities.attachments`）；和 daemon 的上传白名单取交集。 */
  accepted: readonly string[]
  codec?: ImageCodec
}

interface Encoded {
  blob: Blob
  mime: string
  w: number
  h: number
}

/**
 * 把用户选的文件规整成可以上传的样子。解不开 / 压不小抛 `ImageError`。
 * 原样上传时返回的字节就是文件本身（动图 GIF 不会被压成静态图）。
 */
export async function normalizeImage(file: File, opts: NormalizeOptions): Promise<NormalizedImage> {
  const codec = opts.codec ?? codecOverride ?? browserCodec
  const accepted = uploadableMimes(opts.accepted)
  if (accepted.length === 0) throw new ImageError('unsupported_format')
  const declared = (file.type ?? '').toLowerCase()
  if (declared !== '' && !declared.startsWith('image/')) throw new ImageError('not_image')

  const original = await readBytes(file)
  // 文件头比扩展名 / 浏览器报的类型可靠（daemon 也是看文件头核对的）
  const mime = sniffImageMime(original) ?? declared
  let image: DecodedImage
  try {
    image = await codec.decode(file)
  } catch (e) {
    throw new ImageError('undecodable', e instanceof Error ? e.message : undefined)
  }
  try {
    const { width, height } = image
    if (!(width > 0 && height > 0)) throw new ImageError('undecodable')
    const longEdge = Math.max(width, height)
    const sourceOk = accepted.includes(mime)
    // 带元数据的 JPEG 一定要重画（方向转正、去掉 GPS 等），哪怕尺寸和大小都合适
    const hasExif = mime === 'image/jpeg' && jpegHasExif(original)
    if (sourceOk && !hasExif && original.length <= REENCODE_BYTES && longEdge <= MAX_LONG_EDGE) {
      return { bytes: original, mime, name: nameForMime(file.name, mime), width, height }
    }

    const edge = Math.min(longEdge, MAX_LONG_EDGE)
    const natural = naturalFormats(mime, accepted)
    const lossy = LOSSY.filter((m) => accepted.includes(m))
    if (natural.length === 0 && lossy.length === 0) throw new ImageError('unsupported_format')

    const encodeAt = async (longSide: number, want: string, quality: number): Promise<Encoded | null> => {
      const { w, h } = fit(width, height, longSide)
      const blob = await codec.encode(image, w, h, want, quality)
      if (!blob || blob.size === 0) return null
      const got = (blob.type || want).toLowerCase()
      // 浏览器编不出请求的格式时会退回 PNG；PNG 也不在 agent 收的格式里就当这次没成
      if (!accepted.includes(got)) return null
      return { blob, mime: got, w, h }
    }
    const finish = async (e: Encoded): Promise<NormalizedImage> => ({ bytes: await readBytes(e.blob), mime: e.mime, name: nameForMime(file.name, e.mime), width: e.w, height: e.h })

    // 1. 缩到长边 ≤ 2 000，按原格式重画；够小就用它
    let first: Encoded | null = null
    for (const want of natural) {
      first = await encodeAt(edge, want, JPEG_QUALITY)
      if (first) break
    }
    if (first && first.blob.size <= REENCODE_BYTES) return await finish(first)

    // 2. 太大：有损重编码（WebP 优先，浏览器编不出就 JPEG）。第一步已经是这个格式的就直接用它
    let lossyMime: string | null = null
    for (const want of lossy) {
      const e = first && first.mime === want ? first : await encodeAt(edge, want, JPEG_QUALITY)
      if (!e || e.mime !== want) continue
      lossyMime ??= want
      if (e.blob.size <= MAX_IMAGE_BYTES) return await finish(e)
      break // 这个格式编得出来，只是还太大：下面降质量
    }
    // 没有可用的有损格式（agent 只收 PNG，或浏览器编不出）：第一步的结果不超硬上限就用
    if (lossyMime === null && first && first.blob.size <= MAX_IMAGE_BYTES) return await finish(first)

    // 3. 还超硬上限：降质量（只对有损格式有用），再一路缩小尺寸
    const ladder = lossyMime ?? first?.mime ?? natural[natural.length - 1] ?? lossy[0]!
    if (lossyMime) {
      for (const q of QUALITY_STEPS) {
        const e = await encodeAt(edge, lossyMime, q)
        if (e && e.mime === lossyMime && e.blob.size <= MAX_IMAGE_BYTES) return await finish(e)
      }
    }
    for (let side = Math.floor(edge * 0.75); side >= MIN_LONG_EDGE; side = Math.floor(side * 0.75)) {
      const e = await encodeAt(side, ladder, 0.8)
      if (e && e.blob.size <= MAX_IMAGE_BYTES) return await finish(e)
    }
    throw new ImageError('too_large')
  } finally {
    image.close()
  }
}

// ---- 浏览器的编解码 ----

class BrowserImage implements DecodedImage {
  constructor(
    readonly source: ImageBitmap | HTMLImageElement,
    readonly width: number,
    readonly height: number,
    private readonly cleanup?: () => void,
  ) {}
  close(): void {
    if ('close' in this.source) this.source.close()
    this.cleanup?.()
  }
}

async function decodeWithImg(file: Blob): Promise<BrowserImage> {
  const url = URL.createObjectURL(file)
  const img = new Image()
  img.decoding = 'async'
  try {
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve()
      img.onerror = () => reject(new Error('the browser could not decode the image'))
      img.src = url
    })
  } catch (e) {
    URL.revokeObjectURL(url)
    throw e
  }
  return new BrowserImage(img, img.naturalWidth, img.naturalHeight, () => URL.revokeObjectURL(url))
}

export const browserCodec: ImageCodec = {
  async decode(file) {
    if (typeof createImageBitmap === 'function') {
      try {
        // 明确要求按 EXIF 方向转正：规整后的图不再带 EXIF（重画时去掉了），方向只能在解码时落实
        const bmp = await createImageBitmap(file, { imageOrientation: 'from-image' })
        return new BrowserImage(bmp, bmp.width, bmp.height)
      } catch {
        // 有的浏览器的 createImageBitmap 不认某些格式或这个选项，退回 <img>（<img> 默认按 EXIF 方向显示）
      }
    }
    return decodeWithImg(file)
  },

  async encode(image, width, height, mime, quality) {
    if (!(image instanceof BrowserImage)) return null
    const draw = (ctx: OffscreenCanvasRenderingContext2D | CanvasRenderingContext2D): void => {
      // JPEG 没有透明通道：不铺底色的话透明区域会变成黑色
      if (mime === 'image/jpeg') {
        ctx.fillStyle = '#ffffff'
        ctx.fillRect(0, 0, width, height)
      }
      ctx.imageSmoothingEnabled = true
      ctx.imageSmoothingQuality = 'high'
      ctx.drawImage(image.source, 0, 0, width, height)
    }
    if (typeof OffscreenCanvas === 'function') {
      const canvas = new OffscreenCanvas(width, height)
      const ctx = canvas.getContext('2d')
      if (!ctx) return null
      draw(ctx)
      return canvas.convertToBlob({ type: mime, quality })
    }
    const canvas = document.createElement('canvas')
    canvas.width = width
    canvas.height = height
    const ctx = canvas.getContext('2d')
    if (!ctx) return null
    draw(ctx)
    return new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, mime, quality))
  },
}
