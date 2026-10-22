/**
 * 用 TS 写的 fake daemon：Noise responder + 脚本化的 JSON-RPC 方法，用来端到端测 SDK。
 * 独立于 SDK 的传输层：直接用 `ws` + `@anycon/protocol` 的编解码，密码学由注入的 `Crypto`（wasm 或 mock）提供。
 *
 * 脚本化行为（看 `session.send` 的内容）：
 * - 默认：user_message → turn_start → thinking → text_delta ×2 → turn_done(usage)
 * - 含 "permission"：中间发一个 tool 权限请求，等 `permission.respond`；允许则 tool_call/tool_result，否则 "skipped"
 *   - 另含 "mode_change" / "plan"：请求的 kind 换成对应的（走 allowAll 之类自动放行的边界）
 *   - 另含 "always-only"：选项里没有 allow_once，只有 allow_always / reject_once（不能悄悄升级的边界）
 * - 含 "error"：发不可恢复的 error 事件
 * - 含 "slow"：每个事件之间等 60ms（给取消 / 重连留窗口）
 *
 * worktree（ADR-34）：`session.create` 带 `workspace = { mode: 'worktree' }` 时先答 `worktree.state = creating`，
 * 约 10 ms 后变 `active`（发 `git` 事件 `worktree_created` 和 `session.changed`）；`prompt` 在那之后才开始 turn。
 * `session.send` / `session.sync_base` 对已归档会话答 `invalid`（`reason = 'archived'`）；`session.archive` 在会话运行或等审批时答 `busy`。
 * `session.archive` / `session.delete` 按 `worktree` 处置改 worktree 的状态（`keep` → kept，其余 → removed）并发
 * `session.changed`；`session.git_status` / `diff` / `sync_base` 给脚本化的结果（`syncResults` 队列可预置 `sync_base` 的答复）。
 *
 * 附件（ADR-35，protocol.md § 5.6）：`blob.put_begin` / `put_chunk` / `put_end` / `get` / `discard` 按真 daemon 的规则
 * （白名单、大小、sha256、每设备 8 个进行中的上传、块长度、乱序 / 重发、`finalizing`、缺块、hash / 文件头校验、
 * `put_end` 幂等、分页、discard 的三种情况）；`blobRefusal` 让 `put_begin` 答 `busy`（`quota` / `disk_full`），
 * `chunkHook` 给块加延迟或让它失败，`chunksPeak` 记同时在处理的块数。`session.send` / `session.create` 核对附件
 * （不存在 → `not_found` / `blob`，元数据不符 → `invalid`，别的会话的 → `invalid` / `blob_in_use`，合计超 20 MiB →
 * `invalid` / `too_large`），通过后绑定到会话、`user_message` 带 daemon 的记录，回复里回显附件名和大小。
 */
import {
  AnyconRpcError,
  PROTOCOL_VERSION,
  Reassembler,
  RpcPeer,
  bytesOfHex,
  controlBody,
  controlEnvelope,
  decodeEnvelope,
  decodeFrame,
  encodeEnvelope,
  encodeFrames,
  encodeRpc,
  hexOf,
  makeEnvelope,
  makeNotification,
  resetBody,
  type AgentInfo,
  type AttachmentRef,
  type BlobGetParams,
  type BlobGetResult,
  type BlobIdParams,
  type BlobPutBeginParams,
  type BlobPutBeginResult,
  type BlobPutChunkParams,
  type ClearableSetting,
  type Control,
  type Crypto,
  type DiffFile,
  type Envelope,
  type Event,
  type Handshake,
  type Keypair,
  type PairAck,
  type PairSecrets,
  type PermissionRequest,
  type SessionArchiveParams,
  type SessionCreateParams,
  type SessionDeleteParams,
  type SessionDiffParams,
  type SessionDiffResult,
  type SessionEvent,
  type SessionGitStatus,
  type SessionGitStatusParams,
  type SessionSendParams,
  type SessionSettings,
  type SessionSummary,
  type SessionSyncBaseParams,
  type SessionSyncBaseResult,
  type SessionWorktree,
  type Transport,
  type WorkspaceBranchesParams,
  type WorkspaceBranchesResult,
  type WorkspaceRemoveWorktreeParams,
  type WorkspaceWorktreesParams,
  type WorktreeInfo,
} from '@anycon/protocol'
import { createHash } from 'node:crypto'
import { WebSocket } from 'ws'

const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

export interface Peer {
  id: string
  publicKey: Uint8Array
  /** 配对时对方在第 3 条握手消息 payload 里自报的信息（如有）。 */
  info?: { name?: string; kind?: string; platform?: string; version?: string }
}

interface SessionRecord {
  summary: SessionSummary
  events: SessionEvent[]
  watchers: Set<string>
  messages: Map<string, number>
  /** message_id → 那条消息的附件 blob_id（幂等重发时核对）。 */
  messageBlobs: Map<string, string>
  cancel: boolean
  turn: number
}

/** 进行中的上传。 */
export interface FakeUpload {
  peer: string
  name: string
  mime: string
  size: number
  sha256: string
  chunkSize: number
  chunks: Map<number, Uint8Array>
  finalizing: boolean
}

/** 完成的 blob。 */
export interface FakeBlob {
  ref: AttachmentRef
  bytes: Uint8Array
  /** 第一次被消息引用时绑定的会话。 */
  sessionId?: string
}

const IMAGE_MIMES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp']
const MAX_UPLOADS_PER_DEVICE = 8
const MAX_MESSAGE_ATTACHMENTS = 20
const MAX_MESSAGE_BYTES = 20 * 1024 * 1024
const BLOB_GET_MAX = 512 * 1024

const ascii = (s: string): number[] => Array.from(s, (c) => c.charCodeAt(0))

