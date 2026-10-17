# 部署

> 镜像 `ghcr.io/anycon-ai/hub` 由发布流水线（`.github/workflows/release.yml`）在每个版本标签上构建，`linux/amd64` 和 `linux/arm64` 两种架构，标签是 `<版本>`（如 `0.1.0`）、`<主>.<次>`（`0.1`，跟着最新的补丁版）和 `latest`（预发布版只有 `<版本>`）。镜像里的 `/usr/share/doc/anycon-hub/` 是许可证和第三方许可证清单。
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

打开 hub 地址，粘贴 token 登录，之后这个浏览器不用再粘。不想用自动生成的 token 可以设 `ANYCON_ADMIN_TOKEN`；token 丢了的时候，改它重启就是恢复出口。注册默认关闭，之后加人用管理员生成的邀请链接。除了 token，配了 SMTP 还能用邮箱验证码登录（先用 token 登录，在「账号设置」里绑定邮箱）；passkey、GitHub、Google 登录还没实现（见 § 3.1）。

hub 跑在家里、家里走局域网 IP 的部署方式见 § 4「家里跑 hub：局域网 + 公网」。

## 3. 环境变量

下面是 **v0.1 实际生效**的变量：

| 变量 | 默认 | 说明 |
|---|---|---|
| `ANYCON_PUBLIC_URL` | （必填） | 公网地址，必须是不带路径的 `http(s)://host[:port]`。WebSocket 的 Origin 校验、cookie 属性依赖它。局域网 IP 访问不用写在这里，hub 对私有网段和 localhost 的 http 自动放行。**配对链接不用它**：链接由 daemon 按自己配置里的 `hub.url` 生成（`${hub.url}/pair#code=…`），所以 `anycon init --hub` 填的地址要和用户浏览器访问的一致 |
| `ANYCON_LISTEN` | `0.0.0.0:8080` | 监听地址 |
| `ANYCON_MODE` | `single` | v0.1 只有 `single`；设成 `multi` 启动直接报错 |
| `ANYCON_ADMIN_TOKEN` | （首启自动生成） | 管理员登录 token。不设则首次启动生成并写入 `/data/admin-token`。改它重启即重置登录 |
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

**已识别、但 v0.1 没有实现**的变量：设置了不会报错，hub 启动时在日志里警告「ignored」。这些是后续版本的设计目标：

| 变量 | 计划 | 说明 |
|---|---|---|
| `ANYCON_QUEUE_ENABLED` / `ANYCON_QUEUE_MAX_COUNT` / `ANYCON_QUEUE_MAX_BYTES` / `ANYCON_QUEUE_TTL` | v0.1.x | hub 侧离线队列（默认 `false`、50 条、256 KiB、1 小时）。关闭时 hub 不暂存任何消息，对端离线的消息留在客户端本地待重发 |
| `ANYCON_OAUTH_GITHUB_CLIENT_ID` / `_SECRET`、`ANYCON_OAUTH_GOOGLE_CLIENT_ID` / `_SECRET` | v0.2 | GitHub / Google 登录 |
| `ANYCON_ADMIN_EMAILS`、`ANYCON_QUOTA_*` | v0.2 | `multi` 模式的管理员和配额 |
| `ANYCON_METRICS` | v0.3 | `/metrics` Prometheus 端点（这一个目前只在 debug 日志里提一句，不在启动警告里） |

如果 hub 发现自己被 `http://` 而非 `https://` 访问（通过 `X-Forwarded-Proto` 或 `ANYCON_PUBLIC_URL` 判断），启动日志会给醒目警告，但不拦截——本地开发要用。

### 3.1 登录方式的配置

