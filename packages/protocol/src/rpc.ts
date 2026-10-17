/**
 * 内层 JSON-RPC 2.0：类型、编解码、类型守卫，以及与传输无关的 `RpcPeer`。
 *
 * 一个 `data` 信封 = 一条 JSON-RPC 消息（请求 / 响应 / 通知之一），不合并。
 * 错误以 `error.data.code`（稳定 snake_case）为准，`AnyconRpcError.code` 直接给出它。
 */
import { ERROR_CODE_NUMBERS } from './constants.js'
import { ProtocolError } from './errors.js'
import type {
  ErrorCode,
  RpcError,
  RpcErrorData,
  RpcId,
  RpcMessage,
  RpcNotification,
  RpcRequest,
  RpcResponse,
} from './generated.js'

export type {
  ErrorCode,
  RpcError,
  RpcErrorData,
  RpcId,
  RpcMessage,
  RpcNotification,
  RpcRequest,
  RpcResponse,
}

const JSONRPC = '2.0'

// ---------- 守卫 ----------

/** 有 `method` 且有 `id`。 */
export function isRequest(m: RpcMessage): m is RpcRequest {
  return 'method' in m && 'id' in m && m.id !== undefined && m.id !== null
}

/** 有 `method` 无 `id`。 */
export function isNotification(m: RpcMessage): m is RpcNotification {
  return 'method' in m && !('id' in m && m.id !== undefined && m.id !== null)
}

/** 没有 `method`。 */
export function isResponse(m: RpcMessage): m is RpcResponse {
  return !('method' in m)
}

// ---------- 构造（键序与 Rust 结构体声明一致） ----------

export function makeRequest(id: RpcId, method: string, params: unknown = {}): RpcRequest {
  return { jsonrpc: JSONRPC, id, method, params: params as RpcRequest['params'] }
}

export function makeNotification(method: string, params: unknown = {}): RpcNotification {
  return { jsonrpc: JSONRPC, method, params: params as RpcNotification['params'] }
}

export function makeResult(id: RpcId, result: unknown): RpcResponse {
  return { jsonrpc: JSONRPC, id, result }
}

export function makeError(id: RpcId, error: RpcError): RpcResponse {
  return { jsonrpc: JSONRPC, id, error }
}

// ---------- 编解码 ----------

const textEncoder = new TextEncoder()
const textDecoder = new TextDecoder('utf-8', { fatal: true })

const isSet = <T>(v: T | null | undefined): v is T => v !== null && v !== undefined

/** `RpcError` 的字段顺序 code, message, data；`data` 里的可选字段为 null / undefined 时省略。 */
function canonicalError(err: RpcError): RpcError {
  const out: RpcError = { code: err.code, message: err.message }
  const data = err.data
  if (data !== undefined && data !== null) {
    const d: RpcErrorData = { code: data.code }
    if (isSet(data.field)) d.field = data.field
    if (isSet(data.reason)) d.reason = data.reason
    if (isSet(data.retry_after_ms)) d.retry_after_ms = data.retry_after_ms
    out.data = d
  }
  return out
}

/** 按 Rust 侧的字段顺序重排，保证和 `serde_json::to_vec` 字节一致。 */
function canonicalRpc(msg: RpcMessage): RpcMessage {
  if (isRequest(msg)) return makeRequest(msg.id, msg.method, msg.params ?? {})
  if (isNotification(msg)) return makeNotification(msg.method, msg.params ?? {})
  const out: RpcResponse = { jsonrpc: JSONRPC, id: msg.id }
  if (msg.result !== undefined) out.result = msg.result
  if (msg.error !== undefined && msg.error !== null) out.error = canonicalError(msg.error)
  return out
}

export function encodeRpc(msg: RpcMessage): Uint8Array {
  return textEncoder.encode(JSON.stringify(canonicalRpc(msg)))
}

export function decodeRpc(bytes: Uint8Array): RpcMessage {
  let value: unknown
  try {
    value = JSON.parse(textDecoder.decode(bytes))
  } catch (cause) {
    throw new ProtocolError('rpc message is not valid JSON', { cause })
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new ProtocolError('rpc message must be a JSON object')
  }
  const m = value as Record<string, unknown>
  if (m.jsonrpc !== JSONRPC) throw new ProtocolError('rpc message is not jsonrpc 2.0')
  const hasId = typeof m.id === 'number' || typeof m.id === 'string'
  if ('method' in m) {
    if (typeof m.method !== 'string') throw new ProtocolError('rpc method must be a string')
    if ('id' in m && !hasId) throw new ProtocolError('rpc id must be a number or string')
    return value as unknown as RpcRequest | RpcNotification
  }
  if (!hasId) throw new ProtocolError('rpc response needs a numeric or string id')
  return value as unknown as RpcResponse
}

