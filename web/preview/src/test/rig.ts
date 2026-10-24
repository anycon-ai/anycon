// 隧道连接层的测试台：假 WebSocket + 假 hub（按信封说话）+ 假 daemon（真的握手、真的帧编解码，只是密码学是假的）。
import {
  bytesOfHex,
  controlEnvelope,
  decodeControl,
  decodeEnvelope,
  decodeFrame,
  encodeControl,
  encodeEnvelope,
  encodeFrames,
  handshakeAccept,
  handshakeReject,
  hexOf,
  makeEnvelope,
  parseHandshakeInit,
  previewPrologue,
  resetBody,
  type Control,
  type Envelope,
  type Handshake,
  type PreviewHello,
  type PreviewLaunch,
  type TunnelFrame,
} from '@anycon/protocol'
import { PreviewTunnel, type LinkDeps, type SocketLike } from '../sw/tunnel/link'
import { createFakePreviewCrypto, fakePreviewResponder } from './fakeNoise'
import { TestPeer } from './peer'

export class FakeSocket implements SocketLike {
  binaryType = 'blob'
  readyState = 0
  onopen: ((ev: unknown) => void) | null = null
  onmessage: ((ev: { data: unknown }) => void) | null = null
  onclose: ((ev: { code: number; reason?: string }) => void) | null = null
  onerror: ((ev: unknown) => void) | null = null
  /** P 发出的字节。 */
  readonly sent: Uint8Array[] = []
  closedByClient: { code?: number; reason?: string } | null = null
  /** 让 `send` 失败（模拟连接不可写）。 */
  failWrites = false

  constructor(readonly url: string) {}

  send(data: Uint8Array): void {
    if (this.failWrites) throw new Error('socket not writable')
    this.sent.push(data)
  }
  close(code?: number, reason?: string): void {
    this.readyState = 3
    this.closedByClient = { code, reason }
  }

  // ---- hub 一侧 ----
  open(): void {
    this.readyState = 1
    this.onopen?.({})
  }
  deliver(bytes: Uint8Array): void {
    const copy = bytes.slice()
    this.onmessage?.({ data: copy.buffer })
  }
  serverClose(code: number): void {
    this.readyState = 3
    this.onclose?.({ code })
  }
}

export const NODE_ID = '0123456789abcdef0123456789abcdef'
export const SELF_ID = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
export const DAEMON_PUBLIC = new Uint8Array(32).fill(0x42)

export function makeLaunch(over: Partial<PreviewLaunch> = {}): PreviewLaunch {
  return {
    v: 1,
    node: NODE_ID,
    node_public: hexOf(DAEMON_PUBLIC),
    label: 'abcdefghijklmnopqrstuvwxyz',
    port: 5173,
    ticket_id: '00112233445566778899aabbccddeeff',
    psk: '11'.repeat(32),
    token: 'preview-token',
    expires_at_ms: Date.now() + 3_600_000,
    path: '/',
    ...over,
  }
}

export interface RigOptions {
  launch?: PreviewLaunch
  rng?: () => number
  crypto?: LinkDeps['crypto']
}

/** 一个 `PreviewTunnel` 加上它周围的一切。 */
export class Rig {
  readonly sockets: FakeSocket[] = []
  readonly launch: PreviewLaunch
  readonly tunnel: PreviewTunnel
  /** daemon 一侧的握手和传输（每次握手重建）。 */
  peer: TestPeer | null = null
  private responder: Handshake | null = null
  /** daemon 回给 P 的 hello。 */
  hello: (launch: PreviewLaunch) => PreviewHello = (l) => ({ port: l.port, expires_at_ms: l.expires_at_ms })
  /** daemon 的握手回复：`accept`（默认）、`reject:<原因>`、`garbage`（接受但 Noise 部分是乱码）或 `silent`（不回）。 */
  handshakeMode: 'accept' | 'silent' | 'garbage' | `reject:${string}` = 'accept'
  daemonPublic = DAEMON_PUBLIC
  handshakes = 0
  /** daemon 收到的 P 的 data 信封（解密之前）。 */
  readonly dataEnvelopes: Envelope[] = []
  private hubSeq = 0
  private daemonSeq = 0

  constructor(opts: RigOptions = {}) {
    this.launch = opts.launch ?? makeLaunch()
    const crypto = createFakePreviewCrypto()
    this.tunnel = new PreviewTunnel({
      launch: this.launch,
      wsUrl: 'wss://abcdefghijklmnopqrstuvwxyz.preview.example/v1/ws',
      createSocket: (url) => {
        const s = new FakeSocket(url)
        this.sockets.push(s)
        return s
      },
      crypto: opts.crypto ?? (() => Promise.resolve(crypto)),
      rng: opts.rng ?? (() => 0.5),
      origin: 'https://abcdefghijklmnopqrstuvwxyz.preview.example',
    })
  }

