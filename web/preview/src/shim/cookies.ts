// `document.cookie`（spec/web.md § 13.2 shim「cookie」）。
//
// 预览 origin 不用浏览器自己的 cookie 罐：浏览器会把它们随每个到 hub 的请求（`/v1/ws`、`/__anycon/*`）一起发出去，
// 而 dev app 的 cookie 是预览内容，hub 不该看到。所以这里在 `document` 上定义一个 `cookie` 访问器，背后是页面自己的
// 一份快照（非 HttpOnly 的 cookie）：读从快照读；写先改快照（页面马上能读到），再把原样的赋值字符串交给
// service worker 存进它的 cookie jar（这样页面自己设的 cookie 也会随请求发出去）；worker 的 cookie 有变化时发来全量快照。
import { CookieJar, parseCookiePairs } from '../shared/cookies'
import type { Channel } from './channel'

export function installCookies(doc: Document, loc: Location, channel: Channel, initial: string, now: () => number = Date.now): () => void {
  const jar = new CookieJar(parseCookiePairs(initial, now()))
  const original = Object.getOwnPropertyDescriptor(doc, 'cookie')
  Object.defineProperty(doc, 'cookie', {
    configurable: true,
    enumerable: true,
    get: () => jar.documentCookie(loc.pathname, now()),
    set: (value: unknown) => {
      const text = String(value)
      if (jar.set(text, loc.pathname, now(), true)) channel.post({ type: 'cookie.set', value: text })
    },
  })
  const unsubscribe = channel.subscribe((message) => {
    if (message.type === 'cookies') jar.replace(message.cookies)
  })
  // 页面可能已经有了 worker 里更新的 cookie（比如登录后跳转回来）：要一份当前的
  channel.post({ type: 'cookies.sync' })
  return () => {
    unsubscribe()
    if (original) Object.defineProperty(doc, 'cookie', original)
    else delete (doc as unknown as Record<string, unknown>).cookie
  }
}
