// 预览（ADR-39）测试用的夹具：票据、端口、节点信息、设备 token。
import { putIdentity } from '@/db'
import type { NodeInfo, PreviewInfo, PreviewOpenResult, PreviewPort } from '@/protocol'
import { nodeInfo } from './factories'

/** 节点的设备 id：真实的是 32 位小写 hex，启动参数要校验它。 */
export const PREVIEW_NODE = 'b'.repeat(32)
export const PREVIEW_TEMPLATE = 'https://{label}.preview.test'
/** 合法的预览 label：26 个 `[a-z2-7]`。 */
export const LABEL = 'abcdefghijklmnopqrstuvwxyz'
/** 节点的 Noise 静态公钥（32 字节）；启动参数里是它的小写 hex。 */
export const NODE_KEY = new Uint8Array(32).fill(0xab)
export const NODE_KEY_HEX = 'ab'.repeat(32)
export const DEVICE_TOKEN = 'device-token-xyz'
export const TICKET_ID = 'c'.repeat(32)
export const PSK = 'd'.repeat(64)
export const HOUR_MS = 3_600_000

/** 支持预览（`capabilities.preview = true`）的节点信息。 */
export function previewNodeInfo(over: Partial<NodeInfo> = {}): NodeInfo {
  const base = nodeInfo()
  return { ...base, capabilities: { ...base.capabilities, preview: true }, ...over }
}

export function previewPort(port: number, over: Partial<PreviewPort> = {}): PreviewPort {
  return { port, source: 'session', session_id: 'sess_a', pid: 4242, process: 'node', ...over }
}

export function openResult(over: Partial<PreviewOpenResult> = {}): PreviewOpenResult {
  return { label: LABEL, port: 5173, ticket_id: TICKET_ID, psk: PSK, expires_at_ms: Date.now() + 24 * HOUR_MS, session_id: 'sess_a', ...over }
}

export function previewInfo(over: Partial<PreviewInfo> = {}): PreviewInfo {
  return {
    label: LABEL,
    port: 5173,
    ticket_id: TICKET_ID,
    session_id: 'sess_a',
    created_by: 'e'.repeat(32),
    created_by_name: 'Chrome on Mac',
    created_at_ms: Date.now() - HOUR_MS,
    expires_at_ms: Date.now() + 23 * HOUR_MS,
    tunnels: 0,
    ...over,
  }
}

/** 把设备 token 放进本地库（预览令牌请求用它做 Bearer）；传 `null` 是还没有 token 的浏览器。 */
export async function seedDeviceToken(token: string | null = DEVICE_TOKEN): Promise<void> {
  await putIdentity({ id: 'self', device_id: 'f'.repeat(32), ...(token === null ? {} : { device_token: token }), static_keypair: { public: new Uint8Array(32), secret: new Uint8Array(32) }, device_name: 'test' })
}
