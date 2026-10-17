// `notificationclick`（spec/web.md § 7）：有已打开的本 origin 窗口就聚焦它并让页面跳转（不整页刷新：Noise 会话、草稿都在页面内存里），
// 没有就 `openWindow`。通知里的 `data.url` 是我们自己拼的站内路径，但仍然按不可信输入检查一遍：只认同源的 `/…` 绝对路径（`@/lib/safePath`）。
import { isSafePath } from '@/lib/safePath'
import type { WindowClientLike } from './types'

/** 页面收到这条消息就做站内跳转（`main.tsx` 里登记）。 */
export const NAVIGATE_MESSAGE = 'anycon:navigate'

export interface NavigateMessage {
  type: typeof NAVIGATE_MESSAGE
  url: string
}

/** 通知数据里的跳转路径（同源的绝对路径，见 `isSafePath`）；不合格的一律回首页。 */
export function safeClickPath(data: unknown, origin: string): string {
  const url = typeof data === 'object' && data !== null ? (data as Record<string, unknown>).url : undefined
  return isSafePath(url, origin) ? url : '/'
}

/** 本 origin 的窗口里挑一个：有焦点的 > 可见的 > 第一个。 */
export function pickWindow(windows: readonly WindowClientLike[], origin: string): WindowClientLike | undefined {
  const own = windows.filter((w) => {
    try {
      return new URL(w.url).origin === origin
    } catch {
      return false
    }
  })
  return own.find((w) => w.focused) ?? own.find((w) => w.visibilityState === 'visible') ?? own[0]
}

export interface ClickDeps {
  origin: string
  windows(): Promise<readonly WindowClientLike[]>
  openWindow(url: string): Promise<unknown>
}

export async function handleNotificationClick(data: unknown, deps: ClickDeps): Promise<void> {
  const path = safeClickPath(data, deps.origin)
  const target = pickWindow(await deps.windows(), deps.origin)
  if (target) {
    try {
      await target.focus()
      const message: NavigateMessage = { type: NAVIGATE_MESSAGE, url: path }
      target.postMessage(message)
      return
    } catch {
      /* 聚焦失败（窗口刚关、系统不允许）：开一个新的 */
    }
  }
  await deps.openWindow(`${deps.origin}${path}`)
}
