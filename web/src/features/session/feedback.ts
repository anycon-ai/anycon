// turn 结束时的提示音和震动（外观设置里的两个偏好，默认都关，spec/web.md § 4.6）。
// 提示音用 WebAudio 现场合成两个短的正弦音，不带音频文件（CSP 不放开 media-src，也不多一份资源）。
// 震动用 `navigator.vibrate`：只有 Android 上的浏览器真的会震；iOS Safari 没有这个 API，桌面浏览器有 API 但没有马达，
// 所以设置页只在 `canVibrate()` 为真时给这个开关。
import { isAndroid } from '@/lib/device'

type AudioContextCtor = typeof AudioContext

let ctx: AudioContext | null = null

function audioContext(): AudioContext | null {
  if (typeof window === 'undefined') return null
  const Ctor: AudioContextCtor | undefined = window.AudioContext ?? (window as Window & { webkitAudioContext?: AudioContextCtor }).webkitAudioContext
  if (!Ctor) return null
  if (!ctx) {
    try {
      ctx = new Ctor()
    } catch {
      return null
    }
  }
  return ctx
}

/** 两个音：A5 → E6，各约 0.25 秒，音量很小。 */
const NOTES: readonly (readonly [freq: number, at: number])[] = [
  [880, 0],
  [1318.5, 0.09],
]

/** 响一声。浏览器不支持 WebAudio、或者还没有用户手势解锁时静默不响。 */
export function playChime(): void {
  const c = audioContext()
  if (!c) return
  if (c.state === 'suspended') void c.resume().catch(() => undefined)
  const t0 = c.currentTime + 0.01
  for (const [freq, at] of NOTES) {
    const osc = c.createOscillator()
    osc.type = 'sine'
    osc.frequency.value = freq
    const gain = c.createGain()
    // 指数包络不能从 0 开始：从一个听不见的小值起
    gain.gain.setValueAtTime(0.0001, t0 + at)
    gain.gain.exponentialRampToValueAtTime(0.06, t0 + at + 0.015)
    gain.gain.exponentialRampToValueAtTime(0.0001, t0 + at + 0.25)
    osc.connect(gain)
    gain.connect(c.destination)
    osc.start(t0 + at)
    osc.stop(t0 + at + 0.3)
  }
}

/**
 * 浏览器要求先有一次用户手势才能出声（iOS Safari 尤其严格）。提示音打开时，在页面上的第一次点按里把音频上下文建好 / 恢复，
 * 之后 turn 结束时才响得出来。返回取消监听的函数。
 */
export function primeChimeOnGesture(): () => void {
  if (typeof document === 'undefined') return () => undefined
  const prime = (): void => {
    const c = audioContext()
    if (c?.state === 'suspended') void c.resume().catch(() => undefined)
  }
  document.addEventListener('pointerdown', prime, { once: true, capture: true })
  return () => document.removeEventListener('pointerdown', prime, { capture: true })
}

/** 这台设备上震动是不是真的有用：Android 上、有 `navigator.vibrate`。 */
export function canVibrate(): boolean {
  return typeof navigator !== 'undefined' && typeof navigator.vibrate === 'function' && isAndroid()
}

/** 短震一下。不支持时什么都不做。 */
export function vibrateOnce(): void {
  if (!canVibrate()) return
  try {
    navigator.vibrate(30)
  } catch {
    // 有的浏览器在没有用户激活时会抛：不震就是了
  }
}
