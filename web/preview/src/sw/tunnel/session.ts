// 一条已经握好手的隧道传输（docs/protocol.md § 11.3–11.5）：Noise 传输 + 回执 + stream 多路复用 + 流控。
//
// 这里只管「消息 ↔ 帧 ↔ stream」，不碰 hub：加密后的消息交给 `deps.send`（由连接层包成信封、走发送方规则），
// 收到的密文由连接层调 `recv`。
//
// - 每条发出的消息带当前的 `ack`（已经收到并解密的对端消息条数）。手里没有别的要发时：收到 ≥ 256 KiB 或 ≥ 32 条还没回执的
//   消息立即发纯回执，否则最迟 50 ms；收到纯回执本身不触发回执；
// - 未被回执的密文上限 2 MiB（`deps.inflightBytes()` 由发送方统计）：满了就不再发带帧的消息，纯回执不受限；
// - stream 窗口：`DATA` / `WS_MESSAGE` 的数据字节占窗口，发送方受对端给的窗口限制，收方消费后用 `WINDOW` 归还
//   （`TunnelStream.consumed`，由调用方在页面真正读走数据时调用）；
// - REQUEST 帧按 stream id 的先后进同一个队列（id 严格递增），其余帧按 stream 轮流装进消息。
import {
  TUNNEL_ACK_DELAY_MS,
  TUNNEL_ACK_EVERY_BYTES,
  TUNNEL_ACK_EVERY_MESSAGES,
  TUNNEL_CLIENT_INFLIGHT,
  TUNNEL_FRAME_HEADER,
  TUNNEL_MAX_DATA_FRAME,
  TUNNEL_MAX_MESSAGE,
  TUNNEL_MAX_STREAMS,
  TUNNEL_MAX_WS_MESSAGE,
  TUNNEL_STREAM_WINDOW,
  TUNNEL_WS_BINARY,
  TUNNEL_WS_FIN,
  TunnelFrameType,
  decodeGoaway,
  decodeMessage,
  decodeReset,
  decodeResponseHead,
  encodeMessage,
  encodeReset,
  encodeRequestHead,
  newTunnelFrame,
  parseWindow,
  parseWsClose,
  parseWsMessage,
  windowPayload,
  wsClose,
  wsMessage,
  type PreviewGoaway,
  type PreviewRequestHead,
  type PreviewResetCode,
  type PreviewResponseHead,
  type Transport,
  type TunnelFrame,
} from '@anycon/protocol'

/**
 * 密文比明文多的字节，按最坏情况算：Noise 单条消息最多 65519 字节明文，所以 128 KiB 的消息被切成 2 片，
 * 每片 2 字节长度加 16 字节标签；再加 `WHOLE` 框架头 1 字节。
 */
const CIPHER_OVERHEAD = 2 * (2 + 16) + 1
/** 攒够这么多已消费的字节才发一次 `WINDOW`（初始窗口的一半），免得每个小块一个帧。 */
const WINDOW_UPDATE_THRESHOLD = TUNNEL_STREAM_WINDOW / 2
const MAX_WINDOW = 0x7fff_ffff

export type StreamEnd =
  /** daemon 发来 `RESET`（或我们因为对端违规自己重置）。 */
  | { kind: 'reset'; code: PreviewResetCode; message?: string }
  /** 隧道本身没了（断线、对端离线、传输重置）。 */
  | { kind: 'transport'; message: string }

export interface StreamListener {
  response?(head: PreviewResponseHead): void
  /** HTTP 响应体的一段（也用于 WebSocket 升级被拒时的响应体）。 */
  data?(chunk: Uint8Array): void
  /** 对端这个方向的消息体结束。 */
  end?(): void
  /** 一条完整的 WebSocket 消息。 */
  wsMessage?(message: { binary: boolean; data: Uint8Array }): void
  /** 对端关闭了 WebSocket。 */
  wsClose?(code: number, reason: string): void
  /** 非正常结束。之后不会再有别的回调。 */
  failed?(end: StreamEnd): void
}

