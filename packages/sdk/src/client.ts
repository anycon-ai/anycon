/**
 * `Client`：一个 hub 连接（一台设备，单设备单连接）+ 到若干节点的端到端通道。
 * `connect()` 是「一个 Client + 一个 Node」的快捷方式；同时连多个节点就用 `Client.open()` 再多次 `client.node()`。
 */
import {
  bytesOfHex,
  loadCrypto,
  type ClientInfo,
  type Crypto,
  type Keypair,
} from '@anycon/protocol'

import { defaultStorage } from './default-storage.js'
import { Emitter } from './emitter.js'
import {
  AnyconError,
  ConnectionError,
  NodeRejected,
  PairingRequired,
  ProtocolMismatch,
} from './errors.js'
import { HubConnection, type BackoffOptions } from './hub.js'
import { NodeLink } from './link.js'
import { Node, type NodeOptions } from './node.js'
import type { PermissionPolicy } from './permissions.js'
import { detectPlatform } from './runtime.js'
import {
  ROTATE_CLOSE_WAIT_MS,
  ROTATE_RECHECK_MS,
  ROTATE_RETRY_MS,
  ROTATE_TIMEOUT_MS,
  TOKEN_MAX_AGE_MS,
  isTokenDue,
  nextRotationDelay,
} from './rotation.js'
import { StateStore, envCredentials, type SdkStorage, type StoredNode } from './storage.js'
import { SDK_VERSION } from './version.js'
import { toWsUrl, type WebSocketFactory } from './ws.js'

export interface ClientOptions {
  /** 身份 / 节点存储。默认：Node 下 `~/.anycon/sdk/<hub-host>/<profile>/`，浏览器下 IndexedDB。 */
  storage?: SdkStorage
  /** 存储 profile，默认 `ANYCON_PROFILE` 或 `default`。 */
  profile?: string
  /** 覆盖自报的设备信息（`kind` 固定为 `sdk`）。 */
  clientInfo?: Partial<Omit<ClientInfo, 'kind'>>
  /** 默认的权限策略。 */
  onPermission?: PermissionPolicy
  /** SDK 代答 deny 前等多久（只对 SDK 自己在用的会话）。默认 10 分钟。 */
  permissionTimeoutMs?: number
  /** 掉线后自动重连，默认 true（指数退避 1s → 30s）。 */
  reconnect?: boolean | BackoffOptions
  /** RPC 请求默认超时。默认 30 秒。 */
  requestTimeoutMs?: number
  /** 等握手完成的上限。默认 20 秒。 */
  connectTimeoutMs?: number
  /** 单次握手等回复的时间。默认 5 秒；重试 `handshakeAttempts` 次。 */
  handshakeTimeoutMs?: number
  handshakeAttempts?: number
  /**
   * 保留最近发出的信封原文的字节上限（hub 退回 `backpressure` 时按原样重发它们）。默认 16 MiB，协议要求不低于
   * 8 MiB。超过上限时 SDK 暂停发新的信封（把上限当作在途窗口）直到最老的一批过了保留期，不会逐出还在路上的；
   * 调小只会让大消息发得更慢。主要给测试用。
   */
  resendBufferBytes?: number
  /** 发出不满这么久的信封永不逐出（它们可能还在路上），毫秒。默认 10000。主要给测试用。 */
  resendRetainMs?: number
  /**
   * 设备 token 轮换（docs/spec/sdk.md § 2）。默认：身份由 SDK 自己存（`pair()` 写的、或你给的 `storage`）时，
   * token 满 30 天（或没有签发时间的老记录）在连上 hub 后自动轮换并写回存储；凭据来自环境变量
   * （`ANYCON_DEVICE_TOKEN`）时不自动轮换——SDK 没法保存新 token，要轮换请自己调 `client.rotateToken()` 并保存结果。
   * 除 `auto` 外的几项主要给测试用。
   */
  tokenRotation?: {
    /** 设成 `false` 关闭自动轮换（默认 `true`；环境变量凭据不论如何都不自动轮换）。 */
    auto?: boolean
    /** token 多久算该轮换，毫秒。默认 30 天。 */
    maxAgeMs?: number
    /** 发出请求后没等到回复，多久再发一次，毫秒。默认 75 秒（头 3 次发送之间的间隔）。 */
    retryAfterMs?: number
    /** 头几次发送之后（以及 token 还不到期时）复查的间隔，毫秒。默认 1 小时。 */
    recheckMs?: number
    /** `close()` 最多多等多久，让刚发出的轮换请求的回复落盘，毫秒。默认 2 秒。 */
    closeWaitMs?: number
  }
  /**
   * 设备 token 轮换成功时调用（和 `tokenRotated` 事件同一时刻、同一个参数）。凭据来自环境变量、由你保存新 token 时用它——
   * 在 `connect()` / `Client.open()` 里直接传，不会错过 `open` 返回之前就发生的轮换。回调里抛错或返回的 promise 被拒都会被吞掉。
   */
  onTokenRotated?: (rotation: TokenRotation) => void | Promise<void>
  /** 注入 Noise 实现（默认 wasm）。 */
  crypto?: Crypto
  /** 注入 WebSocket 构造（默认：浏览器原生 / Node 用 `ws`）。 */
  webSocket?: WebSocketFactory
  /**
   * WebSocket 升级请求附带的 HTTP 头（仅 Node / Bun；浏览器忽略）。
   * 用途：hub 前面的反向代理要求的头（`CF-Access-Client-Id` 等）。已配对的设备用 token 认证，不需要 cookie。
   */
  headers?: Record<string, string>
}

