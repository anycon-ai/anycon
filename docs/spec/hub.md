# Hub 功能规格

> 细化 [architecture.md § 2.1 / § 3](../architecture.md) 和 [protocol.md § 8](../protocol.md#8-控制消息hub--设备)。本文列出 hub 对外的**全部表面**：HTTP 路由、WebSocket 行为、数据表、限速与配额、日志纪律。字段名以 `crates/protocol` 和 hub 的代码（`crates/hub/src/http`）为准，本文是设计说明。

## 1. 一条判据

hub 的每个功能都要能回答一个问题：**这个功能需要看消息内容吗？** 需要，就不属于 hub。

由此 hub 只有五类事：账号、设备与路由、配对与接入撮合、通知转发、静态文件。下面所有章节都是这五类的展开。

## 2. HTTP 表面

> **实现范围**：静态与健康、`/api/config`、`POST /auth/token` · `/auth/invite` · `/auth/logout`、邮箱验证码登录（`/auth/email/*`，配了 SMTP 才有；[ADR-37](../decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)）、`/api/me`（含 `sessions`、`tokens`、`email`、`notifications`）、`/api/devices`、`/api/invites`、`/api/push/*`（Web Push）、`/api/admin/status` · `/api/admin/audit` · `GET /api/admin/accounts`（只读、仅管理员、只含账号元数据）。passkey、OAuth、配额、`DELETE /api/me` 不在内（见 [roadmap](../roadmap.md)）；`/api/config` 里的 `queue_enabled` 恒为 `false`。

### 2.1 静态与健康

| 路径 | 说明 |
|---|---|
| `/`、`/assets/*`、`/manifest.webmanifest`、`/sw.js`、`/THIRD-PARTY-LICENSES.txt` | 嵌入的 Web UI（最后一个是 Web UI 里 npm 包的许可证清单，构建时生成）。所有不匹配 `/api`、`/auth`、`/v1` 的路径回退到 `index.html`（SPA 路由） |
| `GET /healthz` | 进程活着。返回版本号，不查库 |
| `GET /readyz` | 数据库可用、migration 已跑完。反代和容器编排用这个 |
| `GET /api/config` | 公开配置：`mode`、`public_url`、启用的登录方式（`["token"]`，配了 SMTP 时 `["token","email"]`）、是否开启队列、`vapid_public_key`（恒有值）、`push_available`（恒 `true`：VAPID 密钥缺失 hub 起不来）、协议版本、`preview_origin_template`（配了预览时如 `"https://{label}.aiplace.app"`，否则 `null`，§ 13）。登录页和客户端启动时拉一次，不需要认证 |

**安全头**：HTML 响应（`index.html` 和 SPA 回退，含 304）带 `Content-Security-Policy`；JSON 和静态资源不带：

```
default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self';
img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; font-src 'self' data:;
worker-src 'self'; manifest-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'
```

配了预览（§ 13）时在 `frame-ancestors` 前面多一段 `frame-src <预览 scheme>://*.<预览域名>[:端口];`，Web UI 才能用 iframe 内嵌预览。没有 `unsafe-inline` 的脚本（`index.html` 里不能有内联脚本，主题脚本是外置文件）；`connect-src 'self'` 让页面只能连 hub 自己；`img-src` 里没有 `https:`，所以 Markdown 里的远程图片加载不出来（Web UI 另外把它们默认渲染成「点击加载」的占位）。HTML 响应另带 `Cross-Origin-Opener-Policy: same-origin`；所有响应带 `X-Content-Type-Options: nosniff`、`Referrer-Policy: no-referrer`、`X-Frame-Options: DENY`。CSP 的作用是限制页面能加载什么、能向哪里发请求（挡被注入内容、第三方脚本和外泄通道）；**它挡不住 hub 自己下发的脚本**——Web UI 的代码由 hub 下发，这是 [architecture.md § 5.6](../architecture.md#56-威胁模型) 里单独声明的信任前提。反代不要覆盖或去掉这些头。

### 2.2 认证 `/auth/*`

登录只发生在 Web UI。所有写操作都要求 `Content-Type: application/json` 且带 `X-Anycon: 1` 头，配合 `SameSite=Lax` cookie 挡 CSRF。

| 路径 | 说明 |
|---|---|
| `POST /auth/token` | `{ token }` → 设 session cookie。仅 `single` 模式。失败按 IP 限速 |
| `POST /auth/passkey/register/begin` · `finish` | 已登录状态下绑定 passkey。`begin` 返回 WebAuthn 创建选项 |
| `POST /auth/passkey/login/begin` · `finish` | 免密登录。支持 conditional UI（浏览器自动填充） |
| `POST /auth/email/send` | `{ email }` → `200 { ok, resend_after_secs }`；邮箱格式不对 `400 invalid_email`；没配 SMTP 时这几条邮箱路由都是 `404 not_found`。**总是** 200（防枚举）：邮箱绑定了账号才发码，邮件在后台任务里发，响应时间不随邮箱是否注册变化。`single` 模式只给已绑定邮箱的账号发（ADR-37），不能用邮箱注册 |
| `POST /auth/email/verify` | `{ email, code }`（码容忍 `123 456` / `123-456`）。成功 `204` + cookie；码不对、过期、用过、不存在、邮箱格式不对一律 `401 invalid_code`（不说是哪一种），失败的响应一律在请求开始后至少 250 ms 才发出（不让响应时间暴露邮箱是否绑定）；每个码**尝试** 5 次就作废（找码、加尝试次数、比对在一条原子的 SQL 里完成，并发猜测也只有 5 次机会）；比对成功时原子认领，并发提交同一个码只有一个成功。`multi` 的「未注册邮箱直接建号」随 `multi` 一起做 |
| `GET /auth/oauth/:provider/start` | 跳转到 provider。query 可带 `invite`（注册）或 `link=1`（已登录时关联身份）。`state` 存服务端，10 分钟有效 |
| `GET /auth/callback/:provider` | 回调。按 [architecture.md § 3.2](../architecture.md#32-账号与身份) 的规则匹配或创建账号，然后 302 到 `/` |
| `POST /auth/logout` | 删当前 web session |

### 2.3 账号与设备 `/api/*`

全部要求登录态。

| 路径 | 说明 |
|---|---|
| `GET /api/me` | `{ id, display_name, email?, is_admin, login_methods }`。`login_methods` 是「现在能用来登录的方式」：`token`（管理员的 bootstrap token，或账号自己建过登录 token）、`email`（绑定了邮箱**且** hub 配了 SMTP），可以是空数组（受邀进来、没建 token、没绑邮箱）；解绑邮箱时的「最后一种登录方式」按它判断 |
| `PATCH /api/me` | 改显示名 |
| `POST /api/me/email` · `POST /api/me/email/verify` · `DELETE /api/me/email` | 绑定 / 换绑邮箱：`{ email }` 发码（邮箱规范化为去空白 + 小写；已被别的账号占用 → `409 email_taken`；限速同 `/auth/email/send`），`{ email, code }` 生效，回更新后的 `/api/me`；错码是 **`400 invalid_code`**（不是 401：账号登录着，前端遇到 401 会当登录失效）；验证时被别人先占了 `409 email_taken`。解绑：账号没有别的登录方式时 `409 last_login_method`，没绑定过也是 `204`。审计 `email_changed`（`detail.change` = `bound` / `changed` / `removed`，不记邮箱）。配了 SMTP 才存在 |
| `GET /api/me/sessions` · `DELETE /api/me/sessions/:id` | 浏览器登录会话列表（UA、IP 前缀、最近活动），可单个踢出 |
| `GET /api/me/tokens` · `POST` · `DELETE /:id` | 登录 token。`POST` 返回明文一次，库里存 blake2 哈希。仅 `single` |
| `GET /api/me/passkeys` · `PATCH /:id`（改名）· `DELETE /:id` | |
| `GET /api/me/identities` · `DELETE /:provider` | OAuth 身份。最后一种登录方式不能删，返回 `409 last_login_method` |
| `GET /api/devices` | 本账号全部设备：id、名称、kind（`daemon` / `web` / `cli` / `sdk`）、平台、origin（web 设备）、创建时间、最近在线、当前是否在线、与哪些设备配对。**也接受设备 token**（`Authorization: Bearer <device token>`，只读，返回内容和 cookie 登录时一样）：CLI 已经是这个 hub 上的设备时用它列节点，不必再登录一次（[spec/cli.md § 3.3](cli.md)）。带了 `Authorization` 头就只看它（不回退到 cookie）；token 不存在、已吊销、不是 `Bearer` 都是 `401 unauthorized`。设备 token 只开放这个接口和 `POST /api/preview/tokens`（§ 13.3，只认设备 token），`PATCH` / `DELETE` 和其余 `/api/*` 仍然只认登录 cookie |
| `PATCH /api/devices/:id` | 改名 |
| `DELETE /api/devices/:id` | 吊销：断连接、删配对、删 push 订阅。**只是 hub 侧的一半**，daemon 侧的公钥删除由客户端通过 `node.revoke_device` 完成；UI 两步一起做 |
| `GET /api/invites` · `POST` · `DELETE /:id` | 邀请链接。`single` 模式管理员专用；`multi` 不开放。单次使用、默认 7 天过期、可附备注 |
| `GET /api/push/subscriptions` · `POST` · `DELETE /:id` | Web Push 订阅，按设备（浏览器）存（§ 6.1）。`POST { device_id, endpoint, keys: { p256dh, auth } }`：`device_id` 必须是本账号未吊销的 web 设备（否则 `404 device_not_found`）；endpoint 必须是 https 且主机在白名单里（否则 `400 push_endpoint_not_allowed`）；同一设备只留一条。`POST` 回 `200` 和这一行；`GET` 的行是 `{ id, device_id, device_name, endpoint_host, created_at_ms }`（endpoint 本身和密钥不回显）|
| `POST /api/push/test` | `{ device_id }`：给这台设备的订阅发一条测试推送（载荷 `{ "category": "test" }`），同步等推送服务的结果（最多 10 秒），回 `{ result: ok | gone | error, status? }`（`gone` 时顺手删了订阅）；设备没有订阅 `404 subscription_not_found`；`TTL: 60` |
| `GET /api/me/notifications` · `PUT` | 通知偏好：每个 `category` 走哪些渠道（push / email），见 § 6.3。`GET` → `{ categories: [{ category, push, email }]（六个，顺序固定）, email_available（配了 SMTP 且账号绑定了邮箱）, push_available }`；`PUT { categories }` 可以只带要改的类别，回完整结果 |
| `GET /api/quota` | 配额用量。`single` 返回全部 unlimited |
| `DELETE /api/me` | 删账号（v0.2，`multi`）。级联删设备、配对、订阅；要求重新认证 |

### 2.4 管理 `/api/admin/*`

`single` 模式下第一个账号是管理员（`accounts.is_admin`）；`multi` 模式的管理员由 `ANYCON_ADMIN_EMAILS` 指定。

| 路径 | 说明 |
|---|---|
| `GET /api/admin/status` | 模式、版本、启动时间、在线连接数、设备数、账号数、队列状态、数据库大小 |
| `GET /api/admin/accounts` | 账号列表（只读元数据：`id`、`display_name`、`email`、`is_admin`、`created_at_ms`）。支持分页（§ 2.5）。v0.1 只有列表；删账号、授予管理员（下面两行）不在 v0.1 |
| `DELETE /api/admin/accounts/:id` | 删账号 |
| `POST /api/admin/accounts/:id/admin` · `DELETE` | 授予 / 取消管理员 |
| `GET /api/admin/audit` | hub 侧审计：登录、设备增删、吊销、配对（只有元数据，见 § 9） |

### 2.5 错误格式与通用约定

```json
{ "error": { "code": "invite_required", "message": "…", "retry_after": 30 } }
```

- `code` 是稳定的 snake_case 枚举，客户端据此做 i18n；`message` 是英文调试信息
- 限速返回 `429` + `Retry-After`
- 可能很长的列表支持 `?limit=&cursor=`（keyset 分页，不做 offset 分页）：v0.1 是 `/api/invites`、`/api/admin/audit`、`/api/admin/accounts`。`limit` 缺省 100，夹在 1–500（不是数字 → 400）；响应体仍是裸数组，下一页的游标（不透明字符串）在 `X-Next-Cursor` 响应头里，没有下一页就没有这个头；带着它原样传回 `?cursor=` 取下一页，格式不对的游标 → 400。`/api/devices`、`/api/me/sessions`、`/api/me/tokens` 是每账号的小集合，不分页
- 不开 CORS。Web UI 同源；SDK 不走 HTTP API，只走 WebSocket。CLI 只有一个例外：账号内接入（§ 4.1）时临时调 `/auth/email/send` · `/auth/email/verify`（`single` 模式也可以 `/auth/token`），拿到的 session cookie 只放在内存里、只用于那一次 WebSocket 升级，结束就 `/auth/logout`（[ADR-36](../decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)）。它和浏览器一样要带 `X-Anycon: 1`。CLI 已经是这个 hub 上的设备时连这次临时登录都省了：列账号里的节点（`GET /api/devices`）带设备 token 就行（§ 2.3）

## 3. WebSocket 表面 `/v1/ws`

协议帧格式见 [protocol.md § 2–3](../protocol.md#2-传输)。这里是 hub 侧的行为规则。

### 3.1 连接生命周期

| 阶段 | 规则 |
|---|---|
| 升级 | 校验 `Origin`（`ANYCON_PUBLIC_URL`、自身 Host、私有网段 http 自动放行）。失败 `403` |
| 认证 | 第一帧必须是 `control.auth { device_token, protocol_version }`，5 秒内未收到则关闭。token 查哈希；设备被吊销则 `auth_fail { reason: revoked }` |
| 单连接 | 一个设备同时只允许一条连接。新连接认证成功后旧连接收到 close code `4001 superseded`。手机切网时常见 |
| 上线通知 | 认证成功后，hub 向该设备发 `auth_ok { device_id, server_time_ms, peers_online, quota? }`（`peers_online` 是此刻在线的配对设备列表），并向这些**在线的配对设备**发 `peer_online` |
| 心跳 | hub 每 30 秒 ping；90 秒无 pong 断开。设备侧不需要自己发心跳 |
| 下线 | 连接关闭后向配对设备发 `peer_offline`。断线后 `last_seen` 更新 |
| 关停 | hub 优雅关停时先发 close code `4002 restarting`，客户端据此用短退避（1–3 秒抖动）重连，而不是指数退避 |

close code 全表（`auth_fail` 总是先于 close 发出）：

| code | 含义 | 客户端该做什么 |
|---|---|---|
| `4001 superseded` | 同一设备的新连接顶掉了这一条 | 别无脑重连抢回来；退避并提示「另一个实例在用这个设备身份」（daemon ≥ 30 秒；web 显示「在此标签页使用」） |
| `4002 restarting` | hub 重启 / 关停 | 1–3 秒抖动后重连 |
| `4003 slow_consumer` | 发送缓冲持续满 30 秒；或一条 `backpressure` 回执进不了它的控制消息队列（它不读 socket，再留着顺序就乱了） | 正常退避重连，重连后按 `seq` 补事件 |
| `4004 revoked` | 设备已被吊销 | token 作废，停止重连，提示重新配对 |
| `4005 auth_timeout` | 5 秒内没发 `auth`，或第一帧不是 `auth`（先收到 `auth_fail { protocol }`）；或者无身份的**临时连接**存活超过 10 分钟 | 客户端 bug / 配对没完成，不要自动重连；重新发起配对 |
| `1008 policy` | 其余认证失败（`auth_fail { invalid \| version }`）；「一直被限速退回、期间没有任何信封被转发」持续 10 秒以上；或同一目标上被退回未放行的 `seq` 超过 4096 个（发送方不守规则） | `invalid` / `version` 是终态；限速则退避 |
| `1011 internal` | hub 内部错误 | 指数退避重连 |

升级阶段（还没成为 WebSocket）的 HTTP 错误：`403 origin_not_allowed`、`429` + `Retry-After`（`/v1/ws` 每 IP 每分钟 30 次）、`503`（正在关停）。

### 3.2 转发规则

`data` / `handshake` / `notify` 信封只在满足**全部**条件时转发：

1. `from` 等于当前连接的设备 id（防伪造）
2. `(from, to)` 在 `pairings` 表中，或处于一次进行中的配对撮合 / 接入会话（仅 `handshake`，§ 4 / § 4.1）
3. 信封大小不超过 `ANYCON_MAX_ENVELOPE_BYTES`（默认 256 KiB，最小 205824 字节 ≈ 201 KiB，即一个最坏情况的满片分片信封；更小的值 hub 拒绝启动）。这个上限是防御性的；一条 RPC 消息比它大没关系，**发送方在传输层分片**（≤ 200 KiB / 片，[ADR-30](../decisions.md#adr-30-内层分片与大小上限)），hub 只看到若干个正常大小的 `data` 信封

条件 1、2 或 3 不满足时不是静默丢弃，而是给发送方（含临时连接）回 `control.dropped { seq, reason }`：超过 `ANYCON_MAX_ENVELOPE_BYTES` → `too_large`（说明发送方没有分片，是客户端 bug）；`from` 不符或 `(from, to)` 未配对 → `not_paired`。`DropReason` 里还有 `rate_limited`，hub 不发它：速率超限走 § 3.3 的退回路径，不丢。`kind = control` 的超大帧、格式错误的帧、版本不对的帧不回执。超过上限 2 倍的 WebSocket 帧在协议层直接断开连接（`max(2 × 上限, 64 KiB)`）。**满足全部条件的信封只有两种结局：转发，或退回 `backpressure`（§ 3.3）。hub 从不静默丢弃已配对双方之间的 `data` / `notify`**——Noise 的 nonce 是顺序的，丢一封就是整条传输失步。`backpressure` 回执本身进不了发送方的控制消息队列时（它不读 socket），hub 立即以 `4003` 断开它，否则它不知道有信封被退回、顺序会乱。

目标离线时按 [protocol.md § 7](../protocol.md#7-对端离线) 处理：回 `undeliverable { to, seq }`，或入队（队列开启时）。

### 3.3 背压

每条连接一个有界发送缓冲（默认 4 MiB）。缓冲满说明接收方太慢（手机弱网），hub **不无限缓存**：

- `data` / `notify` 信封进不了目标缓冲时，向发送方回 `control.backpressure { to, seq }`，并对这对 `(发送连接, to)` **粘住**：维护一个被退回 `seq` 的有序集合，集合非空时只放行等于集合最小值的 `seq`（成功就移除），其它一律退回（每封都回 `backpressure`）并加入集合；集合超过 4096 项就以 `1008` 断开发送方（第 4097 个不入集合、不回执）。这样顺序保证不依赖发送方是否收齐了回执——回执晚到、发送方先重发了后面的 `seq`，hub 也只是再退回一次（[protocol.md § 3.1](../protocol.md#31-消息大小)）。`handshake` 信封不参与粘住（缓冲满或超速时同样退回 `backpressure`，但不加入集合也不受集合影响；发送方应按 `retry_after_ms` 延后再握手）；任一方向成功转发的 `handshake`、发送连接结束、目标断开 / 被新连接顶掉 / 吊销都清掉这对设备两个方向的粘住状态（传输重置后发送方不会再重发旧信封）。粘住状态按发送**连接**记，同一设备的新旧连接互不干扰。没有时间兜底：既不重发也不握手的发送方会一直被退回，直到它断开
- 速率超限（§ 3.4）走同一条退回路径，不丢
- daemon 侧对慢客户端的处理见 [daemon.md § 12](daemon.md#12-多客户端)
- 缓冲持续满 30 秒：断开慢连接，close code `4003 slow_consumer`
- **控制消息也计入这个缓冲**（`auth_ok`、`peer_online`、各种回执），另有独立的控制消息上限（数据上限的 1/16，夹在 64–256 KiB 之间），超过就丢并开始计「满」。一个不读 socket 的连接即使只收控制消息，缓冲也会持续满并被 `4003` 断开，hub 内存因此有界（上界约数据上限 + 控制上限）
- `dropped`、`rate_limited` 和超限的 `pair_failed { rate_limited }` / `enroll_failed { rate_limited }`、对不存在会话的 `enroll_cancel` 回的 `enroll_failed { not_found }` 共用一个预算，每连接每秒最多一条，其余丢弃；`undeliverable` / `backpressure` 不限频（客户端靠它们给每个 `seq` 判结果）

### 3.4 速率

每条连接的信封速率上限默认 500/秒；入站字节预算按连接种类：客户端连接（web / cli / sdk、临时连接）默认 10 MiB/分钟（`ANYCON_RATE_CLIENT_BYTES_PER_MIN`），daemon 连接默认 100 MiB/分钟（`ANYCON_RATE_DAEMON_BYTES_PER_MIN`）——daemon 是它所有客户端的汇聚点，一次历史补齐或几台设备同时 watch 就能超过 10 MiB。预算是令牌桶（容量 = 每分钟字节数，匀速补充，只在转发成功后扣额度，被退回的信封不占额度），不是固定 60 秒窗口。超限的 `data` / `notify` / `handshake` **不丢**：按 § 3.3 退回 `backpressure`（`data` / `notify` 还会粘住），另外每秒至多一条 `control.rate_limited { retry_after_ms }` 告诉发送方停多久（夹在 100 ms–30 s，默认预算下一般 1–2 秒；语义是整条连接暂停）。`1008` 的判定是「一直被限速退回、期间没有任何信封被转发」持续 10 秒——老老实实按提示暂停再重发的发送方不会被断，只会被匀速限流；缓冲满造成的退回不计入。控制消息超过自己的上限仍然是丢弃（§ 3.3）。**回执限频**：`rate_limited`、`dropped` 和超限的 `pair_failed { rate_limited }` / `enroll_failed { rate_limited }`、对不存在会话的 `enroll_cancel` 回的 `enroll_failed { not_found }` 共用一个预算，每连接每秒至多一条，不会因为对方狂发而反过来灌满它的发送缓冲；`undeliverable` / `backpressure` 不限频（见 § 3.3）。

## 4. 配对撮合

[protocol.md § 4.1](../protocol.md#41-配对流程) 给了消息序列，这里是 hub 侧的约束：

| 项 | 规则 |
|---|---|
| 短码 | **hub 看不到短码**。daemon 本地生成（默认 16 位，字母表去掉 `0 O 1 I L`，约 79 bit；`--short` 8 位，约 40 bit），两端各自发 `match_id`（短码经 Argon2id 派生的前 8 字节，16 hex）来撮合，PSK 是同一次派生的后 32 字节。hub 不记录、不落库、不写日志 `match_id`（它是短码的函数，记下来就等于给离线穷举留了线索）。hub 没有生成短码的代码，`pair_begin` 只带 `match_id`，登记成功回 `pair_registered { expires_at_ms }` |
| 有效期 | 5 分钟。只存在内存里，hub 重启即作废 |
| 唯一性 | 一个 daemon 同时只有一个有效短码，再次 `pair_begin` 作废前一个 |
| 错误尝试 | `pair_join` 的 match_id 对不上就 `pair_failed { not_found }`；按 IP 限速 `pair_join`：10 次 / 分钟 |
| 一次性 | `pair_matched` 后短码立即失效，第三方再 join 得到 `pair_failed { reason: not_found }` |
| 临时连接 | 没有身份的临时连接最长存活 10 分钟（到点 `4005`），一条临时连接一生最多发 3 次 `pair_begin`，第 4 次起 `pair_failed { rate_limited }`。`match_id` 必须是 16 个 hex 字符（大小写不敏感，统一成小写），格式不对的 `pair_begin` 回 `pair_failed { not_found }` 且不登记（不会让对方超时）。同一个 `match_id` 从新连接重新登记会顶掉旧连接的登记（同一个短码重试安全）。内存里只存 `blake2s(进程内随机盐 ‖ match_id)`，不存 `match_id` 原文 |
| 握手超时 | matched 后 60 秒内双方未 `pair_confirm` 则撮合作废，通知双方 |
| `pair_done` 不是终点 | hub 发出 `pair_done` 只代表 hub 这边的配对关系已建立。新设备要等 daemon 经加密传输发来的 `pair.ack` 才落盘，所以一个伪造 `pair_done` 的 hub 骗不了客户端（[protocol.md § 4.1](../protocol.md#41-配对流程)）。hub 不参与也看不到 `pair.ack` |
| 残余风险 | hub 不知道短码，但拿到了 `match_id`：恶意 hub 可以离线穷举短码直到 `match_id` 对上，猜中就能对这次配对做中间人。Argon2id 让每次猜测约 0.1 s + 64 MiB，8 位短码（2^40）要数年 GPU 量级，16 位（2^79）不可行；经不信任的 hub 配对不要用 `--short`。在线猜测受 `pair_join` 限速、短码一次性和 5 分钟 TTL 约束（[ADR-29](../decisions.md#adr-29-短码不经过-hub)） |
| 确认 | 两边都发 `pair_confirm` 后 hub 才写 `pairings`、签发新设备的 device token、创建设备记录。只有一边确认视为失败 |
| 配对链接 | `${hub.url}/pair#code=<短码>`，由 **daemon** 按自己配置的 `hub.url` 生成（hub 不生成）。短码在 fragment 里，浏览器不会把它发给 hub，不会出现在 hub 或反代的请求日志里。`anycon pair --qr` 和 Web 的「添加设备」把它渲染成二维码 |
| 跨账号 | v0.1 配对双方必须属于同一账号，两边已经各属于不同账号 → `pair_failed { forbidden }`。新设备（还没有账号归属）在 `pair_join` 时携带当前 web 登录态的 cookie（Web）或空（CLI / SDK）；空的情况下新设备归入 daemon 所属的账号。这意味着 CLI / SDK 不需要登录，和 [architecture.md § 3.1](../architecture.md#31-登录方式) 一致 |
| daemon 首次身份 | daemon 第一次连接没有 token，以 `kind = daemon` 的临时连接发 `pair_begin`（其他 kind 的临时连接不行）。配对完成时 hub 给它建设备记录、签发 token、原地升级连接，账号取自 joiner（web cookie 或已认证设备）；双方都没账号则 `pair_failed { forbidden }`，不写库 |

### 4.1 接入撮合

[ADR-36](../decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)，消息序列见 [protocol.md § 4.4](../protocol.md#44-账号内接入)。已登录账号的设备不需要短码，直接请求接入本账号的某台 daemon。**能不能接入由 daemon 验证**（扫码密钥、接入码、节点口令都不经过 hub）；hub 这一层只确认归属和防滥用：

| 项 | 规则 |
|---|---|
| 谁能发 `enroll_begin` | 本账号的正式设备，或升级请求带本账号登录态（web session cookie）的临时连接。都没有 → `enroll_failed { forbidden }`。临时连接的 `kind` 不限，但 `daemon` 不能发 |
| 目标 | `node` 必须是本账号未吊销的 `daemon` 设备，否则 `not_found`（不区分「不存在」和「不是你的」）；不在线 → `offline`。发起方已经和它配对过也放行（比如这台设备丢了本地的节点记录），收尾规则和重新配对一样 |
| `enroll_id` | 随机 128 bit，hub 分配，不是秘密；发给双方（`enroll_started` / `enroll_request`），之后双方之间的 `handshake` 信封按它放行 |
| 并发 | 每台 daemon 同时最多 3 个进行中的接入会话，多出来 `busy`；每条连接同时最多 1 个 |
| 有效期 | 5 分钟。到点没走到 `pair_done` → 双方 `enroll_failed { timeout }`。只在内存里，hub 重启即作废 |
| 结束 | 任一方 `enroll_cancel` → 另一方 `enroll_failed { cancelled }`；任一方断线 → 另一方 `peer_gone` |
| 收尾 | 和 § 4 一样：两边 `pair_confirm` 后 hub 才写 `pairings`；发起方是临时连接时，建设备记录、签发 device token、原地升级连接，账号就是它登录的那个账号；`pair_done` 之后的 `pair.ack` hub 看不到 |
| 限速 | `enroll_begin` 10 次 / 分钟 / IP、30 次 / 小时 / 账号，计入每日配对与接入配额（§ 7）。**这些只防滥用，不是安全边界**：hub 本身可能就是攻击者，在线猜测的次数由 daemon 限（[daemon.md § 15.1](daemon.md#151-账号内接入)） |
| 不记录 | `handshake` 体（含 `EnrollHello` 里新设备自报的提示信息），和配对一样。`enroll_id` 可以出现在 debug 日志里 |

## 5. 设备与设备 token

```
device
  id            随机 128 bit，对外的 DeviceId
  account_id
  kind          daemon | web | cli | sdk
  name          客户端提供，可改。Web 默认 "Chrome · macOS · hub.example.com"
  platform      客户端自报：os、arch、客户端版本
  origin        仅 web：该浏览器访问 hub 的 origin（区分局域网/公网两份）
  token_hash    device token 的 blake2 哈希
  created_at · last_seen_at · revoked_at
```

- device token 是 32 字节随机数，配对确认时签发，只传一次；客户端存在自己的配置目录 / IndexedDB
- 轮换：`control.rotate_token` → hub 返回新 token，旧 token 保留 60 秒宽限（v0.2）
- 吊销 = `revoked_at` 置位，不物理删除，保留审计。连接立即断开，close code `4004 revoked`
- hub **不存设备的 Noise 公钥**。配对关系只是一条 `(device_a, device_b)` 记录，用于路由判定

## 6. 通知

[ADR-37](../decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)。hub 能看到的只有 `notify` 信封外层的明文（`category`、不透明的 `session_id`）和信封的收发设备，推送和邮件也只用这些。

### 6.1 Web Push

- **VAPID**：首次启动生成 P-256 密钥对，存 `${ANYCON_DATA_DIR}/vapid.json`（0600；文件坏了启动报错，不静默重新生成——那会让所有订阅失效）。公钥经 `/api/config.vapid_public_key` 下发（base64url，65 字节未压缩点）。JWT（ES256）的 `aud` 是 endpoint 的 origin，`exp` 12 小时，`sub` 取 `ANYCON_VAPID_SUBJECT`，没设时 `ANYCON_PUBLIC_URL` 是 https 就用它，否则 `mailto:` + `ANYCON_EMAIL_FROM` 的地址，再没有就 `mailto:noreply@<PUBLIC_URL 的主机名>`
- **订阅**按设备存（§ 2.3）；吊销设备时删它的订阅
- **endpoint 白名单**：https，主机是 `fcm.googleapis.com`、`*.push.services.mozilla.com`、`*.push.apple.com`、`*.notify.windows.com` 之一。hub 每条通知都会 POST 这个用户给的 URL，不设限就是 SSRF
- **推给谁**：daemon 发出的每封 `notify` 信封（daemon 本来就给每台已配对设备各发一封），发送方必须是账号里没吊销的 daemon、收件设备必须是同一账号里没吊销的设备（否则忽略）；每台收件设备每分钟最多判定 30 次（防刷，不是环境变量，超出的不推不发邮件，信封转发不受影响）。hub 只推给**这封信的收件设备**的订阅，并且只在它**没有在线连接**时推；再看账号偏好里这个类别的 `push` 开没开。在线的设备照常经 WebSocket 收到信封（页面在后台由页面自己弹系统通知，[web.md § 7](web.md#7-通知)）
- **载荷**（RFC 8291 `aes128gcm` 加密）：

```json
{ "category": "permission_requested", "node": "<daemon device id>", "session": "<session id>" }
```

  `session` 是随机不透明串，`node` 用于客户端点开后直达 `/n/:node/s/:session`。没有任何正文。`enroll_requested` 和 `security`（账号内接入，§ 4.1）的 `session` 为 `null`，点开后直达节点设置的「接入」标签
- **请求头**：`TTL: 3600`（`enroll_requested` 是 300，和接入会话一样长）；`permission_requested`、`enroll_requested` 带 `Urgency: high`，其余 `normal`；`Authorization: vapid t=<JWT>, k=<公钥>`
- **结果**：201 / 2xx 成功；404 / 410 删订阅；其余记一条 warn（只记 endpoint 的主机名和状态码——endpoint 的路径本身是个凭据）。发送在后台任务里做，每个请求 10 秒超时，全 hub 同时最多 16 个，不阻塞信封转发

### 6.2 邮件

- 走 `ANYCON_SMTP_URL`（[deploy.md § 3](../deploy.md#3-环境变量)），和验证码同一条通道
- 类别：`permission_requested`、`error`、`enroll_requested`、`security`
- 条件：账号绑定了邮箱、偏好里这个类别开了 `email`、**账号此刻没有任何 web 设备在线**（web 代表有人在看；一直在线的 SDK / CLI 设备不算，否则一台常驻的 SDK 节点会让邮件永远不发）
- 去重：同一账号同一类别 10 分钟最多一封（daemon 给每台设备各发一封 `notify`，这条同时把它们合成一封）
- 正文是固定模板，中英双语：「节点 X 的一个会话在等你审批」/「节点 X 有设备请求接入」……+ 链接（`${ANYCON_PUBLIC_URL}/n/<node>/s/<session>`，接入相关的是 `/n/<node>/settings?tab=enroll`），不含任何会话内容。节点名取 hub 的设备记录
- 验证码邮件：「你的 Anycon 登录码是 123 456，10 分钟内有效。不是你本人操作就忽略这封邮件。」，按请求的 `Accept-Language` 选中文或英文

### 6.3 偏好

`notification_prefs(account_id, category, push bool, email bool)`，默认 `permission_requested`、`error`、`enroll_requested`、`security` 全开，`turn_finished` 只开 push，`session_idle` 全关。`security` 的邮件在手边没有任何已配对设备时最有用（比如口令接入被锁定）。`PUT` 的数组可以只带要改的类别（没出现的不动，重复的取最后一个），回完整结果；未知类别 / 缺字段 `400 bad_request` 且整个请求不生效；`turn_finished` / `session_idle` 的 `email` 一律存成 `false`（只有四类会发邮件）。

## 7. 配额与限速

### 7.1 配额（仅 `multi`）

按账号计，由 `ANYCON_QUOTA_*` 配置：

| 项 | 默认 | 超限行为 |
|---|---|---|
| 设备总数 | 10 | `pair_confirm` 失败 `quota_exceeded` |
| daemon 数 | 3 | 同上 |
| 同时在线连接 | 10 | 新连接 `auth_fail { reason: quota }` |
| 每日配对与接入次数 | 20 | `pair_begin` / `pair_join` / `enroll_begin` 拒绝（`enroll_failed { quota }`） |
| 队列（若开启） | 50 条 / 256 KiB / 1h | 见 protocol.md § 7.2 |

**不限流量字节数**。hub 只转发，流量成本低；限制字节会让「传一个日志文件」这种合理需求变得难受。用量通过 `control.quota` 在连接时和变化时推给设备，Web 在设置页展示。

### 7.2 限速（两种模式都有）

| 对象 | 默认 |
|---|---|
| `POST /auth/token` | 10 次 / 分钟 / IP |
| `POST /auth/email/send` | 1 次 / 60 秒 / 邮箱；5 次 / 小时 / IP；**每个邮箱每天最多发 10 个码**（不管请求来自哪个 IP） |
| `POST /auth/email/verify` | 每个码尝试 5 次作废（一次提交对这个邮箱所有有效的码各计一次：同一时刻能提交的猜测仍不超过 5 次）；20 次 / 分钟 / IP；**每个邮箱每天累计验证失败 30 次后，当天不再接受这个邮箱的码**（换 IP 也一样；被人这样锁住的账号仍可用 token 登录） |
| `POST /api/me/email` | 5 次 / 小时 / IP（和登录发码各用各的桶）；60 秒重发限制按（账号，邮箱）计；每个邮箱每天最多 10 个绑定码（和登录码分开计）。刚绑定完马上用同一个邮箱登录不会被挡 |
| `POST /api/me/email/verify` | 20 次 / 分钟 / IP；失败至少 250 ms 才回；每个邮箱每天累计失败 30 次后当天不再接受（和登录分开计） |
| `POST /api/push/subscriptions` · `/api/push/test` | 30 次 / 分钟 / 账号 |
| OAuth start | 20 次 / 分钟 / IP |
| WebSocket 升级 | 30 次 / 分钟 / IP（预览域名上的另算，见 § 13.4） |
| `POST /api/preview/tokens` | 60 次 / 分钟 / 设备 |
| `pair_join` | 10 次 / 分钟 / IP |
| `enroll_begin` | 10 次 / 分钟 / IP；30 次 / 小时 / 账号 |
| 邀请链接尝试 | 10 次 / 分钟 / IP |

邮箱相关的每日上限超出时回 `429`，`Retry-After` 可能是几个小时；这些计数和其它限速桶一样在内存里，hub 重启就清零。知道某个邮箱地址的人能用掉它当天的码额度、或一直占着 60 秒的重发窗口——这是接受的拒绝服务，token 登录不受影响。

限速键是真实 IP，依赖 `ANYCON_TRUSTED_PROXIES`；**IPv6 按 /64 归并**（一个住宅前缀内的地址共用一个桶，轮换地址绕不过去）。默认受信网段是 `172.16.0.0/12`（Docker 默认桥接网段）：不在 Docker 里部署时必须显式设置，否则反代后所有人共用反代 IP 的桶，或者（客户端恰好在这个网段）能伪造 `X-Forwarded-For`。`ANYCON_PUBLIC_URL` 是 https 时 cookie 始终带 `Secure`，不看转发头。令牌桶，内存实现；多实例部署（Postgres 后端）时退化为每实例独立计数，可接受。

## 8. 数据表

全部是元数据。注释里标 **(密文)** 的只有可选队列一处。

| 表 | 内容 |
|---|---|
| `accounts` | id、display_name、email（可空、唯一）、is_admin、created_at |
| `account_tokens` | 登录 token 哈希、label、last_used_at |
| `passkeys` | credential id、公钥、counter、label、transports |
| `identities` | provider、subject、account_id；`(provider, subject)` 唯一 |
| `web_sessions` | 浏览器登录会话：token 哈希、ua、ip 前缀、created、last_seen、expires |
| `email_codes` | 规范化的邮箱、用途（`login` / `bind`）、账号（`bind` 时）、码哈希、过期、尝试次数、创建时间。登录码每个邮箱最多同时有 3 个有效的（新码不让旧码作废，别人请求新码顶不掉你手里那个）；绑定码按（账号，邮箱）存，别的账号请求同一个邮箱的绑定码影响不到你；`accounts` 另有 `lower(email)` 的唯一索引兜底 |
| `oauth_states` | state、pkce verifier、用途（login / link / invite）、过期 |
| `invites` | code 哈希、created_by、note、expires、used_by |
| `devices` | 见 § 5 |
| `pairings` | device_a、device_b、created_at；无向，存归一化顺序 |
| `push_subscriptions` | id、device_id（唯一：一台设备一条）、endpoint、p256dh、auth、created |
| `notification_prefs` | 见 § 6.3 |
| `queue` | **(密文)** 可选离线队列：to_device、seq、from_device、body、enqueued_at |
| `hub_audit` | 见 § 9 |
| `preview_labels` · `preview_tokens` | 预览 label 绑定的节点、预览令牌哈希（§ 13.7） |
| `kv` | schema 版本等杂项（VAPID 密钥在 `${ANYCON_DATA_DIR}/vapid.json`，不在库里，§ 6.1） |

进行中的配对（`match_id`、撮合状态）、进行中的接入会话和限速计数**只在内存**，不落库。

## 9. hub 侧审计

记录账号层面的安全事件，供账号设置页的「安全活动」和管理员页使用：

`login_ok` / `login_fail`（方式：`token` / `invite` / `email`，IP 前缀、UA）· `email_changed`（`bound` / `changed` / `removed`，不记邮箱）· `token_created` / `token_deleted` · `passkey_added` / `removed` · `identity_linked` / `unlinked` · `device_paired` · `enroll_started`（节点、IP 前缀）/ `device_enrolled`（hub 不知道用的是哪种方法；接入只记它，不再同时记 `device_paired`）· `device_revoked` · `invite_created` / `used` · `account_deleted`

保留 90 天，自动清理。每次失败的登录都写一行（`login_fail` 还没有聚合），持续的分布式暴力尝试会让表变大。不记录任何信封内容、不记录 `data` 信封的收发（那是流量日志，不是审计）。

## 10. 日志纪律

| 记录 | 不记录 |
|---|---|
| 连接建立 / 断开，设备 id，close code | 任何 `body` 字节，包括 `notify` 的密文部分 |
| 信封 kind、大小、是否投递 / 丢弃（debug 级） | device token、登录 token、cookie、Authorization |
| HTTP 路径、状态码、耗时 | 邮箱地址（用哈希前 8 位代替） |
| 限速触发、配额拒绝 | OAuth access token、验证码 |
| 推送结果（endpoint 主机名 + 状态码）、邮件发送结果（收件人哈希前 8 位 + SMTP 状态） | 推送 endpoint 的路径、p256dh / auth、VAPID 私钥、SMTP 密码、邮件正文 |
| | 配对的 `match_id`（短码的函数） |
| | 预览令牌；预览域名上的请求路径（只记 `(preview)`） |

`tracing` 字段化输出，默认 `info`。这条表格是 code review 的检查项，不是建议。

## 11. 生命周期与运维

- **启动**：读配置 → 校验（`multi` 必须有至少一种非 token 登录方式、`ANYCON_PUBLIC_URL` 必须是 http(s) URL）→ 跑 migration → 生成 bootstrap token / VAPID（首次）→ 监听。任一步失败直接退出并打印原因
- **升级**：migration 只前进；跨多个 minor 升级时逐个 migration 跑。回滚靠备份
- **备份**：`anycon-hub backup <path>` 用 SQLite online backup API 导出一致快照，容器里可 `docker exec`。Postgres 用 `pg_dump`。（**v0.1 没有 `backup` 子命令**，只有 `serve` / `healthcheck` / `version`；先停容器拷 `/data`，或在宿主机用 `sqlite3 hub.db ".backup …"`。v0.1.x 补）
- **关停**：SIGTERM → 停止接受新连接 → 对所有连接发 `4002 restarting` → 最多等 5 秒 → 退出
- **指标**（v0.3）：`/metrics` Prometheus 格式，只有计数和直方图，不含设备 id 维度

## 12. 不在 hub 里的东西

每次给 hub 加功能前过一遍：

- 会话列表、项目列表、搜索 → daemon
- 「哪个 agent 可用」「模型列表」 → daemon（见 [daemon.md § 5](daemon.md#5-agent-探测与能力声明)）
- 附件存储 → daemon
- 用户偏好里和内容有关的（默认模型、显示思考过程）→ 客户端本地或 daemon；hub 只存通知渠道偏好这种和路由有关的
- 多账号共享一台 daemon → 「团队」功能，路线图「之后再说」

## 13. 预览

[ADR-39](../decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)。hub 在第二个泛域名上为 dev server 预览提供一个很小的 HTTP 表面和一种新的 WebSocket 连接；隧道里的内容 hub 看不到（[protocol.md § 11](../protocol.md#11-预览隧道)）。

### 13.1 配置

| 变量 | 默认 | 说明 |
|---|---|---|
| `ANYCON_PREVIEW_URL` | （空 = 关闭预览） | 预览 origin 的模板：`https://*.<预览域名>[:端口]`，`*` 只能是最左边的一整段，代表 label。必须是 https；唯一的例外是本机开发和测试：主机是 `localhost` 或以 `.localhost` 结尾时可以是 http（如 `http://*.preview.localhost:8080`）。**预览域名不能和 `ANYCON_PUBLIC_URL` 的主机在同一个可注册域名下**（按 Public Suffix List 算 eTLD+1，`localhost` 一类除外），否则启动报错：同站的子域能往父域写 cookie。公共 hub 是 `https://*.aiplace.app` |
| `ANYCON_PREVIEW_BYTES_PER_MIN` | `536870912`（512 MiB），最小 `205824` | 每个账号预览流量（两个方向合计）每分钟的字节预算，令牌桶（§ 13.5） |
| `ANYCON_PREVIEW_MAX_CONNECTIONS` | `16` | 每个账号同时在线的预览连接数上限 |

启动时 info 打出生效的预览 origin 模板（没配就打「preview disabled」）。

### 13.2 预览域名上的 HTTP

`Host`（经受信反代时取原始 `Host`）匹配 `<label>.<预览域名>[:端口]` 的请求走这里，其余照旧走 § 2。label 必须是 26 位小写 base32（`[a-z2-7]{26}`），不是的 → `404`（纯文本）。预览域名上**没有** Web UI、`/api`、`/auth`；Web UI 的 origin 上也**取不到**预览的任何文件——两份前端分开构建（`web/dist`、`web/dist-preview`）、分开嵌入。否则 Web UI 的 origin 上可能被注册一个隧道到别人节点的 service worker，读走 IndexedDB 里的私钥。

| 路径 | 说明 |
|---|---|
| `GET /v1/ws` | 预览连接（§ 13.4） |
| `GET /__anycon/config.json` | `{ label, hub_origin, protocol_version }`（`hub_origin` 是 `ANYCON_PUBLIC_URL` 的 origin），`Cache-Control: no-store` |
| `GET /__anycon/enter` | 引导页（`web/dist-preview/index.html`） |
| `GET /__anycon/sw.js` | 预览的 service worker。带 `Service-Worker-Allowed: /`、`Cache-Control: no-cache` |
| `GET /__anycon/<其他文件>` | `web/dist-preview` 里的文件；`/__anycon/assets/*` 是 immutable，其余 `no-cache`；不存在 `404` |
| 其他 `GET` / `HEAD` | 引导页，`200`，`Cache-Control: no-store`。service worker 还没接管（第一次打开、硬刷新、被浏览器清掉）时页面请求会落到这里，引导页装好 service worker 后重新加载 |
| 其他方法 | `405` |

响应头：HTML 带下面的 CSP；所有响应带 `X-Content-Type-Options: nosniff`、`Referrer-Policy: no-referrer`。**不带** `X-Frame-Options` 和 COOP（要能被 Web UI 嵌入，嵌入方由 `frame-ancestors` 限定）。

```
default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; img-src 'self' data:;
style-src 'self' 'unsafe-inline'; worker-src 'self'; frame-ancestors <ANYCON_PUBLIC_URL 的 origin>;
base-uri 'none'; form-action 'none'
```

这份 CSP 只管引导页；dev app 的页面由 service worker 合成，头部见 [web.md § 13.2](web.md#132-预览运行时)。

### 13.3 预览令牌 `POST /api/preview/tokens`

在 hub 的主域名上（§ 2.3 一侧）。只认设备 token：`Authorization: Bearer <device token>`，不看 cookie；照常要 `X-Anycon: 1`。

- 请求 `{ node, label }`。`label` 是 daemon `preview.open` 给的
- 校验：没配预览 → `404 preview_disabled`；`label` 格式不对 → `400 invalid_label`；调用的设备不是本 hub 未吊销的正式设备 → `401 unauthorized`；`node` 不是同账号未吊销的 daemon，或调用设备没和它配对 → `404 not_found`；`label` 已经绑定到**别的**节点 → `409 label_taken`；这个节点已绑定 256 个 label → `409 too_many_labels`
- 第一次见到的 `label` 绑定到 `node`（`preview_labels`）。签发令牌：32 字节随机数的 hex，库里只存 blake2s 哈希，24 小时有效；同一设备未过期的令牌超过 64 个时删掉最旧的
- 回 `200 { token, origin, expires_at_ms }`，`origin` 是 `https://<label>.<预览域名>[:端口]`

### 13.4 预览连接

- 升级：只在预览域名上的 `GET /v1/ws`。`Origin` 必须**正好**等于这个请求的预览 origin（scheme + 主机 + 端口），否则 `403 origin_not_allowed`。另算一个 IP 限速桶：120 次 / 分钟 / IP（service worker 被浏览器回收后会重连）
- 认证：第一帧 `auth { preview_token, protocol_version, client }`，不能带 `device_token`、`client.kind` 必须是 `web`，否则 `auth_fail { protocol }`。令牌不存在、过期、或绑定的 label 不是主机名里的 label → `auth_fail { invalid }`；这个账号的预览连接已满 → `auth_fail { quota }`。协议版本规则照旧
- 身份：分配临时 id（`new_id()`），`auth_ok { device_id, peers_online }`，节点在线时 `peers_online = [node]`。它不是设备：不进 `devices`、不算「web 设备在线」（不影响推送判断）、不受临时连接 10 分钟的限制
- 路由：预览连接的对端集合只有 `node`，节点的连接把它加进对端集合。只转发它和节点之间的 `handshake` / `data`；它发的控制消息一律忽略，`notify` 和发给别人的信封回 `dropped { not_paired }`
- 上下线：预览连接认证成功时给节点（在线的话）发 `preview_peer { peer, label }`，**不发** `peer_online`；它断开时给节点发 `peer_offline`。节点上线（含重连）时，hub 把这个节点所有在线的预览连接重新加进它的对端集合，逐条发 `preview_peer`，并给这些预览连接发 `peer_online { node }`；节点下线时给它们发 `peer_offline { node }`
- 结束：令牌过期、被删（设备或节点被吊销，§ 13.6）时 hub 以 `4004` 关掉用它的连接；预览页把 `4004` 当终态，不自动重连

### 13.5 流量预算

- 预览连接发给节点的、节点发给预览连接的 `data` / `handshake` 信封，都从**账号的预览预算**里扣（`ANYCON_PREVIEW_BYTES_PER_MIN`，令牌桶，只在转发成功后扣），**不扣**发送连接自己的每连接字节预算。每连接的信封速率上限（500 / 秒）照旧
- 超出预算：照 § 3.3 退回 `backpressure`（`data` 进粘住集合），另外每秒至多一条 `rate_limited { retry_after_ms, to }`，`to` 是被暂停的那个对端（节点发往预览连接时是预览连接的 id，反过来是节点 id）。发送方只暂停发往 `to` 的信封
- 「一直被限速退回 10 秒就断开」只看每连接预算；预览预算的退回不计入，否则一个大流量的预览会把 daemon 的连接整个断掉

### 13.6 吊销与清理

- 吊销设备（`DELETE /api/devices/:id` 或 `control.revoke`）：删掉它签发的预览令牌，关掉用这些令牌的预览连接（`4004`）
- 吊销节点：删掉它的全部 label 和令牌，关掉它的预览连接
- 过期令牌由后台任务每分钟清一次

### 13.7 数据表

| 表 | 内容 |
|---|---|
| `preview_labels` | label（主键）、node_id、account_id、created_at_ms |
| `preview_tokens` | token_hash（主键）、label、node_id、account_id、device_id（签发它的设备）、created_at_ms、expires_at_ms |

都是元数据：hub 知道哪个账号的哪台节点有哪些 label、谁什么时候申请了令牌；不知道 label 对应哪个端口。新表放在新的 migration（`0002_preview.sql`）里。
