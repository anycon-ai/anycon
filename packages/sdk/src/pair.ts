/**
 * 配对（docs/protocol.md § 4，ADR-29）：SDK 作为 joiner。
 *
 *   连 hub（device_token = None 的临时连接；已有身份则带 token）
 *   → PairJoin { match_id }（短码本身不发给 hub）→ PairMatched { peer, role = joiner }
 *   → Noise XXpsk3 initiator：msg1 →，← msg2，msg3 →（payload = 本设备的 ClientInfo JSON）
 *   （`match_id` 和 PSK 来自同一次 `derivePairSecrets(code)`：Argon2id，64 MiB × 3 轮，约 0.1–1 s，**只算一次**，
 *   在连 hub 之前算完，所以不会占着一条已认证的连接干等；用完 PSK 清零）
 *   → PairConfirm { peer } → PairDone { peer, device_id, device_token }
 *   → **等 daemon 经新建立的 Noise 传输发来 `pair.ack`**（最多 30 秒），校验：
 *       ack.peer_id == 自己的正式 id、ack.node_id == PairDone.peer、ack.fingerprint == 握手得到的节点公钥的指纹
 *   → 校验通过才写入本地存储：设备身份（私钥 + token）和节点公钥；已有同 id 节点但公钥不同 → `NodeKeyChanged`，不覆盖。
 *
 * 为什么要等 ack：XXpsk3 在第 3 条消息才混入 PSK，initiator 写完第 3 条就「完成」了，但还没收到过任何用 PSK 之后的
 * 密钥加密的东西。一个中间人 hub 可以自己回第 2 条消息、丢掉第 3 条、伪造 `PairDone`——它造不出 `pair.ack`。
 */
import {
  METHODS,
  Reassembler,
  bytesOfHex,
  decodeFrame,
  decodeRpc,
  extractPairCode,
  hexOf,
  isNotification,
  loadCrypto,
  type Crypto,
  type Handshake,
  type Keypair,
  type PairAck,
  type PairSecrets,
  type Transport,
} from '@anycon/protocol'

import { Client, buildClientInfo, selectNode, type ClientOptions } from './client.js'
import { defaultStorage } from './default-storage.js'
import { NodeRejected, PairingFailed, PairingRequired } from './errors.js'
import { HubConnection } from './hub.js'
import { StateStore, type SdkStorage, type StoredIdentity, type StoredNode } from './storage.js'
import { toWsUrl, type WebSocketFactory } from './ws.js'

/** 等 daemon 的 `pair.ack` 的默认时限，毫秒。 */
export const PAIR_ACK_TIMEOUT_MS = 30_000

export interface PairOptions {
  /**
   * 在节点上 `anycon pair` 得到的短码：默认是 16 位 `XXXX-XXXX-XXXX-XXXX`，`anycon pair --short` 是 8 位 `XXXX-XXXX`，
   * 大小写和连字符随意；也可以直接给配对链接（`https://hub.example.com/pair#code=XXXX-XXXX-XXXX-XXXX`）或 `#code=…`。
   */
  code: string
  /** 这台设备的显示名，节点上 `anycon devices` 里看到的就是它。 */
  name: string
  storage?: SdkStorage
  profile?: string
  clientInfo?: ClientOptions['clientInfo']
  crypto?: Crypto
  webSocket?: WebSocketFactory
  /**
   * WebSocket 升级请求附带的 HTTP 头（仅 Node / Bun；浏览器忽略）。
   *
   * 最重要的用途是**账号归属**：从未配对过的节点（daemon 第一次连 hub）在 hub 上还没有账号，而 SDK 作为
   * 临时连接没有 cookie，两边都没有账号时 hub 判 `forbidden`。第一次配对要带上管理员 / 用户的登录 cookie：
   * `headers: { Cookie: 'anycon_session=…' }`（`POST /auth/token` 拿，见 README「账号归属」）。
   * 节点有了账号之后，不带 cookie 的 SDK 配对会被 hub 归到节点的账号下。
   */
  headers?: Record<string, string>
  /** 整个配对的时限，毫秒。默认 90 秒（hub 自己 60 秒无动静就判超时）。 */
  timeoutMs?: number
  /** `PairDone` 之后等 daemon 的 `pair.ack` 的时限，毫秒。默认 30 秒；超时配对失败、什么都不落盘。 */
  ackTimeoutMs?: number
  signal?: AbortSignal
  /** 配对完成后连一次节点取 `node.info.name` 存下来，方便之后按名字选节点。默认 true。 */
  resolveNodeName?: boolean
}