type OutItem =
  | { t: 'body'; data: Uint8Array; off: number }
  | { t: 'ws'; binary: boolean; data: Uint8Array; off: number }
  | { t: 'frame'; frame: TunnelFrame }

interface StreamHost {
  wake(): void
  urgent(frame: TunnelFrame): void
  drop(stream: TunnelStream): void
}

export class TunnelStream {
  readonly out: OutItem[] = []
  /** 还能发给对端多少数据字节（对端给的窗口）。 */
  sendWindow = TUNNEL_STREAM_WINDOW
  /** 对端还能发给我们多少（我们给的窗口）。 */
  recvWindow = TUNNEL_STREAM_WINDOW
  private credit = 0
  gotResponse = false
  /** 响应状态码是 101：WebSocket 已升级。 */
  upgraded = false
  localEnded = false
  remoteEnded = false
  closed = false
  assembly: { binary: boolean; chunks: Uint8Array[]; length: number } | undefined

  constructor(
    private readonly host: StreamHost,
    readonly id: number,
    readonly websocket: boolean,
    readonly listener: StreamListener,
  ) {}

  // ---- 发 ----

  sendWsMessage(binary: boolean, data: Uint8Array): void {
    if (this.closed || this.localEnded) return
    this.out.push({ t: 'ws', binary, data, off: 0 })
    this.host.wake()
  }

  /** 发 `WS_CLOSE`；对端回了 `WS_CLOSE` 之后 stream 结束。 */
  closeWs(code: number, reason: string): void {
    if (this.closed || this.localEnded) return
    this.out.push({ t: 'frame', frame: newTunnelFrame(TunnelFrameType.WsClose, this.id, wsClose(code, reason)) })
    this.host.wake()
  }

  /** 不要了：发 `RESET`，丢掉还没发出去的东西。 */
  abort(code: PreviewResetCode = 'cancelled', message?: string): void {
    if (this.closed) return
    this.host.urgent(newTunnelFrame(TunnelFrameType.Reset, this.id, encodeReset({ code, message })))
    this.host.drop(this)
  }

  // ---- 收 ----

  /** 页面真正读走了 `n` 个数据字节：攒够一半窗口就发 `WINDOW` 归还。 */
  consumed(n: number): void {
    if (this.closed || n <= 0) return
    this.credit += n
    if (this.credit >= WINDOW_UPDATE_THRESHOLD) this.flushCredit()
  }

  /** 把攒着的窗口立刻还给对端。 */
  flushCredit(): void {
    if (this.closed || this.credit === 0 || this.remoteEnded) {
      this.credit = 0
      return
    }
    this.recvWindow += this.credit
    this.host.urgent(newTunnelFrame(TunnelFrameType.Window, this.id, windowPayload(this.credit)))
    this.credit = 0
  }

  /**
   * 把这个 stream 排队的东西装进一条消息。`space` 是消息还剩的字节。返回用掉的字节。
   * 控制帧（`END` / `WS_CLOSE`）不占窗口；带数据的帧每次最多发一个，让出机会给别的 stream。
   */
  drain(frames: TunnelFrame[], space: number): number {
    let used = 0
    while (this.out.length > 0) {
      const item = this.out[0]!
      if (item.t === 'frame') {
        const need = TUNNEL_FRAME_HEADER + item.frame.payload.length
        if (space - used < need) break
        frames.push(item.frame)
        used += need
        this.out.shift()
        if (item.frame.type === TunnelFrameType.End || item.frame.type === TunnelFrameType.WsClose) this.localEnded = true
        continue
      }
      const prefix = item.t === 'ws' ? 1 : 0
      const remaining = item.data.length - item.off
      const room = space - used - TUNNEL_FRAME_HEADER - prefix
      const n = Math.min(TUNNEL_MAX_DATA_FRAME, remaining, this.sendWindow, room)
      if (n < 0 || (n === 0 && remaining > 0)) break
      const chunk = item.data.subarray(item.off, item.off + n)
      item.off += n
      this.sendWindow -= n
      const last = item.off >= item.data.length
      if (item.t === 'ws') {
        const flags = (item.binary ? TUNNEL_WS_BINARY : 0) | (last ? TUNNEL_WS_FIN : 0)
        frames.push(newTunnelFrame(TunnelFrameType.WsMessage, this.id, wsMessage(flags, chunk)))
      } else {
        frames.push(newTunnelFrame(TunnelFrameType.Data, this.id, chunk))
      }
      used += TUNNEL_FRAME_HEADER + prefix + n
      if (last) this.out.shift()
      break
    }
    return used
  }

