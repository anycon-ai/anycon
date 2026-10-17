// 账号内接入（ADR-36）的假 daemon：responder 一侧，按 docs/protocol.md § 4.4 和 spec/daemon.md § 15.1 处理 `EnrollFrame`。
// 用同一份 `crypto`（假的或真 wasm）算 CPace / XXpsk3，所以密码学上是真的两方：口令 / 码 / 密钥不一致就握不上手。
// 只模拟 web 端能观察到的行为：回复的顺序、失败计数的可见部分（attempts_left、backoff、locked）、批准流程；
// 悲观计数、持久化、退避的时间算法是 daemon 自己的测试守着的事。
import type { ClientInfo, EnrollFrame, EnrollHint, EnrollMethod, EnrollRejectReason } from '@/protocol'
import type { Cpace, FullCrypto, Handshake, Keypair, Transport } from '@/transport/crypto'
import { bytesOfHex, decodeEnrollFrame, hexOf, type Envelope } from '@/transport/wire'
import type { FakeSocket } from './harness'

type Reply = Extract<EnrollFrame, { type: 'reply' }>

export interface FakeEnrollSession {
  id: string
  /** 新设备（发起方）在 hub 上的 id。 */
  peer: string
  sock: FakeSocket
  method: EnrollMethod | null
  approval: boolean
  hint: EnrollHint | null
  nonceN: Uint8Array | null
  nonceD: Uint8Array | null
  cpace: Cpace | null
  hs: Handshake | null
  /** 等批准（code 请求批准 / scan）。 */
  waiting: boolean
  /** 这个会话里这个码已经被试过几次（收到新设备的 CPace 份额时 +1，不是批准 / `hello` 时）。 */
  attempts: number
  /** 收到 CPace 份额的时刻；份额之后到第 3 条只给 60 秒（docs/spec/daemon.md § 15.1 的码限额规则）。 */
  shareAt: number | null
  /** 请求批准时发给批准者的码。 */
  code: string | null
  scanSecret: Uint8Array | null
}

export interface FakeEnrollOptions {
  crypto: FullCrypto
  keypair: Keypair
  nodeId: string
  /** 把 daemon → 新设备的帧交给 hub 投递。 */
  send: (session: FakeEnrollSession, frame: EnrollFrame) => void
  /** 握手第 3 条通过：daemon 向 hub `pair_confirm`，并拿到传输和（已认证的）`ClientInfo`。 */
  confirmed: (session: FakeEnrollSession, transport: Transport, info: ClientInfo) => void
}

export class FakeEnrollDaemon {
  /** 节点口令；`null` = 没设（`password_not_set`）。 */
  password: string | null = null
  readonly salt = Uint8Array.from({ length: 16 }, (_, i) => i + 1)
  passwordState: 'enabled' | 'disabled' | 'locked' | 'backoff' = 'enabled'
  backoffMs = 90_000
  /** 关掉 = 扫码批准和请求批准的 `code` 都 `method_disabled`；接入码（approval = false）不受影响，和 daemon.md 一致的话由调用方设。 */
  approvalEnabled = true
  /** 「添加设备」主动生成的码（`approval = false` 用）；`null` = 没有（`no_code`）。 */
  activeCode: string | null = null
  maxAttempts = 3
  /** 份额 → 第 3 条的窗口（daemon.md § 15.1）。 */
  shareWindowMs = 60_000
  /** 收到 CPace 份额时（码在 `ready` 之后被顶掉 / 过期 / 用完）直接 `rejected`，不再握手。 */
  refuseShare: EnrollRejectReason | null = null
  /** 直接回 `rejected { busy }`。 */
  busy = false