  get socket(): FakeSocket {
    return this.sockets.at(-1)!
  }

  /** 把 P 从某个 socket 发出的信封解出来。 */
  sent(sock: FakeSocket = this.socket): Envelope[] {
    return sock.sent.map((b) => decodeEnvelope(b))
  }

  sentControls(sock: FakeSocket = this.socket): Control[] {
    return this.sent(sock)
      .filter((e) => e.kind === 'control')
      .map((e) => decodeControl(e.body))
  }

  // ---- hub 说话 ----

  hubControl(ctrl: Control, sock: FakeSocket = this.socket): void {
    sock.deliver(encodeEnvelope(controlEnvelope('', ++this.hubSeq, ctrl)))
  }

  /** 连上、认证通过。 */
  async connect(peersOnline: string[] = [NODE_ID], sock: FakeSocket = this.socket): Promise<void> {
    sock.open()
    this.hubControl({ type: 'auth_ok', device_id: SELF_ID, server_time_ms: Date.now(), peers_online: peersOnline }, sock)
    await settle()
  }

  /** 走完握手到 ready（假 daemon 自动回复）。 */
  async ready(): Promise<void> {
    this.tunnel.start()
    await this.connect()
    await this.answerHandshake()
    await settle()
  }

  /** daemon 读 P 最近一条握手信封并按 `handshakeMode` 回复。 */
  async answerHandshake(sock: FakeSocket = this.socket): Promise<void> {
    await settle()
    const hs = [...this.sent(sock)].reverse().find((e) => e.kind === 'handshake')
    if (!hs) throw new Error('P has not sent a handshake')
    this.handshakes += 1
    if (this.handshakeMode === 'silent') return
    if (this.handshakeMode === 'garbage') {
      this.daemonEnvelope(sock, 'handshake', handshakeAccept(Uint8Array.from({ length: 24 }, (_, i) => i * 7)))
      await settle()
      return
    }
    if (this.handshakeMode.startsWith('reject:')) {
      this.daemonEnvelope(sock, 'handshake', handshakeReject(this.handshakeMode.slice(7)))
      await settle()
      return
    }
    const { ticketId, noise } = parseHandshakeInit(hs.body)
    this.responder = fakePreviewResponder(this.daemonPublic, bytesOfHex(this.launch.psk), previewPrologue(this.launch.label, ticketId))
    this.responder.read(noise)
    const reply = this.responder.write(new TextEncoder().encode(JSON.stringify(this.hello(this.launch))))
    this.peer = new TestPeer(this.responder.intoTransport())
    this.daemonEnvelope(sock, 'handshake', handshakeAccept(reply))
    await settle()
  }

  /** daemon 发一个信封给 P（经 hub）。 */
  daemonEnvelope(sock: FakeSocket, kind: Envelope['kind'], body: Uint8Array): void {
    sock.deliver(encodeEnvelope(makeEnvelope(NODE_ID, SELF_ID, kind, ++this.daemonSeq, body)))
  }

  /** daemon 发一条隧道消息。 */
  daemonSend(frames: TunnelFrame[], ack?: number, sock: FakeSocket = this.socket): void {
    const ct = this.peer!.write(frames, ack)
    this.daemonEnvelope(sock, 'data', encodeFrames(ct, 0)[0]!)
  }

  daemonReset(sock: FakeSocket = this.socket): void {
    this.daemonEnvelope(sock, 'data', resetBody())
  }

  /** P 发出、还没读过的 data 信封，解成帧。`since` 之前的跳过。 */
  readData(sock: FakeSocket = this.socket, since = 0): Array<{ env: Envelope; ack: number; frames: TunnelFrame[] }> {
    const out: Array<{ env: Envelope; ack: number; frames: TunnelFrame[] }> = []
    for (const env of this.sent(sock).slice(since)) {
      if (env.kind !== 'data') continue
      const frame = decodeFrame(env.body)
      if (frame.kind !== 'whole') continue
      const m = this.peer!.read(frame.data)
      out.push({ env, ack: m.ack, frames: m.frames })
    }
    return out
  }
}

/** 让挂着的 Promise / 微任务都跑完。 */
export async function settle(): Promise<void> {
  for (let i = 0; i < 8; i++) await Promise.resolve()
}

export { encodeControl }
