import { describe, expect, it, vi } from 'vitest'
import { fileMime, ImageError, isImageMime, jpegHasExif, MAX_IMAGE_BYTES, MAX_LONG_EDGE, nameForMime, normalizeImage, REENCODE_BYTES, sanitizeName, sniffImageMime, uploadableMimes, type DecodedImage, type ImageCodec } from './normalize'

const ALL = ['image/png', 'image/jpeg', 'image/gif', 'image/webp']
const MiB = 1024 * 1024

const PNG_HEAD = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
const JPEG_HEAD = [0xff, 0xd8, 0xff, 0xe0]
/** SOI + APP1 段（Exif）：长度 16，内容 `Exif\0\0` 加几个字节。 */
const JPEG_EXIF_HEAD = [0xff, 0xd8, 0xff, 0xe1, 0x00, 0x10, 0x45, 0x78, 0x69, 0x66, 0x00, 0x00, 1, 2, 3, 4, 5, 6, 0xff, 0xda]
/** SOI + APP0（JFIF，长度 16）+ APP1（XMP 的标识 `http://ns.adobe.com/xap/1.0/` 的开头）。 */
const JPEG_XMP_HEAD = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 0xff, 0xe1, 0x00, 0x08, 0x68, 0x74, 0x74, 0x70, 0x3a, 0x2f]
const GIF_HEAD = [0x47, 0x49, 0x46, 0x38, 0x39, 0x61]
const WEBP_HEAD = [0x52, 0x49, 0x46, 0x46, 0, 0, 0, 0, 0x57, 0x45, 0x42, 0x50]

/** 一个有文件头、指定大小的 File。 */
function fileOf(head: number[], size: number, name: string, type: string): File {
  const bytes = new Uint8Array(size)
  bytes.set(head)
  return new File([bytes], name, { type })
}

interface Call {
  w: number
  h: number
  mime: string
  quality: number
}

/** 假编解码：尺寸由测试给，编码后的字节数 = 像素数 × 每像素字节数（JPEG / WebP 还乘质量）。 */
function fakeCodec(dims: { w: number; h: number }, over: { bpp?: Record<string, number>; fallbackToPng?: string[]; decodeError?: boolean; alwaysHuge?: boolean } = {}) {
  const bpp: Record<string, number> = { 'image/png': 3, 'image/jpeg': 0.5, 'image/webp': 0.4, ...over.bpp }
  const calls: Call[] = []
  const close = vi.fn()
  const codec: ImageCodec = {
    async decode(): Promise<DecodedImage> {
      if (over.decodeError) throw new Error('boom')
      return { width: dims.w, height: dims.h, close }
    },
    async encode(_img, w, h, mime, quality) {
      calls.push({ w, h, mime, quality })
      const type = over.fallbackToPng?.includes(mime) ? 'image/png' : mime
      const lossy = type !== 'image/png'
      // 不真的分配几十 MB：只要 `size` 和 `type` 对得上就行
      if (over.alwaysHuge) return { size: 99 * MiB, type, arrayBuffer: async () => new ArrayBuffer(1) } as unknown as Blob
      return new Blob([new Uint8Array(Math.round(w * h * (bpp[type] ?? 1) * (lossy ? quality : 1)))], { type })
    },
  }
  return { codec, calls, close }
}