  /** 两个方向都结束了吗。 */
  get done(): boolean {
    return this.localEnded && this.remoteEnded
  }
}

export interface SessionDeps {
  transport: Transport
  /**
   * 交出一条加密好的消息。`index` 是它在这条传输里的编号（从 0 起，不含握手），`pure` 是纯回执。
   * 连接层负责包成信封、按发送方规则写出。
   */
  send(ciphertext: Uint8Array, index: number, pure: boolean): void
  /** 当前未被回执的密文字节数（发送方统计）。 */
  inflightBytes(): number
  /** 对端累计回执：编号小于 `count` 的消息对端都收到了。 */
  onPeerAck(count: number): void
  onGoaway(goaway: PreviewGoaway): void
  /** 传输失步（解密失败、帧格式错、回执不合理）：连接层要丢掉传输、重新握手。 */
  onDesync(reason: string): void
}

export class TunnelSession implements StreamHost {
  private readonly streams = new Map<number, TunnelStream>()
  private readonly urgentFrames: TunnelFrame[] = []
  private readonly slotWaiters: Array<{ resolve: () => void; reject: (e: Error) => void }> = []
  private nextStreamId = 1
  private sentCount = 0
  private recvCount = 0
  /** 最近一次发出的 `ack` 值。 */
  private ackSent = 0
  private unackedMessages = 0
  private unackedBytes = 0
  private ackNow = false
  private ackTimer: ReturnType<typeof setTimeout> | null = null
  private flushQueued = false
  private rotation = 0
  private dead = false

  constructor(private readonly deps: SessionDeps) {}

  get openStreams(): number {
    return this.streams.size
  }

  get destroyed(): boolean {
    return this.dead
  }

  // ---- 打开 stream ----

  /**
   * 打开一个 stream（HTTP 或 WebSocket）并排队 `REQUEST` 帧。同时打开的上限 256，超了等有 stream 结束。
   * HTTP：`body` 是整个请求体（没有就空），`REQUEST`、`DATA`\*、`END` 一起排队，所以通常在同一条消息里发出；
   * WebSocket（`head.websocket`）不带请求体，也不发 `END`，之后用 `sendWsMessage` / `closeWs`。
   */
  async openStream(head: PreviewRequestHead, listener: StreamListener, body?: Uint8Array): Promise<TunnelStream> {
    while (this.streams.size >= TUNNEL_MAX_STREAMS) {
      if (this.dead) break
      await new Promise<void>((resolve, reject) => this.slotWaiters.push({ resolve, reject }))
    }
    if (this.dead) throw new Error('tunnel closed')
    const payload = encodeRequestHead({ ...head, websocket: head.websocket ?? false })
    const id = this.nextStreamId++
    const stream = new TunnelStream(this, id, head.websocket === true, listener)
    this.streams.set(id, stream)
    this.urgentFrames.push(newTunnelFrame(TunnelFrameType.Request, id, payload))
    if (!stream.websocket) {
      if (body && body.length > 0) stream.out.push({ t: 'body', data: body, off: 0 })
      stream.out.push({ t: 'frame', frame: newTunnelFrame(TunnelFrameType.End, id) })
    }
    this.wake()
    return stream
  }

  // ---- StreamHost ----

