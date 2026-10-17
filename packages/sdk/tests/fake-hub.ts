/**
 * 最小 fake hub：真实 WebSocket（`ws` 服务端），实现 Auth / PairBegin（按 match_id 登记，ADR-29）/ PairJoin /
 * PairMatched / PairConfirm / PairDone，并在两个已配对（或配对撮合中）的设备之间原样转发 handshake / data / notify
 * 信封。超过 `maxEnvelopeBytes` 的信封回 `dropped { too_large }`（和真实 hub 一致）。
 * 不碰密码学，也不解析 payload。控制消息的编解码用 `@anycon/protocol`。
 *
 * 顺序保证（粘住规则，docs/protocol.md § 3.1，与 crates/hub 的 `ws/sticky.rs` 同一个模型）：对 `(from, to)` 的
 * data / notify 只有「转发」或「退回 `backpressure { to, seq }`」，从不静默丢。每对 `(from, to)` 记一个**被退回 seq 的
 * 有序集合**：集合为空时按缓冲 / 预算裁决；集合非空时只有 seq 等于集合最小值的信封才会被尝试转发（转发成功就从集合里
 * 去掉），其它任何 seq 一律退回并加入集合。所以发送方不管收齐回执没有，hub 都不会放行乱序的信封。
 * 发出一封 handshake 并被转发会清掉整个集合（发送方换了传输，被退回的那些它已经不要了）；任一方断开同样清除。
 * `interceptor` 返回 `'backpressure'` 表示「这一封的目标缓冲满了」，`'rate_limited'` 表示「超了速率预算」
 * （同样退回，另外回一条 `rate_limited { retry_after_ms }`，回执每秒至多一条）。
 *
 * 测试可以让它「作恶」：`forgePairDoneOnConfirm`（joiner 一确认就伪造 PairDone，host 从未确认）、
 * `tamperPairDonePeer`（PairDone 里报一个假的节点 id）。
 */
import { randomBytes } from 'node:crypto'
import type { AddressInfo } from 'node:net'

import {
  controlBody,
  controlEnvelope,
  decodeEnvelope,
  encodeEnvelope,
  type Control,
  type Envelope,
} from '@anycon/protocol'
import { WebSocketServer, type WebSocket } from 'ws'

interface Device {
  id: string
  token: string
  kind: string
  revoked: boolean
  paired: Set<string>
}

interface Pairing {
  host: Conn
  joiner: Conn
  confirmed: Set<string>
}

interface Conn {
  ws: WebSocket
  id: string
  authed: boolean
  temp: boolean
  pairing?: Pairing
  seq: number
  /** 上一条「回执」（dropped / rate_limited）发出的时间：真实 hub 每连接每秒至多一条。 */
  lastReceiptAt: number
}

/**
 * 对每封要转发的信封做裁决：`drop` 静默丢弃（模拟「坏 hub」，正常 hub 从不这样）；`backpressure` = 目标缓冲满，
 * 退回并粘住；`rate_limited` = 超速率预算，同样退回并粘住，另外回一条 `rate_limited { retry_after_ms }`
 * （回执每连接每秒至多一条）。被粘住的 seq 的重发也会再过一遍裁决（缓冲可能还是满的）。
 */
export type Interceptor = (
  env: Envelope,
  from: Conn,
) => 'drop' | 'backpressure' | 'rate_limited' | undefined

export interface LoggedEnvelope {
  from: string
  to: string
  kind: Envelope['kind']
  seq: number
  size: number
}

const rid = () => randomBytes(16).toString('hex')