登录方式由配置决定，和模式无关（[ADR-22](decisions.md#adr-22-账号登录方式与身份模型)、[ADR-23](decisions.md#adr-23-token-登录与局域网双-origin)）。没配的方式在登录页不显示。

**token 登录**：`single` 模式专属，不需要任何外部服务，局域网明文 HTTP 下也能用。见 § 2。

**邮箱验证码**：配 `ANYCON_SMTP_URL` + `ANYCON_EMAIL_FROM`（§ 3）。`single` 模式下只给已绑定邮箱的账号用：先用 token 登录，在「账号设置 → 个人资料」里绑定邮箱（会发一封验证码确认邮箱归属），之后任何设备上都能用邮箱 + 验证码登录。不能用邮箱注册，加人仍然走邀请。

**Web Push**：无需配置（可选 `ANYCON_VAPID_SUBJECT`）。要求 https 域名（浏览器只在安全上下文里给推送）；iOS 只有加到主屏的 PWA 能收。在「账号设置 → 通知」里对每个浏览器单独开启。

下面是还没实现的方式，写在这里是为了让部署计划有个预期；**hub 现在不认这些配置**（SMTP 服务商：Resend、SES、Postmark、Mailgun 等都提供 SMTP 端点，直接填进 `ANYCON_SMTP_URL` 即可）：

- **passkey**（v0.1.x）：无需配置。WebAuthn 的 RP ID 取自 `ANYCON_PUBLIC_URL` 的域名，所以 hub 一定要在最终域名上跑，中途换域名所有 passkey 作废，用户需要用其他方式登录后重新绑定。要求 https 域名
- **GitHub**（v0.2）：在 GitHub 的 Settings → Developer settings → OAuth Apps 新建一个 OAuth App，Authorization callback URL 填 `${ANYCON_PUBLIC_URL}/auth/callback/github`。hub 请求的 scope 是 `read:user user:email`，后者用于读取已验证邮箱
- **Google**（v0.2）：在 Google Cloud Console 新建 OAuth client（类型 Web application），Authorized redirect URI 填 `${ANYCON_PUBLIC_URL}/auth/callback/google`。hub 只用 `openid email profile` 三个基础 scope，不需要 Google 的应用审核；但 OAuth consent screen 的发布状态要设为 **In production**，否则 Testing 状态有 100 个用户的上限，且用户每 7 天要重新授权

## 4. 反向代理

**反代必须支持 WebSocket 升级**，这是最常踩的坑。

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
- 局域网上用 token 或邮箱验证码登录。passkey、GitHub、Google 只在公网域名可用
- 同一个浏览器在两个地址上是两台设备，各配对一次，设备列表里会有两条。手机在家连 wifi 时直接用公网域名，就没有这个问题
- 局域网是明文 HTTP：同网段能看到登录态，看不到会话内容，内层是 Noise 密文。介意的话家里也走公网域名

推荐把公网域名作为唯一入口，局域网 IP 留给没有公网域名、断网和首次设置的场景。原理见 [architecture.md § 3.3](architecture.md#33-局域网与公网两个-origin)。

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

## 9. 公共 hub（anycon.ai）

> **v0.2 的设计目标，v0.1 不提供。** `multi` 模式、配额、邮箱注册和 GitHub / Google 登录都还没实现，下面是它们上线时的样子。

公共 hub 运行同一份 release，`ANYCON_MODE=multi`。限制项：

| 项 | 免费额度 |
|---|---|
| 设备数 | 2 |
| 并发连接 | 3 |
| 中继流量 | 不限制 |
| 离线队列 | 关闭 |

达到设备数或连接数上限时，新的配对或连接会被拒绝并提示，不影响已有的。客户端通过 `control.quota` 消息能看到当前用量。

公共 hub 不开启离线队列，因此不暂存任何数据：节点离线时消息留在你的客户端本地，节点上线后自动重发。

登录方式：邮箱验证码、GitHub、Google；登录后可在设置里绑定 passkey 作为日常免密登录。不提供 token 登录，没有密码。hub 数据库里关于你的只有邮箱、OAuth 身份 ID、passkey 公钥和设备列表。

公共 hub 的成本和捐赠公开。建议正式使用自建。**用公共 hub 的 Web UI = 信任它的运营者不会下发恶意前端代码**（见 [SECURITY.md](../SECURITY.md) 的「已知限制」）；隐私敏感的使用请自建，或用 CLI / SDK 连公共 hub。经公共 hub 配对时用默认的 16 位短码（不要加 `--short`），8 位短码面对恶意 hub 有离线穷举的残余风险（[ADR-29](decisions.md#adr-29-短码不经过-hub)）。