/** 和真 daemon 的 `sniff_mime` 一样只比前缀。 */
function sniffMime(b: Uint8Array): string | undefined {
  const starts = (prefix: number[], at = 0) => prefix.every((x, i) => b[at + i] === x)
  if (starts([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) return 'image/png'
  if (starts([0xff, 0xd8, 0xff])) return 'image/jpeg'
  if (starts(ascii('GIF87a')) || starts(ascii('GIF89a'))) return 'image/gif'
  if (b.length >= 12 && starts(ascii('RIFF')) && starts(ascii('WEBP'), 8)) return 'image/webp'
  return undefined
}

/** PNG 的宽高在 IHDR 里（偏移 16 / 20，大端）；别的格式 fake 不读。 */
function pngSize(b: Uint8Array): { width: number; height: number } | undefined {
  if (sniffMime(b) !== 'image/png' || b.length < 24) return undefined
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength)
  return { width: v.getUint32(16), height: v.getUint32(20) }
}

const sha256Of = (b: Uint8Array) => createHash('sha256').update(b).digest('hex')
const reasoned = (
  code: 'invalid' | 'busy' | 'not_found',
  message: string,
  reason: string,
): AnyconRpcError => AnyconRpcError.of(code, message, { reason })

interface PendingPermission {
  request: PermissionRequest
  resolve: (optionId: string) => void
}

export interface FakeDaemonOptions {
  hubUrl: string
  crypto: Crypto
  /** `FakeHub.registerDevice('daemon')` 的结果。 */
  device: { id: string; token: string }
  name?: string
  keypair?: Keypair
  /** `node.info` 报的协议版本，默认当前版本。 */
  protocolVersion?: number
  /** 启动时已认识的对端（模拟 daemon 重启后读 peers.json）。 */
  peers?: Peer[]
  /**
   * 配对完成后发给 joiner 的 `pair.ack`：默认如实发；`false` = 不发（模拟旧 daemon / 被 hub 吞掉）；
   * 给对象 = 覆盖其中的字段（模拟伪造 / 出错的 ack）。
   */
  pairAck?: false | Partial<PairAck>
}

export const MOCK_AGENT: AgentInfo = {
  id: 'mock',
  label: 'Mock Agent',
  driver: 'mock',
  enabled: true,
  status: 'ready',
  version: '0.0.0',
  probed_at_ms: 0,
  models: [
    {
      id: 'mock-large',
      label: 'Mock Large',
      default: true,
      thinking: {
        default: 'high',
        options: [
          { id: 'low', label: 'Low' },
          { id: 'high', label: 'High' },
        ],
      },
    },
    { id: 'mock-small', label: 'Mock Small' },
  ],
  modes: [
    { id: 'ask', label: 'Ask', risk: 'normal', default: true },
    { id: 'yolo', label: 'YOLO', risk: 'dangerous' },
  ],
}

const projectOf = (name: string) => ({
  name,
  path: `/tmp/fake-workspace/${name}`,
  modified_at_ms: 2,
  active_sessions: 0,
})

export class FakeDaemon {
  readonly opts: FakeDaemonOptions
  readonly keypair: Keypair
  readonly peers = new Map<string, Peer>()
  readonly sessions = new Map<string, SessionRecord>()
  /** 收到的 RPC 请求（方法名 + 对端），断言用。 */
  readonly requests: Array<{ peer: string; method: string; params: unknown }> = []
  /** 每个对端累计完成的 IK 握手次数。 */
  readonly handshakes = new Map<string, number>()
  /** 收到的 RESET / 解密失败 / 分片乱序的次数（和真 daemon 一样：丢传输并发 RESET）。 */
  resetsSent = 0
  /** 收到的 `data` 信封数（含分片）和其中最大的一封 body 长度，分片断言用。 */
  readonly dataIn = new Map<string, { count: number; maxBody: number }>()
  /** 发出的 `data` 信封数和最大 body 长度。 */
  readonly dataOut = { count: 0, maxBody: 0 }
  readonly #transports = new Map<string, Transport>()
  readonly #reassemblers = new Map<string, Reassembler>()
  #msgId = 0
  readonly #rpc = new Map<string, RpcPeer>()
  readonly #permissions = new Map<string, PendingPermission>()
  #ws: WebSocket | undefined
  #seq = 0
  #sessionCounter = 0
  #authed: Promise<void> | undefined
  #pairWaiters: Array<(c: Control) => void> = []
  /** 配对中：`peer` 是 PairMatched 里的临时 id；握手结束后暂存公钥，等 PairDone 给出正式 id 再入 peers。 */
  #pairing:
    | {
        peer: string
        hs: Handshake
        learned?: Omit<Peer, 'id'>
        /** msg3 之后的新传输，PairDone 之后用它发 `pair.ack`。 */
        transport?: Transport
      }
    | undefined
  #pairCodeWaiter: ((code: string) => void) | undefined
  /** 让 `session.send` 的脚本多等一会儿（毫秒）。 */
  scriptDelayMs = 0
  /** 让 `session.history` 的答复晚一点（毫秒）：制造「watch 之后、补历史之前又来了实时事件」的竞争。 */
  historyDelayMs = 0
  /** anycon 建的 worktree（`workspace.worktrees` 的数据源），测试里可以直接改（比如把 `uncommitted` 设成非 0）。 */
  readonly worktrees = new Map<string, WorktreeInfo>()
  /** 预置 `session.sync_base` 的答复（先进先出）：结果对象 = 成功，`AnyconRpcError` = 抛出；队列空 = `up_to_date`。 */
  readonly syncResults: Array<SessionSyncBaseResult | AnyconRpcError> = []
  #worktreeCounter = 0
  /** `session.create` 带 prompt 的幂等：message_id → 会话 id。 */
  readonly #createdByMessage = new Map<string, string>()
  /** 进行中的上传（blob_id → 状态）。 */
  readonly uploads = new Map<string, FakeUpload>()
  /** 完成的 blob（blob_id → 记录和内容）。 */
  readonly blobs = new Map<string, FakeBlob>()
  #blobCounter = 0
  /** `put_begin` 给的 `chunk_size`。 */
  blobChunkSize = 512 * 1024
  /** `sessions.max_attachment_bytes`（真 daemon 默认 10 MiB）。 */
  maxAttachmentBytes = 10 * 1024 * 1024
  /** 让 `put_begin` 答 `busy` 并带这个 `data.reason`（`quota` / `disk_full`）。 */
  blobRefusal: 'quota' | 'disk_full' | undefined
  /** 每块到达时调用（这一块第几次到达）：可以等一会儿（制造并发 / 超时）或抛错。 */
  chunkHook: ((index: number, attempt: number) => void | Promise<void>) | undefined
  /** 同时在处理的 `put_chunk` 数和峰值。 */
  chunksInFlight = 0
  chunksPeak = 0
  /** `blob_id:index` → 收到的次数。 */
  readonly chunkArrivals = new Map<string, number>()

  constructor(opts: FakeDaemonOptions) {
    this.opts = opts
    this.keypair = opts.keypair ?? opts.crypto.generateKeypair()
    for (const p of opts.peers ?? []) this.peers.set(p.id, p)
  }

  get id(): string {
    return this.opts.device.id
  }

  /** 连 hub 并认证；之后可以 `beginPairing()` 或直接等客户端 IK 握手。 */
  start(): Promise<void> {
    this.#authed = new Promise<void>((resolve, reject) => {
      const ws = new WebSocket(`${this.opts.hubUrl.replace(/^http/, 'ws')}/v1/ws`)
      this.#ws = ws
      ws.binaryType = 'arraybuffer'
      ws.on('open', () => {
        this.#sendControl({
          type: 'auth',
          device_token: this.opts.device.token,
          protocol_version: PROTOCOL_VERSION,
          client: {
            name: this.opts.name ?? 'fake-node',
            kind: 'daemon',
            platform: 'test',
            version: '0.0.0',
          },
        })
      })
      ws.on('message', (data) => {
        const env = decodeEnvelope(new Uint8Array(data as ArrayBuffer))
        if (env.kind === 'control') {
          const ctrl = controlBody(env)
          if (ctrl.type === 'auth_ok') resolve()
          else if (ctrl.type === 'auth_fail')
            reject(new Error(`fake daemon auth failed: ${ctrl.reason}`))
          else this.#onControl(ctrl)
        } else if (env.kind === 'handshake') this.#onHandshake(env)
        else if (env.kind === 'data') this.#onData(env)
      })
      ws.on('error', reject)
      ws.on('close', () => {
        this.#transports.clear()
        this.#reassemblers.clear()
        this.#rpc.clear()
      })
    })
    return this.#authed
  }

  /**
   * 发起配对，返回短码。短码在本地生成，hub 只拿到 `match_id`（ADR-29）。
   * `long` = 16 位长短码。
   */
  beginPairing(options: { long?: boolean } = {}): Promise<string> {
    const code = options.long
      ? this.opts.crypto.generatePairCodeLong()
      : this.opts.crypto.generatePairCode()
    // 一次 KDF 同时给出 match_id 和 PSK（真实 wasm 是 Argon2id，约 0.1–0.3 s，别算两遍）。
    const secrets = this.opts.crypto.derivePairSecrets(code)
    this.#secrets = secrets
    return new Promise((resolve) => {
      this.#pairCodeWaiter = () => resolve(code)
      this.#sendControl({ type: 'pair_begin', match_id: secrets.matchId })
    })
  }

  /** 等一次配对完成（对端公钥已记录）。 */
  waitPaired(): Promise<Peer> {
    return new Promise((resolve) => {
      this.#pairWaiters.push((c) => {
        if (c.type === 'pair_done') {
          const peer = this.peers.get(c.peer)
          if (peer) resolve(peer)
        }
      })
    })
  }

  /**
   * 改某个会话的后台任务数并发 `session.changed`。真 daemon 的通知里总带这个字段；
   * 传 `undefined` 模拟旧 daemon（通知里没有该字段）。
   */
  setBackgroundTasks(sessionId: string, count: number | undefined): void {
    const rec = this.#session(sessionId)
    const { background_tasks: _previous, ...rest } = rec.summary
    rec.summary = count === undefined ? rest : { ...rest, background_tasks: count }
    this.#notifyChanged(rec)
  }

  /** 非正常断线（模拟 daemon 崩溃 / 网络断）。 */
  dropConnection(): void {
    this.#ws?.terminate()
  }

  stop(): void {
    this.#ws?.close(1000)
  }

  // ---------- 信封 ----------

  #sendControl(ctrl: Control): void {
    this.#seq += 1
    this.#ws?.send(encodeEnvelope(controlEnvelope(this.id, this.#seq, ctrl)))
  }

  #sendEnvelope(to: string, kind: 'handshake' | 'data', body: Uint8Array): void {
    this.#seq += 1
    if (kind === 'data') {
      this.dataOut.count += 1
      this.dataOut.maxBody = Math.max(this.dataOut.maxBody, body.length)
    }
    this.#ws?.send(encodeEnvelope(makeEnvelope(this.id, to, kind, this.#seq, body)))
  }

  /** 一条密文 → 一个 msg_id → 若干 `data` 信封（ADR-30）。 */
  #sendCiphertext(to: string, ciphertext: Uint8Array): void {
    const msgId = this.#msgId
    this.#msgId = (this.#msgId + 1) >>> 0
    for (const body of encodeFrames(ciphertext, msgId)) this.#sendEnvelope(to, 'data', body)
  }

  /** 传输失步：丢掉这个对端的传输，发 RESET 请它重新握手（真 daemon 的做法）。 */
  #resetPeer(peerId: string): void {
    this.#transports.get(peerId)?.free()
    this.#transports.delete(peerId)
    this.#rpc.delete(peerId)
    this.#reassemblers.delete(peerId)
    this.resetsSent += 1
    this.#sendEnvelope(peerId, 'data', resetBody())
  }

  /** 测试用：主动请 `peerId` 重新握手。 */
  sendReset(peerId: string): void {
    this.#resetPeer(peerId)
  }

  /** 测试用：往 `peerId` 发一个原样的 data body（构造乱序分片等）。 */
  sendRawData(peerId: string, body: Uint8Array): void {
    this.#sendEnvelope(peerId, 'data', body)
  }

  /** 测试用：往 `peerId` 发一封解不开的 data（模拟计数器失步 / 被篡改）。 */
  sendGarbage(peerId: string): void {
    const junk = new Uint8Array(40).fill(7)
    this.#sendEnvelope(peerId, 'data', Uint8Array.of(0, ...junk))
  }

  #onControl(ctrl: Control): void {
    switch (ctrl.type) {
      case 'pair_registered':
        this.#pairCodeWaiter?.('')
        this.#pairCodeWaiter = undefined
        break
      case 'pair_matched':
        // host：XXpsk3 responder，PSK 要等第一条握手消息到来时才能知道短码——测试里短码由
        // beginPairing() 返回，这里用最近一次的短码。
        this.#pairing = {
          peer: ctrl.peer,
          hs: this.opts.crypto.pairing('responder', this.keypair, this.#psk()),
        }
        break
      case 'pair_done': {
        // PairDone.peer 是 joiner 的正式 id，peers 以它为键（不是 PairMatched 里的临时 id）。
        const learned = this.#pairing?.learned
        const transport = this.#pairing?.transport
        if (learned) this.peers.set(ctrl.peer, { id: ctrl.peer, ...learned })
        this.#pairing = undefined
        // 握手之后的第一条加密消息：`pair.ack`（joiner 校验通过才落盘）。
        if (transport && learned && this.opts.pairAck !== false) {
          const ack: PairAck = {
            node_id: this.id,
            peer_id: ctrl.peer,
            name: this.opts.name ?? 'fake-node',
            version: '0.0.0-test',
            fingerprint: this.opts.crypto.fingerprint(this.keypair.public),
            ...this.opts.pairAck,
          }
          const note = encodeRpc(makeNotification('pair.ack', ack))
          this.#sendCiphertext(ctrl.peer, transport.encrypt(note))
        }
        transport?.free()
        break
      }
      case 'peer_offline':
        this.#transports.delete(ctrl.device)
        this.#reassemblers.delete(ctrl.device)
        this.#rpc.delete(ctrl.device)
        break
      default:
        break
    }
    for (const w of this.#pairWaiters.splice(0)) w(ctrl)
  }

  #secrets: PairSecrets | undefined
  /** 让握手用另一个短码派生的 PSK（模拟 daemon 和 joiner 的 PSK 对不上）。 */
  setPairCode(code: string): void {
    this.#secrets = this.opts.crypto.derivePairSecrets(code)
  }
  #psk(): Uint8Array {
    if (!this.#secrets) throw new Error('fake daemon: no pairing in progress')
    return this.#secrets.psk
  }

  #onHandshake(env: Envelope): void {
    // 配对中的握手（XXpsk3，responder）
    if (this.#pairing && this.#pairing.peer === env.from) {
      const { hs, peer } = this.#pairing
      try {
        const payload = hs.read(env.body)
        if (hs.isFinished()) {
          // msg3 读完：握手结束。传输留着，PairDone 之后用它发 pair.ack。
          const transport = hs.intoTransport()
          const info = payload.length > 0 ? safeJson(payload) : undefined
          this.#pairing.learned = {
            publicKey: transport.remotePublic.slice(),
            ...(info ? { info } : {}),
          }
          this.#pairing.transport = transport
          this.#sendControl({ type: 'pair_confirm', peer })
        } else {
          this.#sendEnvelope(peer, 'handshake', hs.write())
        }
      } catch {
        this.#pairing = undefined // 短码错误等：不确认，hub 超时
      }
      return
    }
    // 重连握手（IK，responder）：from 必须在 peers 里，且公钥对得上
    const known = this.peers.get(env.from)
    if (!known) return
    try {
      const hs = this.opts.crypto.reconnect('responder', this.keypair, known.publicKey)
      hs.read(env.body)
      const reply = hs.write()
      const transport = hs.intoTransport()
      this.#transports.get(env.from)?.free()
      this.#transports.set(env.from, transport)
      this.#reassemblers.set(env.from, new Reassembler())
      this.handshakes.set(env.from, (this.handshakes.get(env.from) ?? 0) + 1)
      this.#rpc.set(env.from, this.#makeRpc(env.from))
      this.#sendEnvelope(env.from, 'handshake', reply)
    } catch {
      // 公钥不符 / 握手损坏：忽略（真 daemon 记日志）
    }
  }

  #onData(env: Envelope): void {
    const stat = this.dataIn.get(env.from) ?? { count: 0, maxBody: 0 }
    stat.count += 1
    stat.maxBody = Math.max(stat.maxBody, env.body.length)
    this.dataIn.set(env.from, stat)
    const transport = this.#transports.get(env.from)
    const rpc = this.#rpc.get(env.from)
    const reassembler = this.#reassemblers.get(env.from)
    if (!transport || !rpc || !reassembler) return
    let plain: Uint8Array
    try {
      const ciphertext = reassembler.push(decodeFrame(env.body))
      if (!ciphertext) return // 等后续分片
      plain = transport.decrypt(ciphertext)
    } catch {
      this.#resetPeer(env.from)
      return
    }
    rpc.handle(plain)
  }

  #notify(peerId: string, method: string, params: unknown): void {
    this.#rpc.get(peerId)?.notify(method, params)
  }

  #notifyAll(method: string, params: unknown): void {
    for (const id of this.#rpc.keys()) this.#notify(id, method, params)
  }

  // ---------- RPC ----------

  #makeRpc(peerId: string): RpcPeer {
    const rpc = new RpcPeer({
      send: (bytes) => {
        const transport = this.#transports.get(peerId)
        if (!transport) return
        this.#sendCiphertext(peerId, transport.encrypt(bytes))
      },
    })
    const on = <P, R>(method: string, fn: (params: P) => R | Promise<R>) =>
      rpc.onRequest<P, R>(method, (params) => {
        this.requests.push({ peer: peerId, method, params })
        return fn(params)
      })

    // 测试用：原样回显参数（大消息分片往返）。
    on<unknown, unknown>('test.echo', (p) => p)
    on('node.info', () => ({
      node_id: this.id,
      name: this.opts.name ?? 'fake-node',
      version: '0.0.0-test',
      protocol_version: this.opts.protocolVersion ?? PROTOCOL_VERSION,
      platform: 'test',
      workspace_root: '/tmp/fake-workspace',
      capabilities: { workspace: true, fs: false, search: false, blobs: true, audit: false },
      started_at_ms: 1,
    }))
    on<{ if_none_match?: string | null; refresh?: boolean }, unknown>('agent.list', (p) =>
      p.if_none_match === 'etag-1' && !p.refresh
        ? { agents: [], etag: 'etag-1', not_modified: true }
        : { agents: [MOCK_AGENT], etag: 'etag-1' },
    )
    const projects: Array<{ name: string }> = [{ name: 'app' }]
    const trash: Array<{ id: string; original_name: string }> = []
    on('workspace.list', () => ({
      root: '/tmp/fake-workspace',
      projects: projects.map((p) => ({ ...projectOf(p.name), modified_at_ms: 1 })),
    }))
    on<{ name: string }, unknown>('workspace.create', (p) => {
      if (projects.some((x) => x.name === p.name))
        throw AnyconRpcError.of('exists', 'project exists')
      projects.push({ name: p.name })
      return projectOf(p.name)
    })
    // 返回类型与 schema/methods.json 的注册表一致（和真 daemon 一样）：rename → RenamedProject `{ from, to }`，
    // trash → TrashEntry，restore → Project；session.rename / archive / unarchive / restart / promote → `{}`。
    // 关联目录：name -> 路径；browse 只认 `/home/me` 这棵假目录树（link_roots = ["/home/me"]）。
    const linked = new Map<string, string>()
    on<{ path: string; name?: string | null }, unknown>('workspace.link', (p) => {
      if (!p.path.startsWith('/home/me/'))
        throw AnyconRpcError.of('forbidden', 'outside link_roots')
      const name = p.name ?? p.path.slice(p.path.lastIndexOf('/') + 1)
      if (projects.some((x) => x.name === name)) throw AnyconRpcError.of('exists', 'project exists')
      projects.push({ name })
      linked.set(name, p.path)
      return { ...projectOf(name), path: p.path, kind: 'linked' }
    })
    on<{ name: string }, unknown>('workspace.unlink', (p) => {
      if (!linked.delete(p.name)) throw AnyconRpcError.of('invalid', 'not a linked project')
      projects.splice(
        projects.findIndex((x) => x.name === p.name),
        1,
      )
      return {}
    })
    on<{ path?: string | null; show_hidden?: boolean }, unknown>('workspace.browse', (p) => {
      const path = p.path ?? '/home/me'
      if (path !== '/home/me') throw AnyconRpcError.of('not_found', 'no such directory')
      const entries = [
        { name: '.config', path: '/home/me/.config', linkable: false, blocked: 'hidden' },
        { name: 'app', path: '/home/me/app', git: true, linkable: true },
      ].filter((e) => p.show_hidden === true || !e.name.startsWith('.'))
      return { path, roots: ['/home/me'], linkable: true, entries }
    })
    on<{ from: string; to: string }, unknown>('workspace.rename', (p) => {
      const found = projects.find((x) => x.name === p.from)
      if (!found) throw AnyconRpcError.of('not_found', 'no such project')
      found.name = p.to
      return { from: p.from, to: p.to }
    })
    on<{ name: string }, unknown>('workspace.trash', (p) => {
      const idx = projects.findIndex((x) => x.name === p.name)
      if (idx < 0) throw AnyconRpcError.of('not_found', 'no such project')
      projects.splice(idx, 1)
      const entry = { id: `${p.name}-20261011T120000Z`, original_name: p.name }
      trash.push(entry)
      return { ...entry, bytes: 1234, trashed_at_ms: 3 }
    })
    on('workspace.trash_list', () => ({
      entries: trash.map((t) => ({ ...t, bytes: 1234, trashed_at_ms: 3 })),
    }))
    on<{ id: string }, unknown>('workspace.restore', (p) => {
      const idx = trash.findIndex((t) => t.id === p.id)
      if (idx < 0) throw AnyconRpcError.of('not_found', 'no such trash entry')
      const [entry] = trash.splice(idx, 1)
      projects.push({ name: entry!.original_name })
      return projectOf(entry!.original_name)
    })
    on<{ id: string }, unknown>('workspace.purge', (p) => {
      const idx = trash.findIndex((t) => t.id === p.id)
      if (idx < 0) throw AnyconRpcError.of('not_found', 'no such trash entry')
      trash.splice(idx, 1)
      return {}
    })
    on<{ url: string }, unknown>('workspace.clone', () => {
      const jobId = `job-${Math.random().toString(36).slice(2, 8)}`
      void (async () => {
        await wait(5)
        this.#notify(peerId, 'job.progress', { job_id: jobId, stage: 'receiving', percent: 50 })
        await wait(5)
        this.#notify(peerId, 'job.done', { job_id: jobId, ok: true })
      })()
      return { job_id: jobId }
    })

    on<SessionCreateParams, SessionSummary>('session.create', (p) => {
      if (p.agent === 'missing') {
        throw new AnyconRpcError(-32002, 'agent unavailable', {
          code: 'agent_unavailable',
          reason: 'not_installed',
        })
      }
      if (p.agent !== 'mock') throw AnyconRpcError.of('not_found', `unknown agent ${p.agent}`)
      if (p.settings?.thinking === 'bogus') {
        throw new AnyconRpcError(-32001, 'invalid settings', {
          code: 'invalid_settings',
          field: 'thinking',
          reason: 'not supported by this model',
        })
      }
      if (p.settings?.mode === 'busy') throw AnyconRpcError.of('node_busy', 'too many sessions')
      const prompt = p.prompt ? p.prompt : undefined
      const hasMessage = prompt !== undefined || (p.attachments?.length ?? 0) > 0
      if (hasMessage && !p.message_id) {
        throw AnyconRpcError.of(
          'invalid',
          'message_id is required when prompt or attachments are given',
        )
      }
      // 带第一条消息的创建按 message_id 幂等：重试拿到第一次创建的会话（在核对附件之前，不会得到 blob_in_use）。
      const prior =
        hasMessage && p.message_id ? this.#createdByMessage.get(p.message_id) : undefined
      if (prior && this.sessions.has(prior)) return this.sessions.get(prior)!.summary
      const attachments = this.#checkAttachments(undefined, p.attachments)

      const worktreeRequested = p.workspace?.mode === 'worktree'
      if (worktreeRequested && !p.project) {
        throw AnyconRpcError.of('invalid', 'a scratch session cannot use a worktree')
      }
      if (worktreeRequested && p.project === 'plain') {
        throw AnyconRpcError.of('invalid', 'the project is not worktree-ready', {
          reason: 'not_worktree_ready',
        })
      }
      const id = `sess_${String(++this.#sessionCounter).padStart(22, '0')}`
      const now = Date.now()
      let worktree: SessionWorktree | undefined
      if (p.workspace?.mode === 'worktree') {
        const n = ++this.#worktreeCounter
        const slug = `wt-${n}`
        const branch = p.workspace.branch ?? `anycon/${slug}`
        if (branch === 'main' || [...this.worktrees.values()].some((w) => w.branch === branch)) {
          throw AnyconRpcError.of('exists', `branch ${branch} already exists`)
        }
        const base = p.workspace.base ?? 'origin/main'
        if (base === 'bogus') {
          throw AnyconRpcError.of('invalid', `cannot resolve base ${base}`, { reason: 'bad_base' })
        }
        worktree = {
          id: `wt_${String(n).padStart(22, '0')}`,
          branch,
          base,
          base_sha: 'a'.repeat(40),
          path: `.anycon/worktrees/${slug}`,
          state: 'creating',
        }
      }
      const summary: SessionSummary = {
        id,
        ...(p.project ? { project: p.project } : {}),
        agent: p.agent,
        title: '',
        status: 'idle',
        archived: false,
        settings: { model: 'mock-large', thinking: 'high', mode: 'ask', ...p.settings },
        created_at_ms: now,
        updated_at_ms: now,
        last_seq: 0,
        pending_permissions: 0,
        ...(worktree ? { worktree } : {}),
      }
      const rec: SessionRecord = {
        summary,
        events: [],
        watchers: new Set(),
        messages: new Map(),
        messageBlobs: new Map(),
        cancel: false,
        turn: 0,
      }
      this.sessions.set(id, rec)
      this.#bindAttachments(rec, attachments)
      if (hasMessage && p.message_id) this.#createdByMessage.set(p.message_id, id)
      if (worktree && p.project) {
        this.worktrees.set(worktree.id, {
          id: worktree.id,
          project: p.project,
          branch: worktree.branch,
          base: worktree.base,
          path: worktree.path,
          state: 'creating',
          ahead: 0,
          uncommitted: 0,
          created_at_ms: now,
          session_id: id,
        })
      }
      // 返回之后才建好 worktree、再开始第一个 turn（和真 daemon 的「先答 creating，后台拉起」一致）。
      void (async () => {
        if (worktree) await this.#activateWorktree(rec, worktree)
        if (hasMessage && p.message_id) {
          this.#startTurn(rec, p.message_id, prompt ?? '', peerId, attachments)
        }
      })()
      return summary
    })
    on<{ session_id: string }, SessionSummary>(
      'session.get',
      (p) => this.#session(p.session_id).summary,
    )
    on('session.list', () => ({ sessions: [...this.sessions.values()].map((s) => s.summary) }))
    // 会话管理：真 daemon 都答 `{}`（Empty），改动通过 `session.changed` 通知；这里直接改 summary。
    on<{ session_id: string; title: string }, unknown>('session.rename', (p) => {
      const s = this.#session(p.session_id)
      s.summary = { ...s.summary, title: p.title }
      return {}
    })
    on<SessionArchiveParams, unknown>('session.archive', (p) => {
      const s = this.#session(p.session_id)
      // 运行中 / 等审批的会话不能归档（要先取消或答复）。
      if (s.summary.status === 'running' || s.summary.status === 'waiting_permission') {
        throw AnyconRpcError.of('busy', 'the session has a turn in progress')
      }
      s.summary = { ...s.summary, archived: true }
      this.#disposeWorktree(s, p.worktree ?? 'auto')
      this.#notifyChanged(s)
      return {}
    })
    on<{ session_id: string }, unknown>('session.unarchive', (p) => {
      const s = this.#session(p.session_id)
      s.summary = { ...s.summary, archived: false }
      return {}
    })
    on<SessionDeleteParams, unknown>('session.delete', (p) => {
      const s = this.#session(p.session_id)
      this.#disposeWorktree(s, p.worktree ?? 'auto')
      // 会话删掉之后 worktree 记录还在（如果保留了），但不再属于任何会话。
      const wtId = s.summary.worktree?.id
      const info = wtId ? this.worktrees.get(wtId) : undefined
      if (info) {
        const { session_id: _gone, session_title: _title, ...rest } = info
        this.worktrees.set(info.id, rest)
      }
      this.sessions.delete(p.session_id)
      // 附件随会话删除
      for (const [id, blob] of this.blobs)
        if (blob.sessionId === p.session_id) this.blobs.delete(id)
      this.#notifyAll('session.changed', {
        session_id: p.session_id,
        title: s.summary.title,
        status: s.summary.status,
        deleted: true,
        pending_permissions: 0,
        updated_at_ms: Date.now(),
        last_seq: s.summary.last_seq,
      })
      return {}
    })
    // ---- git（ADR-34）：脚本化的结果 ----
    on<SessionGitStatusParams, SessionGitStatus>('session.git_status', (p) => {
      const s = this.#session(p.session_id)
      const wt = s.summary.worktree
      if (!s.summary.project)
        throw AnyconRpcError.of('invalid', 'not a git repository', { reason: 'not_git' })
      const status: SessionGitStatus = wt
        ? {
            branch: wt.branch,
            head_sha: 'b'.repeat(40),
            base: wt.base,
            base_sha: 'c'.repeat(40),
            ahead: 2,
            behind: 1,
            uncommitted: 3,
            conflicts: 0,
            merging: false,
            upstream: null,
            unpushed: null,
          }
        : { branch: 'main', head_sha: 'b'.repeat(40), uncommitted: 1, conflicts: 0 }
      if (p.include_pr) {
        status.pr = {
          number: 42,
          title: 'Fix the login redirect',
          url: 'https://example.invalid/pull/42',
          state: 'open',
          base_branch: 'main',
          checks_passed: 3,
          checks_failed: 0,
          checks_pending: 1,
        }
      }
      return status
    })
    on<SessionDiffParams, SessionDiffResult>('session.diff', (p) => {
      this.#session(p.session_id)
      const files = diffFiles(p.against ?? 'base')
      const from_sha = p.against === 'uncommitted' ? 'b'.repeat(40) : 'c'.repeat(40)
      if (p.path === undefined || p.path === null) return { files, from_sha }
      const one = files.find((f) => f.path === p.path)
      if (!one) throw AnyconRpcError.of('invalid', `${p.path} is not in the changed files`)
      return {
        files: [
          {
            ...one,
            patch: `diff --git a/${one.path} b/${one.path}\n--- a/${one.path}\n+++ b/${one.path}\n@@ -1 +1 @@\n-old\n+new\n`,
          },
        ],
        from_sha,
      }
    })
    on<SessionSyncBaseParams, SessionSyncBaseResult>('session.sync_base', (p) => {
      const s = this.#session(p.session_id)
      if (s.summary.archived) {
        throw AnyconRpcError.of('invalid', 'the session is archived', { reason: 'archived' })
      }
      if (!s.summary.worktree) throw AnyconRpcError.of('invalid', 'the session has no worktree')
      if (p.abort) return { outcome: 'aborted', commits: 0 }
      const next = this.syncResults.shift()
      if (next instanceof AnyconRpcError) throw next
      return next ?? { outcome: 'up_to_date', commits: 0 }
    })
    on<WorkspaceBranchesParams, WorkspaceBranchesResult>('workspace.branches', (p) => {
      if (p.project === 'plain') {
        throw AnyconRpcError.of('invalid', 'not a git repository', { reason: 'not_git' })
      }
      return {
        current: 'main',
        default_base: 'origin/main',
        local: ['main', 'dev'],
        remote: ['origin/main', 'origin/dev'],
      }
    })
    on<WorkspaceWorktreesParams, { worktrees: WorktreeInfo[] }>('workspace.worktrees', (p) => ({
      worktrees: [...this.worktrees.values()].filter(
        (w) => p.project === undefined || p.project === null || w.project === p.project,
      ),
    }))
    on<WorkspaceRemoveWorktreeParams, unknown>('workspace.remove_worktree', (p) => {
      const w = this.worktrees.get(p.id)
      if (!w) throw AnyconRpcError.of('not_found', `no such worktree ${p.id}`)
      if (w.session_id && !this.sessions.get(w.session_id)?.summary.archived) {
        throw AnyconRpcError.of('busy', 'the session is not archived')
      }
      if (w.uncommitted > 0 && !p.force) {
        throw AnyconRpcError.of('invalid', 'the worktree has uncommitted changes', {
          reason: 'uncommitted_changes',
        })
      }
      this.worktrees.delete(p.id)
      return {}
    })
    on<{ session_id: string }, unknown>('session.restart', (p) => {
      this.#session(p.session_id)
      return {}
    })
    on<{ session_id: string }, unknown>('session.watch', (p) => {
      this.#session(p.session_id).watchers.add(peerId)
      return {}
    })
    on<{ session_id: string }, unknown>('session.unwatch', (p) => {
      this.sessions.get(p.session_id)?.watchers.delete(peerId)
      return {}
    })
    on<{ session_id: string; after_seq?: number; limit?: number }, unknown>(
      'session.history',
      async (p) => {
        const rec = this.#session(p.session_id)
        if (this.historyDelayMs > 0) await wait(this.historyDelayMs)
        const after = p.after_seq ?? 0
        const limit = p.limit ?? 200
        const events = rec.events.filter((e) => e.seq > after)
        return { events: events.slice(0, limit), has_more: events.length > limit }
      },
    )
    on<{ session_id: string; settings: SessionSettings; clear?: ClearableSetting[] }, unknown>(
      'session.set_settings',
      (p) => {
        const rec = this.#session(p.session_id)
        if (rec.summary.status === 'running' || rec.summary.status === 'waiting_permission') {
          throw AnyconRpcError.of('busy', 'turn in progress')
        }
        if (p.settings.thinking === 'bogus') {
          throw new AnyconRpcError(-32001, 'invalid settings', {
            code: 'invalid_settings',
            field: 'thinking',
            reason: 'unsupported',
          })
        }
        // 增量合并，然后 `clear` 里的字段恢复为未设置（两边都出现时以 clear 为准）。
        const merged: SessionSettings = { ...rec.summary.settings, ...p.settings }
        for (const field of p.clear ?? []) {
          if (field === 'env') delete merged.env
          else delete merged[field]
        }
        rec.summary = { ...rec.summary, settings: merged }
        this.#emit(rec, { type: 'settings_changed', settings: rec.summary.settings })
        return { settings: rec.summary.settings }
      },
    )
    on<{ session_id: string }, unknown>('session.cancel', (p) => {
      this.#session(p.session_id).cancel = true
      return {}
    })
    on<SessionSendParams, unknown>('session.send', (p) => {
      const rec = this.#session(p.session_id)
      if (rec.summary.archived) {
        throw AnyconRpcError.of('invalid', 'the session is archived; unarchive it first', {
          reason: 'archived',
        })
      }
      const known = rec.messages.get(p.message_id)
      if (known !== undefined) {
        // 同一个 message_id 配不同的附件不是重试
        const blobs = (p.attachments ?? []).map((a) => a.blob_id).join(',')
        if ((rec.messageBlobs.get(p.message_id) ?? '') !== blobs) {
          throw AnyconRpcError.of(
            'invalid',
            `message_id ${p.message_id} was used with other attachments`,
          )
        }
        return { turn_id: known, queued: false, duplicate: true }
      }
      const attachments = this.#checkAttachments(rec, p.attachments)
      if (!p.content && !attachments) throw AnyconRpcError.of('invalid', 'the message is empty')
      this.#bindAttachments(rec, attachments)
      const turn = this.#startTurn(rec, p.message_id, p.content, peerId, attachments)
      return { turn_id: turn, queued: false, duplicate: false }
    })
    // ---- 附件（ADR-35） ----
    on<BlobPutBeginParams, BlobPutBeginResult>('blob.put_begin', (p) => {
      if (!p.name || new TextEncoder().encode(p.name).length > 255 || /[/\\\p{Cc}]/u.test(p.name)) {
        throw AnyconRpcError.of('invalid', 'bad attachment name')
      }
      if (!IMAGE_MIMES.includes(p.mime)) {
        throw AnyconRpcError.of('unsupported', 'only png, jpeg, gif and webp images are accepted')
      }
      if (p.size < 1 || p.size > this.maxAttachmentBytes) {
        throw reasoned(
          'invalid',
          `size must be between 1 and ${this.maxAttachmentBytes} bytes`,
          'too_large',
        )
      }
      if (!/^[0-9a-f]{64}$/.test(p.sha256)) {
        throw AnyconRpcError.of('invalid', 'sha256 must be 64 lowercase hex characters')
      }
      if (
        [...this.uploads.values()].filter((u) => u.peer === peerId).length >= MAX_UPLOADS_PER_DEVICE
      ) {
        throw AnyconRpcError.of('busy', 'at most 8 uploads can be in progress at the same time')
      }
      if (this.blobRefusal) throw reasoned('busy', `refused: ${this.blobRefusal}`, this.blobRefusal)
      const blobId = (++this.#blobCounter).toString(16).padStart(32, '0')
      this.uploads.set(blobId, {
        peer: peerId,
        name: p.name,
        mime: p.mime,
        size: p.size,
        sha256: p.sha256,
        chunkSize: this.blobChunkSize,
        chunks: new Map(),
        finalizing: false,
      })
      return {
        blob_id: blobId,
        chunk_size: this.blobChunkSize,
        expires_at_ms: Date.now() + 600_000,
      }
    })
    on<BlobPutChunkParams, unknown>('blob.put_chunk', async (p) => {
      const key = `${p.blob_id}:${p.index}`
      const attempt = (this.chunkArrivals.get(key) ?? 0) + 1
      this.chunkArrivals.set(key, attempt)
      this.chunksInFlight += 1
      this.chunksPeak = Math.max(this.chunksPeak, this.chunksInFlight)
      try {
        await this.chunkHook?.(p.index, attempt)
        const up = this.uploads.get(p.blob_id)
        if (!up) throw AnyconRpcError.of('not_found', 'no such upload')
        if (up.finalizing) throw reasoned('invalid', 'put_end has started', 'finalizing')
        const count = Math.ceil(up.size / up.chunkSize)
        if (!Number.isInteger(p.index) || p.index < 0 || p.index >= count) {
          throw AnyconRpcError.of('invalid', `index ${p.index} is out of range`)
        }
        const data = new Uint8Array(Buffer.from(p.data, 'base64'))
        const want = Math.min(up.chunkSize, up.size - p.index * up.chunkSize)
        if (data.length !== want) {
          throw AnyconRpcError.of(
            'invalid',
            `chunk ${p.index} must be ${want} bytes, got ${data.length}`,
          )
        }
        up.chunks.set(p.index, data)
        return {}
      } finally {
        this.chunksInFlight -= 1
      }
    })
    on<BlobIdParams, AttachmentRef>('blob.put_end', (p) => {
      const done = this.blobs.get(p.blob_id)
      if (done) return done.ref
      const up = this.uploads.get(p.blob_id)
      if (!up) throw AnyconRpcError.of('not_found', 'no such upload')
      const count = Math.ceil(up.size / up.chunkSize)
      if (up.chunks.size < count) throw reasoned('invalid', 'chunks are missing', 'incomplete')
      up.finalizing = true
      const bytes = new Uint8Array(up.size)
      for (const [index, chunk] of up.chunks) bytes.set(chunk, index * up.chunkSize)
      if (sha256Of(bytes) !== up.sha256) {
        this.uploads.delete(p.blob_id)
        throw reasoned('invalid', 'the content does not match sha256', 'hash_mismatch')
      }
      if (sniffMime(bytes) !== up.mime) {
        this.uploads.delete(p.blob_id)
        throw reasoned('invalid', `the file is not ${up.mime}`, 'mime_mismatch')
      }
      const ref: AttachmentRef = {
        blob_id: p.blob_id,
        name: up.name,
        mime: up.mime,
        bytes: up.size,
        ...pngSize(bytes),
      }
      this.uploads.delete(p.blob_id)
      this.blobs.set(p.blob_id, { ref, bytes })
      return ref
    })
    on<BlobGetParams, BlobGetResult>('blob.get', (p) => {
      const blob = this.blobs.get(p.blob_id)
      if (!blob) throw AnyconRpcError.of('not_found', 'no such blob')
      const offset = p.offset ?? 0
      if (offset > blob.bytes.length) throw AnyconRpcError.of('invalid', 'offset is past the end')
      const max = Math.min(Math.max(p.max_bytes ?? BLOB_GET_MAX, 1), BLOB_GET_MAX)
      const part = blob.bytes.subarray(offset, offset + max)
      return {
        blob: blob.ref,
        offset,
        data: Buffer.from(part).toString('base64'),
        eof: offset + part.length >= blob.bytes.length,
      }
    })
    on<BlobIdParams, unknown>('blob.discard', (p) => {
      if (this.uploads.delete(p.blob_id)) return {}
      const blob = this.blobs.get(p.blob_id)
      if (blob?.sessionId)
        throw reasoned('invalid', 'the attachment is used by a message', 'in_use')
      this.blobs.delete(p.blob_id)
      return {}
    })
    on<{ request_id: string; option_id?: string }, unknown>('permission.respond', (p) => {
      const pending = this.#permissions.get(p.request_id)
      if (!pending) throw AnyconRpcError.of('already_resolved', 'permission already resolved')
      this.#permissions.delete(p.request_id)
      pending.resolve(p.option_id ?? 'deny')
      this.#notifyAll('permission.resolved', {
        session_id: pending.request.session_id,
        request_id: p.request_id,
        option_id: p.option_id ?? null,
        by_device: peerId,
      })
      return {}
    })
    on('permission.list', () => ({
      requests: [...this.#permissions.values()].map((p) => p.request),
    }))
    return rpc
  }

  /** 开始一个 turn（`session.send` 和 `session.create` 的 prompt 共用）。 */
  #startTurn(
    rec: SessionRecord,
    messageId: string,
    content: string,
    peerId: string,
    attachments?: AttachmentRef[],
  ): number {
    rec.turn += 1
    const turn = rec.turn
    rec.messages.set(messageId, turn)
    rec.messageBlobs.set(messageId, (attachments ?? []).map((a) => a.blob_id).join(','))
    void this.#runTurn(rec, turn, messageId, content, peerId, attachments)
    return turn
  }

  /**
   * 核对一条消息的附件（protocol.md § 5.6），返回 daemon 自己的记录；没有附件返回 undefined。
   * `rec` 为空 = `session.create`（会话还没建）。
   */
  #checkAttachments(
    rec: SessionRecord | undefined,
    list: AttachmentRef[] | null | undefined,
  ): AttachmentRef[] | undefined {
    if (!list || list.length === 0) return undefined
    if (list.length > MAX_MESSAGE_ATTACHMENTS) {
      throw AnyconRpcError.of('invalid', 'at most 20 attachments per message')
    }
    if (new Set(list.map((a) => a.blob_id)).size !== list.length) {
      throw AnyconRpcError.of('invalid', 'the same attachment is listed twice')
    }
    const refs = list.map((a) => {
      const blob = this.blobs.get(a.blob_id)
      if (!blob) throw reasoned('not_found', `attachment ${a.blob_id} does not exist`, 'blob')
      if (blob.ref.name !== a.name || blob.ref.mime !== a.mime || blob.ref.bytes !== a.bytes) {
        throw AnyconRpcError.of('invalid', `attachment ${a.blob_id} does not match the record`)
      }
      if (blob.sessionId !== undefined && blob.sessionId !== rec?.summary.id) {
        throw reasoned(
          'invalid',
          `attachment ${a.blob_id} belongs to another session`,
          'blob_in_use',
        )
      }
      return blob.ref
    })
    if (refs.reduce((n, r) => n + r.bytes, 0) > MAX_MESSAGE_BYTES) {
      throw reasoned(
        'invalid',
        'the attachments of one message add up to more than 20 MiB',
        'too_large',
      )
    }
    return refs
  }

  #bindAttachments(rec: SessionRecord, refs: AttachmentRef[] | undefined): void {
    for (const r of refs ?? []) this.blobs.get(r.blob_id)!.sessionId = rec.summary.id
  }

  /** 约 10 ms 后 worktree 建好：状态 active、`git` 事件、`session.changed`。 */
  async #activateWorktree(rec: SessionRecord, wt: SessionWorktree): Promise<void> {
    await wait(10)
    if (!this.sessions.has(rec.summary.id)) return
    const active: SessionWorktree = { ...wt, state: 'active' }
    rec.summary = { ...rec.summary, worktree: active }
    const info = this.worktrees.get(wt.id)
    if (info) this.worktrees.set(wt.id, { ...info, state: 'active' })
    this.#emit(rec, {
      type: 'git',
      action: 'worktree_created',
      branch: wt.branch,
      base: wt.base,
      count: 0,
    })
    this.#notifyChanged(rec)
  }

  /** 归档 / 删除时处置 worktree：`keep` → kept，其余（fake 里 `auto` 当作干净）→ removed。 */
  #disposeWorktree(rec: SessionRecord, disposal: 'auto' | 'keep' | 'remove'): void {
    const wt = rec.summary.worktree
    if (!wt) return
    const state = disposal === 'keep' ? 'kept' : 'removed'
    rec.summary = { ...rec.summary, worktree: { ...wt, state } }
    const info = this.worktrees.get(wt.id)
    if (info) this.worktrees.set(wt.id, { ...info, state })
  }

  #notifyChanged(rec: SessionRecord): void {
    const s = rec.summary
    this.#notifyAll('session.changed', {
      session_id: s.id,
      title: s.title,
      status: s.status,
      archived: s.archived ?? false,
      pending_permissions: s.pending_permissions,
      ...(s.background_tasks !== undefined ? { background_tasks: s.background_tasks } : {}),
      updated_at_ms: Date.now(),
      last_seq: s.last_seq,
      ...(s.worktree ? { worktree: s.worktree } : {}),
    })
  }

  #session(id: string): SessionRecord {
    const rec = this.sessions.get(id)
    if (!rec) throw AnyconRpcError.of('not_found', `no such session ${id}`)
    return rec
  }

  #emit(rec: SessionRecord, event: Event, turnId?: number): void {
    const se: SessionEvent = {
      session_id: rec.summary.id,
      seq: rec.events.length + 1,
      ts_ms: Date.now(),
      ...(turnId !== undefined ? { turn_id: turnId } : {}),
      event,
    }
    rec.events.push(se)
    rec.summary = { ...rec.summary, last_seq: se.seq, updated_at_ms: se.ts_ms }
    for (const peerId of rec.watchers) this.#notify(peerId, 'session.event', se)
  }

  async #runTurn(
    rec: SessionRecord,
    turn: number,
    messageId: string,
    content: string,
    byDevice: string,
    attachments?: AttachmentRef[],
  ): Promise<void> {
    const pause = async () => wait(content.includes('slow') ? 60 : this.scriptDelayMs || 2)
    const say = async (event: Event) => {
      await pause()
      this.#emit(rec, event, turn)
    }
    rec.cancel = false
    this.#emit(rec, {
      type: 'user_message',
      message_id: messageId,
      content,
      by_device: byDevice,
      ...(attachments ? { attachments } : {}),
    })
    await say({ type: 'turn_start', turn_id: turn, message_id: messageId })
    rec.summary = { ...rec.summary, status: 'running' }
    await say({ type: 'thinking_delta', text: 'hmm' })
    await say({ type: 'text_delta', text: 'Hello ' })
    if (rec.cancel) return this.#finish(rec, turn, 'cancelled')
    if (content.includes('error')) {
      await say({ type: 'error', code: 'agent_crashed', message: 'boom', recoverable: false })
      rec.summary = { ...rec.summary, status: 'error' }
      return
    }
    if (content.includes('permission')) {
      const requestId = `perm-${turn}`
      const request: PermissionRequest = {
        session_id: rec.summary.id,
        request_id: requestId,
        kind: content.includes('mode_change')
          ? 'mode_change'
          : content.includes('plan')
            ? 'plan'
            : 'tool',
        title: 'Run command',
        detail: {
          tool: {
            tool: 'Bash',
            category: 'shell',
            input: { command: 'echo hi' },
            detail: { command: 'echo hi' },
          },
        },
        options: content.includes('always-only')
          ? [
              { id: 'allow_session', label: 'Allow for session', kind: 'allow_always' },
              { id: 'deny', label: 'Deny', kind: 'reject_once', needs_message: true },
            ]
          : [
              { id: 'allow', label: 'Allow', kind: 'allow_once' },
              { id: 'allow_session', label: 'Allow for session', kind: 'allow_always' },
              { id: 'deny', label: 'Deny', kind: 'reject_once', needs_message: true },
            ],
        created_at_ms: Date.now(),
      }
      const decision = new Promise<string>((resolve) =>
        this.#permissions.set(requestId, { request, resolve }),
      )
      rec.summary = { ...rec.summary, status: 'waiting_permission', pending_permissions: 1 }
      await pause()
      this.#emit(
        rec,
        {
          type: 'permission',
          request_id: requestId,
          kind: request.kind,
          title: request.title,
          detail: request.detail,
          options: request.options,
        },
        turn,
      )
      this.#notifyAll('permission.request', request)
      const optionId = await decision
      this.#emit(
        rec,
        {
          type: 'permission_resolved',
          request_id: requestId,
          option_id: optionId,
          by_device: byDevice,
        },
        turn,
      )
      rec.summary = { ...rec.summary, status: 'running', pending_permissions: 0 }
      if (optionId.startsWith('allow')) {
        await say({
          type: 'tool_call',
          id: 't1',
          name: 'Bash',
          category: 'shell',
          title: 'echo hi',
          input: { command: 'echo hi' },
        })
        await say({ type: 'tool_result', id: 't1', output: { stdout: 'hi\n' }, is_error: false })
        await say({ type: 'text_delta', text: 'done' })
      } else {
        await say({ type: 'text_delta', text: 'skipped' })
      }
    } else {
      await say({ type: 'text_delta', text: 'world' })
    }
    if (attachments) {
      // 和 mock agent 一样回显收到的附件（证明 daemon 交给了 agent）
      const list = attachments.map((a) => `${a.name} (${a.mime}, ${a.bytes} bytes)`).join('; ')
      await say({ type: 'text_delta', text: ` Attachments: ${list}` })
    }
    await this.#finish(rec, turn, rec.cancel ? 'cancelled' : 'completed')
  }

  async #finish(
    rec: SessionRecord,
    turn: number,
    reason: 'completed' | 'cancelled',
  ): Promise<void> {
    await wait(this.scriptDelayMs || 2)
    this.#emit(
      rec,
      {
        type: 'turn_done',
        turn_id: turn,
        reason,
        usage: { input_tokens: 10, output_tokens: 20, duration_ms: 5 },
      },
      turn,
    )
    rec.summary = { ...rec.summary, status: 'idle' }
  }
}

/** `session.diff` 的脚本化文件列表：相对 base 两个文件（一个未跟踪），未提交只有那个未跟踪的。 */
function diffFiles(against: 'base' | 'uncommitted'): DiffFile[] {
  return against === 'uncommitted'
    ? [{ path: 'new.txt', status: 'added', additions: 1, deletions: 0, untracked: true }]
    : [
        { path: 'src/a.ts', status: 'modified', additions: 10, deletions: 2 },
        { path: 'new.txt', status: 'added', additions: 1, deletions: 0, untracked: true },
      ]
}

function safeJson(bytes: Uint8Array): Peer['info'] | undefined {
  try {
    return JSON.parse(new TextDecoder().decode(bytes)) as Peer['info']
  } catch {
    return undefined
  }
}

export { bytesOfHex, hexOf }