// ---------- 错误 ----------

const CODE_BY_NUMBER = new Map<number, string>(
  Object.entries(ERROR_CODE_NUMBERS).map(([code, n]) => [n, code]),
)

/** 远端返回（或本地构造）的 JSON-RPC 错误。`code` 是稳定的 snake_case，客户端据此 i18n。 */
export class AnyconRpcError extends Error {
  override readonly name: string = 'AnyconRpcError'
  /** JSON-RPC 数字 code。 */
  readonly rpcCode: number
  readonly data: RpcErrorData | null

  constructor(rpcCode: number, message: string, data?: RpcErrorData | null) {
    super(message)
    this.rpcCode = rpcCode
    this.data = data ?? null
  }

  /** 稳定错误码：优先 `data.code`，没有就按数字 code 反查，再不行是 `internal`。 */
  get code(): ErrorCode | (string & {}) {
    return this.data?.code ?? CODE_BY_NUMBER.get(this.rpcCode) ?? 'internal'
  }
  /** `invalid_settings` 时指出出错字段。 */
  get field(): string | undefined {
    return this.data?.field ?? undefined
  }
  get reason(): string | undefined {
    return this.data?.reason ?? undefined
  }
  get retryAfterMs(): number | undefined {
    return this.data?.retry_after_ms ?? undefined
  }

  static fromWire(err: RpcError): AnyconRpcError {
    return new AnyconRpcError(err.code, err.message, err.data)
  }

  /** 构造稳定码对应的错误（daemon 侧请求处理器、测试用）。 */
  static of(code: ErrorCode, message: string, extra?: Partial<RpcErrorData>): AnyconRpcError {
    return new AnyconRpcError(ERROR_CODE_NUMBERS[code], message, { code, ...extra })
  }

  toWire(): RpcError {
    const wire: RpcError = { code: this.rpcCode, message: this.message }
    if (this.data) wire.data = this.data
    return wire
  }
}

/** 请求超时。 */
export class RpcTimeoutError extends Error {
  override readonly name = 'RpcTimeoutError'
  constructor(
    readonly method: string,
    readonly timeoutMs: number,
  ) {
    super(`rpc ${method} timed out after ${timeoutMs}ms`)
  }
}

// ---------- RpcPeer ----------

export interface RpcPeerOptions {
  /** 发出一条已编码的 JSON-RPC 消息（上层负责加密 + 装信封）。同步抛错会让对应 `request()` 拒绝。 */
  send: (bytes: Uint8Array) => void
  /** `request()` 默认超时，毫秒。默认 30 秒。 */
  timeoutMs?: number
}

export interface RequestOptions {
  timeoutMs?: number
  signal?: AbortSignal
}

interface Pending {
  method: string
  resolve: (value: unknown) => void
  reject: (reason: unknown) => void
  timer: ReturnType<typeof setTimeout> | undefined
  cleanup: () => void
}

type NotificationHandler = (params: unknown, method: string) => void
type RequestHandler = (params: unknown) => unknown | Promise<unknown>

/**
 * 与传输无关的 JSON-RPC 端点。`send` 注入，入站字节喂给 `handle()`。
 * 请求 id 是递增整数；响应按 id 关联；通知按方法名分发（`'*'` 收所有）。
 */
export class RpcPeer {
  readonly #send: (bytes: Uint8Array) => void
  readonly #defaultTimeoutMs: number
  readonly #pending = new Map<RpcId, Pending>()
  readonly #notificationHandlers = new Map<string, Set<NotificationHandler>>()
  readonly #requestHandlers = new Map<string, RequestHandler>()
  #nextId = 1

  constructor(options: RpcPeerOptions) {
    this.#send = options.send
    this.#defaultTimeoutMs = options.timeoutMs ?? 30_000
  }

  /** 未答复的请求数。 */
  get pending(): number {
    return this.#pending.size
  }

