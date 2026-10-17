import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const jsqr = vi.hoisted(() => ({ decode: vi.fn() }))
vi.mock('jsqr', () => ({ default: jsqr.decode }))

import { cameraAvailable, CameraError, createQrReader, openCamera } from './scanner'

const nav = navigator as Navigator & { mediaDevices?: unknown }
const originalMedia = Object.getOwnPropertyDescriptor(navigator, 'mediaDevices')

function setMedia(value: unknown): void {
  Object.defineProperty(navigator, 'mediaDevices', { configurable: true, value })
}

beforeEach(() => {
  jsqr.decode.mockReset()
})
afterEach(() => {
  if (originalMedia) Object.defineProperty(navigator, 'mediaDevices', originalMedia)
  else delete (nav as { mediaDevices?: unknown }).mediaDevices
  delete (globalThis as { BarcodeDetector?: unknown }).BarcodeDetector
  vi.restoreAllMocks()
})

describe('cameraAvailable', () => {
  it('needs navigator.mediaDevices (secure context) with getUserMedia and a video input', async () => {
    setMedia(undefined)
    expect(await cameraAvailable()).toBe(false)
    setMedia({ getUserMedia: vi.fn(), enumerateDevices: async () => [{ kind: 'audioinput' }] })
    expect(await cameraAvailable()).toBe(false)
    setMedia({ getUserMedia: vi.fn(), enumerateDevices: async () => [{ kind: 'audioinput' }, { kind: 'videoinput' }] })
    expect(await cameraAvailable()).toBe(true)
  })

  it('is false when enumerating fails', async () => {
    setMedia({
      getUserMedia: vi.fn(),
      enumerateDevices: async () => {
        throw new Error('nope')
      },
    })
    expect(await cameraAvailable()).toBe(false)
  })
})

describe('openCamera', () => {
  const video = () => {
    const el = document.createElement('video')
    el.play = vi.fn().mockResolvedValue(undefined)
    return el
  }

  it('asks for the environment-facing camera, no audio, and returns a function that stops every track', async () => {
    const track = { stop: vi.fn() }
    const getUserMedia = vi.fn().mockResolvedValue({ getTracks: () => [track, track] })
    setMedia({ getUserMedia })
    const el = video()
    const stop = await openCamera(el)
    expect(getUserMedia).toHaveBeenCalledWith({ video: { facingMode: 'environment' }, audio: false })
    expect(el.muted).toBe(true)
    expect(el.getAttribute('playsinline')).toBe('true')
    stop()
    expect(track.stop).toHaveBeenCalledTimes(2)
    expect(el.srcObject).toBeNull()
  })

  it.each(['NotAllowedError', 'SecurityError', 'PermissionDeniedError'])('maps %s to "denied"', async (name) => {
    setMedia({ getUserMedia: vi.fn().mockRejectedValue(Object.assign(new Error('x'), { name })) })
    await expect(openCamera(video())).rejects.toMatchObject({ code: 'denied' })
  })

  it('any other failure (no device, busy) is "unavailable"; no mediaDevices at all too', async () => {
    setMedia({ getUserMedia: vi.fn().mockRejectedValue(Object.assign(new Error('x'), { name: 'NotFoundError' })) })
    await expect(openCamera(video())).rejects.toBeInstanceOf(CameraError)
    await expect(openCamera(video())).rejects.toMatchObject({ code: 'unavailable' })
    setMedia(undefined)
    await expect(openCamera(video())).rejects.toMatchObject({ code: 'unavailable' })
  })

  it('stops the tracks again if the video cannot start playing', async () => {
    const track = { stop: vi.fn() }
    setMedia({ getUserMedia: vi.fn().mockResolvedValue({ getTracks: () => [track] }) })
    const el = video()
    el.play = vi.fn().mockRejectedValue(new Error('autoplay'))
    await expect(openCamera(el)).rejects.toMatchObject({ code: 'unavailable' })
    expect(track.stop).toHaveBeenCalled()
  })
})

describe('createQrReader', () => {
  it('uses the browser\'s BarcodeDetector when it can read QR codes', async () => {
    const detect = vi.fn().mockResolvedValue([{ rawValue: 'anycon-enroll:1:x' }])
    ;(globalThis as { BarcodeDetector?: unknown }).BarcodeDetector = class {
      static getSupportedFormats = async () => ['qr_code', 'ean_13']
      detect = detect
    }
    const reader = await createQrReader()
    const v = document.createElement('video')
    await expect(reader.read(v)).resolves.toBe('anycon-enroll:1:x')
    expect(detect).toHaveBeenCalledWith(v)
    expect(jsqr.decode).not.toHaveBeenCalled()
  })

  it('...and returns null when the frame has no code', async () => {
    ;(globalThis as { BarcodeDetector?: unknown }).BarcodeDetector = class {
      static getSupportedFormats = async () => ['qr_code']
      detect = async () => []
    }
    const reader = await createQrReader()
    await expect(reader.read(document.createElement('video'))).resolves.toBeNull()
  })

  it('falls back to the bundled jsQR decoder when there is no BarcodeDetector (or it cannot read QR codes)', async () => {
    const drawImage = vi.fn()
    const getImageData = vi.fn().mockReturnValue({ data: new Uint8ClampedArray(16), width: 2, height: 2 })
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue({ drawImage, getImageData } as unknown as CanvasRenderingContext2D)
    jsqr.decode.mockReturnValue({ data: 'from jsqr' })
    ;(globalThis as { BarcodeDetector?: unknown }).BarcodeDetector = class {
      static getSupportedFormats = async () => ['ean_13']
    }
    const reader = await createQrReader()
    const v = document.createElement('video')
    Object.defineProperty(v, 'videoWidth', { value: 1280 })
    Object.defineProperty(v, 'videoHeight', { value: 720 })
    await expect(reader.read(v)).resolves.toBe('from jsqr')
    expect(drawImage).toHaveBeenCalledWith(v, 0, 0, 640, 360) // 缩到长边 640
    expect(jsqr.decode).toHaveBeenCalledWith(expect.any(Uint8ClampedArray), 2, 2, { inversionAttempts: 'dontInvert' })

    jsqr.decode.mockReturnValue(null)
    await expect(reader.read(v)).resolves.toBeNull()
  })

  it('does not decode before the video has a picture', async () => {
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue({ drawImage: vi.fn(), getImageData: vi.fn() } as unknown as CanvasRenderingContext2D)
    const reader = await createQrReader()
    await expect(reader.read(document.createElement('video'))).resolves.toBeNull()
    expect(jsqr.decode).not.toHaveBeenCalled()
  })
})
