// 推送载荷（hub.md § 6.1）：`{ category, node, session }`，RFC 8291 解密后的明文 JSON。载荷来自 hub，按不可信输入处理：
// 类别必须是已知的，id 只认各自的字符集（不会拼出别的路径、query 或别的 origin）。
import type { PushCategory } from '@/features/sync/pushText'

const CATEGORIES: ReadonlySet<string> = new Set<PushCategory>(['permission_requested', 'turn_finished', 'error', 'session_idle', 'enroll_requested', 'security', 'test'])

export interface PushPayload {
  category: PushCategory
  /** 节点（daemon）的设备 id；`test` 没有。 */
  node: string | null
  /** 会话 id；接入类和 `test` 没有。 */
  session: string | null
}

/** 节点（daemon 设备 id）的字符集：字母、数字、`_`、`-`，≤ 128。 */
export function isNodeId(v: unknown): v is string {
  return typeof v === 'string' && /^[A-Za-z0-9_-]{1,128}$/.test(v)
}

/** 会话 id 的字符集：和 hub 放行进推送载荷的一致（`crates/hub/src/notify.rs::clean_session_id`，hub.md § 6.1）——字母、数字、`.`、`_`、`-`，≤ 128。 */
export function isSessionId(v: unknown): v is string {
  return typeof v === 'string' && /^[A-Za-z0-9._-]{1,128}$/.test(v)
}

/** 读不懂（不是对象、类别未知、非 `test` 却没有节点）返回 null。 */
export function parsePushPayload(raw: unknown): PushPayload | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  if (typeof o.category !== 'string' || !CATEGORIES.has(o.category)) return null
  const category = o.category as PushCategory
  const node = isNodeId(o.node) ? o.node : null
  const session = isSessionId(o.session) ? o.session : null
  if (category !== 'test' && node === null) return null
  return { category, node, session }
}

/** `PushEvent.data` → 载荷：没有数据、不是 JSON 都当作读不懂。 */
export function readPushPayload(data: { json(): unknown } | null): PushPayload | null {
  if (!data) return null
  try {
    return parsePushPayload(data.json())
  } catch {
    return null
  }
}