  request<T = unknown>(method: string, params?: unknown, options: RequestOptions = {}): Promise<T> {
    const id = this.#nextId++
    const timeoutMs = options.timeoutMs ?? this.#defaultTimeoutMs
    return new Promise<T>((resolve, reject) => {
      const { signal } = options
      if (signal?.aborted) {
        reject(signal.reason ?? new Error('aborted'))
        return
      }
      const entry: Pending = {
        method,
        resolve: resolve as (value: unknown) => void,
        reject,
        timer: undefined,
        cleanup: () => {
          if (entry.timer !== undefined) clearTimeout(entry.timer)
          signal?.removeEventListener('abort', onAbort)
          this.#pending.delete(id)
        },
      }
      const onAbort = () => {
        entry.cleanup()
        reject(signal?.reason ?? new Error('aborted'))
      }
      if (timeoutMs > 0 && Number.isFinite(timeoutMs)) {
        entry.timer = setTimeout(() => {
          entry.cleanup()
          reject(new RpcTimeoutError(method, timeoutMs))
        }, timeoutMs)
      }
      signal?.addEventListener('abort', onAbort, { once: true })
      this.#pending.set(id, entry)
      try {
        this.#send(encodeRpc(makeRequest(id, method, params ?? {})))
      } catch (err) {
        entry.cleanup()
        reject(err)
      }
    })
  }

  notify(method: string, params?: unknown): void {
    this.#send(encodeRpc(makeNotification(method, params ?? {})))
  }

  /** 订阅通知。`method = '*'` 收全部。返回取消订阅函数。 */
  onNotification<T = unknown>(
    method: string,
    handler: (params: T, method: string) => void,
  ): () => void {
    let set = this.#notificationHandlers.get(method)
    if (!set) {
      set = new Set()
      this.#notificationHandlers.set(method, set)
    }
    const h = handler as NotificationHandler
    set.add(h)
    return () => {
      set.delete(h)
    }
  }

  /** 注册对端发来的请求的处理器（目前方向是 D→C，留给以后）。每个方法至多一个。 */
  onRequest<T = unknown, R = unknown>(
    method: string,
    handler: (params: T) => R | Promise<R>,
  ): () => void {
    this.#requestHandlers.set(method, handler as RequestHandler)
    return () => {
      if (this.#requestHandlers.get(method) === (handler as RequestHandler)) {
        this.#requestHandlers.delete(method)
      }
    }
  }

  /** 处理一条入站消息。解析失败抛 `ProtocolError`；处理器内部的错误不会向外抛。 */
  handle(bytes: Uint8Array): void {
    const msg = decodeRpc(bytes)
    if (isResponse(msg)) {
      this.#onResponse(msg)
    } else if (isRequest(msg)) {
      void this.#onRequest(msg)
    } else {
      this.#onNotification(msg)
    }
  }

  /** 拒绝所有未答复请求（连接断开时调用）。 */
  failAll(error: Error): void {
    const entries = Array.from(this.#pending.values())
    for (const entry of entries) {
      entry.cleanup()
      entry.reject(error)
    }
  }

  #onResponse(msg: RpcResponse): void {
    const entry = this.#pending.get(msg.id)
    if (!entry) return
    entry.cleanup()
    if (msg.error) entry.reject(AnyconRpcError.fromWire(msg.error))
    else entry.resolve(msg.result)
  }

  #onNotification(msg: RpcNotification): void {
    for (const key of [msg.method, '*']) {
      // 先拷贝：订阅者可能在回调里取消订阅。
      const handlers = Array.from(this.#notificationHandlers.get(key) ?? [])
      for (const handler of handlers) {
        try {
          handler(msg.params, msg.method)
        } catch {
          // 订阅者的异常不能影响其他订阅者和连接。
        }
      }
    }
  }

  async #onRequest(msg: RpcRequest): Promise<void> {
    const handler = this.#requestHandlers.get(msg.method)
    let reply: RpcResponse
    if (!handler) {
      reply = makeError(msg.id, {
        code: -32601,
        message: `method not found: ${msg.method}`,
        data: { code: 'unsupported' },
      })
    } else {
      try {
        reply = makeResult(msg.id, (await handler(msg.params)) ?? null)
      } catch (err) {
        const rpcErr =
          err instanceof AnyconRpcError
            ? err
            : AnyconRpcError.of('internal', err instanceof Error ? err.message : String(err))
        reply = makeError(msg.id, rpcErr.toWire())
      }
    }
    try {
      this.#send(encodeRpc(reply))
    } catch {
      // 连接已断，对端也拿不到答复了。
    }
  }
}
