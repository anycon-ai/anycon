// 测试用的假 daemon：接在一个真的 `TunnelSession` 上（假传输），收 REQUEST / DATA / END，按处理函数回响应。
// 只做测试需要的 HTTP / WebSocket 部分，窗口和回执只做到「不卡住」的程度。
import {
  TunnelFrameType as T,
  decodeRequestHead,
  encodeResponseHead,
  parseWsClose,
  parseWsMessage,
  parseWindow,
  type PreviewRequestHead,
  type PreviewResetCode,
  type TunnelFrame,
} from '@anycon/protocol'
import { TunnelSession } from '../sw/tunnel/session'
import { fakeTransportPair } from './fakeNoise'
import { TestPeer, dataFrame, endFrame, frame, resetFrame, responseFrame, wsCloseFrame, wsMessageFrame } from './peer'

export interface DaemonRequest {
  stream: number
  head: PreviewRequestHead
  body: Uint8Array
}

export interface DaemonReply {
  status?: number
  headers?: [string, string][]
  /** 响应体；字符串按 UTF-8。 */
  body?: Uint8Array | string
  /** 响应体分成这些块依次发（覆盖 `body`），每块一条消息。 */
  chunks?: Array<Uint8Array | string>
  /** 响应体是 gzip：`body` 已经是压缩后的字节，帧里标 `encoding = gzip`。 */
  gzip?: boolean
  /** 不回响应，直接 RESET。 */
  reset?: PreviewResetCode
  /** 不回应（挂着）。 */
  hang?: boolean
  /** 响应头发出后不发 END（流保持打开）。 */
  keepOpen?: boolean
}

export type Handler = (req: DaemonRequest) => DaemonReply | Promise<DaemonReply>

export class TestDaemon {
  readonly session: TunnelSession
  readonly peer: TestPeer
  readonly requests: DaemonRequest[] = []
  /** WebSocket stream 上 P 发来的东西。 */
  readonly wsEvents: Array<{ stream: number; kind: 'message' | 'close'; binary?: boolean; data?: Uint8Array; code?: number; reason?: string }> = []
  readonly resets: Array<{ stream: number }> = []
  private readonly pending = new Map<number, { head: PreviewRequestHead; body: Uint8Array[] }>()
  private readonly sizes = new Map<number, number>()
  /** daemon → P 方向每个 stream 排队待发的帧，以及 P 给的窗口。 */
  private readonly outq = new Map<number, TunnelFrame[]>()
  private readonly window = new Map<number, number>()
  private released = 0
  private inflight = 0
  /** WebSocket 请求怎么处理：`accept` 回 101，`reject` 回 403。 */
  wsMode: 'accept' | 'reject' = 'accept'
  wsProtocol = ''

  constructor(private handler: Handler) {
    const { page, daemon } = fakeTransportPair()
    this.peer = new TestPeer(daemon)
    this.session = new TunnelSession({
      transport: page,
      send: (ct, index) => {
        this.sizes.set(index, ct.length)
        this.inflight += ct.length
        queueMicrotask(() => this.receive(ct))
      },
      inflightBytes: () => this.inflight,
      onPeerAck: (n) => {
        for (; this.released < n; this.released++) this.inflight -= this.sizes.get(this.released) ?? 0
      },
      onGoaway: () => {},
      onDesync: (r) => {
        throw new Error(`desync: ${r}`)
      },
    })
  }

  setHandler(handler: Handler): void {
    this.handler = handler
  }

  private receive(ct: Uint8Array): void {
    const m = this.peer.read(ct)
    for (const f of m.frames) this.onFrame(f)
  }

