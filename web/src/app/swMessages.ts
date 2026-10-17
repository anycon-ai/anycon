// service worker → 页面的消息：点了系统通知后，如果这个 origin 已经有窗口开着，service worker 聚焦它并发一条 `NAVIGATE_MESSAGE`，
// 页面做站内跳转（不整页刷新：Noise 会话、草稿都在页面内存里）。消息按不可信输入处理：同源、形状对、只认站内绝对路径。
import { isSafePath } from '@/lib/safePath'
import { NAVIGATE_MESSAGE } from '@/sw/click'

export interface MessageSource {
  addEventListener(type: 'message', listener: (event: MessageEvent<unknown>) => void): void
  removeEventListener(type: 'message', listener: (event: MessageEvent<unknown>) => void): void
}

/** 要跳去哪里；不是我们的消息、路径不合格返回 null。 */
export function navigateTargetOf(data: unknown, origin: string = location.origin): string | null {
  if (typeof data !== 'object' || data === null) return null
  const m = data as Record<string, unknown>
  return m.type === NAVIGATE_MESSAGE && isSafePath(m.url, origin) ? m.url : null
}

/** 登记监听；返回取消函数。没有 service worker 的环境（开发服务器、旧浏览器）什么都不做。 */
export function listenForSwMessages(navigate: (to: string) => void, source: MessageSource | undefined = typeof navigator !== 'undefined' ? navigator.serviceWorker : undefined, origin: string = location.origin): () => void {
  if (!source) return () => undefined
  const onMessage = (event: MessageEvent<unknown>): void => {
    if (event.origin !== origin) return
    const to = navigateTargetOf(event.data, origin)
    if (to) navigate(to)
  }
  source.addEventListener('message', onMessage)
  return () => source.removeEventListener('message', onMessage)
}
