# 部署

> 镜像 `ghcr.io/anycon-ai/hub` 由发布流水线（`.github/workflows/release.yml`）在每个版本标签上构建，`linux/amd64` 和 `linux/arm64` 两种架构，标签是 `<版本>`（如 `0.1.0`）、`<主>.<次>`（`0.1`，跟着最新的补丁版）和 `latest`（预发布版只有 `<版本>`）。镜像里的 `/usr/share/doc/anycon-hub/` 是许可证和第三方许可证清单。
>
> 另有跟着 main 的 `edge` 和 `sha-<7 位提交>` 标签：main 上 CI 全部通过后推送，只有 `linux/amd64`，不是正式版本。用它的话 daemon 也从同一个提交的源码装（`cargo install --path crates/cli --locked`），两边对齐到同一个提交（钉 `sha-…` 比 `edge` 稳）：main 上的协议和迁移随时会变，迁移只前进，换回正式版镜像前要用备份的数据。
>
> 想自己构建（比如改了前端），在仓库根目录构建同名镜像，下面的命令照用：
>
> ```bash
> docker build -f docker/hub.Dockerfile -t ghcr.io/anycon-ai/hub:latest .
> ```

## 1. 边界

anycon-hub 容器**只做一件事**：在一个明文 HTTP 端口上提供服务。

| 由容器负责 | 由你负责 |
|---|---|
| HTTP + WebSocket 服务 | TLS 证书 |
| 数据库（SQLite 在 `/data`） | 反向代理 |
| 数据库迁移 | 域名与 DNS |
| 嵌入的 Web UI | 防火墙 |
| 健康检查端点 | 备份 |

不内置 ACME，不内置 nginx。原因：你已经有自己的反代方案（Caddy、Traefik、nginx、Cloudflare Tunnel），容器里再塞一个只会冲突。

**只跑一个实例。** 路由表、配对 / 接入撮合和限速计数都在进程内存里，不要开多副本，也不要在前面挂负载均衡分流。

## 2. 最小启动

```bash
docker run -d --name anycon-hub \
  --restart unless-stopped \
  -p 127.0.0.1:8080:8080 \
  -v ./anycon-data:/data \
  -e ANYCON_PUBLIC_URL=https://hub.example.com \
  ghcr.io/anycon-ai/hub:latest
```

首次启动，日志里会打印管理员 token：

```
anycon-hub 0.1.0 listening on 0.0.0.0:8080
mode: single
first run — admin token (also saved to /data/admin-token):
  7f3a9c…
open https://hub.example.com and paste it to log in
```

打开 hub 地址，粘贴 token 登录，之后这个浏览器不用再粘。不想用自动生成的 token 可以设 `ANYCON_ADMIN_TOKEN`；token 丢了的时候，改它重启就是恢复出口。注册默认关闭，之后加人用管理员生成的邀请链接。除了 token，配了 SMTP 还能用邮箱验证码登录（先用 token 登录，在「账号设置」里绑定邮箱），配了 OAuth 应用还能用 GitHub、Google 登录（§ 3.1）；passkey 还没实现。

这是 `single` 模式（自建默认）。要让别人自助注册，用 `multi` 模式，见 § 9。

hub 跑在家里、家里走局域网 IP 的部署方式见 § 4「家里跑 hub：局域网 + 公网」。

## 3. 环境变量

下面是**实际生效**的变量。`ANYCON_QUOTA_*`、`ANYCON_ADMIN_EMAILS`、`ANYCON_OAUTH_*` 留空等于没设；设了但值不合法，hub 启动报错（两种模式都校验），在用不到它们的模式里设了合法的值则启动时警告 `ignored`：