export class FakeHub {
  readonly #wss: WebSocketServer
  readonly devices = new Map<string, Device>()
  readonly #byToken = new Map<string, Device>()
  readonly #conns = new Map<string, Conn>()
  readonly #pending = new Map<string, Conn>()
  /** 转发过的信封（只记头部和长度，不记内容）。 */
  readonly log: LoggedEnvelope[] = []
  /** 每个设备累计认证成功的次数（重连断言用）。 */
  readonly authCount = new Map<string, number>()
  interceptor: Interceptor | undefined
  /** 作恶：joiner 一发 pair_confirm 就立刻给它伪造 PairDone（host 没确认过，甚至根本没握手成功）。 */
  forgePairDoneOnConfirm = false
  /** 收到的全部控制消息（含 pair_join 的 match_id），断言「短码从没到过 hub」用。 */
  readonly controlLog: Control[] = []
  /** 把发给 joiner 的 PairDone 推迟这么久（模拟 daemon 的 pair.ack 先于 PairDone 到达）。 */
  delayJoinerPairDoneMs = 0
  /** 作恶：PairDone 里 joiner 看到的节点 id（参数是真实的 host id）。 */
  tamperPairDonePeer: ((hostId: string) => string) | undefined
  /** PairDone 之后 hub 给谁发过 PeerOnline（接收方 id），断言「PeerOnline 先于 pair.ack」用。 */
  readonly peerOnlineAfterPair: string[] = []
  /** 到达 hub 的每一封 data / notify（不管转发还是退回），按到达顺序；只记头部。 */
  readonly arrivals: Array<{
    from: string
    to: string
    kind: Envelope['kind']
    seq: number
    at: number
  }> = []
  /** 被退回（`backpressure`）的信封，按退回顺序。 */
  readonly bounced: Array<{ from: string; to: string; seq: number; at: number }> = []
  /** 关掉粘住规则（只用来写「没有粘住规则的 hub 会怎样」的对照用例）。默认开，和真实 hub 一致。 */
  stickyRule = true
  /** `(from>to)` → 被退回、还没重发成功的 seq 的集合（只放行最小的那个）。 */
  readonly #sticky = new Map<string, Set<number>>()
  /** 被丢弃的信封（超限 / 未配对），只记头部。 */
  readonly dropped: Array<{ from: string; seq: number; reason: 'too_large' | 'not_paired' }> = []
  /** 回执最小间隔（真实 hub 是 1 秒；测试里想一封一回可以调成 0）。 */
  receiptIntervalMs = 1000
  /** `rate_limited` 回执里的 retry_after_ms。 */
  retryAfterMs = 200
  maxEnvelopeBytes = 256 * 1024
  readonly url: string
  /** 收到的第一帧不是合法 auth 的次数。 */
  badFirstFrames = 0

