// 浏览器那一半的编解码（createImageBitmap / <img> 解码；OffscreenCanvas / <canvas> 编码）：jsdom 里都没有，用桩验证调用方式。
import { afterEach, describe, expect, it, vi } from 'vitest'
import { browserCodec, normalizeImage } from './normalize'

afterEach(() => vi.unstubAllGlobals())

const file = (type = 'image/png'): File => new File([Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3])], 'a.png', { type })

interface Ctx {
  fillStyle: string
  imageSmoothingEnabled: boolean
  imageSmoothingQuality: string
  fillRect: ReturnType<typeof vi.fn>
  drawImage: ReturnType<typeof vi.fn>
}
const makeCtx = (log: string[]): Ctx => ({
  fillStyle: '',
  imageSmoothingEnabled: false,
  imageSmoothingQuality: '',
  fillRect: vi.fn(() => log.push('fill')),
  drawImage: vi.fn(() => log.push('draw')),
})

describe('browserCodec.decode', () => {
  it('uses createImageBitmap when there is one, and closes the bitmap', async () => {
    const close = vi.fn()
    const cib = vi.fn(async () => ({ width: 100, height: 50, close }))
    vi.stubGlobal('createImageBitmap', cib)
    const img = await browserCodec.decode(file())
    expect(cib).toHaveBeenCalledTimes(1)
    // 带 EXIF 的 JPEG 重画后不再有元数据：方向要在解码时就按 EXIF 转正
    expect(cib).toHaveBeenCalledWith(expect.anything(), { imageOrientation: 'from-image' })
    expect([img.width, img.height]).toEqual([100, 50])
    img.close()
    expect(close).toHaveBeenCalledTimes(1)
  })

  it('falls back to an <img> element when createImageBitmap cannot read the file, and revokes its object URL on close', async () => {
    vi.stubGlobal('createImageBitmap', vi.fn(() => Promise.reject(new Error('unsupported'))))
    class FakeImage {
      onload: (() => void) | null = null
      onerror: (() => void) | null = null
      decoding = ''
      naturalWidth = 30
      naturalHeight = 20
      set src(_: string) {
        queueMicrotask(() => this.onload?.())
      }
    }
    vi.stubGlobal('Image', FakeImage)
    const revoke = vi.spyOn(URL, 'revokeObjectURL')
    const img = await browserCodec.decode(file())
    expect([img.width, img.height]).toEqual([30, 20])
    expect(revoke).not.toHaveBeenCalled()
    img.close()
    expect(revoke).toHaveBeenCalledTimes(1)
  })

  it('rejects when neither can decode it, and does not leak the object URL', async () => {
    vi.stubGlobal('createImageBitmap', undefined)
    class BrokenImage {
      onload: (() => void) | null = null
      onerror: (() => void) | null = null
      set src(_: string) {
        queueMicrotask(() => this.onerror?.())
      }
    }
    vi.stubGlobal('Image', BrokenImage)
    const revoke = vi.spyOn(URL, 'revokeObjectURL')
    await expect(browserCodec.decode(file())).rejects.toThrow(/decode/)
    expect(revoke).toHaveBeenCalledTimes(1)
  })
})

describe('browserCodec.encode', () => {
  async function decoded() {
    vi.stubGlobal('createImageBitmap', vi.fn(async () => ({ width: 100, height: 50, close: () => {} })))
    return browserCodec.decode(file())
  }

  it('draws the scaled image on an OffscreenCanvas and encodes with the wanted type and quality; JPEG gets a white background first', async () => {
    const log: string[] = []
    const ctx = makeCtx(log)
    const convert = vi.fn(async ({ type }: { type: string; quality: number }) => new Blob(['x'], { type }))
    class FakeOffscreen {
      constructor(
        readonly width: number,
        readonly height: number,
      ) {}
      getContext = () => ctx
      convertToBlob = convert
    }
    vi.stubGlobal('OffscreenCanvas', FakeOffscreen)
    const img = await decoded()

    const jpeg = await browserCodec.encode(img, 60, 30, 'image/jpeg', 0.9)
    expect(jpeg?.type).toBe('image/jpeg')
    expect(convert).toHaveBeenLastCalledWith({ type: 'image/jpeg', quality: 0.9 })
    expect(ctx.fillStyle).toBe('#ffffff')
    expect(ctx.drawImage).toHaveBeenLastCalledWith(expect.anything(), 0, 0, 60, 30)
    expect(ctx.imageSmoothingQuality).toBe('high')
    expect(log).toEqual(['fill', 'draw'])

    log.length = 0
    await browserCodec.encode(img, 60, 30, 'image/png', 0.9)
    expect(log).toEqual(['draw']) // PNG 保留透明
  })

  it('uses a <canvas> element and toBlob when there is no OffscreenCanvas', async () => {
    const log: string[] = []
    const ctx = makeCtx(log)
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue(ctx as unknown as CanvasRenderingContext2D)
    const toBlob = vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation((cb, type) => cb(new Blob(['x'], { type })))
    const img = await decoded()
    const out = await browserCodec.encode(img, 40, 20, 'image/webp', 0.8)
    expect(out?.type).toBe('image/webp')
    expect(toBlob).toHaveBeenCalledWith(expect.any(Function), 'image/webp', 0.8)
    expect(ctx.drawImage).toHaveBeenCalledWith(expect.anything(), 0, 0, 40, 20)
  })

  it('returns null when a 2d context is not available', async () => {
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue(null)
    const img = await decoded()
    expect(await browserCodec.encode(img, 10, 10, 'image/png', 0.9)).toBeNull()
  })
})

describe('normalizeImage with the browser codec', () => {
  it('a small PNG goes through untouched', async () => {
    vi.stubGlobal('createImageBitmap', vi.fn(async () => ({ width: 100, height: 50, close: () => {} })))
    const f = file()
    const out = await normalizeImage(f, { accepted: ['image/png'] })
    expect(out).toMatchObject({ mime: 'image/png', width: 100, height: 50, name: 'a.png' })
    expect(out.bytes).toEqual(new Uint8Array(await f.arrayBuffer()))
  })
})
