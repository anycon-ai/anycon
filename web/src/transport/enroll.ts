// 账号内接入的发起方（新设备 = Noise initiator）状态机：ADR-36，docs/protocol.md § 4.4。
//
// 序列（N = 本设备，D = daemon，H = hub）：
//   N → H `enroll_begin { node }` → H → N `enroll_started`
//   N ⇄ D（handshake 信封，body 是 msgpack 的 `EnrollFrame`）：
//     hello → reply { wait_approval | ready } → [cpace] → noise msg1 / msg2 / msg3
//   N → H `pair_confirm` → H → N `pair_done`（然后由 client.ts 照配对的收尾：`pair.ack` 校验、落盘）
//
// 三种方法只差 PSK 的来源：`scan` = HKDF(扫码密钥 r)，`code` / `password` = CPace 的输出。
// 这里不碰 IndexedDB、不碰身份：只产出「握手成功的传输 + 对端公钥 + pair_done」，落盘由调用方做。
import type { ClientInfo, Control, EnrollFailReason, EnrollFrame, EnrollHint, EnrollMethod, EnrollRejectReason } from '@/protocol'
import { PAIR_STEP_TIMEOUT_MS } from '@/lib/constants'
import { buildEnrollQr } from '@/lib/enrollQr'
import type { PairErrorCode } from './client'
import type { FullCrypto, Handshake, Keypair, Transport } from './crypto'
import type { PairDoneEvent, PairEvent } from './events'
import { MailboxClosed, MailboxTimeout, type Mailbox } from './mailbox'
import { bytesOfHex, decodeEnrollFrame, encodeEnrollFrame, hexOf } from './wire'

/** 接入进行到哪一步（UI 的进度条）。 */
export type EnrollStage =
  | 'connecting'
  | 'starting'
  /** `scan` 等已配对设备扫码；`code` 请求批准时等批准。 */
  | 'waiting'
  /** `code`：等用户输入 6 位码。 */
  | 'code'
  /** `password`：Argon2id（64 MiB，浏览器里约 0.1–1 s，会卡主线程，所以单独一步让 UI 先画出来）。 */
  | 'deriving'
  | 'verifying'
  | 'handshaking'
  /** `pair_confirm` → `pair_done`。 */
  | 'confirming'
  /** 等 daemon 经新传输发来的 `pair.ack` 并校验。 */
  | 'finishing'
  | 'done'

/**
 * 接入失败的原因：自己的（连接 / 校验）、hub 的 `enroll_failed.reason`、daemon 的 `rejected.reason`。
 *
 * `hub_no_reply`：`enroll_begin` 发出去之后 20 秒什么回执都没有。hub 对 `enroll_begin` 限频，而超限的 `enroll_failed { rate_limited }`
 * 和 `pair_failed` 共用「每连接每秒一条」的预算，同一秒里的第二条会被悄悄丢掉——用户看不到「请求太频繁」，只会等满 20 秒。
 * 和 `timeout`（会话到期、二维码该换了）是两回事：这里不能自动重来，不然会继续撞限频。
 */
export type EnrollErrorCode = PairErrorCode | EnrollFailReason | EnrollRejectReason | 'hub_no_reply'

export class EnrollError extends Error {
  constructor(
    readonly code: EnrollErrorCode,
    message?: string,
    readonly detail: { retryAfterMs?: number; attemptsLeft?: number } = {},
  ) {
    super(message ?? code)
    this.name = 'EnrollError'
  }
  get retryAfterMs(): number | undefined {
    return this.detail.retryAfterMs
  }
  get attemptsLeft(): number | undefined {
    return this.detail.attemptsLeft
  }
}

export interface EnrollParams {
  /** daemon 的正式设备 id（来自账号的设备列表）。CPace 的信道标识绑定它：hub 撒谎就握不上手。 */
  node: string
  method: EnrollMethod
  /** 仅 `code`：`true` = 请求已配对设备批准，`false` = 用「添加设备」里主动生成的码。 */
  approval: boolean
  /** 新设备自报的信息。未经认证，只用于批准界面。 */
  hint: EnrollHint
  /** 仅 `password`。 */
  password?: string
}