| 变量 | 默认 | 说明 |
|---|---|---|
| `ANYCON_PUBLIC_URL` | （必填） | 公网地址，必须是不带路径的 `http(s)://host[:port]`。WebSocket 的 Origin 校验、cookie 属性依赖它。局域网 IP 访问不用写在这里，hub 对私有网段和 localhost 的 http 自动放行。**配对链接不用它**：链接由 daemon 按自己配置里的 `hub.url` 生成（`${hub.url}/pair#code=…`），所以 `anycon init --hub` 填的地址要和用户浏览器访问的一致 |
| `ANYCON_LISTEN` | `0.0.0.0:8080` | 监听地址 |
| `ANYCON_MODE` | `single` | `single` 或 `multi`（不区分大小写），其他值启动报错。`multi` 是开放注册 + 配额，没有 bootstrap token、token 登录和邀请，要求至少配一种登录途径（SMTP 或 OAuth），见 § 9 |
| `ANYCON_ADMIN_TOKEN` | （首启自动生成） | 仅 `single`。管理员登录 token。不设则首次启动生成并写入 `/data/admin-token`。改它重启即重置登录。`multi` 里设了会被忽略（启动警告），也不生成、不读写 `admin-token` |
| `ANYCON_ADMIN_EMAILS` | （空） | 仅 `multi`。逗号分隔的邮箱：账号邮箱在这个列表里的就是管理员（读取时判断，不写进库里，所以改了重启即生效，去掉就失去权限）。每一项必须是合法邮箱，不合法启动报错（错误里只说第几项，不回显地址）。`single` 里设了被忽略（警告）。`multi` 里留空会警告：这时只有库里已经被授予管理员的账号能管理 hub，而全新的库里没有这样的账号 |
| `ANYCON_QUOTA_DEVICES` | `10` | 仅 `multi`。每个账号最多多少台（未吊销的）设备，含 daemon。正整数，≥ 1 |
| `ANYCON_QUOTA_DAEMONS` | `3` | 仅 `multi`。每个账号最多多少台 daemon（节点） |
| `ANYCON_QUOTA_CONNECTIONS` | `10` | 仅 `multi`。每个账号最多同时多少条在线连接（正式设备的；临时连接和预览连接不算） |
| `ANYCON_QUOTA_DAILY_PAIRINGS` | `20` | 仅 `multi`。每个账号滚动 24 小时内最多多少次配对和接入尝试（`pair_begin`、`enroll_begin`）。计数在内存里，hub 重启清零 |
| `ANYCON_OAUTH_GITHUB_CLIENT_ID` / `ANYCON_OAUTH_GITHUB_CLIENT_SECRET` | （空） | GitHub 登录的 OAuth 应用凭据，两种模式都能用。要么两个都设，要么都不设（只设一个启动报错），值不能含空白。配了登录页才出现 GitHub 按钮；回调地址和应用的设置见 § 3.1。client secret 不进日志。`ANYCON_PUBLIC_URL` 不是 https 时启动警告（本机开发可以） |
| `ANYCON_OAUTH_GOOGLE_CLIENT_ID` / `ANYCON_OAUTH_GOOGLE_CLIENT_SECRET` | （空） | 同上，Google 登录 |
| `ANYCON_DATA_DIR` | `/data` | SQLite 文件、管理员 token 等 |
| `ANYCON_DATABASE_URL` | `sqlite://<DATA_DIR>/hub.db` | v0.1 只支持 SQLite（`sqlite://路径`）；`postgres://…` 会被拒绝，Postgres 在 v0.2 |
| `ANYCON_TRUSTED_PROXIES` | `172.16.0.0/12` | 逗号分隔的 CIDR / IP：只有直连对端落在这里，hub 才相信 `X-Forwarded-For` / `X-Forwarded-Proto`。默认值是 Docker 默认桥接网段，所以反代也在 Docker 默认网络里时开箱即用；**不在 Docker 里部署时务必显式设置**（填反代的地址），否则要么限速只看到反代的 IP，要么（客户端恰好在这个网段时）客户端能伪造转发头。空串 = 不信任任何代理（`ANYCON_PUBLIC_URL` 是 https 而这里为空时，启动日志会警告：反代后所有人共用反代 IP 的限速桶）。启动时会 info 打出生效的网段。限速靠真实 IP（IPv6 按 /64 归并），这个配错了限速就失效。`ANYCON_PUBLIC_URL` 是 https 时 session cookie 始终带 `Secure`，不依赖转发头 |
| `ANYCON_MAX_ENVELOPE_BYTES` | `262144`（256 KiB），最小 `205824`（一个最坏情况的满片分片信封；更小的值拒绝启动） | 单个信封（外层 WebSocket 帧里的一个 `Envelope`）的大小上限。超过的信封被丢弃，hub 回发送方 `control.dropped { too_large }`（每连接每秒至多一条）；WebSocket 层的硬上限是 `max(2 × 这个值, 64 KiB)`，更大的帧直接断开连接。这是防御性上限，**一条 RPC 消息比它大没关系**：客户端和 daemon 会按 200 KiB 一片分片（[ADR-30](decisions.md#adr-30-内层分片与大小上限)）。调大它没有好处，除非你有特殊的反代 / 内存约束；低于一个满片的分片信封时 hub 拒绝启动 |
| `ANYCON_RATE_CLIENT_BYTES_PER_MIN` | `10485760`（10 MiB），最小 `205824` | 每条客户端连接（web / cli / sdk、临时连接）每分钟的入站字节预算（令牌桶：容量 = 这个值，匀速补充，只有转发成功的信封才扣额度）。超过的信封不丢，hub 退回 `backpressure` 并每秒至多一条 `rate_limited { retry_after_ms }`，发送方暂停后按序重发；只有「一直被限速退回、期间没有任何信封被转发」持续 10 秒才以 `1008` 断开（[spec/hub.md § 3.4](spec/hub.md#34-速率)）。低于最小值拒绝启动 |
| `ANYCON_RATE_DAEMON_BYTES_PER_MIN` | `104857600`（100 MiB），最小 `205824` | 每条 daemon 连接的入站预算。daemon 是它所有客户端的汇聚点：几台设备同时 watch、重连后补历史、大的工具输出都算在这一条连接上，所以比客户端高一个量级。流量大的节点（多设备、长会话）可以再调高；它只是防止一个设备拖垮 hub |
| `ANYCON_LOG` | `info` | tracing 过滤表达式；没设时回落到 `RUST_LOG` |
| `ANYCON_SMTP_URL` | （空 = 不发邮件） | 邮箱验证码登录和邮件通知（[ADR-37](decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)）。`smtps://user:pass@smtp.example.com:465`（隐式 TLS）或 `smtp://user:pass@smtp.example.com:587`（STARTTLS，必须成功）；用户名密码里的特殊字符要百分号编码（`@` → `%40`）。`smtp+plain://host:25` 不加密，只允许本机和内网：`localhost`、`*.localhost`、回环 / 私有（RFC 1918）/ 链路本地 / `100.64.0.0/10`、IPv6 的 `::1` / `fc00::/7` / `fe80::/10`，写主机名也行（docker compose 里可以直接写服务名，`smtp+plain://mailpit:1025`）：hub 每次发信前自己解析它，解析出的地址必须**全部**是本机 / 内网，然后直接连这个 IP（不会被 DNS 搜索域或改解析骗到公网）。IP 字面量不在这些范围、或主机名解析出公网地址：IP 字面量启动就报错，主机名在发信时失败并记 warn（错误信息不带 URL，它含密码）——所以写错成公网主机名时，启动不会报错，要看第一封信的日志。配了它，登录页才出现「邮箱」，账号设置里才能绑定邮箱 |
| `ANYCON_EMAIL_FROM` | （配了 SMTP 就必填） | 发件人，`Anycon <noreply@example.com>` 或裸地址。要是 SMTP 服务商允许的发件地址 |
| `ANYCON_VAPID_SUBJECT` | `ANYCON_PUBLIC_URL`（https 时），否则 `mailto:` + 发件人，再没有就 `mailto:noreply@<PUBLIC_URL 的主机名>` | Web Push 的 VAPID `sub`：推送服务出问题时联系你的方式，`mailto:you@example.com` 或 https 地址。VAPID 密钥本身首次启动自动生成，存 `<DATA_DIR>/vapid.json`，**备份 `/data` 时一起备份**（丢了所有浏览器要重新开启推送） |
| `ANYCON_PREVIEW_URL` | （空 = 关闭预览） | dev server 预览的 origin 模板（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)），如 `https://*.preview-example.net`：`*` 是最左边的一整段，代表每个预览的 label。**预览域名必须和 `ANYCON_PUBLIC_URL` 不在同一个可注册域名下**（`hub.example.com` 配 `*.preview.example.com` 会启动报错），要单独的域名和泛域名证书，见 § 4「dev server 预览」。只有本机开发可以用 http：`http://*.preview.localhost:8080` |
| `ANYCON_PREVIEW_BYTES_PER_MIN` | `536870912`（512 MiB），最小 `205824` | 每个账号预览流量（两个方向合计）每分钟的预算。超了只暂停预览，不影响会话 |
| `ANYCON_PREVIEW_MAX_CONNECTIONS` | `16` | 每个账号同时在线的预览连接数（一个预览域名的所有标签页共用一条） |

**已识别、但还没有实现**的变量：设置了不会报错，hub 启动时在日志里警告「ignored」。这些是后续版本的设计目标：

| 变量 | 计划 | 说明 |
|---|---|---|
| `ANYCON_QUEUE_ENABLED` / `ANYCON_QUEUE_MAX_COUNT` / `ANYCON_QUEUE_MAX_BYTES` / `ANYCON_QUEUE_TTL` | v0.1.x | hub 侧离线队列（默认 `false`、50 条、256 KiB、1 小时）。关闭时 hub 不暂存任何消息，对端离线的消息留在客户端本地待重发 |
| 其余的 `ANYCON_QUOTA_*`（队列配额等） | 随队列 | 上面四个之外的配额变量 |
| `ANYCON_METRICS` | v0.3 | `/metrics` Prometheus 端点（这一个目前只在 debug 日志里提一句，不在启动警告里） |

如果 hub 发现自己被 `http://` 而非 `https://` 访问（通过 `X-Forwarded-Proto` 或 `ANYCON_PUBLIC_URL` 判断），启动日志会给醒目警告，但不拦截——本地开发要用。

### 3.1 登录方式的配置

登录方式由配置决定，和模式基本无关（[ADR-22](decisions.md#adr-22-账号登录方式与身份模型)、[ADR-23](decisions.md#adr-23-token-登录与局域网双-origin)）；token 是例外，只有 `single` 有。没配的方式在登录页不显示。

**token 登录**：`single` 模式专属，不需要任何外部服务，局域网明文 HTTP 下也能用。见 § 2。

**邮箱验证码**：配 `ANYCON_SMTP_URL` + `ANYCON_EMAIL_FROM`（§ 3）。`single` 模式下只给已绑定邮箱的账号用：先用 token 登录，在「账号设置 → 个人资料」里绑定邮箱（会发一封验证码确认邮箱归属），之后任何设备上都能用邮箱 + 验证码登录。不能用邮箱注册，加人仍然走邀请。`multi` 模式下对任何邮箱发码，验证通过时这个邮箱还没有账号就创建一个。

**GitHub / Google**（两种模式都能用，配了凭据才出现）：

- **GitHub**：在 GitHub 的 Settings → Developer settings → OAuth Apps 新建一个 OAuth App，Authorization callback URL 填 `${ANYCON_PUBLIC_URL}/auth/callback/github`，把 Client ID 和生成的 Client secret 填进 `ANYCON_OAUTH_GITHUB_CLIENT_ID` / `_CLIENT_SECRET`。hub 请求的 scope 是 `read:user user:email`，后者用于读取已验证邮箱。读不到邮箱（比如用 GitHub App、没开 Email addresses 权限）也能登录，只是这次登录没有已验证邮箱可用来匹配账号
- **Google**：在 Google Cloud Console 新建 OAuth client（类型 Web application），Authorized redirect URI 填 `${ANYCON_PUBLIC_URL}/auth/callback/google`，Client ID 和 secret 填进 `ANYCON_OAUTH_GOOGLE_CLIENT_ID` / `_CLIENT_SECRET`。hub 只用 `openid email profile` 三个基础 scope，不需要 Google 的应用审核；但 OAuth consent screen 的发布状态要设为 **In production**，否则 Testing 状态有 100 个用户的上限，且用户每 7 天要重新授权
- **回调只回公网地址**：回调 URL 必须是 `${ANYCON_PUBLIC_URL}/auth/callback/<provider>`，所以 `ANYCON_PUBLIC_URL` 要填最终的公网 https 域名（提供方要求 https 回调；用 http 只在本机开发时可以，hub 启动时会警告）。从局域网 IP 或别的域名打开 Web UI 时，登录页的 GitHub / Google 按钮是禁用的；hub 的 OAuth 入口也会把从别的地址来的请求先转到公网地址上
- **反代要把原始 `Host` 传给 hub**（§ 4）：OAuth 入口靠它判断请求是不是来自公网地址
- **账号怎么对应**：先按提供方的稳定用户 ID 匹配已关联的身份；没有，再看提供方**声明已验证**的邮箱是不是某个账号的邮箱，是就关联到那个账号；都不是：`multi` 创建新账号，`single` 要邀请——管理员发的邀请链接打开后，登录页的 GitHub / Google 按钮会带上邀请码，由 hub 在 OAuth 登录时兑换，没有邀请码得到「这个 hub 仅限受邀注册」。没有已验证邮箱的身份不会匹配任何账号：这样的用户先用别的方式登录已有账号，再在「账号设置 → 登录方式」里关联。规则见 [architecture.md § 3.2](architecture.md#32-账号与身份)
- hub 只存 `(提供方, 提供方的用户 ID)` 属于哪个账号，不存用户名、头像和 access token

**Web Push**：无需配置（可选 `ANYCON_VAPID_SUBJECT`）。要求 https 域名（浏览器只在安全上下文里给推送）；iOS 只有加到主屏的 PWA 能收。在「账号设置 → 通知」里对每个浏览器单独开启。

下面是还没实现的方式，写在这里是为了让部署计划有个预期；**hub 现在不认这些配置**（SMTP 服务商：Resend、SES、Postmark、Mailgun 等都提供 SMTP 端点，直接填进 `ANYCON_SMTP_URL` 即可）：

- **passkey**（v0.1.x）：无需配置。WebAuthn 的 RP ID 取自 `ANYCON_PUBLIC_URL` 的域名，所以 hub 一定要在最终域名上跑，中途换域名所有 passkey 作废，用户需要用其他方式登录后重新绑定。要求 https 域名

## 4. 反向代理

**反代必须支持 WebSocket 升级**，这是最常踩的坑。

**反代必须把原始 `Host` 传给 hub。** OAuth 登录的入口靠它判断请求是不是来自 `ANYCON_PUBLIC_URL` 的域名，预览域名的路由（下面「dev server 预览」）也靠它。Caddy、Traefik 默认保留；nginx 要写 `proxy_set_header Host $host`（下面的示例有）；Cloudflare Tunnel 不要用 `httpHostHeader` 改写它。反代把 `Host` 改成了内部地址时，OAuth 入口至多把请求转到公网地址一次（跳转地址带 `hop=1`，带着它的请求不再跳），不会死循环，但预览会找不到域名。

### Caddy

```
hub.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy 自动处理 TLS 和 WebSocket。

### nginx

```nginx
server {
    listen 443 ssl http2;
    server_name hub.example.com;
    # ssl_certificate …

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 3600s;   # 长连接
    }
}
```

### Traefik（docker labels）

```yaml
labels:
  - traefik.enable=true
  - traefik.http.routers.anycon.rule=Host(`hub.example.com`)
  - traefik.http.routers.anycon.tls.certresolver=letsencrypt
  - traefik.http.services.anycon.loadbalancer.server.port=8080
```

### Cloudflare Tunnel

hub 也可以放在家里，不一定要 VPS：

```yaml
# cloudflared config
ingress:
  - hostname: hub.example.com
    service: http://localhost:8080
  - service: http_status:404
```

Cloudflare 免费版支持 WebSocket。注意 Cloudflare 会终止 TLS 并看到外层信封，但内层仍是 Noise 密文。它也在 Web UI 的信任链上：浏览器拿到的 JS 经过它，一个想读你会话的中间方改一份 JS 就够了（见下面「反代也是信任链的一环」）。

### 家里跑 hub：局域网 + 公网

hub 和 daemon 跑在同一台 Mac mini 上是常见配置：家里用 `http://192.168.1.10:8080` 直连，出门经 Cloudflare Tunnel 或端口转发走 `https://hub.example.com`。

- `ANYCON_PUBLIC_URL` 填公网域名。局域网 IP 不用配置，hub 对私有网段和 localhost 的 http 请求自动放行
- 局域网上用 token（`single`）或邮箱验证码登录。GitHub、Google（以及将来的 passkey）只在公网域名可用
- 同一个浏览器在两个地址上是两台设备，各配对一次，设备列表里会有两条。手机在家连 wifi 时直接用公网域名，就没有这个问题
- 局域网是明文 HTTP：同网段能看到登录态，看不到会话内容，内层是 Noise 密文。介意的话家里也走公网域名

推荐把公网域名作为唯一入口，局域网 IP 留给没有公网域名、断网和首次设置的场景。原理见 [architecture.md § 3.3](architecture.md#33-局域网与公网两个-origin)。

### dev server 预览：第二个域名

预览跑在单独的泛域名上（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)、[spec/hub.md § 13](spec/hub.md#13-预览)）：每个预览是 `https://<label>.<预览域名>`，hub 用同一个端口、按 `Host` 区分。要准备三样：

1. **一个单独的可注册域名**。不能是 hub 域名的子域：同站的子域能往父域写 cookie，预览里跑的是 agent 写的代码。公共 hub 用 `aiplace.app`
2. **泛域名 DNS**：`*.<预览域名>` 指向 hub（和 hub 主域名同一台机器）
3. **泛域名证书**：`*.<预览域名>`，Let's Encrypt 要用 DNS-01 验证（Caddy 要带对应 DNS 服务商插件的构建，或者用下面的 Cloudflare 方案）

然后设 `ANYCON_PREVIEW_URL=https://*.<预览域名>`，重启 hub。反代要把原始 `Host` 传过去、支持 WebSocket，和主域名一样。nginx：

```nginx
server {
    listen 443 ssl http2;
    server_name *.preview-example.net;
    # ssl_certificate：*.preview-example.net 的泛域名证书

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 3600s;
    }
}
```

**用 Cloudflare 时**（公共 hub 的做法）：

- 域名托管在 Cloudflare，加一条**橙云（代理）**的泛域名记录 `*` → hub 服务器。免费的 Universal SSL 覆盖一级泛域名 `*.<预览域名>`，所以 label 直接放在一级（`<label>.aiplace.app`，不要 `<label>.preview.aiplace.app`——二级泛域名要付费的证书）
- 源站证书用 Cloudflare Origin CA 签一张 `*.<预览域名>` 的，SSL 模式 Full (strict)
- WebSocket 默认就开着。**关掉会往页面里注入脚本或改写内容的功能**（Rocket Loader、Email Obfuscation、自动压缩 HTML 一类）：它们会被 CSP 拦掉，还会破坏 service worker 的引导
- 缓存保持「遵循源站头」（Respect Existing Headers）：hub 给 `sw.js`、引导页发的是 `no-cache` / `no-store`
- Cloudflare 终止 TLS，看得到外层信封和预览运行时的代码，看不到隧道里的内容；它在预览运行时的信任链上，和 Web UI 一样（下面「反代也是信任链的一环」）
- 用 Cloudflare Tunnel 时，ingress 里加一条 `hostname: "*.<预览域名>"`，DNS 加一条泛域名 CNAME 指向隧道

公共 hub 的预览流量全部经过它，`ANYCON_PREVIEW_BYTES_PER_MIN` 按机器带宽设。

### hub 下发的响应头和 `index.html`

hub 对 HTML 响应（Web UI 和 SPA 回退）下发 CSP，`script-src 'self' 'wasm-unsafe-eval'`，**没有 `unsafe-inline`**：`index.html` 里不能有内联 `<script>`（主题脚本是外置的 `/theme.js`），也不能用 `blob:` Worker / `eval`。所以 hub 前面的反代**不要**改写 `index.html` 注入脚本（比如统计代码、Cloudflare 的自动注入类功能要关掉），会被浏览器拦掉。自己改前端并重新构建时同理。

### 反代也是信任链的一环

Web UI 的代码由 hub 下发，经你的反代 / 隧道 / CDN 到浏览器。E2E 保护的是会话内容不被 hub 和中间方在转发时看到，但**不保护你不被「下发给你的 JS」出卖**：任何能改这份 JS 的人（hub 运营者、被入侵的 hub、终止 TLS 的 CDN 或隧道）都能让页面把 Noise 私钥和明文发走（[architecture.md § 5.6](architecture.md#56-威胁模型)、[ADR-32](decisions.md#adr-32-如实声明web-客户端代码由-hub-下发)）。部署上的含义：

- 隐私敏感的使用**自建 hub**，并让 TLS 终止在你自己控制的反代上；用公共 hub 时，敏感操作走 CLI / SDK，不走浏览器
- **不要覆盖或去掉 hub 发的安全头**，尤其是 `Content-Security-Policy`（`default-src 'self'`、`connect-src 'self'`、`img-src 'self' data: blob:`、`frame-ancestors 'none'`，完整策略见 [spec/hub.md § 2.1](spec/hub.md#21-静态与健康)）。反代自己再加头可以，但不要和它冲突
- 钉住镜像版本（`ghcr.io/anycon-ai/hub:0.1.x` 或 digest），升级前读 changelog，别让 `latest` 在你不知道的时候换了前端代码

### docker compose 示例（含 Caddy）

```yaml
services:
  hub:
    image: ghcr.io/anycon-ai/hub:latest
    restart: unless-stopped
    environment:
      ANYCON_PUBLIC_URL: https://hub.example.com
    volumes:
      - ./anycon-data:/data

  caddy:
    image: caddy:2
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile
      - caddy-data:/data

volumes:
  caddy-data:
```

完整示例在 `docker/` 目录。

## 5. 备份

SQLite 模式下，`/data` 目录就是全部状态：

```bash
docker exec anycon-hub anycon-hub backup /data/backup.db   # 在线一致性备份（v0.1.x，v0.1 的镜像还没有 backup 子命令）
```

v0.1：直接停容器复制 `/data` 目录（或在宿主机上对 `hub.db` 跑 `sqlite3 hub.db ".backup backup.db"`）。

**提醒**：hub 里没有会话历史。历史在每台 daemon 的数据目录（默认 `~/.anycon/`），那边要单独备份：停掉 daemon 复制整个目录，或者在线 `sqlite3 ~/.anycon/daemon.db ".backup backup.db"`（附件在 `~/.anycon/blobs/`，一并复制）。

## 6. 升级

```bash
docker pull ghcr.io/anycon-ai/hub:latest
docker stop anycon-hub && docker rm anycon-hub
docker run -d --name anycon-hub …                 # 和 § 2 同样的参数，数据卷不变
```

`docker restart` 不会换镜像，要删掉容器再用新镜像建一个（数据都在 `/data` 卷里）。compose 下是 `docker compose pull && docker compose up -d`。钉了版本的话，先把标签改成新版本。hub 启动时自动跑迁移。兼容策略：

- hub 和 daemon 兼容相邻一个 minor 版本，daemon 比 hub 新或旧一个 minor 都能工作
- 0.x 阶段不保证，升级前看 [CHANGELOG](../CHANGELOG.md)；拿不准时 hub、daemon、CLI、SDK 一起升

## 7. 健康检查与监控

- `GET /healthz` → `200`，JSON `{ "status": "ok", "version": … }`，不查库
- `GET /readyz` → 数据库可用时 `200`，否则 `503`
- `/metrics`（Prometheus 格式：在线设备数、信封转发速率、未送达数、握手失败数、队列深度）是 v0.3 的计划，v0.1 没有

## 8. daemon 侧

安装 `anycon`（daemon 和 CLI 是同一个二进制），三选一：

```bash
brew install anycon-ai/tap/anycon        # macOS / Linux 的 Homebrew
curl -fsSL https://github.com/anycon-ai/anycon/releases/latest/download/install.sh | sh
cargo install --path crates/cli --locked # 从源码，在仓库根目录
```

预构建的二进制覆盖 macOS arm64 / x64 和 Linux x64 / arm64（glibc 2.35 及以上，Ubuntu 22.04、Debian 12 及更新）；其它平台（musl、更老的 glibc）从源码构建。安装脚本校验 sha256、装到 `~/.local/bin`，不用 sudo；环境变量加在 `sh` 前面：`… | ANYCON_VERSION=0.1.0 sh` 装指定版本，`ANYCON_INSTALL_DIR` 改安装目录。每个 [GitHub Release](https://github.com/anycon-ai/anycon/releases) 附带各平台的压缩包、`SHA256SUMS` 和构建来源证明（`gh attestation verify <文件> --repo anycon-ai/anycon`），压缩包里有 `THIRD-PARTY-LICENSES.txt`。

```bash
anycon init --hub https://hub.example.com --workspace ~/anycon
anycon service install                   # launchd (macOS) / systemd user (Linux)
anycon pair
anycon doctor                            # 检查网络、agent CLI 是否安装、权限
```

升级（`brew upgrade anycon` 或重跑安装脚本）后 `anycon service restart` 换上新版本。服务文件记的路径升级后不变：Homebrew 装的记 formula 的固定入口 `<prefix>/opt/anycon/bin/anycon`，不记带版本号的 Cellar 路径。

daemon 数据目录 `~/.anycon/`：

```
~/.anycon/                      ANYCON_HOME 可改
├── config.toml                 daemon 配置
├── daemon.db                   会话历史、FTS、审计（SQLite）
├── keys/
│   ├── node.key                Noise 静态私钥，0600
│   └── device-token            hub 签发的设备 token，0600
├── peers.json                  已配对设备公钥（数据库的镜像）
├── daemon.sock                 本机 CLI 通道，0600
├── logs/                       daemon 日志，默认保留 7 天
├── client/<hub-host>/          CLI 作为客户端设备时的密钥和 token
└── blobs/                      图片附件（ADR-35）
```

完整说明见 [spec/cli.md § 2](spec/cli.md#2-目录与配置)。

Mac 睡眠后 daemon 会自动重连，但如果 Mac mini 要 7×24 可用，系统设置里要关掉「允许电脑自动进入睡眠」。

## 9. 公共 hub 与 `multi` 模式

公共 hub（anycon.ai）运行的是同一份 release，只是配置不同：`ANYCON_MODE=multi`，加上 SMTP、GitHub / Google 的 OAuth 应用、`ANYCON_ADMIN_EMAILS`，以及它自己选的 `ANYCON_QUOTA_*`。自建的人也可以这样配，得到一个开放注册的 hub。anycon.ai 公共 hub 的上线见 [roadmap](roadmap.md)。

`multi` 和默认的 `single` 的区别（[architecture.md § 3](architecture.md#3-运行模式)）：

| | `single` | `multi` |
|---|---|---|
| 注册 | 关闭；管理员发邀请链接（OAuth 新用户也凭邀请） | 开放：第一次邮箱验证码或 GitHub / Google 登录就建账号 |
| token 登录、邀请 | 有（bootstrap token 首次启动生成） | 没有：不生成 bootstrap token，相关路由 `404` |
| 管理员 | bootstrap 账号，加上被授予的 | `ANYCON_ADMIN_EMAILS` 里的邮箱，加上被授予的 |
| 配额 | 无 | 有 |

最小的 `multi` 启动（至少要有 SMTP 或一个 OAuth 应用，否则 hub 拒绝启动——没有 bootstrap token，没人登录得进来）：

```bash
docker run -d --name anycon-hub \
  --restart unless-stopped \
  -p 127.0.0.1:8080:8080 \
  -v ./anycon-data:/data \
  -e ANYCON_PUBLIC_URL=https://hub.example.com \
  -e ANYCON_MODE=multi \
  -e ANYCON_ADMIN_EMAILS=you@example.com \
  -e ANYCON_SMTP_URL=smtps://user:pass@smtp.example.com:465 \
  -e ANYCON_EMAIL_FROM='Anycon <noreply@example.com>' \
  ghcr.io/anycon-ai/hub:latest
```

用 `ANYCON_ADMIN_EMAILS` 里的邮箱登录（第一次登录就会创建账号），就是管理员；之后在 `/admin` 里可以搜索账号、授予 / 取消管理员、删除账号。靠 `ANYCON_ADMIN_EMAILS` 当上的管理员在页面上取消不了，要改配置。已有的 `single` 数据库可以直接切到 `multi`：账号和设备原样保留，token 和邀请登录停止工作，所以原来的管理员账号要有邮箱或 OAuth 身份才进得来。反过来切回 `single` 也行：bootstrap token 登录的账号由 `kv.bootstrap_admin_id` 记着（升级前跑过 `single` 的库沿用当时最早的管理员；记着的账号被删了就新建一个 `admin` 账号），不会因为 `multi` 里把管理员授予了别的用户就登录到那个用户的账号里。

**限制**：每个账号的上限就是 `ANYCON_QUOTA_*` 的值（默认 10 台设备、其中最多 3 台 daemon、10 条同时在线的连接、滚动 24 小时内 20 次配对 / 接入）。公共 hub 用的数字以它自己的配置为准，用户在「账号设置 → 配额」里看得到。达到设备数或 daemon 数上限时，新的配对或接入被拒绝并提示；达到连接数上限时，新的连接被拒绝；都不影响已有的。客户端通过 `auth_ok.quota` 和 `quota_update` 看到当前用量（[protocol.md § 8](protocol.md#8-控制消息hub--设备)）。不限制中继流量；预览流量另有按账号的限速（`ANYCON_PREVIEW_BYTES_PER_MIN`）。

公共 hub 不开启离线队列，因此不暂存任何数据：节点离线时消息留在你的客户端本地，节点上线后自动重发。

dev server 预览用 `aiplace.app`（`https://<label>.aiplace.app`），每个账号的预览流量另有限速；预览内容经 Noise 隧道，公共 hub 只转发密文。

登录方式：邮箱验证码、GitHub、Google，取决于它配了哪些。不提供 token 登录，没有密码；passkey 还没实现。hub 数据库里关于你的有显示名、邮箱（你绑定的，或 GitHub / Google 声明已验证的）、关联的 GitHub / Google 账号的提供方和稳定用户 ID（不存用户名、头像和 access token）、设备列表和审计记录。

**删除账号**：用户可以在「账号设置 → 个人资料」自己删（需要 10 分钟内登录过，否则先退出再登录一次），管理员可以在 `/admin` 删别人的。删除会立刻断开这个账号的所有连接，并从 hub 删掉它的设备、配对、登录方式、推送订阅和审计记录，只留一条没有 IP 的删除记录；节点上的会话历史不受影响，节点要接入别的账号或 hub 得重新配对。

公共 hub 的成本和捐赠公开。建议正式使用自建。**用公共 hub 的 Web UI = 信任它的运营者不会下发恶意前端代码**（见 [SECURITY.md](../SECURITY.md) 的「已知限制」）；隐私敏感的使用请自建，或用 CLI / SDK 连公共 hub。经公共 hub 配对时用默认的 16 位短码（不要加 `--short`），8 位短码面对恶意 hub 有离线穷举的残余风险（[ADR-29](decisions.md#adr-29-短码不经过-hub)）。
