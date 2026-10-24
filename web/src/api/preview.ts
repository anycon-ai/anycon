// 预览令牌（docs/spec/hub.md § 13.3，ADR-39）：hub 防滥用的那一层。
import { apiFetch } from './client'
import type { ApiPreviewToken } from './types'

/**
 * `POST /api/preview/tokens { node, label }`：已配对设备拿设备 token 换一个预览令牌，hub 同时把 `label` 绑定到这个节点。
 * 只认 `Authorization: Bearer <设备 token>`，不看 cookie。令牌是密钥，不写日志、不放进路由状态。
 *
 * 错误码：`404 preview_disabled`（hub 没配预览）、`400 invalid_label`、`401 unauthorized`（不是本 hub 的正式设备）、
 * `404 not_found`（节点不存在或没和这台设备配对）、`409 label_taken` / `409 too_many_labels`。
 */
export function requestPreviewToken(deviceToken: string, node: string, label: string, signal?: AbortSignal): Promise<ApiPreviewToken> {
  return apiFetch<ApiPreviewToken>('/api/preview/tokens', { method: 'POST', body: { node, label }, bearer: deviceToken, signal })
}