export interface PairResult {
  /** 节点（daemon）的设备 id。 */
  nodeId: string
  /** 节点名；`resolveNodeName: false` 或节点没应答时为空。 */
  nodeName?: string
  /** 本设备在 hub 上的设备 id。 */
  deviceId: string
  /** 节点公钥指纹（`3f:a2:…`），可与节点上的 `anycon devices` / 日志对照。 */
  nodeFingerprint: string
  node: StoredNode
}

export async function pair(hubUrl: string, options: PairOptions): Promise<PairResult> {
  const code = extractPairCode(options.code)
  if (code === undefined) {
    throw new PairingFailed(
      'not_found',
      'not a valid pair code or pair link (16 characters by default, 8 with `anycon pair --short`, e.g. 7K3P-Q2MX-W4NH-8CRD, or https://hub/pair#code=7K3P-Q2MX-W4NH-8CRD)',
    )
  }
  const crypto = options.crypto ?? (await loadCrypto())
  const storage = options.storage ?? (await defaultStorage(hubUrl, options.profile))
  const store = new StateStore(storage)
  const existing = await store.getIdentity()

  // 一次 Argon2id 同时给出 match_id 和 PSK。身份作废后重试 attempt 时复用，不再算第二遍。
  let secrets: PairSecrets
  try {
    secrets = crypto.derivePairSecrets(code)
  } catch (cause) {
    throw new PairingFailed('not_found', 'cannot derive the pairing keys from this code', { cause })
  }
  let result: PairResult
  try {
    try {
      result = await attempt(hubUrl, options, secrets, crypto, store, existing)
    } catch (err) {
      // 本地 token 已经失效或设备被吊销（`auth_fail { invalid | revoked }` / close 4004）：作废的身份留着只会让
      // 每次 pair() 都卡在这里。清掉它，当作没配对过，用全新身份再试一次。
      if (existing && (err instanceof PairingRequired || err instanceof NodeRejected)) {
        await store.clearIdentity()
        result = await attempt(hubUrl, options, secrets, crypto, store, undefined)
      } else {
        throw err
      }
    }
  } finally {
    secrets.psk.fill(0)
  }

  if (options.resolveNodeName === false) return result
  return { ...result, ...(await resolveName(hubUrl, options, crypto, storage, result.nodeId)) }
}

async function attempt(
  hubUrl: string,
  options: PairOptions,
  secrets: PairSecrets,
  crypto: Crypto,
  store: StateStore,
  existing: StoredIdentity | undefined,
): Promise<PairResult> {
  const keypair = existing
    ? crypto.keypairFromSecret(bytesOfHex(existing.secretKey))
    : crypto.generateKeypair()
  const clientInfo = buildClientInfo({ ...options.clientInfo, name: options.name })
  const hub = new HubConnection({
    url: toWsUrl(hubUrl),
    ...(existing ? { token: existing.deviceToken, deviceId: existing.deviceId } : {}),
    client: clientInfo,
    reconnect: false,
    ...(options.webSocket ? { webSocket: options.webSocket } : {}),
    ...(options.headers ? { headers: options.headers } : {}),
  })
  try {
    await hub.start()
    return await join(hub, options, secrets, crypto, keypair, store, existing, clientInfo)
  } finally {
    hub.close()
  }
}

/** `pair.ack` 里的名字来自节点（已认证），但仍然去掉控制字符 / 转义序列并限长再存、再显示。 */
function cleanName(name: string): string {
  let out = ''
  for (const ch of name) {
    const code = ch.codePointAt(0) ?? 0
    if (code < 0x20 || (code >= 0x7f && code <= 0x9f)) continue // C0 / DEL / C1（含 ESC、NUL、CSI）
    out += ch
  }
  return out.trim().slice(0, 64)
}