  private constructor(wss: WebSocketServer, port: number) {
    this.#wss = wss
    this.url = `http://127.0.0.1:${port}`
    wss.on('connection', (ws) => this.#onConnection(ws))
  }

  static start(): Promise<FakeHub> {
    return new Promise((resolve, reject) => {
      const wss = new WebSocketServer({ host: '127.0.0.1', port: 0, path: '/v1/ws' })
      wss.once('listening', () => resolve(new FakeHub(wss, (wss.address() as AddressInfo).port)))
      wss.once('error', reject)
    })
  }

  stop(): Promise<void> {
    for (const client of this.#wss.clients) client.terminate()
    return new Promise((resolve) => this.#wss.close(() => resolve()))
  }

  /** 预先登记一台已有 token 的设备（比如 daemon）。 */
  registerDevice(kind = 'daemon'): { id: string; token: string } {
    const device: Device = {
      id: rid(),
      token: `tok_${rid()}`,
      kind,
      revoked: false,
      paired: new Set(),
    }
    this.devices.set(device.id, device)
    this.#byToken.set(device.token, device)
    return { id: device.id, token: device.token }
  }

  isOnline(id: string): boolean {
    return this.#conns.get(id)?.authed === true
  }

  revoke(id: string): void {
    const device = this.devices.get(id)
    if (device) device.revoked = true
    this.closeConnection(id, 4004)
  }

  /** 服务端直接断开（模拟网络掉线，客户端看到 1006）。 */
  dropConnection(id: string): void {
    this.#conns.get(id)?.ws.terminate()
  }

  closeConnection(id: string, code: number): void {
    this.#conns.get(id)?.ws.close(code, 'test')
  }

  /** 对某设备发一条控制消息（测试用）。 */
  sendControl(id: string, ctrl: Control): void {
    const conn = this.#conns.get(id)
    if (conn) this.#sendControl(conn, ctrl)
  }

  /** 回执（`dropped`、`rate_limited`）：和真实 hub 一样每连接每秒至多一条，其余不回。 */
  #receipt(conn: Conn, ctrl: Control): void {
    const now = Date.now()
    if (now - conn.lastReceiptAt < this.receiptIntervalMs) return
    conn.lastReceiptAt = now
    this.#sendControl(conn, ctrl)
  }

  #sendControl(conn: Conn, ctrl: Control): void {
    conn.seq += 1
    if (conn.ws.readyState === conn.ws.OPEN) {
      conn.ws.send(encodeEnvelope({ ...controlEnvelope('', conn.seq, ctrl), to: conn.id }))
    }
  }

  #onConnection(ws: WebSocket): void {
    const conn: Conn = { ws, id: '', authed: false, temp: false, seq: 0, lastReceiptAt: 0 }
    const authTimer = setTimeout(() => ws.close(4005, 'auth timeout'), 5000)
    ws.on('message', (data, isBinary) => {
      if (!isBinary) return
      const bytes = new Uint8Array(data as Buffer)
      let env: Envelope
      try {
        env = decodeEnvelope(bytes)
      } catch {
        return
      }
      if (!conn.authed) {
        clearTimeout(authTimer)
        this.#onAuth(conn, env)
        return
      }
      if (env.kind === 'control') {
        try {
          this.#onControl(conn, controlBody(env))
        } catch {
          // ignore
        }
        return
      }
      this.#route(conn, env, bytes)
    })
    ws.on('close', () => {
      clearTimeout(authTimer)
      this.#onClose(conn)
    })
    ws.on('error', () => {})
  }