describe('normalizeImage: pass-through', () => {
  it('uploads an accepted, small, modest-resolution image exactly as it is', async () => {
    const { codec, calls, close } = fakeCodec({ w: 800, h: 600 })
    const file = fileOf(PNG_HEAD, 100_000, 'shot.png', 'image/png')
    const out = await normalizeImage(file, { accepted: ALL, codec })
    expect(out).toMatchObject({ mime: 'image/png', name: 'shot.png', width: 800, height: 600 })
    expect(out.bytes).toEqual(new Uint8Array(await file.arrayBuffer()))
    expect(calls).toEqual([])
    expect(close).toHaveBeenCalledTimes(1)
  })

  it('keeps an animated GIF untouched while it is within the limits', async () => {
    const { codec, calls } = fakeCodec({ w: 400, h: 300 })
    const out = await normalizeImage(fileOf(GIF_HEAD, 1 * MiB, 'anim.gif', 'image/gif'), { accepted: ALL, codec })
    expect(out.mime).toBe('image/gif')
    expect(out.bytes.length).toBe(1 * MiB)
    expect(calls).toEqual([])
  })

  it('goes by the file header, not the declared type or the extension, and fixes the extension', async () => {
    const { codec } = fakeCodec({ w: 100, h: 100 })
    const out = await normalizeImage(fileOf(JPEG_HEAD, 5000, 'photo.png', 'image/png'), { accepted: ALL, codec })
    expect(out).toMatchObject({ mime: 'image/jpeg', name: 'photo.jpg' })
  })

  it('accepts exactly the limits: a 2 000 px long edge and 1.5 MiB', async () => {
    expect(MAX_LONG_EDGE).toBe(2000)
    expect(REENCODE_BYTES).toBe(1.5 * MiB)
    const { codec, calls } = fakeCodec({ w: 2000, h: 100 })
    const out = await normalizeImage(fileOf(PNG_HEAD, 1.5 * MiB, 'edge.png', 'image/png'), { accepted: ALL, codec })
    expect(out.bytes.length).toBe(1.5 * MiB)
    expect(calls).toEqual([])
  })

  it('one pixel over the edge limit, or one byte over 1.5 MiB, is processed', async () => {
    const wide = fakeCodec({ w: 2001, h: 100 }, { bpp: { 'image/png': 1 } })
    const a = await normalizeImage(fileOf(PNG_HEAD, 10_000, 'wide.png', 'image/png'), { accepted: ALL, codec: wide.codec })
    expect(wide.calls).toEqual([{ w: 2000, h: 100, mime: 'image/png', quality: 0.9 }])
    expect(a.width).toBe(2000)
    const heavy = fakeCodec({ w: 800, h: 600 }, { bpp: { 'image/png': 1 } })
    await normalizeImage(fileOf(PNG_HEAD, 1.5 * MiB + 1, 'heavy.png', 'image/png'), { accepted: ALL, codec: heavy.codec })
    expect(heavy.calls.length).toBeGreaterThan(0)
  })
})

describe('normalizeImage: EXIF', () => {
  it('detects an APP1 segment (Exif or XMP) in the JPEG header, and nothing else', () => {
    expect(jpegHasExif(Uint8Array.from(JPEG_EXIF_HEAD))).toBe(true)
    expect(jpegHasExif(Uint8Array.from(JPEG_XMP_HEAD))).toBe(true)
    // 只有 JFIF（APP0）
    expect(jpegHasExif(Uint8Array.from([0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0, 0, 0xff, 0xda, 0x00, 0x02]))).toBe(false)
    // 图像数据（SOS）之后出现的 0xFFE1 不算
    expect(jpegHasExif(Uint8Array.from([0xff, 0xd8, 0xff, 0xda, 0x00, 0x02, 0xff, 0xe1, 0x00, 0x08]))).toBe(false)
    expect(jpegHasExif(Uint8Array.from(PNG_HEAD))).toBe(false)
    expect(jpegHasExif(new Uint8Array(0))).toBe(false)
  })

  it('always redraws a JPEG that carries Exif — even a small one — so the orientation is applied and the metadata (GPS …) is gone', async () => {
    const { codec, calls } = fakeCodec({ w: 1200, h: 800 })
    const file = fileOf(JPEG_EXIF_HEAD, 300_000, 'IMG_0001.JPG', 'image/jpeg')
    const out = await normalizeImage(file, { accepted: ALL, codec })
    expect(calls).toEqual([{ w: 1200, h: 800, mime: 'image/jpeg', quality: 0.9 }])
    expect(out).toMatchObject({ mime: 'image/jpeg', width: 1200, height: 800, name: 'IMG_0001.JPG' })
    // 重画出来的字节，不是原文件（原文件的头里有 Exif 段）
    expect(jpegHasExif(out.bytes)).toBe(false)
    expect(out.bytes.length).not.toBe(300_000)
  })

  it('treats XMP (also in APP1) the same way, and leaves a JPEG with only JFIF alone', async () => {
    const xmp = fakeCodec({ w: 640, h: 480 })
    await normalizeImage(fileOf(JPEG_XMP_HEAD, 50_000, 'a.jpg', 'image/jpeg'), { accepted: ALL, codec: xmp.codec })
    expect(xmp.calls).toHaveLength(1)
    const plain = fakeCodec({ w: 640, h: 480 })
    const out = await normalizeImage(fileOf(JPEG_HEAD, 50_000, 'a.jpg', 'image/jpeg'), { accepted: ALL, codec: plain.codec })
    expect(plain.calls).toEqual([])
    expect(out.bytes.length).toBe(50_000)
  })

  it('a PNG that merely contains the bytes of an Exif header is not touched', async () => {
    const { codec, calls } = fakeCodec({ w: 100, h: 100 })
    const f = fileOf([...PNG_HEAD, 0xff, 0xe1, 0x00, 0x10], 4000, 'a.png', 'image/png')
    await normalizeImage(f, { accepted: ALL, codec })
    expect(calls).toEqual([])
  })
})