/** 一次设备 token 轮换的结果（`client.rotateToken()` 的返回值，也是 `tokenRotated` 事件的参数）。 */
export interface TokenRotation {
  /** hub 签发的新设备 token。敏感；旧 token 在轮换 60 秒后失效。 */
  deviceToken: string
  /** 新 token 的签发时间（SDK 收到它的时刻），Unix 毫秒。 */
  issuedAtMs: number
  /**
   * SDK 是否已把新 token 写进自己的存储。`false` 表示要你自己保存：凭据来自环境变量，或写存储时出错（见 `persistError`）。
   * 没保存就丢了新 token 的话，旧 token 一失效这台设备就得重新配对。
   */
  persisted: boolean
  /** 写存储失败时的错误；这时新 token 仍在内存里，本进程的重连会用它。 */
  persistError?: Error
}

export interface ClientEvents {
  /** hub 认证成功（含每次重连后）。 */
  connected: []
  /**
   * 设备 token 轮换成功（自动的、或 `rotateToken()` 触发的）。凭据由你自己管理（环境变量）时在这里保存新 token。
   */
  tokenRotated: [TokenRotation]
  /** hub 连接断开。 */
  disconnected: [willReconnect: boolean]
  /** 不会再恢复的错误（被顶掉、设备被吊销、token 失效）。 */
  error: [Error]
}

export function buildClientInfo(overrides: ClientOptions['clientInfo'] = {}): ClientInfo {
  return {
    name: overrides.name ?? 'anycon-sdk',
    kind: 'sdk',
    platform: overrides.platform ?? detectPlatform(),
    version: overrides.version ?? SDK_VERSION,
    ...(overrides.origin !== undefined ? { origin: overrides.origin } : {}),
  }
}