/** 校验 `pair.ack` 的 params 形状（对端发来的东西不能信）。 */
function parseAck(params: unknown): PairAck | undefined {
  if (typeof params !== 'object' || params === null) return undefined
  const p = params as Record<string, unknown>
  const { node_id, peer_id, name, version, fingerprint } = p
  if (
    typeof node_id !== 'string' ||
    typeof peer_id !== 'string' ||
    typeof name !== 'string' ||
    typeof version !== 'string' ||
    typeof fingerprint !== 'string'
  ) {
    return undefined
  }
  return { node_id, peer_id, name, version, fingerprint }
}

/** 同一次配对里，pair_done 之前最多缓冲这么多封 data（hub 不保证 pair_done 一定先于 daemon 的第一封 data 到达）。 */
const EARLY_DATA_LIMIT = 16

function join(
  hub: HubConnection,
  options: PairOptions,
  secrets: PairSecrets,
  crypto: Crypto,
  keypair: Keypair,
  store: StateStore,
  existing: StoredIdentity | undefined,
  clientInfo: ReturnType<typeof buildClientInfo>,
): Promise<PairResult> {
  return new Promise<PairResult>((resolve, reject) => {
    const { matchId, psk } = secrets
    const ackTimeoutMs = options.ackTimeoutMs ?? PAIR_ACK_TIMEOUT_MS
    /** PairMatched.peer：握手信封的收件人（host 是首次连 hub 的 daemon 时是临时 id）。 */
    let peer: string | undefined
    let handshake: Handshake | undefined
    /** msg3 之后的新传输：只用来读 daemon 的 `pair.ack`，之后丢弃（重连走 IK）。 */
    let transport: Transport | undefined
    let nodePublic: Uint8Array | undefined
    let done: { peer: string; deviceId: string; deviceToken: string } | undefined
    const early: Uint8Array[] = []
    const reassembler = new Reassembler()
    let settled = false
    /** ack 校验通过、开始落盘之后，hub 断线 / 超时都不再影响结果。 */
    let committed = false
    let ackTimer: ReturnType<typeof setTimeout> | undefined
    const offs: (() => void)[] = []

    const settle = (fn: () => void) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      if (ackTimer !== undefined) clearTimeout(ackTimer)
      for (const off of offs) off()
      options.signal?.removeEventListener('abort', onAbort)
      try {
        handshake?.free()
      } catch {
        // 已被消费
      }
      try {
        transport?.free()
      } catch {
        // 已释放
      }
      fn()
    }
    const fail = (err: unknown) => {
      if (committed) return
      settle(() => reject(err))
    }
    const onAbort = () => fail(new PairingFailed('cancelled', 'pairing was cancelled'))
    const timer = setTimeout(() => fail(new PairingFailed('timeout')), options.timeoutMs ?? 90_000)
    if (options.signal?.aborted) {
      fail(new PairingFailed('cancelled', 'pairing was cancelled'))
      return
    }
    options.signal?.addEventListener('abort', onAbort, { once: true })

    const onHandshake = (body: Uint8Array) => {
      if (!handshake || !peer || nodePublic) return
      try {
        handshake.read(body) // msg2
        const info = new TextEncoder().encode(
          JSON.stringify({
            name: clientInfo.name,
            kind: clientInfo.kind,
            platform: clientInfo.platform,
            version: clientInfo.version,
          }),
        )
        hub.sendEnvelope(peer, 'handshake', handshake.write(info)) // msg3：PSK 在这里混入
        if (!handshake.isFinished()) throw new Error('handshake did not finish after message 3')
        transport = handshake.intoTransport()
        handshake = undefined
        nodePublic = transport.remotePublic.slice()
        hub.sendControl({ type: 'pair_confirm', peer })
      } catch (cause) {
        fail(
          new PairingFailed(
            'handshake',
            `Noise handshake failed: ${cause instanceof Error ? cause.message : String(cause)}`,
            { cause },
          ),
        )
      }
    }

    const onDone = (msg: {
      peer: string
      device_id?: string | null
      device_token?: string | null
    }) => {
      // `PairDone.peer` 永远是对方的**正式** id；`PairMatched.peer`（握手信封的收件人）在 host 是第一次连 hub 的
      // daemon（还没有 token）时是临时 id，两者不同——节点从此以 `PairDone.peer` 为键。
      // 但 hub 是不可信的：这个 id 要等 daemon 的 `pair.ack` 确认之后才算数。
      if (!nodePublic || !transport) {
        fail(new PairingFailed('protocol', 'pair_done arrived before the Noise handshake finished'))
        return
      }
      if (done) return
      const deviceToken = msg.device_token ?? existing?.deviceToken
      const deviceId = msg.device_id ?? existing?.deviceId ?? hub.deviceId
      if (!deviceToken) {
        fail(new PairingFailed('protocol', 'the hub did not issue a device token'))
        return
      }
      if (msg.device_id) hub.adoptDeviceId(msg.device_id)
      done = { peer: msg.peer, deviceId, deviceToken }
      ackTimer = setTimeout(
        () =>
          fail(
            new PairingFailed(
              'timeout',
              `the node did not confirm the pairing (no pair.ack within ${Math.round(ackTimeoutMs / 1000)}s); nothing was saved`,
            ),
          ),
        ackTimeoutMs,
      )
      for (const body of early.splice(0)) onAckData(body)
    }

    /** daemon → 新传输 → `pair.ack`。只认 `PairDone.peer` 发来的第一条消息。 */
    const onAckData = (body: Uint8Array) => {
      if (settled || committed || !done || !transport || !nodePublic) return
      let ack: PairAck | undefined
      try {
        const frame = decodeFrame(body)
        if (frame.kind === 'reset') {
          fail(new PairingFailed('protocol', 'the node reset the transport instead of confirming'))
          return
        }
        const ciphertext = reassembler.push(frame)
        if (!ciphertext) return // 等后续分片
        const msg = decodeRpc(transport.decrypt(ciphertext))
        if (!isNotification(msg) || msg.method !== METHODS.PAIR_ACK) {
          fail(new PairingFailed('protocol', 'the first message from the node is not pair.ack'))
          return
        }
        ack = parseAck(msg.params)
      } catch (cause) {
        fail(
          new PairingFailed(
            'handshake',
            `cannot read the node's pair.ack (wrong key, or someone in the middle): ${cause instanceof Error ? cause.message : String(cause)}`,
            { cause },
          ),
        )
        return
      }
      if (!ack) {
        fail(new PairingFailed('protocol', 'pair.ack is malformed'))
        return
      }
      const mismatch = ackMismatch(ack, done, crypto.fingerprint(nodePublic))
      if (mismatch) {
        fail(
          new PairingFailed(
            'protocol',
            `pair.ack does not match what the hub reported (${mismatch}); nothing was saved`,
          ),
        )
        return
      }
      // 校验通过。先查节点公钥有没有被换（NodeKeyChanged），再落盘。
      committed = true
      if (ackTimer !== undefined) clearTimeout(ackTimer)
      clearTimeout(timer)
      const confirmed = done
      const publicKey = nodePublic
      void (async () => {
        try {
          const node: StoredNode = {
            id: confirmed.peer,
            name: cleanName(ack.name),
            publicKey: hexOf(publicKey),
            fingerprint: crypto.fingerprint(publicKey),
            pairedAtMs: Date.now(),
          }
          await store.checkNodeKey(node)
          const identity: StoredIdentity = {
            deviceId: confirmed.deviceId,
            deviceToken: confirmed.deviceToken,
            secretKey: hexOf(keypair.secret),
            name: options.name,
            createdAtMs: existing?.createdAtMs ?? Date.now(),
          }
          await store.setIdentity(identity)
          await store.pinNode(node)
          settle(() =>
            resolve({
              nodeId: node.id,
              ...(node.name ? { nodeName: node.name } : {}),
              deviceId: confirmed.deviceId,
              nodeFingerprint: node.fingerprint ?? '',
              node,
            }),
          )
        } catch (cause) {
          committed = false
          fail(cause)
        }
      })()
    }

    offs.push(
      hub.on('control', (ctrl) => {
        switch (ctrl.type) {
          case 'pair_matched':
            if (ctrl.role !== 'joiner') {
              fail(
                new PairingFailed(
                  'protocol',
                  'this SDK can only join a pairing, but the hub assigned the host role',
                ),
              )
              return
            }
            try {
              peer = ctrl.peer
              handshake = crypto.pairing('initiator', keypair, psk)
              hub.sendEnvelope(peer, 'handshake', handshake.write()) // msg1
            } catch (cause) {
              fail(new PairingFailed('handshake', 'cannot start the Noise handshake', { cause }))
            }
            return
          case 'pair_done':
            onDone(ctrl)
            return
          case 'pair_failed':
            fail(new PairingFailed(ctrl.reason))
            return
          case 'rate_limited':
            fail(
              new PairingFailed(
                'rate_limited',
                `rate limited by the hub; retry in ${ctrl.retry_after_ms}ms`,
              ),
            )
            return
          default:
            return
        }
      }),
      hub.on('envelope', (env) => {
        if (env.kind === 'handshake' && env.from === peer) onHandshake(env.body)
        else if (env.kind === 'data') {
          // 不按 `from` 过滤：`from` 是 hub 填的，不可信；能用握手得到的传输解开、且内容对得上才算数。
          if (!done) {
            if (early.length < EARLY_DATA_LIMIT) early.push(env.body)
          } else onAckData(env.body)
        }
      }),
      hub.on('disconnected', ({ code: closeCode }) =>
        fail(
          new PairingFailed(
            'peer_gone',
            `hub connection closed during pairing (code ${closeCode})`,
          ),
        ),
      ),
      hub.on('fatal', fail),
    )

    try {
      hub.sendControl({ type: 'pair_join', match_id: matchId })
    } catch (err) {
      fail(err)
    }
  })
}

