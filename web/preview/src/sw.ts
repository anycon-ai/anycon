// 预览的 service worker（`/__anycon/sw.js`，scope `/`）。逻辑都在 `./sw/`，这里只做接线（和 Web UI 的 `src/sw.ts` 同一个结构）。
// 同 origin、路径不以 `/__anycon/` 开头的请求全部走隧道；跨 origin 的不拦。
import { loadCrypto } from '@anycon/protocol'
import { parsePageMessage } from './shared/messages'
import { pickLang } from './shared/i18n'
import { loadLaunch } from './shared/launch'
import { WsBridge, type ClientLike } from './sw/bridge'
import { createConfigLoader } from './sw/config'
import { CookieStore, idbCookieStorage } from './sw/cookies'
import { HttpCache, idbIndexStore, type CacheStorageLike } from './sw/http/cache'
import { TunnelHost } from './sw/host'
import { handleRequest, type ProxyDeps } from './sw/proxy'
import { PreviewTunnel, type SocketLike } from './sw/tunnel/link'

const sw = self as unknown as ServiceWorkerGlobalScope

const selfOrigin = sw.location.origin
const wsUrl = `${sw.location.protocol === 'https:' ? 'wss:' : 'ws:'}//${sw.location.host}/v1/ws`
const lang = pickLang(sw.navigator.language)
/** Noise wasm：构建时放在 `assets/` 下（带哈希，可以长期缓存）；没注入就是固定名字。 */
const wasmUrl = new URL(typeof __PREVIEW_WASM_URL__ === 'string' && __PREVIEW_WASM_URL__ !== '' ? __PREVIEW_WASM_URL__ : '/__anycon/anycon_crypto_bg.wasm', sw.location.href)
const version = typeof __PREVIEW_VERSION__ === 'string' ? __PREVIEW_VERSION__ : 'dev'

const host = new TunnelHost({
  loadLaunch: () => loadLaunch(),
  createTunnel: (launch) =>
    new PreviewTunnel({
      launch,
      wsUrl,
      createSocket: (url) => new WebSocket(url) as unknown as SocketLike,
      crypto: () => loadCrypto({ wasm: wasmUrl }),
      origin: selfOrigin,
      clientVersion: version,
    }),
})

const cookies = new CookieStore(idbCookieStorage)
const storage: CacheStorageLike = {
  open: async (name) => {
    const cache = await sw.caches.open(name)
    return {
      match: (url) => cache.match(url, { ignoreVary: true }),
      put: (url, response) => cache.put(url, response),
      delete: (url) => cache.delete(url),
    }
  },
}
const cache = new HttpCache({ storage, index: idbIndexStore })
const config = createConfigLoader()

/** 非 HttpOnly 的 cookie 变了：把全量快照发给本 origin 的全部页面。 */
async function broadcastCookies(): Promise<void> {
  const snapshot = await cookies.visible()
  for (const client of await sw.clients.matchAll({ includeUncontrolled: true, type: 'window' })) {
    client.postMessage({ type: 'cookies', cookies: snapshot })
  }
}

const proxyDeps: ProxyDeps = {
  whenReady: (navigate) => host.whenReady(navigate),
  terminalReason: () => host.terminalReason(),
  cookies,
  cache,
  hubOrigin: async () => (await config()).hubOrigin,
  lang,
  selfOrigin,
  userAgent: sw.navigator.userAgent,
  languages: sw.navigator.languages,
  cookiesChanged: () => void broadcastCookies(),
}

const bridge = new WsBridge({
  whenReady: () => host.whenReady(false),
  cookies,
  selfOrigin,
  userAgent: sw.navigator.userAgent,
  languages: sw.navigator.languages,
})

/** 页面关了没发 close（崩溃、被杀）：定期核对，把没人要的 WebSocket 关掉。 */
let reaper: ReturnType<typeof setInterval> | undefined
function watchOrphans(): void {
  if (reaper !== undefined) return
  reaper = setInterval(() => {
    void sw.clients.matchAll({ includeUncontrolled: true, type: 'window' }).then((clients) => {
      bridge.reap(new Set(clients.map((c) => c.id)))
      if (bridge.openCount === 0 && reaper !== undefined) {
        clearInterval(reaper)
        reaper = undefined
      }
    })
  }, 30_000)
}

sw.addEventListener('install', (event) => {
  event.waitUntil(sw.skipWaiting())
})

sw.addEventListener('activate', (event) => {
  event.waitUntil(sw.clients.claim())
})

sw.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url)
  if (url.origin !== selfOrigin || url.pathname.startsWith('/__anycon/')) return
  event.respondWith(handleRequest(event.request, proxyDeps))
})

sw.addEventListener('message', (event) => {
  const message = parsePageMessage(event.data)
  const source = event.source
  if (!message || !source || !('id' in source)) return
  const client = source as Client
  switch (message.type) {
    case 'keepalive':
      return
    case 'cookies.sync':
      event.waitUntil(cookies.visible().then((snapshot) => client.postMessage({ type: 'cookies', cookies: snapshot })))
      return
    case 'cookie.set':
      event.waitUntil(
        cookies.setFromDocument(message.value, new URL(client.url).pathname).then((changed) => {
          if (changed) return broadcastCookies()
        }),
      )
      return
    default: {
      const target: ClientLike = { id: client.id, postMessage: (m, transfer) => client.postMessage(m, transfer ?? []) }
      event.waitUntil(bridge.onMessage(target, message))
      if (message.type === 'ws.open') watchOrphans()
    }
  }
})
