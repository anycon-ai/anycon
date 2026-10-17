// 内层 JSON-RPC 客户端：id 分配、超时、响应匹配、通知分发。
// 一个 `data` 信封 = 一条 JSON-RPC 消息；发送由注入的 sender 负责（加密 + 信封 + 发给 hub）。
import type { ErrorCode, RpcError } from '@/protocol'
import { RPC_TIMEOUT_MS } from '@/lib/constants'
import { decodeRpc, encodeRpc, isRpcNotification, isRpcResponse, makeRequest, type RpcMessage } from './wire'

export type RpcFailKind = 'rpc' | 'timeout' | 'undeliverable' | 'offline' | 'closed' | 'too_large'

/** 单条请求（明文）的上限。接收端重组上限是 16 MiB 密文，留足余量。 */
export const MAX_REQUEST_BYTES = 8 * 1024 * 1024

export class RpcCallError extends Error {
  constructor(
    public readonly kind: RpcFailKind,
    message: string,
    public readonly rpc?: RpcError,
  ) {
    super(message)
    this.name = 'RpcCallError'
  }
  /** daemon 给的稳定错误码（客户端 i18n 的唯一依据）。 */
  get code(): ErrorCode | undefined {
    return this.rpc?.data?.code
  }
  get field(): string | undefined {
    return this.rpc?.data?.field ?? undefined
  }
  get reason(): string | undefined {
    return this.rpc?.data?.reason ?? undefined
  }
}

/**
 * 发送一条明文 JSON-RPC。返回它占用的全部信封 seq（大消息会分成多片，每片一个信封）；
 * 返回 null 表示当前发不出去（链路没就绪）。
 */
export type RpcSender = (plain: Uint8Array) => number[] | null

interface Pending {
  resolve: (v: unknown) => void
  reject: (e: RpcCallError) => void
  timer: ReturnType<typeof setTimeout>
  seqs: number[]
}

export class RpcClient {
  private nextId = 1
  private readonly pending = new Map<number, Pending>()
  private readonly bySeq = new Map<number, number>()
  onNotification: (method: string, params: unknown) => void = () => {}

  /** 已发出、还没有结果的请求数（页面进后台时用来判断能不能断开 hub 连接）。 */
  get pendingCount(): number {
    return this.pending.size
  }

  constructor(
    private readonly sender: RpcSender,
    private readonly defaultTimeoutMs: number = RPC_TIMEOUT_MS,
  ) {}

  request<R = unknown>(method: string, params: unknown = {}, timeoutMs: number = this.defaultTimeoutMs): Promise<R> {
    const id = this.nextId++
    const bytes = encodeRpc(makeRequest(id, method, params))
    if (bytes.length > MAX_REQUEST_BYTES) return Promise.reject(new RpcCallError('too_large', `${method}: request is ${bytes.length} bytes, the limit is ${MAX_REQUEST_BYTES}`))
    const seqs = this.sender(bytes)
    if (seqs === null) return Promise.reject(new RpcCallError('offline', `${method}: node link not ready`))
    return new Promise<R>((resolve, reject) => {
      const timer = setTimeout(() => this.settle(id, new RpcCallError('timeout', `${method}: timed out after ${timeoutMs} ms`)), timeoutMs)
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer, seqs })
      for (const seq of seqs) this.bySeq.set(seq, id)
    })
  }

  /** 收到一条解密后的明文。 */
  handle(plain: Uint8Array): void {
    let msg: RpcMessage
    try {
      msg = decodeRpc(plain)
    } catch (e) {
      console.warn('anycon: dropping an undecodable JSON-RPC message', e)
      return // 无法对应到任何请求
    }
    if (isRpcResponse(msg)) {
      if (typeof msg.id !== 'number') return
      const p = this.pending.get(msg.id)
      if (!p) return
      if (msg.error) this.settle(msg.id, new RpcCallError('rpc', msg.error.message, msg.error))
      else this.settle(msg.id, undefined, msg.result)
    } else if (isRpcNotification(msg)) {
      this.onNotification(msg.method, msg.params)
    }
  }

  /** hub 回了 `undeliverable { seq }`（或丢弃了某一片）：那条请求没有完整到达 daemon。 */
  undeliverable(seq: number): boolean {
    const id = this.bySeq.get(seq)
    if (id === undefined) return false
    this.settle(id, new RpcCallError('undeliverable', 'peer offline: message not delivered'))
    return true
  }

  rejectAll(kind: RpcFailKind, message: string): void {
    for (const id of [...this.pending.keys()]) this.settle(id, new RpcCallError(kind, message))
  }

  private settle(id: number, err?: RpcCallError, value?: unknown): void {
    const p = this.pending.get(id)
    if (!p) return
    clearTimeout(p.timer)
    this.pending.delete(id)
    for (const seq of p.seqs) this.bySeq.delete(seq)
    if (err) p.reject(err)
    else p.resolve(value)
  }
}
