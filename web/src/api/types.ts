// hub HTTP 响应的形状（docs/spec/hub.md § 2）。hub 的 HTTP 不在 crates/protocol 里，所以这里手写。
import type { NotifyCategory } from '@/protocol'

/** hub 的运行模式：`single`（自托管，邀请制）或 `multi`（公共 hub，开放注册、有配额）。 */
export type HubMode = 'single' | 'multi'
/** 支持的 OAuth 登录方（GitHub / Google）。 */
export type OAuthProvider = 'github' | 'google'
/** 登录方式。`/api/config` 和 `/api/me` 的 `login_methods` 都是这几个值，顺序是 github、google、email、token。 */
export type LoginMethod = OAuthProvider | 'email' | 'token'

export interface ApiConfig {
  /** `single` / `multi`；认不出的值按 `single` 处理（见 `isMultiMode`）。 */
  mode: string
  public_url?: string
  /**
   * 启用的登录方式：`github` / `google`（配了 OAuth 才有）、`email`（配了 SMTP 才有，ADR-37）、
   * `token`（只有 `single`）。
   */
  login_methods: LoginMethod[]
  protocol_version: number
  version: string
  queue_enabled?: boolean
  /** Web Push 的 VAPID 公钥（base64url，65 字节未压缩点）。 */
  vapid_public_key?: string | null
  /** hub 能发 Web Push（有 VAPID 密钥）。 */
  push_available?: boolean
  /** 预览 origin 的模板（如 `https://{label}.aiplace.app`，hub.md § 13.1）；没配预览是 `null`。 */
  preview_origin_template?: string | null
}
export interface ApiMe {
  id: string
  display_name: string
  email?: string | null
  /** 有效管理员（库里的标记，或 `multi` 下邮箱在 `ANYCON_ADMIN_EMAILS` 里）。 */
  is_admin: boolean
  /** 这个账号现在真能用来登录的方式（已配置 + 已绑定）。只剩一种时它不能被移除。 */
  login_methods: LoginMethod[]
}
/** `GET /api/me/identities` 的一行：已关联的 OAuth 身份。hub 不存也不返回用户名、邮箱和 subject。 */
export interface ApiIdentity {
  provider: OAuthProvider
  created_at_ms: number
}
/**
 * `GET /api/quota`：当前用量和上限。`*_max` 为 `null` 表示不限（`single` 恒为 `null`）。
 * `daily_pairings` 是滚动 24 小时内发起的配对与接入次数（`pair_begin` / `enroll_begin`；加入配对 `pair_join` 不计）。
 */
export interface ApiQuota {
  devices: number
  devices_max: number | null
  daemons: number
  daemons_max: number | null
  connections: number
  connections_max: number | null
  daily_pairings: number
  daily_pairings_max: number | null
}
/** `GET /api/admin/accounts` 的一行（只有元数据）。 */
export interface ApiAdminAccount {
  id: string
  display_name: string
  email?: string | null
  /** 有效管理员。 */
  is_admin: boolean
  /** 管理员身份来自 `ANYCON_ADMIN_EMAILS`：在管理页取消不了，要改 hub 配置。 */
  admin_by_config: boolean
  created_at_ms: number
}
export interface ApiBrowserSession {
  id: string
  user_agent: string
  ip_prefix: string
  created_at_ms: number
  last_seen_ms: number
  current: boolean
}
export interface ApiToken {
  id: string
  label: string
  created_at_ms: number
  last_used_ms?: number | null
}
export interface ApiDevice {
  id: string
  name: string
  kind: 'daemon' | 'web' | 'cli' | 'sdk'
  platform: string
  origin?: string | null
  created_at_ms: number
  last_seen_ms?: number | null
  online: boolean
  paired_with: string[]
}
/** `GET /api/invites` 不含明文 code，明文只在创建时返回一次。 */
export interface ApiInvite {
  id: string
  note?: string | null
  created_at_ms?: number
  expires_at_ms: number
  used?: boolean
  used_by?: string | null
  used_at_ms?: number | null
}
export interface ApiInviteCreated {
  id: string
  code: string
  url: string
  expires_at_ms: number
}
export interface ApiAdminStatus {
  mode?: string
  version?: string
  started_at_ms?: number
  connections?: number
  devices?: number
  accounts?: number
  [k: string]: unknown
}

/** `GET /api/push/subscriptions` 的一行：按设备（浏览器）存，endpoint 只给主机名（路径本身是个凭据）。 */
export interface ApiPushSubscription {
  id: string
  device_id: string
  /** hub 附带的设备名；没有就在 `/api/devices` 里按 `device_id` 查。 */
  device_name?: string
  endpoint_host: string
  created_at_ms: number
}
/** `POST /api/push/subscriptions` 的请求体：`PushSubscription.toJSON()` 加上本浏览器的 web 设备 id。 */
export interface ApiPushSubscribe {
  device_id: string
  endpoint: string
  keys: { p256dh: string; auth: string }
}
/** `POST /api/push/test` 的结果（hub.md § 2.3）：推送服务的回应。`status` 是推送服务回的 HTTP 状态码，只在 `error` 时可能有（连不上就没有）。 */
export interface ApiPushTestResult {
  result: 'ok' | 'gone' | 'error'
  status?: number
}

/** `POST /api/preview/tokens` 的结果（hub.md § 13.3）：预览令牌 24 小时有效，`origin` 是 `https://<label>.<预览域名>[:端口]`。 */
export interface ApiPreviewToken {
  token: string
  origin: string
  expires_at_ms: number
}

/** 通知偏好里的一个类别（hub.md § 6.3）：走不走 Web Push、走不走邮件。 */
export interface ApiNotificationPref {
  category: NotifyCategory
  push: boolean
  email: boolean
}
export interface ApiNotifications {
  categories: ApiNotificationPref[]
  /** 配了 SMTP 且账号绑定了邮箱。 */
  email_available: boolean
  push_available: boolean
}