  #onAuth(conn: Conn, env: Envelope): void {
    let ctrl: Control | undefined
    try {
      ctrl = env.kind === 'control' ? controlBody(env) : undefined
    } catch {
      ctrl = undefined
    }
    if (!ctrl || ctrl.type !== 'auth') {
      this.badFirstFrames++
      this.#sendControl(conn, { type: 'auth_fail', reason: 'protocol' })
      conn.ws.close(1008, 'protocol')
      return
    }
    if (ctrl.protocol_version !== 1) {
      this.#sendControl(conn, { type: 'auth_fail', reason: 'version' })
      conn.ws.close(1008, 'version')
      return
    }
    let device: Device | undefined
    if (ctrl.device_token) {
      device = this.#byToken.get(ctrl.device_token)
      if (!device) {
        this.#sendControl(conn, { type: 'auth_fail', reason: 'invalid' })
        conn.ws.close(1008, 'invalid')
        return
      }
      if (device.revoked) {
        this.#sendControl(conn, { type: 'auth_fail', reason: 'revoked' })
        conn.ws.close(1008, 'revoked')
        return
      }
      conn.id = device.id
    } else {
      conn.id = rid()
      conn.temp = true
    }
    const previous = this.#conns.get(conn.id)
    if (previous && previous !== conn) previous.ws.close(4001, 'superseded')
    conn.authed = true
    this.#conns.set(conn.id, conn)
    this.authCount.set(conn.id, (this.authCount.get(conn.id) ?? 0) + 1)
    const peers = device ? [...device.paired].filter((id) => this.isOnline(id)) : []
    this.#sendControl(conn, {
      type: 'auth_ok',
      device_id: conn.id,
      server_time_ms: Date.now(),
      peers_online: peers,
    })
    for (const id of peers) {
      const other = this.#conns.get(id)
      if (other) this.#sendControl(other, { type: 'peer_online', device: conn.id })
    }
  }

  #onControl(conn: Conn, ctrl: Control): void {
    this.controlLog.push(ctrl)
    // 临时连接只能 pair_join / pair_confirm，其余控制消息无声丢弃（和真实 hub 一致）。
    if (conn.temp && ctrl.type !== 'pair_join' && ctrl.type !== 'pair_confirm') return
    switch (ctrl.type) {
      case 'pair_begin': {
        // 短码由 host 本地生成、不发给 hub；hub 只拿到 match_id。
        for (const [k, v] of this.#pending) if (v === conn) this.#pending.delete(k)
        this.#pending.set(ctrl.match_id, conn)
        this.#sendControl(conn, { type: 'pair_registered', expires_at_ms: Date.now() + 300_000 })
        return
      }
      case 'pair_join': {
        const host = this.#pending.get(ctrl.match_id)
        if (!host || host.ws.readyState !== host.ws.OPEN) {
          this.#sendControl(conn, { type: 'pair_failed', reason: 'not_found' })
          return
        }
        this.#pending.delete(ctrl.match_id)
        const pairing: Pairing = { host, joiner: conn, confirmed: new Set() }
        host.pairing = pairing
        conn.pairing = pairing
        this.#sendControl(host, { type: 'pair_matched', peer: conn.id, role: 'host' })
        this.#sendControl(conn, { type: 'pair_matched', peer: host.id, role: 'joiner' })
        return
      }
      case 'pair_confirm': {
        const pairing = conn.pairing
        if (!pairing) return
        pairing.confirmed.add(conn.id)
        if (
          pairing.confirmed.size < 2 &&
          !(this.forgePairDoneOnConfirm && conn === pairing.joiner)
        ) {
          return
        }
        const { host, joiner } = pairing
        const tempId = joiner.id
        let joinerDevice = this.devices.get(joiner.id)
        const issueToken = !joinerDevice
        if (!joinerDevice) {
          // 临时连接升级：换成正式 id（和临时 id 不同），同一条连接继续用。
          const permanentId = rid()
          joinerDevice = {
            id: permanentId,
            token: `tok_${rid()}`,
            kind: 'sdk',
            revoked: false,
            paired: new Set(),
          }
          this.devices.set(permanentId, joinerDevice)
          this.#byToken.set(joinerDevice.token, joinerDevice)
          this.#conns.delete(tempId)
          joiner.id = permanentId
          joiner.temp = false
          this.#conns.set(permanentId, joiner)
        }
        const hostDevice = this.devices.get(host.id)
        joinerDevice.paired.add(host.id)
        hostDevice?.paired.add(joiner.id)
        // host 收到的 peer 是 joiner 的正式 id。作恶模式下 host 从来没确认过，也不会收到 PairDone。
        if (!(this.forgePairDoneOnConfirm && pairing.confirmed.size < 2)) {
          this.#sendControl(host, { type: 'pair_done', peer: joiner.id })
        }
        const toJoiner: Control = {
          type: 'pair_done',
          peer: this.tamperPairDonePeer ? this.tamperPairDonePeer(host.id) : host.id,
          ...(issueToken ? { device_id: joiner.id, device_token: joinerDevice.token } : {}),
        }
        if (this.delayJoinerPairDoneMs > 0) {
          setTimeout(() => this.#sendControl(joiner, toJoiner), this.delayJoinerPairDoneMs)
        } else {
          this.#sendControl(joiner, toJoiner)
        }
        host.pairing = undefined
        joiner.pairing = undefined
        // 真实 hub 在 PairDone 之后紧接着给双方发 PeerOnline——它可能先于 daemon 的 pair.ack 到达，
        // joiner 不能把它当成配对成功的信号。
        this.#sendControl(host, { type: 'peer_online', device: joiner.id })
        this.#sendControl(joiner, { type: 'peer_online', device: host.id })
        this.peerOnlineAfterPair.push(host.id, joiner.id)
        return
      }
      default:
        return
    }
  }

  #route(conn: Conn, env: Envelope, raw: Uint8Array): void {
    if (env.from !== conn.id) return
    if (raw.length > this.maxEnvelopeBytes) {
      this.dropped.push({ from: conn.id, seq: env.seq, reason: 'too_large' })
      this.#receipt(conn, { type: 'dropped', seq: env.seq, reason: 'too_large' })
      return
    }
    const ordered = env.kind === 'data' || env.kind === 'notify'
    const key = `${conn.id}>${env.to}`
    if (ordered) {
      this.arrivals.push({
        from: conn.id,
        to: env.to,
        kind: env.kind,
        seq: env.seq,
        at: Date.now(),
      })
    }
    const stuck = ordered && this.stickyRule ? this.#sticky.get(key) : undefined
    const blocked = stuck && stuck.size > 0 ? Math.min(...stuck) : undefined
    if (stuck && blocked !== undefined && env.seq !== blocked) {
      // 粘住：不是集合最小值，一律退回并记入集合。
      stuck.add(env.seq)
      this.#bounce(conn, env.to, env.seq)
      return
    }
    if (this.interceptor) {
      const verdict = this.interceptor(env, conn)
      if (verdict === 'drop') return
      if (verdict === 'backpressure' || verdict === 'rate_limited') {
        if (ordered && this.stickyRule) {
          const set = this.#sticky.get(key) ?? new Set<number>()
          set.add(env.seq)
          this.#sticky.set(key, set)
        }
        this.#bounce(conn, env.to, env.seq)
        if (verdict === 'rate_limited') {
          this.#receipt(conn, { type: 'rate_limited', retry_after_ms: this.retryAfterMs })
        }
        return
      }
    }
    const pairedNow = this.devices.get(conn.id)?.paired.has(env.to) === true
    const pairingNow =
      env.kind === 'handshake' &&
      conn.pairing !== undefined &&
      (conn.pairing.host.id === env.to || conn.pairing.joiner.id === env.to)
    if (!pairedNow && !pairingNow) {
      this.dropped.push({ from: conn.id, seq: env.seq, reason: 'not_paired' })
      this.#receipt(conn, { type: 'dropped', seq: env.seq, reason: 'not_paired' })
      return
    }
    const target = this.#conns.get(env.to)
    if (!target || !target.authed || target.ws.readyState !== target.ws.OPEN) {
      this.#sticky.delete(key)
      this.#sendControl(conn, { type: 'undeliverable', to: env.to, seq: env.seq })
      return
    }
    this.log.push({ from: env.from, to: env.to, kind: env.kind, seq: env.seq, size: raw.length })
    target.ws.send(raw)
    // 集合最小的那一封重发成功了就去掉它；发送方换了传输（handshake 被转发）则整组清除。
    if (env.kind === 'handshake') this.#sticky.delete(key)
    else if (stuck && blocked !== undefined) stuck.delete(blocked)
  }

  /** 退回一封（`backpressure`），并记入 `bounced`。 */
  #bounce(conn: Conn, to: string, seq: number): void {
    this.bounced.push({ from: conn.id, to, seq, at: Date.now() })
    this.#sendControl(conn, { type: 'backpressure', to, seq })
  }

  #onClose(conn: Conn): void {
    for (const key of this.#sticky.keys()) {
      const [from, to] = key.split('>')
      if (from === conn.id || to === conn.id) this.#sticky.delete(key)
    }
    if (this.#conns.get(conn.id) === conn) {
      this.#conns.delete(conn.id)
      for (const [code, host] of this.#pending) if (host === conn) this.#pending.delete(code)
      const device = this.devices.get(conn.id)
      for (const id of device?.paired ?? []) {
        const other = this.#conns.get(id)
        if (other) this.#sendControl(other, { type: 'peer_offline', device: conn.id })
      }
    }
    const pairing = conn.pairing
    if (pairing) {
      const other = pairing.host === conn ? pairing.joiner : pairing.host
      other.pairing = undefined
      this.#sendControl(other, { type: 'pair_failed', reason: 'peer_gone' })
    }
  }
}