export interface EnrollHooks {
  stage(stage: EnrollStage): void
  /** hub 建好了接入会话。 */
  started?(info: { enrollId: string; expiresAtMs: number }): void
  /** 仅 `scan`：daemon 已经在等批准，二维码内容可以展示了（`anycon-enroll:1:…`，含扫码密钥，只放内存）。 */
  qr?(payload: string): void
  /**
   * 仅 `code`：需要 6 位接入码。返回 `null` = 用户放弃。上一次输错且还有机会时再调一次（`wrong = true`）。
   * 返回的 Promise 可能很久才结算（等用户输入）；接入被取消 / hub 结束会话时流程自己中断，不依赖它。
   */
  askCode?(ctx: { attemptsLeft: number | null; wrong: boolean }): Promise<string | null>
}

/** 一次接入的运行状态，供调用方取消 / 收尾。 */
export class EnrollRun {
  enrollId: string | null = null
  /** hub 已经结束了会话（`enroll_failed` / `pair_failed`）：不用再发 `enroll_cancel`。 */
  hubEnded = false
  /** `pair_confirm` 只发一次（`code` 重试时同一个会话里不重发）。 */
  confirmSent = false
  private flag = false
  private reject: (e: EnrollError) => void = () => {}
  /** 取消时以 `EnrollError('cancelled')` 拒绝。流程里的每个等待都和它赛跑。 */
  readonly cancelled: Promise<never>

  constructor() {
    this.cancelled = new Promise<never>((_, reject) => {
      this.reject = reject
    })
    // 没有人在赛跑时它被拒绝也不算未处理的 rejection
    this.cancelled.catch(() => {})
  }

  get isCancelled(): boolean {
    return this.flag
  }

  cancel(): void {
    if (this.flag) return
    this.flag = true
    this.reject(new EnrollError('cancelled'))
  }
}

export interface EnrollIo {
  crypto: FullCrypto
  keypair: Keypair
  mailbox: Mailbox<PairEvent>
  clientInfo(): ClientInfo
  /** 发控制消息给 hub；连接不可用返回 false。 */
  sendControl(msg: Control): boolean
  /** 发 `handshake` 信封给 daemon；连接不可用返回 false。 */
  sendHandshake(to: string, body: Uint8Array): boolean
  /** Argon2id 之前让 UI 先画出「正在派生密钥…」。 */
  yieldToPaint(): Promise<void>
  now(): number
}

/** 握手成功之后交给调用方的东西（调用方负责 `transport.free()`）。 */
export interface EnrollOutcome {
  enrollId: string
  done: PairDoneEvent
  transport: Transport
  remote: Uint8Array
  /** XXpsk3 第 2 条的载荷（daemon 的自我介绍），未经认证。 */
  helloPayload: Uint8Array
}

/** 接入会话的有效期（hub / daemon 都是 5 分钟）。用本机时钟从 `enroll_started` 起算，不信对端的时间戳。 */
export const ENROLL_TTL_MS = 5 * 60_000
const WAIT_SLACK_MS = 15_000

type ReplyFrame = Extract<EnrollFrame, { type: 'reply' }>
type Round = { ok: true; outcome: EnrollOutcome } | { ok: false; reply: ReplyFrame }

const encodeJson = (v: unknown): Uint8Array => new TextEncoder().encode(JSON.stringify(v))

function protocolError(what: string): EnrollError {
  return new EnrollError('handshake_failed', what)
}

class Initiator {
  private startedAt = 0

  constructor(
    private readonly io: EnrollIo,
    private readonly p: EnrollParams,
    private readonly hooks: EnrollHooks,
    private readonly run: EnrollRun,
  ) {}

  async execute(): Promise<EnrollOutcome> {
    this.hooks.stage('starting')
    if (!this.io.sendControl({ type: 'enroll_begin', node: this.p.node })) throw new EnrollError('hub_unavailable')
    const started = await this.wait(['enroll_started'], PAIR_STEP_TIMEOUT_MS, () => new EnrollError('hub_no_reply'))
    if (started.node !== this.p.node) throw protocolError('enroll_started is for a different node')
    this.run.enrollId = started.enrollId
    this.startedAt = this.io.now()
    this.hooks.started?.({ enrollId: started.enrollId, expiresAtMs: started.expiresAtMs })
    switch (this.p.method) {
      case 'scan':
        return this.scan()
      case 'code':
        return this.code()
      case 'password':
        return this.password()
    }
  }

