// 页面里的 WebSocket（shim 的替换类）↔ 隧道里的 WebSocket stream（spec/web.md § 13.2 shim「WebSocket」）。
//
// 页面发 `ws.open / ws.send / ws.close`，worker 用 `client.postMessage` 回 `ws.open / ws.message / ws.close / ws.error`。
// 一个 WebSocket 一个隧道 stream；stream 的接收窗口在每段数据交给页面之后归还。
import { TUNNEL_MAX_WS_MESSAGE } from '@anycon/protocol'
import type { Lang } from '../shared/i18n'
import type { PageMessage, WorkerMessage } from '../shared/messages'
import type { CookieStore } from './cookies'
import { acceptLanguage } from './http/request'
import type { StreamListener, TunnelSession, TunnelStream } from './tunnel/session'

/** 页面一侧的收件人（service worker 里的 `Client`）。 */
export interface ClientLike {
  readonly id: string
  postMessage(message: WorkerMessage, transfer?: Transferable[]): void
}

export interface BridgeDeps {
  whenReady(): Promise<TunnelSession>
  cookies: CookieStore
  selfOrigin: string
  lang?: Lang
  userAgent?: string
  languages?: readonly string[]
}

/** 我们先关时，等对端回 `WS_CLOSE` 最多等多久。 */
const CLOSE_WAIT_MS = 5_000

type Phase = 'opening' | 'open' | 'closing' | 'closed'

interface Socket {
  client: ClientLike
  id: number
  phase: Phase
  stream?: TunnelStream
  closeTimer?: ReturnType<typeof setTimeout>
  /** 页面在升级完成之前就关了。 */
  closeRequested?: { code?: number; reason?: string }
}

export class WsBridge {
  private readonly sockets = new Map<string, Socket>()

  constructor(private readonly deps: BridgeDeps) {}

  private static key(clientId: string, id: number): string {
    return `${clientId}:${id}`
  }

  get openCount(): number {
    return this.sockets.size
  }

  async onMessage(client: ClientLike, msg: PageMessage): Promise<void> {
    switch (msg.type) {
      case 'ws.open':
        return this.open(client, msg.id, msg.url, msg.protocols)
      case 'ws.send':
        return this.send(client, msg)
      case 'ws.close':
        return this.close(client, msg.id, msg.code, msg.reason)
      default:
        return
    }
  }

  private async open(client: ClientLike, id: number, rawUrl: string, protocols: string[]): Promise<void> {
    const key = WsBridge.key(client.id, id)
    if (this.sockets.has(key)) return
    const sock: Socket = { client, id, phase: 'opening' }
    this.sockets.set(key, sock)
    let url: URL
    try {
      url = new URL(rawUrl, this.deps.selfOrigin)
      const self = new URL(this.deps.selfOrigin)
      if (url.host !== self.host || !/^(wss?|https?):$/.test(url.protocol)) throw new Error('not tunnelable')
    } catch {
      return this.fail(sock)
    }
    let session: TunnelSession
    try {
      session = await this.deps.whenReady()
    } catch {
      return this.fail(sock)
    }
    if (sock.phase !== 'opening') return // 等隧道的时候页面已经放弃了
    const cookie = await this.deps.cookies.header(url.pathname)
    const headers: [string, string][] = [
      ['origin', this.deps.selfOrigin],
      ['sec-fetch-dest', 'websocket'],
      ['sec-fetch-mode', 'websocket'],
      ['sec-fetch-site', 'same-origin'],
    ]
    if (this.deps.userAgent) headers.push(['user-agent', this.deps.userAgent])
    if (this.deps.languages?.length) headers.push(['accept-language', acceptLanguage(this.deps.languages)])
    if (protocols.length > 0) headers.push(['sec-websocket-protocol', protocols.join(', ')])
    if (cookie) headers.push(['cookie', cookie])
    const listener: StreamListener = {
      response: (head) => {
        if (head.status !== 101) {
          // 上游拒绝升级：后面跟的是普通响应，这里不要
          sock.stream?.abort('cancelled')
          return this.fail(sock)
        }
        const protocol = head.headers.find(([n]) => n.toLowerCase() === 'sec-websocket-protocol')?.[1] ?? ''
        sock.phase = 'open'
        this.post(sock, { type: 'ws.open', id, protocol })
        const closing = sock.closeRequested
        if (closing) this.close(client, id, closing.code, closing.reason)
      },
      wsMessage: ({ binary, data }) => {
        sock.stream?.consumed(data.length)
        if (binary) {
          const copy = data.slice().buffer
          this.post(sock, { type: 'ws.message', id, binary: true, data: copy }, [copy])
        } else {
          this.post(sock, { type: 'ws.message', id, binary: false, data: new TextDecoder().decode(data) })
        }
      },
      wsClose: (code, reason) => {
        this.finish(sock, code, reason, true)
      },
      failed: () => this.fail(sock),
    }
    try {
      sock.stream = await session.openStream(
        { method: 'GET', path: url.pathname + url.search, headers, websocket: true },
        listener,
      )
    } catch {
      return this.fail(sock)
    }
    if (sock.phase !== 'opening') sock.stream.abort('cancelled')
  }

