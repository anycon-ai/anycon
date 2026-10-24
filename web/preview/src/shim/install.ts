// shim 的安装（spec/web.md § 13.2 shim）。由 service worker 注入到每个导航响应的 `<head>` 里，作为页面的第一个脚本执行：
// WebSocket 替换类、拦掉 `navigator.serviceWorker.register`、`document.cookie`、保活。尽量小，不依赖任何库。
import { serviceWorkerChannel, type Channel } from './channel'
import { installCookies } from './cookies'
import { blockServiceWorkerRegistration } from './serviceWorker'
import { installWebSocket } from './websocket'

/** 每 20 秒告诉 service worker 还有页面开着，它就不会因为空闲被浏览器回收（否则经它转发的 WebSocket 会断）。 */
export const KEEPALIVE_INTERVAL_MS = 20_000

const MARK = Symbol.for('anycon.preview.shim')

export interface ShimOptions {
  /** `<script data-cookies="…">`：注入时 document.cookie 的值。 */
  cookies: string
  /** 测试用：别的通道。 */
  channel?: Channel
}

export function installShim(win: Window & typeof globalThis, options: ShimOptions): () => void {
  const marked = win as unknown as Record<symbol, unknown>
  if (marked[MARK]) return () => {}
  marked[MARK] = true
  const container = win.navigator.serviceWorker as ServiceWorkerContainer | undefined
  const channel = options.channel ?? (container ? serviceWorkerChannel(container) : { post: () => false, subscribe: () => () => {} })
  const undo = [
    blockServiceWorkerRegistration(container, (m) => win.console.warn(m)),
    installWebSocket(win, channel),
    installCookies(win.document, win.location, channel, options.cookies),
  ]
  const timer = win.setInterval(() => channel.post({ type: 'keepalive' }), KEEPALIVE_INTERVAL_MS)
  return () => {
    win.clearInterval(timer)
    for (const u of undo.reverse()) u()
    delete marked[MARK]
  }
}