  // ---- 三种方法 ----

  private async scan(): Promise<EnrollOutcome> {
    const secret = this.io.crypto.generateEnrollSecret()
    try {
      this.sendHello()
      await this.awaitReady(() => {
        this.hooks.stage('waiting')
        this.hooks.qr?.(buildEnrollQr(this.p.node, this.run.enrollId!, secret))
      })
      const psk = this.io.crypto.deriveEnrollScanPsk(secret, this.p.node)
      const round = await this.noise(psk)
      if (!round.ok) throw this.rejection(round.reply)
      return round.outcome
    } finally {
      secret.fill(0)
    }
  }

  private async password(): Promise<EnrollOutcome> {
    const password = this.p.password ?? ''
    if (password === '') throw new EnrollError('invalid')
    const nonce = this.sendHello()
    const ready = await this.awaitReady(() => this.hooks.stage('waiting'))
    if (!ready.salt) throw protocolError('ready reply without a salt')
    this.hooks.stage('deriving')
    await this.io.yieldToPaint()
    let prs: Uint8Array
    try {
      prs = this.io.crypto.deriveEnrollPasswordPrs(password, bytesOfHex(ready.salt))
    } catch (e) {
      throw protocolError(`could not derive the password key: ${(e as Error).message}`)
    }
    const round = await this.cpaceThenNoise(prs, 'password', nonce, ready)
    if (!round.ok) throw this.rejection(round.reply)
    return round.outcome
  }

  private async code(): Promise<EnrollOutcome> {
    const ask = this.hooks.askCode
    if (!ask) throw new Error('enroll: method "code" needs hooks.askCode')
    let attemptsLeft: number | null = null
    let wrong = false
    for (;;) {
      const nonce = this.sendHello()
      const ready = await this.awaitReady(() => this.hooks.stage('waiting'))
      this.hooks.stage('code')
      const raw = await this.raceUser(ask({ attemptsLeft, wrong }))
      if (raw === null) throw new EnrollError('cancelled')
      let normalized: string
      try {
        normalized = this.io.crypto.normalizeEnrollCode(raw)
      } catch {
        throw new EnrollError('invalid_code')
      }
      const round = await this.cpaceThenNoise(new TextEncoder().encode(normalized), 'code', nonce, ready)
      if (round.ok) return round.outcome
      const left = round.reply.attempts_left ?? undefined
      // 码输错且还有机会：同一个会话里重发 hello（新 nonce）；用完了（0）就结束
      if (round.reply.reason === 'wrong_secret' && (left === undefined || left > 0)) {
        attemptsLeft = left ?? null
        wrong = true
        continue
      }
      throw this.rejection(round.reply)
    }
  }

  // ---- 共用的几步 ----

  /** 发 `hello`，返回本轮的 nonce（CPace 的 sid 要用）。 */
  private sendHello(): Uint8Array {
    const nonce = this.io.crypto.generateEnrollNonce()
    this.send({
      type: 'hello',
      v: 1,
      method: this.p.method,
      ...(this.p.method === 'code' ? { approval: this.p.approval } : {}),
      nonce: hexOf(nonce),
      hint: this.p.hint,
    })
    return nonce
  }

  /** 等 daemon 对 `hello` 的答复直到 `ready`；中途的 `wait_approval` 只通知一次，然后等到会话到期。 */
  private async awaitReady(onWait: () => void): Promise<ReplyFrame> {
    let waiting = false
    for (;;) {
      const f = await this.waitFrame(waiting ? this.untilExpiry() : PAIR_STEP_TIMEOUT_MS)
      if (f.type !== 'reply') throw protocolError(`expected a reply to hello, got ${f.type}`)
      if (f.status === 'rejected') throw this.rejection(f)
      if (f.status === 'wait_approval') {
        if (!waiting) {
          waiting = true
          onWait()
        }
        continue
      }
      return f
    }
  }