export class Client extends Emitter<ClientEvents> {
  readonly hubUrl: string
  readonly #hub: HubConnection
  readonly #store: StateStore
  readonly #crypto: Crypto
  readonly #keypair: Keypair
  readonly #options: ClientOptions
  readonly #envNode: StoredNode | undefined
  readonly #nodes = new Map<string, Node>()
  readonly #links = new Map<string, NodeLink>()
  readonly #onlinePeers = new Set<string>()
  readonly #identityStored: boolean
  readonly #autoRotate: boolean
  readonly #rotation: {
    maxAgeMs: number
    retryAfterMs: number
    recheckMs: number
    closeWaitMs: number
  }
  #tokenIssuedAtMs: number | undefined
  #rotateTimer: ReturnType<typeof setTimeout> | undefined
  /** 这条 hub 连接上已经发了几次 `rotate_token`（新连接清零）。 */
  #rotateSends = 0
  /** 最近一次 `rotate_token` 的发出时刻；回复处理完（或连接没了）清掉。 */
  #rotateSentAt: number | undefined
  /** 正在落盘的 `token_rotated` 个数。 */
  #persisting = 0
  /** `close()` 在等回复时留下的唤醒函数。 */
  #rotationWaiters: Array<() => void> = []
  #pendingRotation:
    | {
        promise: Promise<TokenRotation>
        resolve: (r: TokenRotation) => void
        reject: (e: Error) => void
        timer: ReturnType<typeof setTimeout>
      }
    | undefined
  #fatal: Error | undefined
  #closed = false

  private constructor(
    hubUrl: string,
    hub: HubConnection,
    store: StateStore,
    crypto: Crypto,
    keypair: Keypair,
    options: ClientOptions,
    envNode: StoredNode | undefined,
    identityStored: boolean,
    tokenIssuedAtMs: number | undefined,
  ) {
    super()
    this.#identityStored = identityStored
    this.#tokenIssuedAtMs = tokenIssuedAtMs
    // 只有 SDK 自己存着身份时才自动轮换：环境变量里的 token 换了没地方存，换完旧的还作废，等于把设备锁在外面。
    this.#autoRotate = identityStored && options.tokenRotation?.auto !== false
    this.#rotation = {
      maxAgeMs: options.tokenRotation?.maxAgeMs ?? TOKEN_MAX_AGE_MS,
      retryAfterMs: options.tokenRotation?.retryAfterMs ?? ROTATE_RETRY_MS,
      recheckMs: options.tokenRotation?.recheckMs ?? ROTATE_RECHECK_MS,
      closeWaitMs: options.tokenRotation?.closeWaitMs ?? ROTATE_CLOSE_WAIT_MS,
    }
    this.hubUrl = hubUrl
    this.#hub = hub
    this.#store = store
    this.#crypto = crypto
    this.#keypair = keypair
    this.#options = options
    this.#envNode = envNode

    hub.on('authed', (ok) => {
      this.#rotateSends = 0
      this.#clearRotateTimer()
      if (this.#autoRotate) this.#checkRotation()
      this.#onlinePeers.clear()
      for (const id of ok.peers_online) this.#onlinePeers.add(id)
      for (const link of this.#links.values()) link.onHubAuthed(ok.peers_online)
      this.emit('connected')
    })
    hub.on('control', (ctrl) => {
      if (ctrl.type === 'peer_online') this.#onlinePeers.add(ctrl.device)
      else if (ctrl.type === 'peer_offline') this.#onlinePeers.delete(ctrl.device)
      for (const link of this.#links.values()) link.onControl(ctrl)
    })
    hub.on('envelope', (env) => this.#links.get(env.from)?.onEnvelope(env))
    hub.on('tokenRotated', (deviceToken) => void this.#onTokenRotated(deviceToken))
    hub.on('disconnected', ({ willReconnect }) => {
      this.#stopRotation(
        new ConnectionError(
          'the hub connection dropped before the token rotation was confirmed; ' +
            'the hub may or may not have rotated the token, the previous token stays valid for 60 s: reconnect and rotate again',
        ),
      )
      this.#onlinePeers.clear()
      for (const link of this.#links.values()) link.onHubDown()
      this.emit('disconnected', willReconnect)
    })
    hub.on('fatal', (err) => {
      this.#fatal = err
      this.#stopRotation(err)
      if (err instanceof NodeRejected) void this.#forgetRevokedIdentity()
      for (const link of this.#links.values()) link.onHubDown()
      this.emit('error', err)
    })
  }

  /**
   * 读取本地身份，连接 hub 并认证。没有身份抛 `PairingRequired`；
   * hub 不认 token 抛 `PairingRequired`，设备被吊销抛 `NodeRejected`，协议不兼容抛 `ProtocolMismatch`。
   */
  static async open(hubUrl: string, options: ClientOptions = {}): Promise<Client> {
    const storage = options.storage ?? (await defaultStorage(hubUrl, options.profile))
    const store = new StateStore(storage)
    const env = envCredentials()
    const stored = await store.getIdentity()
    const secretKey = env.identity?.secretKey ?? stored?.secretKey
    const deviceToken = env.identity?.deviceToken ?? stored?.deviceToken
    if (!secretKey || !deviceToken) throw new PairingRequired()
    const crypto = options.crypto ?? (await loadCrypto())
    const keypair = crypto.keypairFromSecret(bytesOfHex(secretKey))
    const deviceId = env.identity?.deviceId ?? stored?.deviceId
    const hub = new HubConnection({
      url: toWsUrl(hubUrl),
      token: deviceToken,
      ...(deviceId ? { deviceId } : {}),
      client: buildClientInfo(options.clientInfo),
      reconnect: options.reconnect ?? true,
      ...(options.webSocket ? { webSocket: options.webSocket } : {}),
      ...(options.headers ? { headers: options.headers } : {}),
    })
    const client = new Client(
      hubUrl,
      hub,
      store,
      crypto,
      keypair,
      options,
      env.node,
      !env.identity,
      env.identity ? undefined : stored?.tokenIssuedAtMs,
    )
    try {
      await hub.start()
    } catch (err) {
      hub.close()
      if (err instanceof NodeRejected) await client.#forgetRevokedIdentity()
      throw err
    }
    return client
  }

  /** hub 分配的本设备 id。 */
  get deviceId(): string {
    return this.#hub.deviceId
  }

  /** hub 连接是否已认证。 */
  get connected(): boolean {
    return this.#hub.ready
  }

  /** 不会再恢复的错误；设置之后所有调用都会抛它。 */
  get fatalError(): Error | undefined {
    return this.#fatal
  }

  /** 已配对的节点（本地记录，不管在不在线）。 */
  async pairedNodes(): Promise<StoredNode[]> {
    const nodes = await this.#store.getNodes()
    if (this.#envNode && !nodes.some((n) => n.id === this.#envNode?.id)) nodes.push(this.#envNode)
    return nodes
  }

  /**
   * 连接一个已配对的节点：`selector` 是节点名或 id 前缀；只配对了一个节点时可省略。
   * 节点不在线时也返回（`node.online === false`，调用抛 `NodeOffline`，`waitOnline()` 等上线）。
   * 握手被拒抛 `NodeRejected`。同一个节点重复调用返回同一个 `Node`。
   */
  async node(selector?: string, options: NodeOptions = {}): Promise<Node> {
    if (this.#closed) throw new AnyconError('client_closed', 'the client has been closed')
    if (this.#fatal) throw this.#fatal
    const stored = selectNode(await this.pairedNodes(), selector)
    const existing = this.#nodes.get(stored.id)
    if (existing) return existing

    const link = new NodeLink(this.#hub, this.#crypto, this.#keypair, stored, {
      ...(this.#options.requestTimeoutMs !== undefined
        ? { requestTimeoutMs: this.#options.requestTimeoutMs }
        : {}),
      ...(this.#options.handshakeTimeoutMs !== undefined
        ? { handshakeTimeoutMs: this.#options.handshakeTimeoutMs }
        : {}),
      ...(this.#options.handshakeAttempts !== undefined
        ? { handshakeAttempts: this.#options.handshakeAttempts }
        : {}),
      ...(this.#options.resendBufferBytes !== undefined
        ? { resendBufferBytes: this.#options.resendBufferBytes }
        : {}),
      ...(this.#options.resendRetainMs !== undefined
        ? { resendRetainMs: this.#options.resendRetainMs }
        : {}),
    })
    const node = new Node(this, link, {
      ...(this.#options.onPermission ? { onPermission: this.#options.onPermission } : {}),
      ...(this.#options.permissionTimeoutMs !== undefined
        ? { permissionTimeoutMs: this.#options.permissionTimeoutMs }
        : {}),
      ...options,
    })
    this.#links.set(stored.id, link)
    this.#nodes.set(stored.id, node)

    const settled = waitSettled(link, this.#options.connectTimeoutMs ?? 20_000)
    link.onHubAuthed([...this.#onlinePeers])
    try {
      await settled
      if (link.ready) await node.info().catch(rethrowProtocol)
    } catch (err) {
      this.#drop(stored.id)
      throw err
    }
    return node
  }

  /**
   * 请求 hub 轮换本设备的 token，等到新 token 回来（默认最多 10 秒，`timeoutMs` 可改）并返回。
   *
   * - SDK 自己存着身份时，新 token 和签发时间会先写回存储，再用于之后的重连；凭据来自环境变量时 SDK 没有地方存，
   *   `persisted` 是 `false`——**你必须自己保存返回值里的 `deviceToken`**（或在 `tokenRotated` 事件里保存）。
   *   旧 token 在轮换 60 秒后失效，没保存就等于把这台设备锁在外面，只能重新配对。
   * - hub 对同一设备 1 分钟内的第二次请求不回复，这里表现为 `rotate_timeout`；没连上 hub 抛 `not_connected`。
   * - 已经有一个请求在等回复时返回同一个结果，不重复发。
   */
  rotateToken(options: { timeoutMs?: number } = {}): Promise<TokenRotation> {
    if (this.#closed)
      return Promise.reject(new AnyconError('client_closed', 'the client has been closed'))
    if (this.#fatal) return Promise.reject(this.#fatal)
    if (!this.#hub.ready) {
      return Promise.reject(
        new AnyconError('not_connected', 'not connected to the hub: wait for the connection first'),
      )
    }
    if (this.#pendingRotation) return this.#pendingRotation.promise
    const timeoutMs = options.timeoutMs ?? ROTATE_TIMEOUT_MS
    let resolve!: (r: TokenRotation) => void
    let reject!: (e: Error) => void
    const promise = new Promise<TokenRotation>((res, rej) => {
      resolve = res
      reject = rej
    })
    const timer = setTimeout(() => {
      if (this.#pendingRotation?.promise !== promise) return
      this.#pendingRotation = undefined
      reject(
        new AnyconError(
          'rotate_timeout',
          `the hub did not answer the token rotation request within ${Math.round(timeoutMs / 1000)} s ` +
            '(it ignores a second request within a minute of the previous rotation, and hubs without rotation never answer)',
        ),
      )
    }, timeoutMs)
    unrefTimer(timer)
    this.#pendingRotation = { promise, resolve, reject, timer }
    if (!this.#sendRotateRequest()) {
      clearTimeout(timer)
      this.#pendingRotation = undefined
      reject(
        new AnyconError('not_connected', 'not connected to the hub: wait for the connection first'),
      )
    }
    return promise
  }

  #sendRotateRequest(): boolean {
    try {
      this.#hub.sendControl({ type: 'rotate_token' })
      this.#rotateSentAt = Date.now()
      return true
    } catch {
      return false
    }
  }

  #clearRotateTimer(): void {
    if (this.#rotateTimer !== undefined) clearTimeout(this.#rotateTimer)
    this.#rotateTimer = undefined
  }

  #armRotateTimer(delayMs: number): void {
    this.#clearRotateTimer()
    this.#rotateTimer = setTimeout(() => this.#checkRotation(), delayMs)
    unrefTimer(this.#rotateTimer)
  }

  /** 停掉自动轮换的计时，并让还在等回复的 `rotateToken()` 以 `reason` 失败（连接没了，回复不会来）。 */
  #stopRotation(reason: Error): void {
    this.#clearRotateTimer()
    this.#rotateSentAt = undefined
    this.#wakeRotationWaiters()
    const pending = this.#pendingRotation
    if (!pending) return
    clearTimeout(pending.timer)
    this.#pendingRotation = undefined
    pending.reject(reason)
  }

  /**
   * 自动轮换的一次检查：token 到期就发 `rotate_token`（回复由 `#onTokenRotated` 处理），然后安排下一次——
   * 头几次间隔 `retryAfterMs`（回复可能丢了），之后和「还没到期」时一样按 `recheckMs` 复查（长连接不重连也要换）。
   */
  #checkRotation(): void {
    this.#rotateTimer = undefined
    if (this.#closed || this.#fatal || !this.#autoRotate || !this.#hub.ready) return
    const { maxAgeMs, retryAfterMs, recheckMs } = this.#rotation
    if (!isTokenDue(this.#tokenIssuedAtMs, Date.now(), maxAgeMs)) {
      this.#armRotateTimer(recheckMs)
      return
    }
    this.#sendRotateRequest()
    this.#rotateSends += 1
    this.#armRotateTimer(nextRotationDelay(this.#rotateSends, retryAfterMs, recheckMs))
  }

  /**
   * hub 回了新 token。先写存储（token 和签发时间一次写完）再换掉重连用的 token：写成功之前新 token 不会成为
   * 唯一的凭据。写失败也照样换——旧 token 60 秒后就失效，留着它只会让下次重连失败——并在事件里报告 `persisted: false`。
   * 超时之后才到的回复同样处理（hub 已经轮换过了，新 token 不能丢）。
   */
  async #onTokenRotated(deviceToken: string): Promise<void> {
    const issuedAtMs = Date.now()
    let persisted = false
    let persistError: Error | undefined
    this.#persisting += 1
    try {
      if (this.#identityStored) {
        try {
          persisted = await this.#store.rotateIdentityToken(deviceToken, issuedAtMs)
        } catch (cause) {
          persistError = cause instanceof Error ? cause : new Error(String(cause))
        }
      }
      this.#hub.setToken(deviceToken)
      this.#tokenIssuedAtMs = issuedAtMs
      this.#rotateSends = 0
    } finally {
      this.#persisting -= 1
      this.#rotateSentAt = undefined
      this.#wakeRotationWaiters()
    }
    const result: TokenRotation = {
      deviceToken,
      issuedAtMs,
      persisted,
      ...(persistError ? { persistError } : {}),
    }
    const pending = this.#pendingRotation
    if (pending) {
      clearTimeout(pending.timer)
      this.#pendingRotation = undefined
      pending.resolve(result)
    }
    try {
      const returned = this.#options.onTokenRotated?.(result)
      if (returned instanceof Promise) returned.catch(() => undefined)
    } catch {
      // 回调的异常不能影响连接
    }
    this.emit('tokenRotated', result)
  }

  #wakeRotationWaiters(): void {
    const waiters = this.#rotationWaiters
    this.#rotationWaiters = []
    for (const wake of waiters) wake()
  }

  /** `close()` 之前：刚发出的 `rotate_token` 还没收到 / 处理完回复时，最多再等一小会儿，免得新 token 丢掉。 */
  async #settleRotationBeforeClose(): Promise<void> {
    const { closeWaitMs } = this.#rotation
    const sent = this.#rotateSentAt
    const left =
      this.#persisting > 0 ? closeWaitMs : sent === undefined ? 0 : sent + closeWaitMs - Date.now()
    if (left <= 0) return
    await new Promise<void>((resolve) => {
      const timer = setTimeout(resolve, left)
      unrefTimer(timer)
      this.#rotationWaiters.push(() => {
        clearTimeout(timer)
        resolve()
      })
    })
  }

  /** 设备被 hub 吊销（4004 / auth_fail revoked）：token 已作废，清掉本地凭据，下次 connect 抛 PairingRequired。 */
  async #forgetRevokedIdentity(): Promise<void> {
    if (this.#identityStored) await this.#store.clearIdentity().catch(() => undefined)
  }

  /** @internal 补上节点名，下次可以用名字选节点。 */
  async rememberNodeName(id: string, name: string): Promise<void> {
    await this.#store.updateNode(id, { name }).catch(() => undefined)
  }

  /** @internal Node.close()。 */
  async release(node: Node, closeClient: boolean): Promise<void> {
    this.#drop(node.id)
    if (closeClient) await this.close()
  }

  async close(): Promise<void> {
    if (this.#closed) return
    this.#closed = true
    await this.#settleRotationBeforeClose()
    this.#stopRotation(new AnyconError('client_closed', 'the client has been closed'))
    for (const id of Array.from(this.#links.keys())) this.#drop(id)
    this.#hub.close()
  }

  #drop(id: string): void {
    this.#nodes.get(id)?.markClosed()
    this.#links.get(id)?.close()
    this.#links.delete(id)
    this.#nodes.delete(id)
  }
}

/** 按节点名（精确）或 id 前缀选节点。 */
export function selectNode(nodes: readonly StoredNode[], selector?: string): StoredNode {
  if (nodes.length === 0) {
    throw new PairingRequired('no node is paired with this device yet: call pair() first')
  }
  if (selector === undefined) {
    if (nodes.length === 1) return nodes[0]!
    const names = nodes.map((n) => n.name ?? n.id.slice(0, 8)).join(', ')
    throw new AnyconError(
      'node_required',
      `several nodes are paired (${names}); pass { node } to choose one`,
    )
  }
  const byName = nodes.filter((n) => n.name === selector)
  const matches = byName.length > 0 ? byName : nodes.filter((n) => n.id.startsWith(selector))
  if (matches.length === 1) return matches[0]!
  if (matches.length > 1) {
    throw new AnyconError(
      'node_ambiguous',
      `"${selector}" matches ${matches.length} paired nodes; use a longer id prefix`,
    )
  }
  const names = nodes.map((n) => n.name ?? n.id.slice(0, 8)).join(', ')
  throw new AnyconError('node_not_found', `no paired node matches "${selector}" (paired: ${names})`)
}

function rethrowProtocol(err: unknown): void {
  // info() 失败：协议不兼容要报出去；节点中途掉线之类忽略（connect 仍然成功，之后的调用自己会抛）。
  if (err instanceof ProtocolMismatch) throw err
}

/** 等通道落定：ready 或节点不在线（peer_offline）都算，noise_failed 抛错。 */
function waitSettled(link: NodeLink, timeoutMs: number): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    let timer: ReturnType<typeof setTimeout> | undefined
    let off: (() => void) | undefined
    const finish = (fn: () => void) => {
      off?.()
      if (timer !== undefined) clearTimeout(timer)
      fn()
    }
    const check = (): boolean => {
      switch (link.state) {
        case 'ready':
        case 'peer_offline':
          finish(resolve)
          return true
        case 'noise_failed':
          finish(() => reject(link.failure ?? new NodeRejected()))
          return true
        case 'closed':
          finish(() => reject(new AnyconError('client_closed', 'the client has been closed')))
          return true
        default:
          return false
      }
    }
    off = link.on('state', () => {
      check()
    })
    timer = setTimeout(
      () =>
        finish(() =>
          reject(new NodeRejected('timed out waiting for the Noise handshake with the node')),
        ),
      timeoutMs,
    )
    check()
  })
}

/** 计时器不拖住 Node 进程退出（浏览器的计时器没有 `unref`）。 */
function unrefTimer(timer: ReturnType<typeof setTimeout>): void {
  ;(timer as { unref?: () => void }).unref?.()
}