describe('normalizeImage: shrinking and re-encoding', () => {
  it('shrinks a long edge over 2 000 px to 2 000 (not 2 048) and keeps the original format when that is small enough', async () => {
    const { codec, calls } = fakeCodec({ w: 4000, h: 3000 })
    const out = await normalizeImage(fileOf(JPEG_HEAD, 6 * MiB, 'cam.jpg', 'image/jpeg'), { accepted: ALL, codec })
    expect(calls).toEqual([{ w: 2000, h: 1500, mime: 'image/jpeg', quality: 0.9 }])
    expect(out).toMatchObject({ mime: 'image/jpeg', name: 'cam.jpg', width: 2000, height: 1500 })
    expect(out.bytes.length).toBeLessThanOrEqual(REENCODE_BYTES)
  })

  it('a shrunk PNG still over 1.5 MiB is re-encoded lossy: WebP 0.9 when the agent takes it and the browser can make it', async () => {
    const { codec, calls } = fakeCodec({ w: 3000, h: 2000 })
    const out = await normalizeImage(fileOf(PNG_HEAD, 5 * MiB, 'big.png', 'image/png'), { accepted: ALL, codec })
    expect(calls).toEqual([
      { w: 2000, h: 1333, mime: 'image/png', quality: 0.9 },
      { w: 2000, h: 1333, mime: 'image/webp', quality: 0.9 },
    ])
    expect(out).toMatchObject({ mime: 'image/webp', name: 'big.webp', width: 2000, height: 1333 })
    expect(out.bytes.length).toBeLessThanOrEqual(MAX_IMAGE_BYTES)
  })

  it('… otherwise JPEG 0.9: the browser cannot encode WebP, or the agent does not take it', async () => {
    const noWebp = fakeCodec({ w: 3000, h: 2000 }, { fallbackToPng: ['image/webp'] })
    const a = await normalizeImage(fileOf(PNG_HEAD, 5 * MiB, 'big.png', 'image/png'), { accepted: ALL, codec: noWebp.codec })
    expect(noWebp.calls.map((c) => c.mime)).toEqual(['image/png', 'image/webp', 'image/jpeg'])
    expect(a).toMatchObject({ mime: 'image/jpeg', name: 'big.jpg' })

    const agentNoWebp = fakeCodec({ w: 3000, h: 2000 })
    const b = await normalizeImage(fileOf(PNG_HEAD, 5 * MiB, 'big.png', 'image/png'), { accepted: ['image/png', 'image/jpeg'], codec: agentNoWebp.codec })
    expect(agentNoWebp.calls.map((c) => c.mime)).toEqual(['image/png', 'image/jpeg'])
    expect(b).toMatchObject({ mime: 'image/jpeg', name: 'big.jpg' })
  })

  it('an original over 1.5 MiB is re-encoded even when it is small in pixels', async () => {
    const { codec, calls } = fakeCodec({ w: 1000, h: 1000 }, { bpp: { 'image/png': 2.5 } })
    const out = await normalizeImage(fileOf(PNG_HEAD, 2 * MiB, 'flat.png', 'image/png'), { accepted: ALL, codec })
    // 重画成 PNG 还是 2.4 MiB > 1.5 MiB，再转 WebP
    expect(calls.map((c) => [c.mime, c.w])).toEqual([['image/png', 1000], ['image/webp', 1000]])
    expect(out).toMatchObject({ mime: 'image/webp', width: 1000 })
  })

  it('a long edge far over the old 7 999 px limit is just shrunk to 2 000, and a small PNG stays PNG', async () => {
    const { codec, calls } = fakeCodec({ w: 8200, h: 400 }, { bpp: { 'image/png': 0.5 } })
    const out = await normalizeImage(fileOf(PNG_HEAD, 50_000, 'wide.png', 'image/png'), { accepted: ALL, codec })
    expect(calls).toEqual([{ w: 2000, h: 98, mime: 'image/png', quality: 0.9 }])
    expect(out).toMatchObject({ mime: 'image/png', width: 2000, height: 98 })
  })

  it('re-encodes a GIF that is over 1.5 MiB (a canvas cannot encode GIF: PNG first, then lossy)', async () => {
    const { codec, calls } = fakeCodec({ w: 1000, h: 1000 }, { bpp: { 'image/png': 5 } })
    const out = await normalizeImage(fileOf(GIF_HEAD, 6 * MiB, 'huge.gif', 'image/gif'), { accepted: ALL, codec })
    expect(calls.map((c) => c.mime)).toEqual(['image/png', 'image/webp'])
    expect(out).toMatchObject({ mime: 'image/webp', name: 'huge.webp' })
  })

  it('lowers the JPEG quality, then the dimensions, until the result fits the 3.75 MiB hard limit', async () => {
    // 只收 JPEG 的 agent；2000×2000：质量 0.9 时 4.3 MB 超限，0.8 时 3.84 MB 刚好够
    const { codec, calls } = fakeCodec({ w: 2000, h: 2000 }, { bpp: { 'image/jpeg': 1.2 } })
    const out = await normalizeImage(fileOf(JPEG_HEAD, 6 * MiB, 'dense.jpg', 'image/jpeg'), { accepted: ['image/jpeg'], codec })
    expect(calls.map((c) => c.quality)).toEqual([0.9, 0.8])
    expect(out.bytes.length).toBeLessThanOrEqual(MAX_IMAGE_BYTES)

    const dense = fakeCodec({ w: 2000, h: 2000 }, { bpp: { 'image/jpeg': 3 } })
    const out2 = await normalizeImage(fileOf(JPEG_HEAD, 6 * MiB, 'dense.jpg', 'image/jpeg'), { accepted: ['image/jpeg'], codec: dense.codec })
    expect(dense.calls.map((c) => c.quality)).toEqual([0.9, 0.8, 0.7, 0.6, 0.8, 0.8])
    expect(dense.calls.map((c) => c.w)).toEqual([2000, 2000, 2000, 2000, 1500, 1125])
    expect(out2.width).toBe(1125)
    expect(out2.bytes.length).toBeLessThanOrEqual(MAX_IMAGE_BYTES)
  })

  it('gives up with too_large when nothing fits, and still releases the bitmap', async () => {
    const { codec, close } = fakeCodec({ w: 6000, h: 6000 }, { alwaysHuge: true })
    await expect(normalizeImage(fileOf(PNG_HEAD, 50 * MiB, 'x.png', 'image/png'), { accepted: ALL, codec })).rejects.toMatchObject({ name: 'ImageError', code: 'too_large' })
    expect(close).toHaveBeenCalledTimes(1)
  })

  it('for an agent that only accepts PNG: keeps the shrunk PNG if it is under the hard limit, otherwise shrinks the dimensions', async () => {
    // 2000² × 0.5 = 1.9 MiB：超过 1.5 MiB，但没有有损格式可换、也没超 3.75 MiB
    const fits = fakeCodec({ w: 2200, h: 2200 }, { bpp: { 'image/png': 0.5 } })
    const a = await normalizeImage(fileOf(PNG_HEAD, 9 * MiB, 'x.png', 'image/png'), { accepted: ['image/png'], codec: fits.codec })
    expect(fits.calls.map((c) => c.w)).toEqual([2000])
    expect(a).toMatchObject({ mime: 'image/png', width: 2000 })

    // 2000² × 1 = 3.8 MiB > 3.75 MiB，缩到 1500²（2.1 MiB）才够
    const { codec, calls } = fakeCodec({ w: 4000, h: 4000 }, { bpp: { 'image/png': 1 } })
    const out = await normalizeImage(fileOf(PNG_HEAD, 9 * MiB, 'x.png', 'image/png'), { accepted: ['image/png'], codec })
    expect(calls.map((c) => c.w)).toEqual([2000, 1500])
    expect(out).toMatchObject({ mime: 'image/png', width: 1500 })
  })
})