  private async cpaceThenNoise(prs: Uint8Array, method: 'code' | 'password', nonce: Uint8Array, ready: ReplyFrame): Promise<Round> {
    const { crypto } = this.io
    let psk: Uint8Array
    try {
      if (!ready.nonce || !ready.cpace) throw protocolError('ready reply without nonce / cpace share')
      this.hooks.stage('verifying')
      const sid = crypto.enrollSid(nonce, bytesOfHex(ready.nonce))
      const ci = crypto.enrollChannelId(this.p.node, method)
      const cpace = crypto.cpace(prs, ci, sid)
      try {
        this.send({ type: 'cpace', share: hexOf(cpace.share) })
        psk = cpace.finish('initiator', bytesOfHex(ready.cpace))
      } catch (e) {
        cpace.free()
        throw e
      }
    } catch (e) {
      if (e instanceof EnrollError) throw e
      throw protocolError(`CPace failed: ${(e as Error).message}`)
    } finally {
      prs.fill(0)
    }
    return this.noise(psk)
  }

  /** XXpsk3，本设备是 initiator：msg1（空）→ msg2 → msg3（`ClientInfo`），然后 `pair_confirm` 等 `pair_done`。 */
  private async noise(psk: Uint8Array): Promise<Round> {
    this.hooks.stage('handshaking')
    let hs: Handshake | null = this.io.crypto.pairing('initiator', this.io.keypair, psk)
    psk.fill(0) // 握手对象已经拷走了它
    let transport: Transport | null = null
    try {
      this.send({ type: 'noise', msg: hexOf(hs.write()) })
      const second = await this.waitFrame(PAIR_STEP_TIMEOUT_MS)
      if (second.type === 'reply' && second.status === 'rejected') return { ok: false, reply: second }
      if (second.type !== 'noise') throw protocolError(`expected noise message 2, got ${second.type}`)
      let helloPayload: Uint8Array
      try {
        helloPayload = hs.read(bytesOfHex(second.msg))
        this.send({ type: 'noise', msg: hexOf(hs.write(encodeJson(this.io.clientInfo()))) })
      } catch (e) {
        if (e instanceof EnrollError) throw e
        throw protocolError((e as Error).message)
      }
      if (!hs.isFinished()) throw protocolError('handshake incomplete')
      const remote = hs.remotePublic()
      if (!remote) throw protocolError('no remote key')
      transport = hs.intoTransport()
      hs = null

      // PSK 到第 3 条才混进去，所以写完第 3 条不代表 daemon 认了：它要么 `pair_confirm`（hub 随后 `pair_done`），要么回 `rejected`
      this.hooks.stage('confirming')
      if (!this.run.confirmSent) {
        this.run.confirmSent = true
        if (!this.io.sendControl({ type: 'pair_confirm', peer: this.p.node })) throw new EnrollError('hub_unavailable')
      }
      const deadline = this.io.now() + PAIR_STEP_TIMEOUT_MS
      for (;;) {
        const next = await this.wait(['done', 'handshake'], Math.max(1, deadline - this.io.now()))
        if (next.t === 'handshake') {
          const f = this.tryDecode(next.body)
          if (!f) continue
          if (f.type === 'reply' && f.status === 'rejected') return { ok: false, reply: f }
          throw protocolError(`unexpected ${f.type} frame after the handshake`)
        }
        const kept = transport
        transport = null
        return { ok: true, outcome: { enrollId: this.run.enrollId!, done: next, transport: kept, remote, helloPayload } }
      }
    } finally {
      hs?.free()
      transport?.free()
    }
  }

  // ---- 等待、编解码 ----

  private send(frame: EnrollFrame): void {
    if (!this.io.sendHandshake(this.p.node, encodeEnrollFrame(frame))) throw new EnrollError('hub_unavailable')
  }

  /**
   * 解 `EnrollFrame`；不是接入帧的 `handshake` 信封直接忽略。比如重新解锁一台本机已有记录的节点（`noise_failed`）时，
   * 它的 IK 重连握手可能还有残留信封从同一个 daemon 过来——那不是接入的一部分，也不该让接入失败。
   */
  private tryDecode(body: Uint8Array): EnrollFrame | null {
    try {
      return decodeEnrollFrame(body)
    } catch {
      console.warn('anycon: ignoring a handshake envelope that is not an enrollment frame')
      return null
    }
  }