  // ---- 观察 ----
  readonly hellos: Array<{ method: EnrollMethod; approval: boolean | undefined; nonce: string; hint: EnrollHint }> = []
  /** 收到的全部帧（含 hello），按到达顺序。 */
  readonly received: EnrollFrame[] = []
  /** 连续口令失败次数（发出 CPace 份额时 +1，第 3 条通过清零——悲观计数）。 */
  failures = 0
  /** 握手第 3 条通过的次数。 */
  completed = 0
  /** 最近一次通过的握手里，新设备带来的 `ClientInfo`。 */
  lastClient: ClientInfo | null = null

  session: FakeEnrollSession | null = null

  constructor(private readonly o: FakeEnrollOptions) {}

  begin(id: string, peer: string, sock: FakeSocket): void {
    this.session = { id, peer, sock, method: null, approval: false, hint: null, nonceN: null, nonceD: null, cpace: null, hs: null, waiting: false, attempts: 0, shareAt: null, code: null, scanSecret: null }
  }

  end(): void {
    this.session?.cpace?.free()
    this.session?.hs?.free()
    this.session = null
  }

  /** hub 把 N 发来的 `handshake` 信封交给 daemon。 */
  onEnvelope(env: Envelope): void {
    const s = this.session
    if (!s) return
    const frame = decodeEnrollFrame(env.body)
    this.received.push(frame)
    switch (frame.type) {
      case 'hello':
        return this.onHello(s, frame)
      case 'cpace':
        return this.onCpace(s, frame.share)
      case 'noise':
        return this.onNoise(s, frame.msg)
      case 'reply':
        return
    }
  }

  // ---- 批准（测试里扮演「已配对设备」）----

  /** `node.enroll_approve`（code）：给这个请求一个码，daemon 回 `ready`。返回码。 */
  approveCode(code = '482913'): string {
    const s = this.need()
    s.code = code
    s.waiting = false
    this.ready(s, 'code', new TextEncoder().encode(code))
    return code
  }

  /** `node.enroll_approve`（scan）：带二维码里的密钥，daemon 回 `ready`。 */
  approveScan(secret: Uint8Array): void {
    const s = this.need()
    s.scanSecret = Uint8Array.from(secret)
    s.waiting = false
    s.nonceD = this.o.crypto.generateEnrollNonce()
    s.hs = this.o.crypto.pairing('responder', this.o.keypair, this.o.crypto.deriveEnrollScanPsk(secret, this.o.nodeId))
    this.reply(s, { status: 'ready', nonce: hexOf(s.nonceD) })
  }

  deny(): void {
    this.reject(this.need(), 'denied')
  }

  // ---- 内部 ----

  private need(): FakeEnrollSession {
    if (!this.session) throw new Error('no enroll session')
    return this.session
  }

  private reply(s: FakeEnrollSession, r: Omit<Reply, 'type'>): void {
    this.o.send(s, { type: 'reply', ...r })
  }

  private reject(s: FakeEnrollSession, reason: EnrollRejectReason, extra: Partial<Pick<Reply, 'retry_after_ms' | 'attempts_left'>> = {}): void {
    this.reply(s, { status: 'rejected', reason, ...extra })
  }

  private onHello(s: FakeEnrollSession, f: Extract<EnrollFrame, { type: 'hello' }>): void {
    s.method = f.method
    s.approval = f.approval === true
    s.hint = f.hint
    s.nonceN = bytesOfHex(f.nonce)
    s.cpace?.free()
    s.cpace = null
    s.shareAt = null
    this.hellos.push({ method: f.method, approval: f.approval, nonce: f.nonce, hint: f.hint })
    if (this.busy) return this.reject(s, 'busy')

    if (f.method === 'password') {
      if (this.password === null) return this.reject(s, 'password_not_set')
      if (this.passwordState === 'disabled') return this.reject(s, 'method_disabled')
      if (this.passwordState === 'locked') return this.reject(s, 'locked')
      if (this.passwordState === 'backoff') return this.reject(s, 'backoff', { retry_after_ms: this.backoffMs })
      this.failures += 1 // 悲观计数：发出份额之前先记一次失败
      return this.ready(s, 'password', this.o.crypto.deriveEnrollPasswordPrs(this.password, this.salt))
    }
    if (f.method === 'scan') {
      if (!this.approvalEnabled) return this.reject(s, 'method_disabled')
      s.waiting = true
      return this.reply(s, { status: 'wait_approval' })
    }
    // code
    if (f.approval === true) {
      if (!this.approvalEnabled) return this.reject(s, 'method_disabled')
      if (s.code === null) {
        s.waiting = true
        return this.reply(s, { status: 'wait_approval' })
      }
      // 已经批准过的请求（重试）：直接 ready，同一个码
      return this.ready(s, 'code', new TextEncoder().encode(s.code))
    }
    if (this.activeCode === null || s.attempts >= this.maxAttempts) return this.reject(s, 'no_code')
    s.code = this.activeCode
    return this.ready(s, 'code', new TextEncoder().encode(this.activeCode))
  }