describe('normalizeImage: formats the agent does not accept', () => {
  it('converts a small HEIC to PNG at its original size', async () => {
    const { codec, calls } = fakeCodec({ w: 1000, h: 700 }, { bpp: { 'image/png': 1 } })
    const out = await normalizeImage(fileOf([0, 0, 0, 0x18], 800_000, 'IMG_1.HEIC', 'image/heic'), { accepted: ALL, codec })
    expect(calls).toEqual([{ w: 1000, h: 700, mime: 'image/png', quality: 0.9 }])
    expect(out).toMatchObject({ mime: 'image/png', name: 'IMG_1.png', width: 1000, height: 700 })
  })

  it('re-encodes lossy when the converted PNG would be over 1.5 MiB', async () => {
    const { codec, calls } = fakeCodec({ w: 1800, h: 1200 })
    const out = await normalizeImage(fileOf([0x42, 0x4d], 900_000, 'scan.bmp', 'image/bmp'), { accepted: ALL, codec })
    expect(calls.map((c) => [c.mime, c.w])).toEqual([['image/png', 1800], ['image/webp', 1800]])
    expect(out).toMatchObject({ mime: 'image/webp', name: 'scan.webp', width: 1800 })
  })

  it('shrinks a big converted photo to 2 000 straight away', async () => {
    const { codec, calls } = fakeCodec({ w: 4000, h: 3000 })
    const out = await normalizeImage(fileOf([0, 0, 0, 0x18], 2 * MiB, 'IMG.heic', 'image/heic'), { accepted: ALL, codec })
    expect(calls.map((c) => [c.mime, c.w])).toEqual([['image/png', 2000], ['image/webp', 2000]])
    expect(out).toMatchObject({ mime: 'image/webp', width: 2000, height: 1500 })
  })

  it('converts to the only accepted format', async () => {
    const { codec } = fakeCodec({ w: 300, h: 300 })
    const out = await normalizeImage(fileOf(PNG_HEAD, 10_000, 'a.png', 'image/png'), { accepted: ['image/jpeg'], codec })
    expect(out).toMatchObject({ mime: 'image/jpeg', name: 'a.jpg' })
  })

  it('uses the PNG a browser falls back to when it cannot encode WebP, if PNG is accepted', async () => {
    const { codec } = fakeCodec({ w: 4000, h: 3000 }, { fallbackToPng: ['image/webp'], bpp: { 'image/png': 0.1 } })
    const out = await normalizeImage(fileOf(WEBP_HEAD, 6 * MiB, 'pic.webp', 'image/webp'), { accepted: ALL, codec })
    expect(out).toMatchObject({ mime: 'image/png', name: 'pic.png' })
  })

  it('ignores a fallback format the agent does not accept', async () => {
    const { codec } = fakeCodec({ w: 4000, h: 3000 }, { fallbackToPng: ['image/webp'] })
    await expect(normalizeImage(fileOf(WEBP_HEAD, 6 * MiB, 'pic.webp', 'image/webp'), { accepted: ['image/webp'], codec })).rejects.toMatchObject({ code: 'too_large' })
  })
})

