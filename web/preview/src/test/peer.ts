// 测试用的 daemon 一侧：用同一对假传输解 P 发来的消息、造 daemon 发给 P 的消息。
import {
  TunnelFrameType,
  decodeMessage,
  encodeMessage,
  encodeResponseHead,
  encodeReset,
  newTunnelFrame,
  windowPayload,
  wsClose,
  wsMessage,
  type PreviewBodyEncoding,
  type PreviewResetCode,
  type Transport,
  type TunnelFrame,
} from '@anycon/protocol'

export interface Decoded {
  ack: number
  frames: TunnelFrame[]
  /** 密文长度。 */
  size: number
}

export class TestPeer {
  /** 我（daemon）收到的消息条数 / 发出的消息条数。 */
  received = 0
  sent = 0

  constructor(private readonly transport: Transport) {}

  /** 解一条 P 发来的密文。 */
  read(ciphertext: Uint8Array): Decoded {
    const m = decodeMessage(this.transport.decrypt(ciphertext))
    this.received += 1
    return { ack: Number(m.ack), frames: m.frames, size: ciphertext.length }
  }

  /** 造一条 daemon → P 的消息。`ack` 默认是我已经收到的条数。 */
  write(frames: TunnelFrame[], ack = this.received): Uint8Array {
    this.sent += 1
    return this.transport.encrypt(encodeMessage(ack, frames))
  }
}

export const frame = (type: TunnelFrameType, stream: number, payload: Uint8Array = new Uint8Array(0)): TunnelFrame =>
  newTunnelFrame(type, stream, payload)

export const responseFrame = (
  stream: number,
  status: number,
  headers: [string, string][] = [],
  encoding: PreviewBodyEncoding = 'identity',
): TunnelFrame => frame(TunnelFrameType.Response, stream, encodeResponseHead({ status, headers, encoding }))

export const dataFrame = (stream: number, data: Uint8Array | string): TunnelFrame =>
  frame(TunnelFrameType.Data, stream, typeof data === 'string' ? new TextEncoder().encode(data) : data)

export const endFrame = (stream: number): TunnelFrame => frame(TunnelFrameType.End, stream)

export const resetFrame = (stream: number, code: PreviewResetCode, message?: string): TunnelFrame =>
  frame(TunnelFrameType.Reset, stream, encodeReset({ code, message }))

export const windowFrame = (stream: number, increment: number): TunnelFrame =>
  frame(TunnelFrameType.Window, stream, windowPayload(increment))

export const wsMessageFrame = (stream: number, flags: number, data: Uint8Array | string): TunnelFrame =>
  frame(TunnelFrameType.WsMessage, stream, wsMessage(flags, typeof data === 'string' ? new TextEncoder().encode(data) : data))

export const wsCloseFrame = (stream: number, code: number, reason = ''): TunnelFrame =>
  frame(TunnelFrameType.WsClose, stream, wsClose(code, reason))

export const text = (bytes: Uint8Array): string => new TextDecoder().decode(bytes)
