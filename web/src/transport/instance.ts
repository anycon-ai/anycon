// 全局唯一的 AnyconClient（真实 WebSocket + wasm + IndexedDB）和给业务层用的窄接口。
// 业务代码通过 `rpc()` 发请求，单元测试用 `setRpc()` 换成假的。
import { loadCrypto } from './crypto'
import { AnyconClient, idbStorage } from './client'
import { inlineKdf, workerKdf } from './kdf'

export interface NodeRpc {
  request<R = unknown>(nodeId: string, method: string, params?: unknown, timeoutMs?: number): Promise<R>
  isReady(nodeId: string): boolean
}

let client: AnyconClient | null = null
let override: NodeRpc | null = null

export function getClient(): AnyconClient {
  client ??= new AnyconClient({
    crypto: () => loadCrypto(),
    wsFactory: (url) => new WebSocket(url) as unknown as ReturnType<ConstructorParameters<typeof AnyconClient>[0]['wsFactory']>,
    wsUrl: () => `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/v1/ws`,
    storage: idbStorage,
    // Argon2id 在一次性 Worker 里算，算完就关：页面自己的 wasm 实例不会被撑大 64 MiB（见 ./kdf.ts）
    kdf: (crypto) => (typeof Worker === 'undefined' ? inlineKdf(crypto) : workerKdf(inlineKdf(crypto))),
  })
  return client
}

export function rpc(): NodeRpc {
  return override ?? getClient()
}

/** 测试用。传 null 恢复。 */
export function setRpc(fake: NodeRpc | null): void {
  override = fake
}