/** `pair.ack` 与 hub 报告的是否一致；一致返回 undefined，否则说明哪一项不符。 */
function ackMismatch(
  ack: PairAck,
  done: { peer: string; deviceId: string },
  fingerprint: string,
): string | undefined {
  if (ack.node_id !== done.peer) return 'node_id differs from pair_done.peer'
  if (ack.peer_id !== done.deviceId) return 'peer_id is not this device'
  if (ack.fingerprint.toLowerCase() !== fingerprint.toLowerCase()) {
    return 'fingerprint differs from the key presented in the handshake'
  }
  return undefined
}

export interface ForgetNodeOptions {
  storage?: SdkStorage
  profile?: string
}

/**
 * 忘掉本地记录的某个节点（节点名或 id 前缀，规则同 `connect({ node })`），返回被删的记录。
 * 用在 `NodeKeyChanged` 之后：确认节点的新公钥可信（比如节点重装了）时先忘掉旧记录，再 `pair()`。
 * 不会通知节点或 hub；节点那边是否还认这台设备另说。
 */
export async function forgetNode(
  hubUrl: string,
  node: string,
  options: ForgetNodeOptions = {},
): Promise<StoredNode> {
  const storage = options.storage ?? (await defaultStorage(hubUrl, options.profile))
  const store = new StateStore(storage)
  const stored = selectNode(await store.getNodes(), node)
  await store.removeNode(stored.id)
  return stored
}

async function resolveName(
  hubUrl: string,
  options: PairOptions,
  crypto: Crypto,
  storage: SdkStorage,
  nodeId: string,
): Promise<{ nodeName?: string }> {
  let client: Client | undefined
  try {
    client = await Client.open(hubUrl, {
      storage,
      crypto,
      reconnect: false,
      handshakeTimeoutMs: 3000,
      handshakeAttempts: 1,
      connectTimeoutMs: 8000,
      requestTimeoutMs: 5000,
      ...(options.webSocket ? { webSocket: options.webSocket } : {}),
      ...(options.headers ? { headers: options.headers } : {}),
      ...(options.clientInfo ? { clientInfo: options.clientInfo } : {}),
    })
    const node = await client.node(nodeId)
    if (!node.online) return {}
    const info = await node.info()
    return info.name ? { nodeName: info.name } : {}
  } catch {
    return {} // 取名字是锦上添花，失败不影响配对结果
  } finally {
    await client?.close()
  }
}
