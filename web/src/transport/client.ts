// 传输层的总管：hub WebSocket、认证、重连、配对、每个节点的 Noise 会话。
// 对 UI 层只暴露 `ClientSink`（状态变化回调）和 `request(nodeId, method, params)`。
import type { AuthFailReason, ClientInfo, Control, DropReason, EnrollMethod, NotifyBody, PairAck, PairFailReason } from '@/protocol'
import { APP_VERSION, CLOSE_CODE, METHODS, PAIR_ACK_TIMEOUT_MS, PAIR_STEP_TIMEOUT_MS, PROTOCOL_VERSION, ROTATE_QUICK_SENDS, ROTATE_RECHECK_MS, ROTATE_RETRY_MS } from '@/lib/constants'
import { bytesEqual } from '@/lib/bytes'
import { isValidPairCode } from '@/lib/code'
import { isRecord } from '@/lib/guards'
import { defaultDeviceName } from '@/lib/device'
import { mark, shortId } from '@/lib/timing'
import * as db from '@/db'
import {
  hubReduce,
  initialHub,
  STABLE_AFTER_MS,
  nodeConnState,
  type HubEvent,
  type HubHalt,
  type HubMachine,
  type HubPhase,
  type NodeConnState,
} from './connection'
import type { FullCrypto, Handshake, Keypair, Transport } from './crypto'
import { EnrollError, EnrollRun, runInitiator, type EnrollHooks, type EnrollParams } from './enroll'
import type { PairDoneEvent, PairEvent } from './events'
import { Mailbox, MailboxClosed, MailboxTimeout } from './mailbox'
import { NodeLink } from './nodeLink'
import { decodeNotifyBody } from './notifyBody'
import { nextPaint } from './paint'
import { EnvelopeSender } from './sender'
import { tokenRotationDue } from './tokenRotation'
import type { RpcCallError } from './rpcClient'
import { decodeControl, decodeEnvelope, decodeFrame, decodeRpc, encodeControl, encodeEnvelope, isRpcNotification, makeEnvelope, Reassembler, type Envelope } from './wire'

// ---- 对外类型 ----

export interface WebSocketLike {
  binaryType: string
  readonly readyState: number
  onopen: ((ev: unknown) => void) | null
  onmessage: ((ev: { data: unknown }) => void) | null
  onclose: ((ev: { code: number; reason?: string }) => void) | null
  onerror: ((ev: unknown) => void) | null
  send(data: Uint8Array): void
  close(code?: number, reason?: string): void
}

export interface ClientStorage {
  loadIdentity(): Promise<db.IdentityRecord | undefined>
  saveIdentity(rec: db.IdentityRecord): Promise<void>
  /** 清掉存着的 device token（连同设备 id 和签发时间），仅当它还是 `expected`；返回是否清了（见 `db.clearDeviceToken`）。 */
  clearDeviceToken(expected: string): Promise<boolean>
  loadNodes(): Promise<db.NodeRecord[]>
  saveNode(rec: db.NodeRecord): Promise<void>
  deleteNode(nodeId: string): Promise<void>
  wipe(): Promise<void>
}

export const idbStorage: ClientStorage = {
  loadIdentity: db.getIdentity,
  saveIdentity: db.putIdentity,
  clearDeviceToken: db.clearDeviceToken,
  loadNodes: db.listNodes,
  saveNode: db.putNode,
  deleteNode: db.deleteNode,
  wipe: db.wipeAll,
}

export interface HubSnapshot {
  phase: HubPhase
  halt: HubHalt
  attempt: number
  retryInMs: number | null
  deviceId: string | null
  /** 当前身份是否已经配对过（持有正式 device token）。 */
  paired: boolean
  deviceName: string
}

export interface NodeSnapshot {
  nodeId: string
  conn: NodeConnState
}

export type PairStage = 'connecting' | 'deriving' | 'matching' | 'handshaking' | 'confirming' | 'verifying' | 'done'
export type PairErrorCode =
  | 'hub_unavailable'
  | 'invalid_code'
  | PairFailReason
  | 'handshake_failed'
  | 'no_identity'
  | 'cancelled'
  | 'timeout'
  /** `pair.ack` 与 `pair_done` / 握手得到的公钥对不上（hub 或对端在撒谎）。 */
  | 'ack_invalid'
  /** 本机已有同 id 的节点，但公钥不同：绝不静默覆盖。 */
  | 'key_changed'

/** 配对握手消息 2 里 daemon 带来的自我介绍。 */
export interface NodeHello {
  name?: string
  version?: string
}

export function parseNodeHello(payload: Uint8Array): NodeHello {
  if (payload.length === 0) return {}
  try {
    const o: unknown = JSON.parse(new TextDecoder().decode(payload))
    if (!isRecord(o)) return {}
    return { name: cleanName(o.name) || undefined, version: typeof o.version === 'string' ? o.version.slice(0, 40) : undefined }
  } catch {
    return {}
  }
}

/** 节点名来自对端，只做展示：去掉控制字符、截断。 */
export function cleanName(v: unknown): string {
  if (typeof v !== 'string') return ''
  return [...v]
    .filter((c) => {
      const n = c.codePointAt(0) ?? 0
      return n >= 0x20 && n !== 0x7f && !(n >= 0x80 && n <= 0x9f)
    })
    .join('')
    .trim()
    .slice(0, 120)
}

/** 解出 `pair.ack` 通知的参数；不是 `pair.ack` 或字段不全返回 null。 */
export function parsePairAck(plain: Uint8Array): PairAck | null {
  let msg: ReturnType<typeof decodeRpc>
  try {
    msg = decodeRpc(plain)
  } catch {
    return null
  }
  if (!isRpcNotification(msg) || msg.method !== METHODS.PAIR_ACK) return null
  const p = msg.params
  if (!isRecord(p)) return null
  const { node_id, peer_id, name, version, fingerprint } = p
  if (typeof node_id !== 'string' || typeof peer_id !== 'string' || typeof name !== 'string' || typeof version !== 'string' || typeof fingerprint !== 'string') return null
  return { node_id, peer_id, name, version, fingerprint }
}

const encodeJson = (v: unknown): Uint8Array => new TextEncoder().encode(JSON.stringify(v))

export class PairError extends Error {
  constructor(
    readonly code: PairErrorCode,
    message?: string,
  ) {
    super(message ?? code)
    this.name = 'PairError'
  }
}

export interface PairResult {
  nodeId: string
  publicKey: Uint8Array
}

/** `AnyconClient.enroll` 的参数。 */
export interface EnrollRequest {
  /** daemon 的正式设备 id（账号设备列表里的 `id`）。 */
  node: string
  method: EnrollMethod
  /** 仅 `code`：`true` = 请求已配对设备批准；`false`（缺省）= 用已有的接入码。 */
  approval?: boolean
  /** 本设备的名字：作为 hint 发给 daemon（让批准的人认得出来），也是 XXpsk3 第 3 条里 `ClientInfo` 的名字。 */
  deviceName: string
  /** 仅 `password`。 */
  password?: string
}

