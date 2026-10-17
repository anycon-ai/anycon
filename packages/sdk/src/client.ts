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
import { AnyconError, NodeRejected, PairingRequired, ProtocolMismatch } from './errors.js'
import { HubConnection, type BackoffOptions } from './hub.js'
import { NodeLink } from './link.js'
import { Node, type NodeOptions } from './node.js'
import type { PermissionPolicy } from './permissions.js'
import { detectPlatform } from './runtime.js'
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

export interface ClientEvents {
  /** hub 认证成功（含每次重连后）。 */
  connected: []
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
  ) {
    super()
    this.#identityStored = identityStored
    this.hubUrl = hubUrl
    this.#hub = hub
    this.#store = store
    this.#crypto = crypto
    this.#keypair = keypair
    this.#options = options
    this.#envNode = envNode

    hub.on('authed', (ok) => {
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
    hub.on('disconnected', ({ willReconnect }) => {
      this.#onlinePeers.clear()
      for (const link of this.#links.values()) link.onHubDown()
      this.emit('disconnected', willReconnect)
    })
    hub.on('fatal', (err) => {
      this.#fatal = err
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
    const client = new Client(hubUrl, hub, store, crypto, keypair, options, env.node, !env.identity)
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