  wake(): void {
    if (this.flushQueued || this.dead) return
    this.flushQueued = true
    queueMicrotask(() => {
      this.flushQueued = false
      this.flush()
    })
  }

  urgent(frame: TunnelFrame): void {
    this.urgentFrames.push(frame)
    this.wake()
  }

  drop(stream: TunnelStream): void {
    if (stream.closed) return
    stream.closed = true
    stream.out.length = 0
    stream.assembly = undefined
    this.streams.delete(stream.id)
    this.slotWaiters.shift()?.resolve()
  }

  // ---- 收 ----

  /** 收到一条密文（一个 `data` 信封的 `WHOLE` 内容）。 */
  recv(ciphertext: Uint8Array): void {
    if (this.dead) return
    let message
    try {
      message = decodeMessage(this.deps.transport.decrypt(ciphertext))
    } catch (err) {
      this.deps.onDesync(err instanceof Error ? err.message : 'undecodable tunnel message')
      return
    }
    this.recvCount += 1
    const ack = Number(message.ack)
    if (!Number.isSafeInteger(ack) || ack > this.sentCount) {
      this.deps.onDesync(`peer acked ${message.ack} of ${this.sentCount} messages`)
      return
    }
    this.deps.onPeerAck(ack)
    const pure = message.frames.length === 0
    if (!pure) {
      this.unackedMessages += 1
      this.unackedBytes += ciphertext.length
    }
    for (const frame of message.frames) {
      if (this.dead) return
      this.handleFrame(frame)
    }
    if (!pure) {
      if (this.unackedMessages >= TUNNEL_ACK_EVERY_MESSAGES || this.unackedBytes >= TUNNEL_ACK_EVERY_BYTES) this.ackNow = true
      else this.armAckTimer()
    }
    // 发送窗口可能腾出来了（回执），也可能有 WINDOW 要还：统一在 flush 里处理
    this.wake()
  }

  private handleFrame(frame: TunnelFrame): void {
    if (frame.type === TunnelFrameType.Goaway) {
      let goaway: PreviewGoaway
      try {
        goaway = decodeGoaway(frame.payload)
      } catch {
        goaway = { code: 'shutdown' }
      }
      this.deps.onGoaway(goaway)
      return
    }
    const stream = this.streams.get(frame.stream)
    // 已经结束（或从没打开过）的 stream 的帧一律忽略，包括 RESET 之后到的 WINDOW
    if (!stream || stream.closed) return
    switch (frame.type) {
      case TunnelFrameType.Response:
        return this.onResponse(stream, frame.payload)
      case TunnelFrameType.Data:
        return this.onData(stream, frame.payload)
      case TunnelFrameType.End:
        return this.onEnd(stream)
      case TunnelFrameType.Reset: {
        let reset
        try {
          reset = decodeReset(frame.payload)
        } catch {
          reset = { code: 'internal' as const }
        }
        this.drop(stream)
        stream.listener.failed?.({ kind: 'reset', code: reset.code, message: reset.message ?? undefined })
        return
      }
      case TunnelFrameType.Window: {
        let n: number
        try {
          n = parseWindow(frame.payload)
        } catch {
          return this.protocolError(stream, 'bad window frame')
        }
        stream.sendWindow += n
        if (stream.sendWindow > MAX_WINDOW) return this.protocolError(stream, 'window overflow')
        return
      }
      case TunnelFrameType.WsMessage:
        return this.onWsMessage(stream, frame.payload)
      case TunnelFrameType.WsClose:
        return this.onWsClose(stream, frame.payload)
      default:
        return // REQUEST 只有我们发；别的类型 decodeMessage 已经跳过了
    }
  }

  private protocolError(stream: TunnelStream, message: string): void {
    stream.abort('protocol', message)
    stream.listener.failed?.({ kind: 'reset', code: 'protocol', message })
  }

