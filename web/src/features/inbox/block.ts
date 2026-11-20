// 待答复的请求 → 会话页用的权限块：「待处理」页里的卡片和会话页是同一个 `PermissionCard`，吃同一种数据。
import type { PermissionBlock } from '@/features/session/reducer'
import type { PendingPermission } from '@/stores/sessions'

export function permissionBlock(req: PendingPermission): PermissionBlock {
  return {
    key: `perm-${req.request_id}`,
    seq: 0,
    ts: req.created_at_ms,
    type: 'permission',
    requestId: req.request_id,
    kind: req.kind,
    title: req.title,
    detail: req.detail,
    options: req.options,
    expiresAtMs: req.expires_at_ms,
  }
}
