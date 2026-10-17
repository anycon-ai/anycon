// hub HTTP 响应的形状（docs/spec/hub.md § 2）。hub 的 HTTP 不在 crates/protocol 里，所以这里手写。
import type { NotifyCategory } from '@/protocol'

export interface ApiConfig {
  mode: string
  public_url?: string
  /** 启用的登录方式；配了 SMTP 才有 `email`（ADR-37）。 */
  login_methods: string[]
  protocol_version: number
  version: string
  queue_enabled?: boolean
  /** Web Push 的 VAPID 公钥（base64url，65 字节未压缩点）。 */
  vapid_public_key?: string | null
  /** hub 能发 Web Push（有 VAPID 密钥）。 */
  push_available?: boolean
}
export interface ApiMe {
  id: string
  display_name: string
  email?: string | null
  is_admin: boolean
  login_methods: string[]
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