  private send(client: ClientLike, msg: Extract<PageMessage, { type: 'ws.send' }>): void {
    const sock = this.sockets.get(WsBridge.key(client.id, msg.id))
    if (!sock || sock.phase !== 'open' || !sock.stream) return
    const data = typeof msg.data === 'string' ? new TextEncoder().encode(msg.data) : new Uint8Array(msg.data)
    if (data.length > TUNNEL_MAX_WS_MESSAGE) return this.close(client, msg.id, 1009, 'message too big')
    sock.stream.sendWsMessage(msg.binary, data)
  }

  private close(client: ClientLike, id: number, code?: number, reason?: string): void {
    const key = WsBridge.key(client.id, id)
    const sock = this.sockets.get(key)
    if (!sock || sock.phase === 'closing' || sock.phase === 'closed') return
    if (sock.phase === 'opening') {
      // 还没升级完：等升级结果（成功就立刻关，失败就失败）；连 stream 都没开出来就直接放弃
      sock.closeRequested = { code, reason }
      if (!sock.stream) return this.fail(sock)
      return
    }
    sock.phase = 'closing'
    sock.stream?.closeWs(code ?? 1000, reason ?? '')
    sock.closeTimer = setTimeout(() => {
      sock.stream?.abort('cancelled')
      this.finish(sock, 1006, '', false)
    }, CLOSE_WAIT_MS)
  }

  /** 页面所在的 client 没了（关了标签页 / 导航走了）：把它名下的 WebSocket 都关掉。 */
  reap(liveClientIds: ReadonlySet<string>): void {
    for (const sock of [...this.sockets.values()]) {
      if (liveClientIds.has(sock.client.id)) continue
      sock.stream?.abort('cancelled')
      this.drop(sock)
    }
  }

  /** 隧道没了或者要换：所有 WebSocket 当作异常断开。 */
  private fail(sock: Socket): void {
    if (sock.phase === 'closed') return
    this.post(sock, { type: 'ws.error', id: sock.id })
    this.finish(sock, 1006, '', false)
  }

  private finish(sock: Socket, code: number, reason: string, wasClean: boolean): void {
    if (sock.phase === 'closed') return
    this.post(sock, { type: 'ws.close', id: sock.id, code, reason, wasClean })
    this.drop(sock)
  }

  private drop(sock: Socket): void {
    sock.phase = 'closed'
    if (sock.closeTimer) clearTimeout(sock.closeTimer)
    this.sockets.delete(WsBridge.key(sock.client.id, sock.id))
  }

  private post(sock: Socket, message: WorkerMessage, transfer?: Transferable[]): void {
    try {
      sock.client.postMessage(message, transfer)
    } catch {
      // client 已经没了
    }
  }
}