  private async waitFrame(timeoutMs: number): Promise<EnrollFrame> {
    const deadline = this.io.now() + timeoutMs
    for (;;) {
      const ev = await this.wait(['handshake'], Math.max(1, deadline - this.io.now()))
      const frame = this.tryDecode(ev.body)
      if (frame) return frame
    }
  }

  private untilExpiry(): number {
    return Math.max(1_000, this.startedAt + ENROLL_TTL_MS + WAIT_SLACK_MS - this.io.now())
  }

  private rejection(f: ReplyFrame): EnrollError {
    const reason = f.reason ?? 'invalid'
    return new EnrollError(reason, `rejected: ${reason}`, {
      ...(f.retry_after_ms != null ? { retryAfterMs: f.retry_after_ms } : {}),
      ...(f.attempts_left != null ? { attemptsLeft: f.attempts_left } : {}),
    })
  }

  /** 把邮箱里的失败事件翻成错误；非失败事件原样返回 undefined。 */
  private failure(ev: PairEvent): EnrollError | undefined {
    if (ev.t === 'enroll_failed') {
      // 上一个会话遗留的（比如我们刚取消的那个）：不是这一次的
      if (ev.enrollId && this.run.enrollId && ev.enrollId !== this.run.enrollId) return undefined
      this.run.hubEnded = true
      return new EnrollError(ev.reason)
    }
    if (ev.t === 'failed') {
      this.run.hubEnded = true
      return new EnrollError(ev.reason)
    }
    return undefined
  }

  private mapWaitError(e: unknown): unknown {
    if (e instanceof MailboxTimeout) return new EnrollError('timeout')
    if (e instanceof MailboxClosed) return this.run.isCancelled ? new EnrollError('cancelled') : new EnrollError('hub_unavailable')
    return e
  }

  /**
   * 等邮箱里下一个类型在 `types` 里的事件；hub 的失败事件变成错误，取消变成 `cancelled`。
   * `onTimeout`：等不到时抛这个错误，不给就是通用的 `timeout`。
   */
  private async wait<T extends PairEvent['t']>(types: T[], timeoutMs: number, onTimeout?: () => EnrollError): Promise<Extract<PairEvent, { t: T }>> {
    const deadline = this.io.now() + timeoutMs
    for (;;) {
      let ev: PairEvent
      try {
        ev = await Promise.race([this.io.mailbox.next([...types, 'enroll_failed', 'failed'], Math.max(1, deadline - this.io.now())), this.run.cancelled])
      } catch (e) {
        this.io.mailbox.cancelWait()
        if (e instanceof MailboxTimeout && onTimeout) throw onTimeout()
        throw this.mapWaitError(e)
      }
      const fail = this.failure(ev)
      if (fail) throw fail
      if (ev.t === 'enroll_failed' || ev.t === 'failed') continue // 别的会话的 enroll_failed：忽略，接着等
      return ev as Extract<PairEvent, { t: T }>
    }
  }

  /** 等用户（UI）的输入，同时盯着 hub：会话被结束、连接断了、被取消都要立刻中断，不能一直等一个不会来的输入。 */
  private async raceUser<T>(user: Promise<T>): Promise<T> {
    const watch = this.io.mailbox.next(['enroll_failed', 'failed'], 2 * ENROLL_TTL_MS)
    watch.catch(() => {})
    try {
      const first = await Promise.race([user.then((value) => ({ user: true as const, value })), watch.then((ev) => ({ user: false as const, ev })), this.run.cancelled])
      if (first.user) {
        this.io.mailbox.cancelWait()
        return first.value
      }
      const fail = this.failure(first.ev)
      if (fail) throw fail
      // 别的会话的 enroll_failed：继续等用户
      return await this.raceUser(user)
    } catch (e) {
      this.io.mailbox.cancelWait()
      throw this.mapWaitError(e)
    }
  }
}

/** 跑一次接入的发起方流程，返回握手成功的传输（调用方负责 `transport.free()` 和后续落盘）。 */
export function runInitiator(io: EnrollIo, params: EnrollParams, hooks: EnrollHooks, run: EnrollRun): Promise<EnrollOutcome> {
  return new Initiator(io, params, hooks, run).execute()
}