export interface ClientSink {
  hub(s: HubSnapshot): void
  node(s: NodeSnapshot): void
  /** 与该节点的 Noise 握手完成，可以开始同步。 */
  nodeReady(nodeId: string): void
  notification(nodeId: string, method: string, params: unknown): void
  /** 已配对节点发来的明文 `notify` 信封（类别 + 会话 id，没有正文）。 */
  notify?(nodeId: string, body: NotifyBody): void
  /** 启动时从存储读出了已配对节点：在任何连接事件之前先让 UI 层建好节点条目。 */
  nodesLoaded?(nodes: db.NodeRecord[]): void
  /** 配对成功登记了新节点（在它的连接事件之前同步调用）。 */
  nodeAdded?(node: db.NodeRecord): void
  quota?(quota: Extract<Control, { type: 'auth_ok' }>['quota']): void
  /** hub 丢弃了一个我们发的 `data` 信封（见 `AnyconClient.onDropped`）。`nodeId` 是那封信的目标节点，查不到时为 null。 */
  dropped?(reason: DropReason, nodeId: string | null): void
}

export interface ClientDeps {
  crypto: () => Promise<FullCrypto>
  wsFactory: (url: string) => WebSocketLike
  /** 返回 hub 的 WebSocket URL（相对当前 origin）。 */
  wsUrl: () => string
  storage: ClientStorage
  rng?: () => number
  origin?: () => string
  /** 派生配对密钥（同步的 Argon2id，会卡主线程约 0.1–1 s）之前调用，让 UI 先画出「正在派生密钥…」。默认等下一帧。 */
  yieldToPaint?: () => Promise<void>
}

const WS_OPEN = 1

const noopSink: ClientSink = { hub() {}, node() {}, nodeReady() {}, notification() {} }

export class AnyconClient {
  private sink: ClientSink = noopSink
  private crypto: FullCrypto | null = null
  private identity: db.IdentityRecord | null = null
  private keypair: Keypair | null = null
  private hubMachine: HubMachine = initialHub
  private ws: WebSocketLike | null = null
  private wsAuthName = ''
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null
  private authTimer: ReturnType<typeof setTimeout> | null = null
  private stableTimer: ReturnType<typeof setTimeout> | null = null
  /** 下一次「该不该轮换 device token」的检查（`checkRotation`）；断开 / 停止时清掉。 */
  private rotateTimer: ReturnType<typeof setTimeout> | null = null
  /** 这条连接上已经发了几次 `rotate_token`（新连接清零）。 */
  private rotateSends = 0
  /** 这条连接认证用的 device token；`null` = 临时连接。hub 说它 `invalid` 时要知道到底是哪一份被拒了。 */
  private authToken: string | null = null
  private deviceId: string | null = null
  private seq = 0
  private started = false
  private loadedNodes: db.NodeRecord[] = []
  private pairPeer: string | null = null
  /** `pair_done` 里的对端正式 id：配对完成后到 `pair.ack` 到达之前，来自它的 `data` 信封进配对邮箱而不是节点链路。 */
  private pairDonePeer: string | null = null
  private readonly links = new Map<string, NodeLink>()
  /** hub 报告的在线对端（含还没登记成节点的，比如刚配完对、PeerOnline 比登记早到）。 */
  private readonly onlinePeers = new Set<string>()
  private pairMailbox: Mailbox<PairEvent> | null = null
  private pairing = false
  /** 进行中的账号内接入（同一时刻最多一个，和 `pairing` 互斥）。 */
  private enrollRun: EnrollRun | null = null
  /** 我们已经取消的接入会话 id：hub 对已结束的会话再收到 `enroll_cancel` 会回 `enroll_failed { not_found }`，别把它当成下一次接入的失败。 */
  private readonly staleEnrollIds: string[] = []
  private lastFrameAt = 0
  /** 页面在后台满 30 秒、主动断开了 hub 连接（`suspendForBackground`）：不重连，直到回到前台（`onVisible`）或用户操作。 */
  private suspended = false
  /** 出站信封：重发缓冲、`backpressure` 暂停 + 升序重发、`rate_limited` 暂停（见 sender.ts）。 */
  private readonly sender: EnvelopeSender

  constructor(private readonly deps: ClientDeps) {
    this.sender = new EnvelopeSender({
      write: (bytes) => {
        if (!this.ws || this.ws.readyState !== WS_OPEN) return false
        try {
          this.ws.send(bytes)
          return true
        } catch {
          return false
        }
      },
      // 被退回的信封已经不在缓冲里 / 对端长期收不下：这条传输的 nonce 序列断了，丢掉重新握手
      resetPeer: (to, reason) => this.links.get(to)?.resetTransport(reason),
      // 握手信封被退回、延后到点：同一条握手消息再发一次（不重置、不新起握手）
      retryHandshake: (to) => this.links.get(to)?.resendHandshake(),
      rng: () => (this.deps.rng ?? Math.random)(),
    })
  }

  setSink(sink: ClientSink): void {
    this.sink = sink
  }

  // ---- 生命周期 ----

  /**
   * 载入本机身份和已配对节点，连上 hub。`accountId` 与上次不同则先清空本地库（换账号登录）。
   */
  async start(accountId: string): Promise<{ nodes: db.NodeRecord[] }> {
    if (this.started) return { nodes: this.loadedNodes }
    this.started = true
    this.suspended = false
    mark('transport.start')
    this.crypto = await this.deps.crypto()
    mark('transport.crypto_ready')
    let id = await this.deps.storage.loadIdentity()
    if (id && id.account_id && id.account_id !== accountId) {
      await this.deps.storage.wipe()
      id = undefined
    }
    if (!id) {
      const kp = this.crypto.generateKeypair()
      id = { id: 'self', static_keypair: { public: kp.public, secret: kp.secret }, device_name: defaultDeviceName(), account_id: accountId }
      await this.deps.storage.saveIdentity(id)
    } else if (!id.account_id) {
      id = { ...id, account_id: accountId }
      await this.deps.storage.saveIdentity(id)
    }
    this.identity = id
    this.keypair = { public: id.static_keypair.public, secret: id.static_keypair.secret }
    this.deviceId = id.device_id ?? null
    const nodes = await this.deps.storage.loadNodes()
    mark('transport.storage_loaded')
    this.loadedNodes = nodes
    this.sink.nodesLoaded?.(nodes)
    for (const n of nodes) this.attachNode(n)
    this.publishHub()
    this.connect()
    return { nodes }
  }

