// `kind = notify` 信封的 body：msgpack 的 `NotifyBody { category, session_id, detail }`（protocol.md § 2）。
// 外层是明文，所以 hub 能看到它（并据此触发 Web Push）；这里只用 `category` 和 `session_id`，`detail`（Noise 密文）不处理。
import { decode } from '@msgpack/msgpack'
import type { NotifyBody, NotifyCategory } from '@/protocol'

const CATEGORIES: ReadonlySet<string> = new Set<NotifyCategory>(['permission_requested', 'turn_finished', 'error', 'session_idle', 'enroll_requested', 'security'])

/** 读不懂（不是 msgpack、不是 map、类别未知）返回 null——来自网络的数据，逐字段收窄。 */
export function decodeNotifyBody(bytes: Uint8Array): NotifyBody | null {
  let raw: unknown
  try {
    raw = decode(bytes)
  } catch {
    return null
  }
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return null
  const o = raw as Record<string, unknown>
  if (typeof o.category !== 'string' || !CATEGORIES.has(o.category)) return null
  const session = typeof o.session_id === 'string' && o.session_id !== '' ? o.session_id : null
  return { category: o.category as NotifyCategory, session_id: session }
}
