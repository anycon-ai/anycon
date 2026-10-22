// 禁止页面缩放（外观设置里的开关，默认关，保持系统行为）。iOS 和 Android 处理 viewport 的方式不同，分开做：
// - iOS Safari：聚焦字号小于 16px 的输入框会自动放大且不缩回，`maximum-scale=1` 能止住它；
//   但为了无障碍，Safari 不理 `user-scalable=no`，双指缩放要拦 WebKit 的 `gesturestart` / `gesturechange`，
//   双击缩放靠 `touch-action: manipulation`（index.css 的 `html[data-zoom-lock]`）。
// - Android Chrome：聚焦时本来就不放大；`maximum-scale=1, user-scalable=no` 关掉双指和双击缩放
//   （系统无障碍里的「强制启用缩放」仍然可以覆盖）。
// 其他平台什么都不做，设置页也不显示这个开关。
import { isAndroid, isIos } from './device'

export type ZoomPlatform = 'ios' | 'android'

export function zoomPlatform(): ZoomPlatform | null {
  if (isIos()) return 'ios'
  if (isAndroid()) return 'android'
  return null
}

const preventGesture = (e: Event): void => e.preventDefault()
/** index.html 里原本的 viewport，关掉开关时还原成它。 */
let original: string | null = null

export function applyZoomLock(enabled: boolean, platform: ZoomPlatform | null = zoomPlatform()): void {
  const meta = document.querySelector<HTMLMetaElement>('meta[name="viewport"]')
  const root = document.documentElement
  if (meta && original === null) original = meta.content
  // 先回到系统行为，再按平台加限制
  if (meta && original !== null) meta.content = original
  delete root.dataset.zoomLock
  document.removeEventListener('gesturestart', preventGesture)
  document.removeEventListener('gesturechange', preventGesture)
  if (!enabled || !platform) return
  if (platform === 'ios') {
    if (meta) meta.content = `${original}, maximum-scale=1`
    root.dataset.zoomLock = '1'
    document.addEventListener('gesturestart', preventGesture, { passive: false })
    document.addEventListener('gesturechange', preventGesture, { passive: false })
  } else if (meta) {
    meta.content = `${original}, maximum-scale=1, user-scalable=no`
  }
}