  /** 回 `ready`（带 D 本轮的 nonce 和 CPace 份额），记下 CPace 状态等 N 的份额。 */
  private ready(s: FakeEnrollSession, method: 'code' | 'password', prs: Uint8Array): void {
    const { crypto, nodeId } = this.o
    s.nonceD = crypto.generateEnrollNonce()
    s.cpace = crypto.cpace(prs, crypto.enrollChannelId(nodeId, method), crypto.enrollSid(s.nonceN!, s.nonceD))
    this.reply(s, {
      status: 'ready',
      nonce: hexOf(s.nonceD),
      cpace: hexOf(s.cpace.share),
      ...(method === 'password' ? { salt: hexOf(this.salt) } : {}),
    })
  }

  private onCpace(s: FakeEnrollSession, share: string): void {
    if (!s.cpace) return
    if (this.refuseShare) {
      s.cpace.free()
      s.cpace = null
      return this.reject(s, this.refuseShare)
    }
    // 码的次数在这里扣（每个份额最多换来一次第 3 条的校验），批准和 `hello` 都不扣；份额之后的 60 秒计时也从这里开始
    if (s.method === 'code') s.attempts += 1
    s.shareAt = Date.now()
    try {
      const psk = s.cpace.finish('responder', bytesOfHex(share))
      s.cpace = null
      s.hs?.free()
      s.hs = this.o.crypto.pairing('responder', this.o.keypair, psk)
      psk.fill(0)
    } catch {
      this.reject(s, 'invalid')
    }
  }

  private onNoise(s: FakeEnrollSession, msg: string): void {
    const hs = s.hs
    if (!hs) return this.reject(s, 'invalid')
    // 份额之后超过 60 秒才来的握手消息：daemon 已经结束了这一轮，什么都不回（web 要是有这种停顿，会卡到自己的超时）
    if (s.shareAt !== null && Date.now() - s.shareAt > this.shareWindowMs) return
    try {
      const payload = hs.read(bytesOfHex(msg))
      if (hs.isMyTurn()) {
        // 消息 2：daemon 的自我介绍（未认证）
        const hello = new TextEncoder().encode(JSON.stringify({ name: 'test-node', version: '9.9.9' }))
        this.o.send(s, { type: 'noise', msg: hexOf(hs.write(hello)) })
        return
      }
      if (!hs.isFinished()) return
      // 消息 3 通过：PSK 一致
      this.failures = 0
      this.completed += 1
      this.lastClient = JSON.parse(new TextDecoder().decode(payload)) as ClientInfo
      const transport = hs.intoTransport()
      s.hs = null
      this.o.confirmed(s, transport, this.lastClient)
    } catch {
      // 消息 3 校验失败：PSK 不一致（码 / 口令错，或有中间人）
      s.hs?.free()
      s.hs = null
      if (s.method === 'password') return this.reject(s, 'wrong_secret')
      if (s.method === 'code') return this.reject(s, 'wrong_secret', { attempts_left: Math.max(0, this.maxAttempts - s.attempts) })
      return this.reject(s, 'wrong_secret')
    }
  }
}
