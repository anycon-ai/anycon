// 接入二维码（ADR-36）：`anycon-enroll:1:<node_id>:<enroll_id>:<base32(r)>`。不是 URL：只有 Web UI 内的扫码能读，
// 系统相机扫到它只是一串文字，不能被做成「点这个链接就批准」的钓鱼。r 是新设备生成的 32 字节扫码密钥，只在内存里。
import { base32Decode, base32Encode } from '@/lib/base32'

export const ENROLL_QR_PREFIX = 'anycon-enroll:1:'
const SECRET_BYTES = 32
// 设备 id 和 enroll_id 都是 hub 生成的随机串（现在是 32 位 hex）。这里只防「冒号 / 空白 / 奇怪字符」，不把长度钉死
const ID = /^[A-Za-z0-9_-]{8,64}$/

export function buildEnrollQr(nodeId: string, enrollId: string, secret: Uint8Array): string {
  return `${ENROLL_QR_PREFIX}${nodeId}:${enrollId}:${base32Encode(secret)}`
}

export interface ParsedEnrollQr {
  nodeId: string
  enrollId: string
  /** 32 字节扫码密钥。用完 `fill(0)`。 */
  secret: Uint8Array
}

/** 解析接入二维码的文字；不是 `anycon-enroll:1:` 开头、或格式不对（字段数、id、密钥长度 / 编码）一律返回 null。 */
export function parseEnrollQr(text: string): ParsedEnrollQr | null {
  if (!text.startsWith(ENROLL_QR_PREFIX)) return null
  const parts = text.slice(ENROLL_QR_PREFIX.length).split(':')
  if (parts.length !== 3) return null
  const [nodeId, enrollId, encoded] = parts as [string, string, string]
  if (!ID.test(nodeId) || !ID.test(enrollId)) return null
  const secret = base32Decode(encoded)
  if (!secret || secret.length !== SECRET_BYTES) return null
  return { nodeId, enrollId, secret }
}
