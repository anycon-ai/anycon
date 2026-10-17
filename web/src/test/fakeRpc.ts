import type { ErrorCode } from '@/protocol'
import type { NodeRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'

export type Handler = (params: unknown, nodeId: string) => unknown | Promise<unknown>

/** 业务层测试用的假 RPC：按方法名返回预设结果，记录调用顺序。 */
export class FakeRpc implements NodeRpc {
  ready = true
  calls: Array<{ nodeId: string; method: string; params: unknown }> = []
  handlers: Record<string, Handler> = {}

  isReady(): boolean {
    return this.ready
  }

  async request<R = unknown>(nodeId: string, method: string, params: unknown = {}): Promise<R> {
    this.calls.push({ nodeId, method, params })
    if (!this.ready) throw new RpcCallError('offline', `${method}: node link not ready`)
    const h = this.handlers[method]
    if (!h) throw new RpcCallError('rpc', `no handler for ${method}`, { code: -32009, message: 'unsupported', data: { code: 'unsupported' } })
    return (await h(params, nodeId)) as R
  }

  methods(): string[] {
    return this.calls.map((c) => c.method)
  }
}

export function rpcError(code: ErrorCode, field?: string, reason?: string): RpcCallError {
  return new RpcCallError('rpc', code, makeData(code, field, reason))
}
const makeData = (code: ErrorCode, field?: string, reason?: string) => ({ code: -32000, message: code, data: { code, field, reason } })