  stop(): void {
    this.started = false
    this.suspended = false
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer)
    if (this.authTimer) clearTimeout(this.authTimer)
    if (this.stableTimer) clearTimeout(this.stableTimer)
    this.reconnectTimer = this.authTimer = this.stableTimer = null
    this.clearRotateTimer()
    const ws = this.ws
    this.ws = null
    if (ws) {
      ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null
      try {
        ws.close(1000, 'client stop')
      } catch {
        /* ignore */
      }
    }
    for (const l of this.links.values()) l.dispose()
    this.links.clear()
    this.sender.clear()
    this.hubMachine = initialHub
    this.crypto = null
    this.identity = null
    this.keypair = null
    this.loadedNodes = []
  }

  get snapshot(): HubSnapshot {
    return {
      phase: this.hubMachine.phase,
      halt: this.hubMachine.halt,
      attempt: this.hubMachine.attempt,
      retryInMs: this.hubMachine.retryInMs,
      deviceId: this.deviceId,
      paired: !!this.identity?.device_token,
      deviceName: this.identity?.device_name ?? '',
    }
  }

  get deviceName(): string {
    return this.identity?.device_name ?? defaultDeviceName()
  }

  private publishHub(): void {
    this.sink.hub(this.snapshot)
  }

  private publishNode(nodeId: string): void {
    const link = this.links.get(nodeId)
    if (!link) return
    this.sink.node({ nodeId, conn: nodeConnState(this.hubMachine.phase, link.machine) })
  }

  // ---- 节点管理 ----

  private attachNode(rec: db.NodeRecord): NodeLink {
    const existing = this.links.get(rec.node_id)
    if (existing) {
      existing.rekey(rec.public_key)
      return existing
    }
    const link = new NodeLink(rec.node_id, rec.public_key, {
      crypto: this.crypto!,
      keypair: this.keypair!,
      canSend: () => this.canLinkSend(rec.node_id),
      sendData: (to, bytes) => this.sendEnvelope(to, 'data', bytes),
      sendHandshake: (to, bytes) => this.sendEnvelope(to, 'handshake', bytes),
      // 传输作废（重置 / 重新握手 / 对端下线）：旧传输上排队的和被退回的信封都不能再发
      discardPending: (id) => this.sender.discardPeer(id),
      handshakeHeld: (id) => this.sender.isHandshakeHeld(id),
      onChange: (id) => this.publishNode(id),
      onReady: (id) => {
        mark(`node.ready ${shortId(id)}`)
        this.sender.handshakeDone(id)
        this.publishNode(id)
        this.sink.nodeReady(id)
      },
      onNotification: (id, m, p) => this.sink.notification(id, m, p),
    })
    this.links.set(rec.node_id, link)
    this.publishNode(rec.node_id)
    if (this.onlinePeers.has(rec.node_id) && this.canLinkSend(rec.node_id)) link.peerOnline()
    return link
  }

  /**
   * 节点链路（IK 握手和 Noise 传输）能不能往这个节点发。除了 hub 可写，还要求：
   * - 连接是正式设备的：吊销后 token 已清掉，重新配对走临时连接，hub 不会把它的信封转发给任何已配对节点；
   * - 不是正在跟这个节点做配对握手：撮合之后 hub 放行临时连接发给 host 的所有 `handshake` 帧，daemon 把它们都当配对握手读，
   *   旧链路的 IK 混进来会让这次配对解密失败、被作废。`pair_done` 之后 daemon 就在等 IK 了，不受这条限制。
   */
  private canLinkSend(nodeId: string): boolean {
    if (!this.canSend() || !this.identity?.device_token) return false
    return !(this.pairing && this.pairDonePeer === null && this.pairPeer === nodeId)
  }

  knownNodeIds(): string[] {
    return [...this.links.keys()]
  }

  nodeConn(nodeId: string): NodeConnState {
    const link = this.links.get(nodeId)
    return link ? nodeConnState(this.hubMachine.phase, link.machine) : this.hubMachine.phase
  }

  isReady(nodeId: string): boolean {
    return this.links.get(nodeId)?.ready ?? false
  }

  /** 把节点登记进来（配对完成后）。 */
  async addNode(rec: db.NodeRecord): Promise<void> {
    await this.deps.storage.saveNode(rec)
    this.attachNode(rec)
  }

  async removeNode(nodeId: string): Promise<void> {
    this.links.get(nodeId)?.dispose()
    this.links.delete(nodeId)
    await this.deps.storage.deleteNode(nodeId)
  }

  request<R = unknown>(nodeId: string, method: string, params: unknown = {}, timeoutMs?: number): Promise<R> {
    const link = this.links.get(nodeId)
    if (!link) return Promise.reject(new Error(`unknown node ${nodeId}`))
    return link.request<R>(method, params, timeoutMs)
  }

  // ---- hub 连接 ----

  private canSend(): boolean {
    return this.hubMachine.phase === 'hub_authed' && this.ws?.readyState === WS_OPEN
  }

  private dispatchHub(e: HubEvent): void {
    this.hubMachine = hubReduce(this.hubMachine, e, this.deps.rng)
    this.publishHub()
    for (const id of this.links.keys()) this.publishNode(id)
  }

  private connect(): void {
    if (!this.started || !this.identity || this.suspended) return
    if (this.ws) return
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer)
    this.reconnectTimer = null
    this.dispatchHub({ type: 'connect' })
    mark('hub.connect')
    let ws: WebSocketLike
    try {
      ws = this.deps.wsFactory(this.deps.wsUrl())
    } catch {
      this.onClosed(1006)
      return
    }
    ws.binaryType = 'arraybuffer'
    this.ws = ws
    ws.onopen = () => {
      if (this.ws !== ws) return
      mark('hub.ws_open')
      this.sendAuth()
      this.authTimer = setTimeout(() => ws.close(4005, 'auth timeout'), 10_000)
    }
    ws.onmessage = (ev) => {
      if (this.ws !== ws) return
      this.lastFrameAt = Date.now()
      if (!(ev.data instanceof ArrayBuffer) && !(ev.data instanceof Uint8Array)) return
      this.onFrame(ev.data instanceof Uint8Array ? ev.data : new Uint8Array(ev.data))
    }
    ws.onclose = (ev) => {
      if (this.ws !== ws) return
      this.ws = null
      this.onClosed(ev.code)
    }
    ws.onerror = () => {
      /* close 事件随后会来 */
    }
  }

  private clientInfo(): ClientInfo {
    return {
      name: this.identity?.device_name ?? defaultDeviceName(),
      kind: 'web',
      platform: 'browser',
      version: APP_VERSION,
      origin: this.deps.origin ? this.deps.origin() : location.origin,
    }
  }

  private sendAuth(): void {
    const id = this.identity!
    this.wsAuthName = id.device_name
    this.authToken = id.device_token ?? null
    const auth: Control = {
      type: 'auth',
      device_token: id.device_token ?? null,
      protocol_version: PROTOCOL_VERSION,
      client: this.clientInfo(),
    }
    this.rawSend(makeEnvelope('', '', 'control', ++this.seq, encodeControl(auth)))
  }

  private rawSend(env: Envelope): boolean {
    if (!this.ws || this.ws.readyState !== WS_OPEN) return false
    try {
      this.ws.send(encodeEnvelope(env))
      return true
    } catch {
      return false
    }
  }

  private onClosed(code: number): void {
    if (this.authTimer) clearTimeout(this.authTimer)
    if (this.stableTimer) clearTimeout(this.stableTimer)
    this.authTimer = this.stableTimer = null
    this.clearRotateTimer()
    this.sender.clear()
    this.onlinePeers.clear()
    for (const l of this.links.values()) l.hubDown()
    this.pairMailbox?.close()
    if (code === CLOSE_CODE.REVOKED) void this.forgetToken(null)
    // 主动断开（页面在后台）不是失败：不计退避，也不安排重连
    this.dispatchHub(this.suspended ? { type: 'suspend' } : { type: 'closed', code })
    this.scheduleReconnect()
  }

  private scheduleReconnect(): void {
    if (!this.started || this.suspended) return
    const delay = this.hubMachine.retryInMs
    if (delay === null) return
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer)
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, delay)
  }

  /** 用户在「被别的标签页顶掉」横幅上点了「在此使用」，或者页面回到前台。 */
  takeOver(): void {
    if (!this.started || this.ws) return
    this.suspended = false
    this.hubMachine = { ...this.hubMachine, halt: null, attempt: 0, retryInMs: 0 }
    this.connect()
  }

  /**
   * 页面在后台满 30 秒（spec/web.md § 7）：主动断开和 hub 的连接，也不再自动重连，直到 `onVisible()`。
   * 手机上被冻结 / 挂起的页面收不到信封，浏览器的网络层却还会替它回 WebSocket 的 ping；不断开，hub 会一直以为这台浏览器在线，
   * 一条 Web Push 也不发。断开后 hub 看它「不在」，`notify` 就走推送。
   *
   * 正在配对 / 接入（`pairing`）、或还有请求在路上时不断开，返回 false，调用方稍后再试；
   * 没启动 / 已经断开着的返回 true（该做的都做了：已断开的只取消排队中的重连）。
   */
  suspendForBackground(): boolean {
    if (!this.started) return true
    if (this.pairing || [...this.links.values()].some((l) => l.rpc.pendingCount > 0)) return false
    this.suspended = true
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer)
    this.reconnectTimer = null
    const ws = this.ws
    if (!ws) {
      this.dispatchHub({ type: 'suspend' })
      return true
    }
    this.ws = null
    ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null
    try {
      ws.close(1000, 'page in background')
    } catch {
      /* ignore */
    }
    this.onClosed(1000)
    return true
  }

  /** 页面回到前台：断了立即重连（含后台时主动断开的那种）；还连着的话探测一下节点，半死不活的连接强制重连。 */
  async onVisible(): Promise<void> {
    if (!this.started) return
    this.suspended = false
    if (!this.ws) {
      this.dispatchHub({ type: 'visible' })
      if (this.hubMachine.retryInMs === 0) {
        if (this.reconnectTimer) clearTimeout(this.reconnectTimer)
        this.connect()
      }
      return
    }
    if (this.hubMachine.phase !== 'hub_authed') return
    const ready = [...this.links.values()].find((l) => l.ready)
    if (!ready || Date.now() - this.lastFrameAt < 20_000) return
    try {
      await ready.request('node.info', {}, 5000)
    } catch (e) {
      if ((e as RpcCallError).kind === 'timeout') this.forceReconnect()
    }
  }

  /** 强制断开并立即重连（用户点「重试」/ 探测失败）。 */
  forceReconnect(): void {
    if (!this.started) return
    this.suspended = false
    const ws = this.ws
    if (ws) {
      this.ws = null
      ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null
      try {
        ws.close(1000, 'reconnect')
      } catch {
        /* ignore */
      }
      this.onClosed(1006)
      this.hubMachine = { ...this.hubMachine, attempt: 0, retryInMs: 0 }
      this.scheduleReconnect()
    } else {
      this.hubMachine = { ...this.hubMachine, halt: null, attempt: 0, retryInMs: 0 }
      this.connect()
    }
  }

  /**
   * hub 不认这个设备了：丢掉内存里的 token、设备 id 和签发时间，并清掉库里的。
   * `rejected` 是 hub 说 `invalid` 的那份 token：内存里已经换成别的 token（`adoptStoredToken`）就什么都不做。
   * 设备被吊销时传 `null`（它所有的 token 都作废）。两种情况下库里的 token 如果已经不是内存里这份
   * （别的标签页轮换过 / 重新配对过）都不碰库：那份不是我们该清的（`db.clearDeviceToken`）。
   */
  private async forgetToken(rejected: string | null): Promise<void> {
    // 停掉之后（比如「退出并移除本设备」正在清库）不能再写存储，否则会把刚删掉的库又建出来
    if (!this.started || !this.identity?.device_token) return
    if (rejected && this.identity.device_token !== rejected) return
    const gone = this.identity.device_token
    const { device_token: _t, device_token_issued_at_ms: _i, device_id: _d, ...rest } = this.identity
    this.identity = rest
    this.deviceId = null
    try {
      await this.deps.storage.clearDeviceToken(gone)
    } catch (e) {
      // 别的标签页刚清掉了本地库（本标签页的库已被禁用）或存储不可写：别抛成未处理的 rejection
      console.warn('anycon: could not persist clearing the device token', e)
    }
  }

  /**
   * 同一个浏览器的别的标签页可能已经轮换过 device token 并落盘，库里的比内存里的新：并入内存身份，返回是否换了 token。
   * 只认同一个设备身份（Noise 公钥相同）。`force`：我们手里这份刚被 hub 拒绝，库里有任何不同的 token 都换上；
   * 否则只换签发时间更晚的（轮换之前先看一眼，别替别的标签页重复轮换）。
   */
  private async adoptStoredToken(force: boolean): Promise<boolean> {
    const cur = this.identity
    if (!cur?.device_token) return false
    let stored: db.IdentityRecord | undefined
    try {
      stored = await this.deps.storage.loadIdentity()
    } catch {
      return false
    }
    if (!stored?.device_token || stored.device_token === cur.device_token) return false
    if (!this.started || this.identity?.device_token !== cur.device_token) return false // 等库的时候身份已经变了
    if (!bytesEqual(stored.static_keypair.public, cur.static_keypair.public)) return false
    if (!force && (stored.device_token_issued_at_ms ?? 0) <= (cur.device_token_issued_at_ms ?? 0)) return false
    const { device_id: _d, device_token_issued_at_ms: _i, ...rest } = cur
    this.identity = {
      ...rest,
      device_token: stored.device_token,
      ...(stored.device_token_issued_at_ms !== undefined ? { device_token_issued_at_ms: stored.device_token_issued_at_ms } : {}),
      ...((stored.device_id ?? cur.device_id) !== undefined ? { device_id: stored.device_id ?? cur.device_id } : {}),
    }
    return true
  }

  // ---- device token 轮换（spec/web.md § 6.4） ----

  private clearRotateTimer(): void {
    if (this.rotateTimer) clearTimeout(this.rotateTimer)
    this.rotateTimer = null
  }

  /**
   * `auth_ok` 之后立刻调用一次，之后按需自己排下一次：token 到期就发 `rotate_token`。
   * 头 `ROTATE_QUICK_SENDS` 次之间隔 `ROTATE_RETRY_MS`（hub 同一设备 1 分钟内只轮换一次，多余的请求不回复，所以重发的
   * 间隔要比这长；回执在同一条连接上丢了也有机会补上，重连之后不行：用旧 token 认证的连接不能轮换），之后以及没到期时每 `ROTATE_RECHECK_MS` 看一次、到期就再发：
   * 一直开着的标签页超过 30 天也会轮换，hub 一直不回复也只是每小时一帧。没有 device token 的连接（临时连接）不发。
   */
  private async checkRotation(): Promise<void> {
    this.clearRotateTimer()
    const ws = this.ws
    let delay = ROTATE_RECHECK_MS
    if (this.identity?.device_token) {
      await this.adoptStoredToken(false)
      // 等库的时候连接可能已经换了（新连接有自己的检查）或停了
      if (!this.started || this.ws !== ws || this.hubMachine.phase !== 'hub_authed') return
      const id = this.identity
      if (id?.device_token && tokenRotationDue(id.device_token_issued_at_ms, Date.now())) {
        if (this.rawSend(makeEnvelope('', '', 'control', ++this.seq, encodeControl({ type: 'rotate_token' })))) {
          this.rotateSends += 1
          delay = this.rotateSends < ROTATE_QUICK_SENDS ? ROTATE_RETRY_MS : ROTATE_RECHECK_MS
        }
      } else {
        this.rotateSends = 0 // 不到期了（轮换成功 / 别的标签页轮换过）：下一个 30 天重新数
      }
    }
    if (!this.started || this.ws !== ws || this.hubMachine.phase !== 'hub_authed') return
    this.rotateTimer = setTimeout(() => void this.checkRotation(), delay)
  }

  /**
   * hub 轮换了 device token（回应我们的 `rotate_token`），新 token 只传这一次：新 token 和签发时间作为一份身份一次写入库，
   * 内存里马上换上，之后的重连用它。写库失败也不回退——旧 token 在 hub 只再保留 60 秒，回退没有意义，只能大声说出来。
   * hub 不会给没有 device token 的连接发这条，收到就忽略。
   */
  private onTokenRotated(token: string): void {
    const id = this.identity
    if (!this.started || !id?.device_token || !token) return
    const next: db.IdentityRecord = { ...id, device_token: token, device_token_issued_at_ms: Date.now() }
    this.identity = next
    this.rotateSends = 0
    this.deps.storage
      .saveIdentity(next)
      .catch((e: unknown) =>
        console.warn('anycon: could not persist the rotated device token; the previous token stops working within a minute, so reloading this page may require pairing again', e),
      )
  }

  /** 后台落盘身份。失败要看得见：丢了 token 轮换意味着下次启动认证失败。 */
  private persistIdentity(rec: db.IdentityRecord): void {
    if (!this.started) return
    this.deps.storage.saveIdentity(rec).catch((e: unknown) => console.warn('anycon: could not persist the device identity', e))
  }

  // ---- 出站 ----

  private sendEnvelope(to: string, kind: Envelope['kind'], body: Uint8Array): number | null {
    if (!this.canSend() || !this.deviceId) return null
    const seq = ++this.seq
    this.sender.send(seq, to, kind, encodeEnvelope(makeEnvelope(this.deviceId, to, kind, seq, body)))
    return seq
  }

  /**
   * hub 丢弃了我们的某个 `data` 信封。`Dropped` 每连接每秒至多一条（不是每个被丢的 seq 都有），它是「发错东西」的信号：
   * `too_large` = 没分片（我们的 bug），`not_paired` = 配对关系不对，`rate_limited` = 发太快。
   * 所以**不据此逐条重发**（`backpressure` 才是「稍后重发同一片」）；少了一片 Noise 序列就断了，直接作废这个节点的传输、重新握手
   * （发送暂停由 `rate_limited` 控制消息负责）。UI 层通过 `sink.dropped` 提示用户。
   */
  private onDropped(seq: number, reason: DropReason): void {
    const entry = this.sender.lookup(seq)
    if (reason === 'too_large') console.error(`anycon: hub dropped envelope ${seq} as too large - a data envelope exceeded the hub limit (fragmentation bug)`)
    else console.warn(`anycon: hub dropped envelope ${seq} (${reason})`)
    this.sink.dropped?.(reason, entry?.to ?? null)
    if (!entry) return
    this.sender.forget(seq)
    if (entry.dead) return // 那条传输早就作废重握手了，别连累现在这条
    const link = this.links.get(entry.to)
    link?.undeliverable(seq)
    link?.resetTransport(`hub dropped an envelope (${reason})`)
  }

  // ---- 入站 ----

  private onFrame(bytes: Uint8Array): void {
    let env: Envelope
    try {
      env = decodeEnvelope(bytes)
    } catch {
      return
    }
    switch (env.kind) {
      case 'control': {
        let msg: Control
        try {
          msg = decodeControl(env.body)
        } catch {
          return
        }
        this.onControl(msg)
        return
      }
      case 'handshake': {
        if (this.pairMailbox && this.pairing && env.from === this.pairPeer) {
          this.pairMailbox.push({ t: 'handshake', body: env.body })
          return
        }
        this.links.get(env.from)?.onHandshake(env.body)
        return
      }
      case 'data':
        if (this.pairMailbox && this.pairing && this.pairDonePeer !== null && env.from === this.pairDonePeer) {
          this.pairMailbox.push({ t: 'data', from: env.from, body: env.body })
          return
        }
        this.links.get(env.from)?.onData(env.body)
        return
      case 'notify': {
        // 明文的 notify 信封（ADR-37）：hub 据此给离线的设备推 Web Push；连着 hub 的设备收到它，页面在后台时用来弹系统通知。
        // 只认已配对节点发来的（hub 只转发配对设备之间的信封，但 hub 本身不是可信方），读不懂的丢掉
        if (!this.links.has(env.from)) return
        const body = decodeNotifyBody(env.body)
        if (body) this.sink.notify?.(env.from, body)
        return
      }
    }
  }

  private onControl(msg: Control): void {
    switch (msg.type) {
      case 'auth_ok': {
        mark('hub.auth_ok')
        if (this.authTimer) clearTimeout(this.authTimer)
        this.authTimer = null
        this.deviceId = msg.device_id
        if (this.identity?.device_token && this.identity.device_id !== msg.device_id) {
          this.identity = { ...this.identity, device_id: msg.device_id }
          this.persistIdentity(this.identity)
        }
        this.dispatchHub({ type: 'auth_ok' })
        if (this.stableTimer) clearTimeout(this.stableTimer)
        this.stableTimer = setTimeout(() => this.dispatchHub({ type: 'stable' }), STABLE_AFTER_MS)
        this.rotateSends = 0
        void this.checkRotation()
        if (msg.quota) this.sink.quota?.(msg.quota)
        const online = new Set(msg.peers_online)
        this.onlinePeers.clear()
        for (const id of online) this.onlinePeers.add(id)
        for (const link of this.links.values()) {
          if (online.has(link.nodeId)) link.peerOnline()
          else link.peerOffline()
        }
        return
      }
      case 'auth_fail':
        this.onAuthFail(msg.reason)
        return
      case 'peer_online':
        this.onlinePeers.add(msg.device)
        this.links.get(msg.device)?.peerOnline()
        return
      case 'peer_offline':
        this.onlinePeers.delete(msg.device)
        this.links.get(msg.device)?.peerOffline()
        return
      case 'undeliverable':
        this.links.get(msg.to)?.undeliverable(msg.seq)
        this.sender.onUndeliverable(msg.seq)
        return
      case 'backpressure':
        this.sender.onBackpressure(msg.to, msg.seq)
        return
      case 'rate_limited':
        this.sender.onRateLimited(msg.retry_after_ms)
        return
      case 'quota_update':
        this.sink.quota?.(msg.quota)
        return
      case 'token_rotated':
        this.onTokenRotated(msg.device_token)
        return
      case 'pair_matched':
        this.pairMailbox?.push({ t: 'matched', peer: msg.peer, role: msg.role })
        return
      case 'pair_failed':
        this.pairMailbox?.push({ t: 'failed', reason: msg.reason })
        return
      case 'pair_done':
        if (this.pairing) this.pairDonePeer = msg.peer
        this.pairMailbox?.push({ t: 'done', peer: msg.peer, deviceId: msg.device_id, deviceToken: msg.device_token })
        return
      case 'enroll_started':
        this.pairMailbox?.push({ t: 'enroll_started', enrollId: msg.enroll_id, node: msg.node, expiresAtMs: msg.expires_at_ms })
        return
      case 'enroll_failed':
        // 我们自己取消的会话，hub 再回的 `not_found` 回执：不是后来那一次接入的失败
        if (msg.enroll_id && this.staleEnrollIds.includes(msg.enroll_id)) return
        this.pairMailbox?.push({ t: 'enroll_failed', enrollId: msg.enroll_id ?? null, reason: msg.reason })
        return
      case 'dropped':
        this.onDropped(msg.seq, msg.reason)
        return
      default:
        return
    }
  }

  private onAuthFail(reason: AuthFailReason): void {
    const rejected = this.authToken
    if (reason === 'invalid' && rejected) {
      // 先别下结论：同一个浏览器的另一个标签页可能刚轮换过 token、新的已经在库里，这份只是过期了的旧 token
      this.dropWsAfterAuthFail()
      void this.recoverFromInvalidToken(rejected)
      return
    }
    if (reason === 'revoked') void this.forgetToken(null) // 设备被吊销，它的所有 token 都作废，不分哪一份
    this.dispatchHub({ type: 'auth_fail', reason })
    this.dropWsAfterAuthFail()
    this.scheduleReconnect()
  }

  /** hub 随后会关连接；这里主动关，免得等 close 事件。 */
  private dropWsAfterAuthFail(): void {
    if (this.authTimer) clearTimeout(this.authTimer)
    this.authTimer = null
    if (this.ws) {
      const ws = this.ws
      this.ws = null
      ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null
      try {
        ws.close(1000, 'auth failed')
      } catch {
        /* ignore */
      }
      this.sender.clear()
      for (const l of this.links.values()) l.hubDown()
      this.pairMailbox?.close()
    }
  }

  /**
   * hub 说 `rejected` 这份 token 无效。库里有别的 token（别的标签页轮换过）就换上它、马上重连；
   * 没有才当作真的失效：清掉、停在「需要重新配对」。
   */
  private async recoverFromInvalidToken(rejected: string): Promise<void> {
    await this.adoptStoredToken(true)
    if (!this.started) return
    const cur = this.identity?.device_token
    if (cur && cur !== rejected) {
      this.hubMachine = { ...this.hubMachine, halt: null, attempt: 0, retryInMs: 0 }
      this.connect()
      return
    }
    await this.forgetToken(rejected)
    this.dispatchHub({ type: 'auth_fail', reason: 'invalid' })
    this.scheduleReconnect()
  }

  // ---- 设备名 ----

  async setDeviceName(name: string): Promise<void> {
    if (!this.identity) return
    this.identity = { ...this.identity, device_name: name }
    await this.deps.storage.saveIdentity(this.identity)
    this.publishHub()
  }

  // ---- 配对（joiner：XXpsk3 initiator） ----

  /**
   * 用短码配对一个节点（ADR-29）。成功后节点和新的设备身份都已写入存储。
   *
   * 1. 短码只在本地用：`derivePairSecrets(code)`（一次 Argon2id）得到 `match_id`（发给 hub 撮合）和 PSK，hub 看不到短码；
 *    PSK 在握手对象建好后立刻清零；
   * 2. XXpsk3 握手完成、`pair_confirm` / `pair_done` 之后，**等 daemon 经这条 PSK 绑定的 Noise 传输发来 `pair.ack`**
   *    （30 秒），校验 `peer_id == 我的正式 id`、`node_id == PairDone.peer`、`fingerprint == 握手得到的公钥指纹`，
   *    全部通过才落盘节点。hub 伪造 `pair_done` 骗不到这一步；
   * 3. 本机已有同 id 的节点而公钥不同：失败，不覆盖。
   *
   * `report` 报告进度，UI 据此画「连接 → 派生密钥 → 撮合中 → 握手中 → 确认中 → 校验中 → 完成」。
   */
  async pair(code: string, deviceName: string, report: (stage: PairStage) => void = () => {}): Promise<PairResult> {
    if (this.pairing) throw new PairError('cancelled', 'another pairing is in progress')
    if (!this.started || !this.identity || !this.crypto || !this.keypair) throw new PairError('no_identity')
    const crypto = this.crypto
    const normalized = crypto.normalizeCode(code)
    if (!isValidPairCode(normalized)) throw new PairError('invalid_code')
    this.pairing = true
    let mb: Mailbox<PairEvent> | null = null
    // PSK 只在握手对象建好之前需要；任何路径退出都要清零
    let psk: Uint8Array | null = null
    try {
      report('connecting')
      // 设备被吊销后 token 已清掉：以临时连接重新上线才能配对
      if ((this.hubMachine.halt || this.suspended) && !this.ws) this.takeOver()
      // 设备名在 Auth 里带给 hub：改了名要重新认证一次
      if (deviceName && deviceName !== this.identity.device_name) {
        await this.setDeviceName(deviceName)
        if (this.ws) this.forceReconnect()
      }
      await this.waitAuthed(15_000)

      // 一次 Argon2id 同时得到 match_id 和 PSK。它是同步计算、会卡主线程，所以先让 UI 画出这一步
      report('deriving')
      await (this.deps.yieldToPaint ?? nextPaint)()
      let matchId: string
      let pskBytes: Uint8Array
      try {
        const secrets = crypto.derivePairSecrets(normalized)
        matchId = secrets.matchId
        pskBytes = psk = secrets.psk
      } catch (e) {
        // 短码前面已经校验过，走到这里是派生本身失败（比如分配不出 64 MiB 内存）
        throw new PairError('handshake_failed', `could not derive the pairing keys: ${(e as Error).message}`)
      }

      mb = new Mailbox<PairEvent>()
      this.pairMailbox = mb

      report('matching')
      if (!this.rawSend(makeEnvelope('', '', 'control', ++this.seq, encodeControl({ type: 'pair_join', match_id: matchId })))) throw new PairError('hub_unavailable')

      const matched = await this.expect(mb, ['matched', 'failed'], PAIR_STEP_TIMEOUT_MS)
      if (matched.t === 'failed') throw new PairError(matched.reason)
      if (matched.t !== 'matched' || matched.role !== 'joiner') throw new PairError('handshake_failed', 'unexpected role')
      const peer = matched.peer
      this.pairPeer = peer

      report('handshaking')
      let hello: NodeHello = {}
      let hs: Handshake | null = this.crypto.pairing('initiator', this.keypair, pskBytes)
      pskBytes.fill(0) // 握手对象已经拷走了它
      psk = null
      let transport: Transport | null = null
      try {
        this.sendRaw(peer, 'handshake', hs.write())
        const m2 = await this.expect(mb, ['handshake', 'failed'], PAIR_STEP_TIMEOUT_MS)
        if (m2.t === 'failed') throw new PairError(m2.reason)
        if (m2.t !== 'handshake') throw new PairError('handshake_failed')
        try {
          // 消息 2 的 payload 是 daemon 的自我介绍 {name, version}（此时还没被 PSK 证实，只当作后备）；
          // 消息 3 的 payload 是我们的 ClientInfo（daemon 靠它知道设备显示名）
          hello = parseNodeHello(hs.read(m2.body))
          this.sendRaw(peer, 'handshake', hs.write(encodeJson(this.clientInfo())))
        } catch (e) {
          if (e instanceof PairError) throw e
          throw new PairError('handshake_failed', (e as Error).message)
        }
        if (!hs.isFinished()) throw new PairError('handshake_failed', 'handshake incomplete')
        const remote = hs.remotePublic()
        if (!remote) throw new PairError('handshake_failed', 'no remote key')
        // 这条 PSK 绑定的传输是校验 `pair.ack` 的唯一依据
        transport = hs.intoTransport()
        hs = null

        report('confirming')
        this.rawSend(makeEnvelope('', '', 'control', ++this.seq, encodeControl({ type: 'pair_confirm', peer })))
        const done = await this.expect(mb, ['done', 'failed'], PAIR_STEP_TIMEOUT_MS)
        if (done.t === 'failed') throw new PairError(done.reason)
        if (done.t !== 'done') throw new PairError('handshake_failed')

        return await this.finishPairing(mb, done, transport, remote, hello.name, report)
      } finally {
        hs?.free()
        transport?.free()
      }
    } catch (e) {
      if (e instanceof PairError) throw e
      if (e instanceof MailboxTimeout) throw new PairError('timeout')
      if (e instanceof MailboxClosed) throw new PairError('hub_unavailable')
      throw e
    } finally {
      psk?.fill(0)
      this.pairing = false
      this.pairPeer = null
      this.pairDonePeer = null
      mb?.close()
      if (mb && this.pairMailbox === mb) this.pairMailbox = null
    }
  }

  /**
   * 配对 / 接入的收尾（`pair_done` 之后）：落盘新身份、核对已有节点的公钥、等 daemon 经这条 PSK 绑定的传输发来的
   * `pair.ack` 并校验，全部通过才登记节点。`pair()` 和 `enroll()` 共用——两条路径的落盘和钉公钥规则一字不差（ADR-36）。
   * 失败时不留下节点；身份可能已经升级成正式设备（hub 已经签发了 token），这是对的，下次重试不用再临时登录。
   */
  private async finishPairing(
    mb: Mailbox<PairEvent>,
    done: PairDoneEvent,
    transport: Transport,
    remote: Uint8Array,
    helloName: string | undefined,
    report: (stage: 'verifying' | 'done') => void,
  ): Promise<PairResult> {
    const crypto = this.crypto
    if (!this.identity || !crypto) throw new PairError('no_identity')
    const id = this.identity
    const issued = !!done.deviceToken && done.deviceToken !== id.device_token
    const next: db.IdentityRecord = {
      ...id,
      device_id: done.deviceId ?? id.device_id,
      device_token: done.deviceToken ?? id.device_token,
      // 新 token 是此刻签发的；沿用旧 token 就沿用它的签发时间
      ...(issued ? { device_token_issued_at_ms: Date.now() } : {}),
    }
    if (!next.device_token) throw new PairError('no_identity', 'hub did not issue a device token')
    this.identity = next
    // PairDone 之后同一条连接就是正式设备：后续信封的 from 立刻换成正式 id（旧的临时 id 不再有效）
    if (done.deviceId) this.deviceId = done.deviceId
    await this.deps.storage.saveIdentity(next)
    // 注意：host 若是还没有正式身份的 daemon，PairMatched.peer 是它的临时 id，PairDone.peer 才是正式 id
    const nodeId = done.peer
    const myId = this.deviceId

    // 已有同 id 的节点而公钥不同：只能让用户先移除，绝不静默覆盖固定的公钥
    const existing = (await this.deps.storage.loadNodes()).find((n) => n.node_id === nodeId)
    if (existing && !bytesEqual(existing.public_key, remote)) throw new PairError('key_changed')

    report('verifying')
    const ack = await this.awaitPairAck(mb, transport, nodeId, myId, crypto.fingerprint(remote))

    const now = Date.now()
    const rec: db.NodeRecord = {
      node_id: nodeId,
      // 认证过的名字（ack）优先于握手消息 2 里没被证实的自我介绍
      name: cleanName(ack.name) || helloName || `Node ${nodeId.slice(0, 8)}`,
      public_key: remote,
      paired_at: existing?.paired_at ?? now,
      last_seen: now,
    }
    await this.deps.storage.saveNode(rec)
    this.sink.nodeAdded?.(rec)
    this.attachNode(rec)
    this.publishHub()
    report('done')
    this.pairing = false
    this.pairMailbox = null
    // hub 会紧接着发 PeerOnline（每次 PeerOnline 都意味着重新握手）；旧版本 hub 不发的话，1.5 秒后自己发起 IK 握手。
    setTimeout(() => {
      const l = this.links.get(nodeId)
      // 这 1.5 秒里设备可能已被吊销、正以临时连接重新配对：那时发 IK 会混进配对握手
      if (l && !l.machine.peerOnline && this.canLinkSend(nodeId)) l.peerOnline()
    }, 1500)
    return { nodeId, publicKey: remote }
  }

  /**
   * 等 daemon 经配对传输发来的第一条通知 `pair.ack` 并校验。
   * 解不开的 `data`（过期链路上的残留流量、hub 伪造的垃圾）直接忽略，继续等到超时；
   * 解得开但内容对不上 → `ack_invalid`。
   */
  private async awaitPairAck(mb: Mailbox<PairEvent>, transport: Transport, nodeId: string, myId: string | null, fingerprint: string): Promise<PairAck> {
    const deadline = Date.now() + PAIR_ACK_TIMEOUT_MS
    const reassembler = new Reassembler()
    for (;;) {
      const ev = await this.expect(mb, ['data', 'failed'], Math.max(1, deadline - Date.now()))
      if (ev.t === 'failed') throw new PairError(ev.reason)
      if (ev.t !== 'data' || ev.from !== nodeId) continue
      let plain: Uint8Array
      try {
        const ciphertext = reassembler.push(decodeFrame(ev.body))
        if (!ciphertext) continue
        plain = transport.decrypt(ciphertext)
      } catch (e) {
        console.warn('anycon: ignoring a data frame that does not belong to the pairing transport', e)
        reassembler.clear()
        continue
      }
      const ack = parsePairAck(plain)
      if (!ack) throw new PairError('ack_invalid', 'the first message after pairing is not a valid pair.ack')
      if (!myId || ack.peer_id !== myId) throw new PairError('ack_invalid', 'pair.ack peer_id does not match this device')
      if (ack.node_id !== nodeId) throw new PairError('ack_invalid', 'pair.ack node_id does not match pair_done')
      if (ack.fingerprint !== fingerprint) throw new PairError('ack_invalid', 'pair.ack fingerprint does not match the handshake key')
      return ack
    }
  }

  cancelPairing(): void {
    this.pairMailbox?.close()
  }

  // ---- 账号内接入（ADR-36）：本设备是 Noise initiator ----

  /**
   * 对账号名下的一台 daemon 发起接入（不用短码）。本设备已经登录账号：没有 device token 时就是 hub 的临时连接
   * （升级请求自动带登录 cookie），已经是设备时直接用现在的连接。
   *
   * 流程见 `./enroll.ts`；握手成功、`pair_done` 之后的收尾和 `pair()` 是同一段代码（`finishPairing`）：
   * 校验 `pair.ack`、核对已有节点的公钥、全部通过才落盘。同一时刻只能有一个配对或接入。
   * 失败抛 `EnrollError`；用户取消（`cancelEnroll()`）是 `code = 'cancelled'`。
   */
  async enroll(req: EnrollRequest, hooks: EnrollHooks): Promise<PairResult> {
    if (this.pairing) throw new EnrollError('cancelled', 'another pairing or enrollment is in progress')
    if (!this.started || !this.identity || !this.crypto || !this.keypair) throw new EnrollError('no_identity')
    const crypto = this.crypto
    const keypair = this.keypair
    this.pairing = true
    const run = new EnrollRun()
    this.enrollRun = run
    let mb: Mailbox<PairEvent> | null = null
    let outcomeTransport: Transport | null = null
    let finished = false
    try {
      hooks.stage('connecting')
      // 设备被吊销后 token 已清掉：以临时连接重新上线才能接入
      if ((this.hubMachine.halt || this.suspended) && !this.ws) this.takeOver()
      // 设备名在 Auth 里带给 hub：改了名要重新认证一次
      if (req.deviceName && req.deviceName !== this.identity.device_name) {
        await this.setDeviceName(req.deviceName)
        if (this.ws) this.forceReconnect()
      }
      await this.waitAuthed(15_000, () => run.isCancelled)
      if (run.isCancelled) throw new EnrollError('cancelled')

      mb = new Mailbox<PairEvent>()
      this.pairMailbox = mb
      this.pairPeer = req.node
      const params: EnrollParams = {
        node: req.node,
        method: req.method,
        approval: req.approval ?? false,
        hint: { name: req.deviceName || this.identity.device_name, kind: 'web', platform: 'browser' },
        ...(req.password !== undefined ? { password: req.password } : {}),
      }
      const outcome = await runInitiator(
        {
          crypto,
          keypair,
          mailbox: mb,
          clientInfo: () => this.clientInfo(),
          sendControl: (msg) => this.rawSend(makeEnvelope('', '', 'control', ++this.seq, encodeControl(msg))),
          sendHandshake: (to, body) => this.rawSend(makeEnvelope(this.deviceId ?? '', to, 'handshake', ++this.seq, body)),
          yieldToPaint: () => (this.deps.yieldToPaint ?? nextPaint)(),
          now: () => Date.now(),
        },
        params,
        hooks,
        run,
      )
      outcomeTransport = outcome.transport
      // hub 在 pair_done 里报的对端必须就是我们要接入的那台：对不上说明 hub 在撒谎（pair.ack 也会挡住，这里早一步）
      if (outcome.done.peer !== req.node) throw new EnrollError('ack_invalid', 'pair_done is for a different node')
      const result = await this.finishPairing(mb, outcome.done, outcome.transport, outcome.remote, parseNodeHello(outcome.helloPayload).name, (stage) => hooks.stage(stage === 'verifying' ? 'finishing' : 'done'))
      finished = true
      return result
    } catch (e) {
      throw this.toEnrollError(e, run)
    } finally {
      outcomeTransport?.free()
      // 会话还在 hub 上（我们放弃 / 校验失败 / 超时）：告诉 hub 和 daemon，别让它占着名额到过期。hub 自己结束的不用发
      if (!finished && run.enrollId && !run.hubEnded) {
        this.staleEnrollIds.push(run.enrollId)
        if (this.staleEnrollIds.length > 16) this.staleEnrollIds.shift()
        this.rawSend(makeEnvelope('', '', 'control', ++this.seq, encodeControl({ type: 'enroll_cancel', enroll_id: run.enrollId })))
      }
      this.pairing = false
      this.pairPeer = null
      this.pairDonePeer = null
      mb?.close()
      if (mb && this.pairMailbox === mb) this.pairMailbox = null
      if (this.enrollRun === run) this.enrollRun = null
    }
  }

  /** 放弃进行中的接入（离开页面、切换入口）：`enroll()` 以 `cancelled` 结束，并向 hub 发 `enroll_cancel`。没有进行中的接入时什么都不做。 */
  cancelEnroll(): void {
    const run = this.enrollRun
    if (!run) return
    run.cancel()
    this.pairMailbox?.close()
  }

  private toEnrollError(e: unknown, run: EnrollRun): unknown {
    if (e instanceof EnrollError) return e
    if (e instanceof PairError) return new EnrollError(e.code, e.message)
    if (e instanceof MailboxTimeout) return new EnrollError('timeout')
    if (e instanceof MailboxClosed) return run.isCancelled ? new EnrollError('cancelled') : new EnrollError('hub_unavailable')
    return e
  }

  private async expect(mb: Mailbox<PairEvent>, types: string[], timeoutMs: number): Promise<PairEvent> {
    try {
      return await mb.next(types, timeoutMs)
    } catch (e) {
      if (e instanceof MailboxTimeout) throw new PairError('timeout')
      if (e instanceof MailboxClosed) throw new PairError('hub_unavailable')
      throw e
    }
  }

  /** 配对阶段的 handshake 信封：绕过 canSend 的 nonce 约束，直接发。 */
  private sendRaw(to: string, kind: Envelope['kind'], body: Uint8Array): void {
    const ok = this.rawSend(makeEnvelope(this.deviceId ?? '', to, kind, ++this.seq, body))
    if (!ok) throw new PairError('hub_unavailable')
  }

  private waitAuthed(timeoutMs: number, aborted?: () => boolean): Promise<void> {
    if (this.hubMachine.phase === 'hub_authed' && this.ws?.readyState === WS_OPEN && this.wsAuthName === this.identity?.device_name) return Promise.resolve()
    return new Promise<void>((resolve, reject) => {
      const t0 = Date.now()
      const tick = (): void => {
        if (this.hubMachine.phase === 'hub_authed' && this.ws?.readyState === WS_OPEN && this.wsAuthName === this.identity?.device_name) return resolve()
        if (aborted?.()) return reject(new PairError('cancelled'))
        if (this.hubMachine.halt) return reject(new PairError('hub_unavailable', `hub halted: ${this.hubMachine.halt}`))
        if (Date.now() - t0 > timeoutMs) return reject(new PairError('hub_unavailable', 'hub connection timed out'))
        setTimeout(tick, 100)
      }
      tick()
    })
  }
}