  private onResponse(stream: TunnelStream, payload: Uint8Array): void {
    if (stream.gotResponse) return this.protocolError(stream, 'duplicate response')
    let head: PreviewResponseHead
    try {
      head = decodeResponseHead(payload)
    } catch {
      return this.protocolError(stream, 'bad response head')
    }
    stream.gotResponse = true
    stream.upgraded = stream.websocket && head.status === 101
    stream.listener.response?.(head)
  }

  private onData(stream: TunnelStream, payload: Uint8Array): void {
    if (!stream.gotResponse || stream.upgraded) return this.protocolError(stream, 'unexpected data')
    if (stream.remoteEnded) return this.protocolError(stream, 'data after end')
    if (payload.length > stream.recvWindow) {
      stream.abort('flow_control', 'window exceeded')
      stream.listener.failed?.({ kind: 'reset', code: 'flow_control', message: 'window exceeded' })
      return
    }
    stream.recvWindow -= payload.length
    if (payload.length > 0) stream.listener.data?.(payload)
  }

  private onEnd(stream: TunnelStream): void {
    if (!stream.gotResponse || stream.upgraded) return this.protocolError(stream, 'unexpected end')
    stream.remoteEnded = true
    stream.listener.end?.()
    // WebSocket 升级被拒：请求侧本来就没有 END，响应结束这个 stream 就结束了
    if (stream.websocket) this.drop(stream)
    else this.finishIfDone(stream)
  }

  private onWsMessage(stream: TunnelStream, payload: Uint8Array): void {
    if (!stream.upgraded || stream.remoteEnded) return this.protocolError(stream, 'unexpected websocket message')
    let parsed
    try {
      parsed = parseWsMessage(payload)
    } catch {
      return this.protocolError(stream, 'bad websocket message')
    }
    const { flags, data } = parsed
    if (data.length > stream.recvWindow) {
      stream.abort('flow_control', 'window exceeded')
      stream.listener.failed?.({ kind: 'reset', code: 'flow_control', message: 'window exceeded' })
      return
    }
    stream.recvWindow -= data.length
    const asm = (stream.assembly ??= { binary: (flags & TUNNEL_WS_BINARY) !== 0, chunks: [], length: 0 })
    asm.chunks.push(data.slice())
    asm.length += data.length
    if (asm.length > TUNNEL_MAX_WS_MESSAGE) {
      stream.abort('too_large', 'websocket message too large')
      stream.listener.failed?.({ kind: 'reset', code: 'too_large', message: 'websocket message too large' })
      return
    }
    if ((flags & TUNNEL_WS_FIN) === 0) return
    const whole = concat(asm.chunks, asm.length)
    stream.assembly = undefined
    stream.listener.wsMessage?.({ binary: asm.binary, data: whole })
  }

  private onWsClose(stream: TunnelStream, payload: Uint8Array): void {
    if (!stream.upgraded || stream.remoteEnded) return this.protocolError(stream, 'unexpected websocket close')
    let parsed
    try {
      parsed = parseWsClose(payload)
    } catch {
      parsed = { code: 1002, reason: '' }
    }
    stream.remoteEnded = true
    stream.listener.wsClose?.(parsed.code, parsed.reason)
    // 对端先关：回一个 WS_CLOSE 就结束（1005 / 1006 / 1015 不能出现在帧里）
    if (!stream.localEnded && !stream.out.some((i) => i.t === 'frame' && i.frame.type === TunnelFrameType.WsClose)) {
      const code = [1005, 1006, 1015].includes(parsed.code) ? 1000 : parsed.code
      stream.closeWs(code, '')
    }
    this.finishIfDone(stream)
  }

  private finishIfDone(stream: TunnelStream): void {
    if (stream.done) this.drop(stream)
  }

  // ---- 发 ----

  private armAckTimer(): void {
    if (this.ackTimer !== null) return
    this.ackTimer = setTimeout(() => {
      this.ackTimer = null
      this.ackNow = true
      this.flush()
    }, TUNNEL_ACK_DELAY_MS)
  }

  /** 连接层在回执释放了在途窗口之后调：把攒着的东西发出去。 */
  pump(): void {
    this.wake()
  }