describe('normalizeImage: errors', () => {
  it('rejects things that are not images', async () => {
    const { codec } = fakeCodec({ w: 1, h: 1 })
    await expect(normalizeImage(new File(['hello'], 'a.txt', { type: 'text/plain' }), { accepted: ALL, codec })).rejects.toMatchObject({ code: 'not_image' })
  })

  it('reports an undecodable image', async () => {
    const { codec } = fakeCodec({ w: 1, h: 1 }, { decodeError: true })
    const p = normalizeImage(fileOf(PNG_HEAD, 100, 'broken.png', 'image/png'), { accepted: ALL, codec })
    await expect(p).rejects.toBeInstanceOf(ImageError)
    await expect(p).rejects.toMatchObject({ code: 'undecodable' })
  })

  it('reports an agent whose formats the node cannot take at all', async () => {
    const { codec } = fakeCodec({ w: 1, h: 1 })
    await expect(normalizeImage(fileOf(PNG_HEAD, 100, 'a.png', 'image/png'), { accepted: ['image/bmp'], codec })).rejects.toMatchObject({ code: 'unsupported_format' })
  })

  it('treats a file with no declared type as an attempt to decode (some platforms report none for HEIC)', async () => {
    const { codec } = fakeCodec({ w: 200, h: 200 })
    const out = await normalizeImage(fileOf([0, 0, 0, 0x18], 5000, 'IMG.heic', ''), { accepted: ALL, codec })
    expect(out.mime).toBe('image/png')
  })
})

