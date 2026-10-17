// blob.* 方法的类型化封装：参数和结果类型来自生成的 `RequestMethods`（packages/protocol）。
import type { RequestMethods } from '@/protocol'
import { rpc } from '@/transport/instance'

type Blobish = Extract<keyof RequestMethods, `blob.${string}`>

export function blobCall<M extends Blobish>(nodeId: string, method: M, params: RequestMethods[M]['params'], timeoutMs?: number): Promise<RequestMethods[M]['result']> {
  return rpc().request<RequestMethods[M]['result']>(nodeId, method, params, timeoutMs)
}