  private messageCap(): number {
    const inflight = this.deps.inflightBytes()
    if (inflight === 0) return TUNNEL_MAX_MESSAGE
    return Math.min(TUNNEL_MAX_MESSAGE, TUNNEL_CLIENT_INFLIGHT - inflight - CIPHER_OVERHEAD)
  }

  private flush(): void {
    if (this.dead) return
    // 一条 stream 结束时（两个方向都发完 / 收完）要摘掉
    for (const s of [...this.streams.values()]) this.finishIfDone(s)
    for (;;) {
      const cap = this.messageCap()
      if (cap < 8 + TUNNEL_FRAME_HEADER + 1) break
      const frames = this.collect(cap)
      if (frames.length === 0) break
      if (!this.emit(frames)) return
    }
    for (const s of [...this.streams.values()]) this.finishIfDone(s)
    if (this.ackNow && this.recvCount > this.ackSent) this.emit([])
  }

  /** 装一条消息：先是紧急帧（REQUEST、WINDOW、RESET，按入队顺序），再按 stream 轮流。 */
  private collect(cap: number): TunnelFrame[] {
    const frames: TunnelFrame[] = []
    let size = 8
    while (this.urgentFrames.length > 0) {
      const f = this.urgentFrames[0]!
      const need = TUNNEL_FRAME_HEADER + f.payload.length
      if (size + need > cap) break
      frames.push(f)
      this.urgentFrames.shift()
      size += need
    }
    const ids = [...this.streams.keys()]
    if (ids.length > 0) {
      const start = this.rotation++ % ids.length
      for (let progress = true; progress; ) {
        progress = false
        for (let k = 0; k < ids.length; k++) {
          const stream = this.streams.get(ids[(start + k) % ids.length]!)
          if (!stream || stream.closed || stream.out.length === 0) continue
          const used = stream.drain(frames, cap - size)
          if (used > 0) {
            size += used
            progress = true
          }
        }
      }
    }
    return frames
  }

  private emit(frames: TunnelFrame[]): boolean {
    let ciphertext: Uint8Array
    try {
      ciphertext = this.deps.transport.encrypt(encodeMessage(this.recvCount, frames))
    } catch (err) {
      this.deps.onDesync(err instanceof Error ? err.message : 'tunnel message could not be encrypted')
      return false
    }
    const index = this.sentCount++
    this.ackSent = this.recvCount
    this.unackedMessages = 0
    this.unackedBytes = 0
    this.ackNow = false
    if (this.ackTimer) clearTimeout(this.ackTimer)
    this.ackTimer = null
    this.deps.send(ciphertext, index, frames.length === 0)
    return true
  }

  // ---- 结束 ----

  /** 传输作废（断线、对端离线、重置、终态）：所有 stream 以 `end` 失败，不再收发。 */
  destroy(end: StreamEnd): void {
    if (this.dead) return
    this.dead = true
    if (this.ackTimer) clearTimeout(this.ackTimer)
    this.ackTimer = null
    const streams = [...this.streams.values()]
    this.streams.clear()
    this.urgentFrames.length = 0
    for (const s of streams) {
      s.closed = true
      s.out.length = 0
      s.assembly = undefined
    }
    for (const w of this.slotWaiters.splice(0)) w.reject(new Error('tunnel closed'))
    for (const s of streams) s.listener.failed?.(end)
    try {
      this.deps.transport.free()
    } catch {
      // 已经释放过
    }
  }

  /** 诊断 / 测试：已发出和已收到的消息条数。 */
  get counters(): { sent: number; received: number } {
    return { sent: this.sentCount, received: this.recvCount }
  }
}

function concat(chunks: Uint8Array[], length: number): Uint8Array {
  if (chunks.length === 1) return chunks[0]!
  const out = new Uint8Array(length)
  let at = 0
  for (const c of chunks) {
    out.set(c, at)
    at += c.length
  }
  return out
}
