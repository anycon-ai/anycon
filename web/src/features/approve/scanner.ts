// 应用内扫码（ADR-36）：摄像头 + 二维码解码。画面只在本地解码，不上传、不录制、不截图。
// 优先用浏览器自带的 `BarcodeDetector`；没有的话用打包进来的 jsQR（CSP 只允许同源脚本，不能从 CDN 拉）。

export type CameraErrorCode = 'denied' | 'unavailable'

export class CameraError extends Error {
  constructor(
    readonly code: CameraErrorCode,
    message?: string,
  ) {
    super(message ?? code)
    this.name = 'CameraError'
  }
}

/** 有摄像头吗：`navigator.mediaDevices` 可用（安全上下文）而且枚举得到视频输入。没有就不显示扫码入口。 */
export async function cameraAvailable(): Promise<boolean> {
  const md = typeof navigator === 'undefined' ? undefined : navigator.mediaDevices
  if (!md || typeof md.getUserMedia !== 'function' || typeof md.enumerateDevices !== 'function') return false
  try {
    return (await md.enumerateDevices()).some((d) => d.kind === 'videoinput')
  } catch {
    return false
  }
}

/** 打开后置摄像头，画面接到 `video` 上。返回关闭函数（停掉所有轨道）。权限被拒 → `CameraError('denied')`。 */
export async function openCamera(video: HTMLVideoElement): Promise<() => void> {
  const md = navigator.mediaDevices
  if (!md || typeof md.getUserMedia !== 'function') throw new CameraError('unavailable')
  let stream: MediaStream
  try {
    stream = await md.getUserMedia({ video: { facingMode: 'environment' }, audio: false })
  } catch (e) {
    const name = (e as { name?: string } | null)?.name
    if (name === 'NotAllowedError' || name === 'SecurityError' || name === 'PermissionDeniedError') throw new CameraError('denied')
    throw new CameraError('unavailable', e instanceof Error ? e.message : undefined)
  }
  video.srcObject = stream
  video.muted = true
  video.setAttribute('playsinline', 'true')
  const stop = (): void => {
    for (const track of stream.getTracks()) track.stop()
    video.srcObject = null
  }
  try {
    await video.play()
  } catch (e) {
    stop()
    throw new CameraError('unavailable', e instanceof Error ? e.message : undefined)
  }
  return stop
}

export interface QrReader {
  /** 从当前画面解码一个二维码，没有返回 null。 */
  read(video: HTMLVideoElement): Promise<string | null>
}

interface BarcodeDetectorLike {
  detect(source: CanvasImageSource): Promise<Array<{ rawValue: string }>>
}
interface BarcodeDetectorCtor {
  new (opts: { formats: string[] }): BarcodeDetectorLike
  getSupportedFormats?: () => Promise<string[]>
}

/** 解码器：`BarcodeDetector` 支持二维码就用它，否则用 jsQR（按需加载，不进主包）。 */
export async function createQrReader(): Promise<QrReader> {
  const Detector = (globalThis as { BarcodeDetector?: BarcodeDetectorCtor }).BarcodeDetector
  if (Detector) {
    try {
      const formats = (await Detector.getSupportedFormats?.()) ?? ['qr_code']
      if (formats.includes('qr_code')) {
        const detector = new Detector({ formats: ['qr_code'] })
        return {
          read: async (video) => {
            const found = await detector.detect(video)
            return found[0]?.rawValue ?? null
          },
        }
      }
    } catch (e) {
      console.warn('anycon: BarcodeDetector is unusable; falling back to the bundled QR decoder', e)
    }
  }
  const { default: jsQR } = await import('jsqr')
  const canvas = document.createElement('canvas')
  const ctx = canvas.getContext('2d', { willReadFrequently: true })
  return {
    read: async (video) => {
      if (!ctx || video.videoWidth === 0 || video.videoHeight === 0) return null
      // 缩到长边 640：够识别、解码快
      const scale = Math.min(1, 640 / Math.max(video.videoWidth, video.videoHeight))
      canvas.width = Math.round(video.videoWidth * scale)
      canvas.height = Math.round(video.videoHeight * scale)
      ctx.drawImage(video, 0, 0, canvas.width, canvas.height)
      const image = ctx.getImageData(0, 0, canvas.width, canvas.height)
      return jsQR(image.data, image.width, image.height, { inversionAttempts: 'dontInvert' })?.data ?? null
    },
  }
}