  private onFrame(f: TunnelFrame): void {
    switch (f.type) {
      case T.Request: {
        const head = decodeRequestHead(f.payload)
        this.pending.set(f.stream, { head, body: [] })
        if (head.websocket) {
          this.send([this.wsMode === 'accept' ? responseFrame(f.stream, 101, this.wsProtocol ? [['sec-websocket-protocol', this.wsProtocol]] : []) : responseFrame(f.stream, 403), ...(this.wsMode === 'reject' ? [dataFrame(f.stream, 'no'), endFrame(f.stream)] : [])])
        }
        return
      }
      case T.Data:
        this.pending.get(f.stream)?.body.push(f.payload)
        return
      case T.End: {
        const p = this.pending.get(f.stream)
        if (!p) return
        this.pending.delete(f.stream)
        const body = concat(p.body)
        const req = { stream: f.stream, head: p.head, body }
        this.requests.push(req)
        void Promise.resolve(this.handler(req)).then((reply) => this.reply(f.stream, reply))
        return
      }
      case T.Reset:
        this.resets.push({ stream: f.stream })
        this.pending.delete(f.stream)
        this.outq.delete(f.stream)
        return
      case T.Window:
        this.window.set(f.stream, (this.window.get(f.stream) ?? 256 * 1024) + parseWindow(f.payload))
        this.pump(f.stream)
        return
      case T.WsMessage: {
        const { flags, data } = parseWsMessage(f.payload)
        this.wsEvents.push({ stream: f.stream, kind: 'message', binary: (flags & 1) !== 0, data })
        return
      }
      case T.WsClose: {
        const { code, reason } = parseWsClose(f.payload)
        this.wsEvents.push({ stream: f.stream, kind: 'close', code, reason })
        this.send([wsCloseFrame(f.stream, code)])
        return
      }
      default:
        return
    }
  }

  private reply(stream: number, r: DaemonReply): void {
    if (r.hang) return
    if (r.reset) return this.send([resetFrame(stream, r.reset, 'test')])
    const bytes = (b: Uint8Array | string | undefined): Uint8Array => (b === undefined ? new Uint8Array(0) : typeof b === 'string' ? new TextEncoder().encode(b) : b)
    const head = frame(
      T.Response,
      stream,
      encodeResponseHead({ status: r.status ?? 200, headers: r.headers ?? [], encoding: r.gzip ? 'gzip' : 'identity' }),
    )
    const chunks = r.chunks ?? (r.body === undefined ? [] : [r.body])
    const frames: TunnelFrame[] = [head]
    for (const c of chunks) {
      const b = bytes(c)
      for (let at = 0; at < b.length; at += 32 * 1024) frames.push(dataFrame(stream, b.subarray(at, at + 32 * 1024)))
    }
    if (!r.keepOpen) frames.push(endFrame(stream))
    this.outq.set(stream, [...(this.outq.get(stream) ?? []), ...frames])
    this.pump(stream)
  }

  /** 按 P 给的窗口把排队的帧发出去：DATA 超窗口就停，等 WINDOW。 */
  private pump(stream: number): void {
    const queue = this.outq.get(stream)
    if (!queue) return
    let window = this.window.get(stream) ?? 256 * 1024
    const ready: TunnelFrame[] = []
    while (queue.length > 0) {
      const f = queue[0]!
      if (f.type === T.Data) {
        if (f.payload.length > window) break
        window -= f.payload.length
      }
      ready.push(queue.shift()!)
    }
    this.window.set(stream, window)
    if (queue.length === 0) this.outq.delete(stream)
    // 每条消息装不超过 96 KiB 的帧
    let batch: TunnelFrame[] = []
    let size = 0
    for (const f of ready) {
      if (size + f.payload.length > 96 * 1024) {
        this.send(batch)
        batch = []
        size = 0
      }
      batch.push(f)
      size += f.payload.length + 9
    }
    if (batch.length > 0) this.send(batch)
  }

  /** daemon → P 发一条消息（直接交给 session）。 */
  send(frames: TunnelFrame[]): void {
    this.session.recv(this.peer.write(frames))
  }

  wsSend(stream: number, data: string | Uint8Array): void {
    this.send([wsMessageFrame(stream, typeof data === 'string' ? 2 : 3, data)])
  }
}

function concat(chunks: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0))
  let at = 0
  for (const c of chunks) {
    out.set(c, at)
    at += c.length
  }
  return out
}

export const flushMicrotasks = async (): Promise<void> => {
  for (let i = 0; i < 20; i++) await Promise.resolve()
}