describe('helpers', () => {
  it('sniffImageMime recognises the four formats and nothing else', () => {
    expect(sniffImageMime(Uint8Array.from(PNG_HEAD))).toBe('image/png')
    expect(sniffImageMime(Uint8Array.from(JPEG_HEAD))).toBe('image/jpeg')
    expect(sniffImageMime(Uint8Array.from(GIF_HEAD))).toBe('image/gif')
    expect(sniffImageMime(Uint8Array.from(WEBP_HEAD))).toBe('image/webp')
    expect(sniffImageMime(Uint8Array.from([1, 2, 3]))).toBeNull()
    expect(sniffImageMime(new Uint8Array(0))).toBeNull()
  })

  it('sanitizeName keeps names the daemon accepts (1–255 bytes, no slash, backslash or control characters)', () => {
    expect(sanitizeName('a/b\\c\u0007d.png')).toBe('a_b_c_d.png')
    expect(sanitizeName('')).toBe('image')
    expect(sanitizeName('..')).toBe('image')
    const long = sanitizeName(`${'界'.repeat(200)}.png`)
    expect(new TextEncoder().encode(long).length).toBeLessThanOrEqual(255)
    expect(long.endsWith('.png')).toBe(true)
  })

  it('sanitizeName also replaces what the daemon rejects: C1 controls, bidirectional controls and the line / paragraph separators', () => {
    // 逐个字符：U+0085（C1）、U+009F（C1）、U+200E U+200F、U+202A–U+202E、U+2066–U+2069、U+2028 U+2029、DEL
    for (const ch of ['\u0085', '\u009f', '\u200e', '\u200f', '\u202a', '\u202b', '\u202c', '\u202d', '\u202e', '\u2066', '\u2067', '\u2068', '\u2069', '\u2028', '\u2029', '\u007f']) {
      expect(sanitizeName(`a${ch}b.png`), `U+${ch.codePointAt(0)!.toString(16)}`).toBe('a_b.png')
    }
    // 把扩展名倒着显示的那一招
    expect(sanitizeName('invoice\u202Egnp.exe')).toBe('invoice_gnp.exe')
    // 不是控制符的空白、零宽连接符（emoji 序列要用）原样保留
    expect(sanitizeName('a\u00a0b\u200dc.png')).toBe('a\u00a0b\u200dc.png')
  })

  it('nameForMime swaps a wrong extension, keeps a right one, and adds a missing one', () => {
    expect(nameForMime('a.heic', 'image/png')).toBe('a.png')
    expect(nameForMime('a.JPEG', 'image/jpeg')).toBe('a.JPEG')
    expect(nameForMime('photo', 'image/jpeg')).toBe('photo.jpg')
    expect(nameForMime('x.tar.gz', 'image/png')).toBe('x.tar.png')
    // 和 daemon 对齐：jpg / jpeg 不分大小写，别的（jpe …）算对不上
    expect(nameForMime('a.JPG', 'image/jpeg')).toBe('a.JPG')
    expect(nameForMime('x.jpe', 'image/jpeg')).toBe('x.jpg')
    // 内容是 JPEG 而名字叫 x.html：下载时不能存成 .html
    expect(nameForMime('x.html', 'image/jpeg')).toBe('x.jpg')
  })

  it('uploadableMimes intersects the agent list with the node whitelist', () => {
    expect(uploadableMimes(['image/png', 'image/bmp', 'image/webp'])).toEqual(['image/png', 'image/webp'])
    expect(uploadableMimes([])).toEqual([])
  })
})

describe('fileMime / isImageMime', () => {
  it('the type a file is uploaded with: lower case, no parameters, application/octet-stream when empty or not something the node takes', () => {
    expect(fileMime('application/pdf')).toBe('application/pdf')
    expect(fileMime('Text/Plain; charset=UTF-8')).toBe('text/plain')
    expect(fileMime('application/vnd.openxmlformats-officedocument.wordprocessingml.document')).toBe('application/vnd.openxmlformats-officedocument.wordprocessingml.document')
    expect(fileMime('')).toBe('application/octet-stream')
    expect(fileMime('text')).toBe('application/octet-stream')
    expect(fileMime('a b/c')).toBe('application/octet-stream')
    expect(fileMime('*/*')).toBe('application/octet-stream')
    expect(fileMime('.x/y')).toBe('application/octet-stream')
    expect(fileMime('x/+y')).toBe('application/octet-stream')
    expect(fileMime(`application/${'x'.repeat(130)}`)).toBe('application/octet-stream')
  })

  it('only png / jpeg / gif / webp count as images that can be shown inline', () => {
    expect(['image/png', 'image/jpeg', 'image/gif', 'image/webp'].every(isImageMime)).toBe(true)
    expect(isImageMime('image/svg+xml')).toBe(false)
    expect(isImageMime('image/heic')).toBe(false)
    expect(isImageMime('text/html')).toBe(false)
  })
})
