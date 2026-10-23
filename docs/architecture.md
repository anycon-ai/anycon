# 架构

本文描述 anycon 的组件划分、运行模式、数据归属、加密模型和 workspace 模型。协议细节见 [protocol.md](protocol.md)，选型理由见 [tech-stack.md](tech-stack.md)。

## 1. 一张图

```
                         ┌───────────────────────────────┐
                         │          anycon-hub           │
                         │  账号 · 设备路由 · 配对/接入   │
                         │  配额计数 · Web UI 静态文件    │
                         │                               │
                         │   不接触会话内容，只见元数据   │
                         └───────┬───────────────┬───────┘
                 出站 WebSocket  │               │  WebSocket
                                 │               │
          ┌──────────────────────┴──┐       ┌────┴─────────────────────┐
          │   anycon daemon          │       │   客户端                  │
          │   (Mac mini / 任意机器)   │ Noise │   Web UI / CLI / SDK      │
          │                          │◄═════►│                           │
          │  会话历史 (SQLite)        │  E2E  │  本地缓存                 │
          │  workspace 扫描           │       │  Noise 静态密钥           │
          │  adapter → agent 进程     │       │                           │
          │  Noise 静态密钥           │       └───────────────────────────┘
          └──────────┬───────────────┘
                     │ spawn
          ┌──────────┴───────────────┐
          │ Claude Code · Codex      │
          │ OpenCode · Pi · 自定义    │
          └──────────────────────────┘
```

**hub 是邮递员，不是中间人。** 它知道信从哪来到哪去，不知道信里写了什么。前提是你用的客户端代码没被它动过：CLI / SDK 的代码是你自己装的，而 Web UI 的代码恰好由 hub 下发，这一点在 [§ 5.6](#56-威胁模型) 里单独讲。

## 2. 组件

### 2.1 `anycon-hub`

单个 Rust 二进制，职责刻意压到最少：

| 职责 | 说明 |
|---|---|
| 账号 | 登录（token / passkey / 邮箱验证码 / GitHub / Google）、身份关联、邀请、设备列表 |
| 设备路由 | 维护「设备 ID → 当前 WebSocket 连接」映射，转发信封 |
| 离线处理 | 目标设备离线时丢弃信封并通知发送方。可选开启密文队列暂存，默认关闭 |
| 配对撮合 | 按 `match_id`（短码的哈希；hub 看不到短码本身）把两个待配对的连接接到一起，之后只转发 |
| 接入撮合 | 已登录账号的设备请求接入本账号的某台 daemon 时，确认归属后把两条连接接到一起，之后只转发；能不能接入由 daemon 验证（§ 5.2.1） |
| 通知 | 把 `notify` 信封的类别转成 Web Push |
| 配额 | 按信封元数据计数：设备数、连接数。不限会话流量；预览流量按账号限速 |
| 静态文件 | 嵌入的 Web UI。**浏览器运行的就是 hub 下发的这份代码**，见 § 5.6 |
| 预览 | 配了预览域名时：在 `<label>.<预览域名>` 上提供预览运行时（另一份嵌入的前端），签发预览令牌，把预览连接的密文转给对应的 daemon（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)） |

hub **不**做：解析消息内容、存会话历史、存附件、搜索、运行 agent。

### 2.2 `anycon`（daemon + CLI）

同一个二进制，`anycon daemon` 启动守护进程，其余子命令是 CLI。跑在用户自己的机器上。

| 职责 | 说明 |
|---|---|
| 连接 | 对 hub 保持一条出站 WebSocket，心跳、指数退避重连、睡眠唤醒恢复 |
| 密钥 | 持有自己的 Noise 静态密钥和所有已配对客户端的公钥 |
| 接入 | 验证账号内接入（扫码批准、接入码、节点口令），自己限制在线猜测（§ 5.2.1） |
| 会话存储 | SQLite（WAL），会话、消息、附件、FTS5 搜索索引 |
| workspace | 扫描根目录，一级子文件夹即项目；fs watch 根目录；新建/克隆/重命名/回收站 |
| 会话管理 | 创建、恢复、取消、并发上限 |
| adapter | 拉起 agent 进程，把 agent 的事件流翻译成协议事件，把用户输入和权限答复送回去 |
| 审计 | 本地记录谁通过哪个设备做了什么 |
| 预览 | 给已配对设备签发预览票据；接受预览页的 Noise 隧道，把里面的 HTTP / WebSocket 转给本机允许的端口（[spec/daemon.md § 19](spec/daemon.md#19-预览)） |
| 服务 | `anycon service install` 注册 launchd / systemd |

### 2.3 客户端

- **Web UI**：React SPA，嵌在 hub 里，由 hub 下发给浏览器。自己管理 WebSocket 和状态层，只和 `packages/protocol` 共享类型与编解码，不依赖 `@anycon/sdk`。因为代码来自 hub，它的信任基础是「你信任这个 hub 的运营者」，CLI / SDK 没有这个前提（§ 5.6）
- **预览运行时**：预览 origin（`<label>.<预览域名>`）上的引导页、service worker 和 shim，同样由 hub 下发，但和 Web UI 分开构建、跑在另一个可注册域名下：预览的是 agent 写的任意代码，不能和 Web UI 同 origin（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)）
- **CLI**：`anycon send`、`anycon ls`、`anycon attach` 等，走和 Web 同一套协议
- **TS SDK / Python SDK**：两个方向——作为客户端调用节点，或把自定义 agent 注册为节点。v0.1 只有 TS SDK 的客户端方向（[roadmap](roadmap.md)）

所有客户端都要配置 hub 地址，没有默认值（anycon.ai 公共 hub 在 v0.2）。

### 2.4 `crates/crypto`

Noise 握手和对称加密的唯一实现。编译成三种产物：

- Rust crate，daemon 和 CLI 直接用
- wasm，Web UI 和 TS SDK 用
- Python wheel（PyO3，abi3），Python SDK 用（v0.1.x）

三端一份实现，握手永远对得上。

### 2.5 `crates/protocol`

协议类型的唯一真源。用 `schemars` 导出 JSON Schema，再生成 TS 类型（pydantic 模型在 v0.1.x）。生成产物提交进仓库，CI 校验是否与源码一致。

## 3. 运行模式

一个 hub 二进制，两种模式，用配置切换（v0.1 只实现了 `single`，登录方式是 token 和已绑定邮箱的验证码；`multi`、passkey、OAuth 见 [roadmap](roadmap.md)）。**数据模型从第一天就是 账号 → 设备 → 会话 的多租户形态**，`single` 模式只是恰好只有一个账号。

| | `single`（自建默认） | `multi`（anycon.ai 公共 hub） |
|---|---|---|
| 注册 | 关闭；管理员用 bootstrap token 登录（首次启动生成并打印，或 `ANYCON_ADMIN_TOKEN`） | 开放，任一登录方式首次登录即创建账号 |
| 加人 | 管理员生成邀请链接 | 自助注册 |
| 登录方式 | token、passkey；配置了 SMTP / OAuth 后同样可用邮箱验证码、GitHub、Google | 邮箱验证码、GitHub、Google、passkey。**不提供 token，没有密码** |
| 配额 | 不启用 | 启用 |
| 数据库 | SQLite | SQLite 或 Postgres |
| E2E | 启用 | 启用 |

两种模式跑同一份 release，不存在「官方特供分支」。登录方式由配置决定（有没有配 SMTP、有没有配 OAuth provider），不由模式决定；模式只管三件事：注册是否开放、配额是否启用、是否允许 token 登录。见 [ADR-22](decisions.md#adr-22-账号登录方式与身份模型)、[ADR-23](decisions.md#adr-23-token-登录与局域网双-origin)。没有密码，没有二次验证：账号权限很小（§ 5.6），不值得为它背一套密码和 TOTP。（节点口令是 daemon 自己的门，不是账号密码，hub 不经手，见 § 5.2.1。）

### 3.1 登录方式

账号登录主要发生在 Web UI。CLI 只在账号内接入（§ 5.2.1）时临时登录一次（邮箱验证码，`single` 模式也可以用 token），登录态只在内存里，接入结束就退出；之后和以前一样是持设备 token 的设备。SDK 不登录账号，通过配对拿到设备 token（见 § 5.2）。

| 方式 | 说明 | 可用条件 | 局域网 http 下 |
|---|---|---|---|
| token | 粘一次换长期 session cookie。bootstrap token 来自 `ANYCON_ADMIN_TOKEN`，或首次启动自动生成、写入 `/data/admin-token` 并打印到日志；账号设置里可为任何账号再生成，只展示一次，库里存哈希。丢了 passkey 时改环境变量重启就是恢复出口 | 仅 `single` 模式 | 可用 |
| passkey | WebAuthn，免密。用其他方式登录后在设置里绑定，之后作为日常登录 | https 域名 | 不可用：WebAuthn 要求安全上下文，RP ID 不能是 IP |
| 邮箱验证码 | 6 位数字码发到邮箱，10 分钟有效，错 5 次作废；同一邮箱 60 秒内只发一次，按 IP 和邮箱双重限速。`single` 模式只给**已在账号设置里绑定了邮箱**的账号用，不能用邮箱注册（[ADR-37](decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)）；`multi` 的「未注册邮箱直接建号」随 `multi` 一起做 | 配置了 SMTP | 可用 |
| GitHub | OAuth 授权码流程 + PKCE + `state`，scope `read:user user:email`，用 `/user/emails` 取已验证的主邮箱 | 配置了 GitHub OAuth app | 不可用：回调只回公网域名 |
| Google | OIDC 授权码流程 + PKCE + `state`，scope `openid email profile`，用 access token 调 userinfo，信任其中的 `email_verified` | 配置了 Google OAuth client | 不可用：同上 |

未配置的方式在登录页不显示。`multi` 模式若既没配 SMTP 也没配任何 OAuth provider，hub 启动时直接报错，因为没有任何人能登录。

Web 登录会话是随机 token 存库，`HttpOnly; SameSite=Lax` cookie，`Secure` 在 `ANYCON_PUBLIC_URL` 是 https 时始终带，否则按请求是否 https（可信代理的 `X-Forwarded-Proto`）决定，不设 `Domain`。不用 JWT（理由见 [tech-stack.md](tech-stack.md)）。

### 3.2 账号与身份

一个账号可以挂多个登录身份，表结构从 v0.1 就是这个形态：

```
account
  ├── email?           已验证、唯一。single 模式下只用 token 的账号可以没有
  ├── tokens[]         登录 token 的哈希 + 标签，可多个；bootstrap token 来自配置，不在库里
  ├── passkeys[]       WebAuthn credential，可多个
  └── identities[]     { provider: github | google, subject: provider 的稳定用户 ID }
```

关联规则：

- OAuth 登录按 `(provider, subject)` 匹配，**不按邮箱或用户名**——这两个都可以改
- 没匹配到身份时：provider 声明该邮箱已验证且与某个账号的邮箱相同 → 自动关联并登录；provider 没有声明已验证 → 先走一次邮箱验证码，再关联。未验证邮箱自动关联是经典的账号接管漏洞
- 邮箱也不匹配 → `multi` 创建新账号，`single` 要求邀请链接
- 用户可在设置里增删身份、换邮箱（新邮箱要验证）；最后一种登录方式不能删
- 邀请链接不绑定登录方式，受邀者用任一已配置的方式完成注册

### 3.3 局域网与公网：两个 origin

hub 跑在家里时的典型用法：家里走 `http://192.168.1.10:8080`，出门走 `https://hub.example.com`。浏览器把它们当成两个毫无关系的站点，hub 能处理一部分，另一部分是同源策略决定的，绕不过去。

hub 这边：

- cookie 的 `Secure` 按请求协议决定（`ANYCON_PUBLIC_URL` 是 https 时始终带），不设 `Domain`，各 origin 各自登录一次
- WebSocket 的 Origin 校验接受 `ANYCON_PUBLIC_URL` 和请求自身的 Host。Host 只用于这个校验和 cookie 属性，**不用于生成任何链接**，避免 Host 头投毒
- 私有网段（RFC 1918）和 localhost 的 http 请求自动放行，不需要配置项
- `ANYCON_PUBLIC_URL` 仍是公网域名：OAuth 回调、配对链接、WebAuthn RP ID 只用它

浏览器那边，hub 改不了：

- **IndexedDB 按 origin 隔离**。Web 客户端的 Noise 静态密钥和本地缓存都在里面，所以同一个浏览器在两个 origin 上是两台设备，要配对两次，设备列表里出现两条。设备名要带上 origin 以便区分
- passkey、GitHub、Google 只在 https 公网域名可用，局域网上用 token 或邮箱验证码
- PWA 安装和 Web Push 订阅按 origin，各自一份

**推荐家里也用公网域名**，一个 origin 一切自然；手机在家连 wifi 时直接用公网域名即可，经隧道绕一圈几十毫秒。局域网 IP 适合三种情况：没有公网域名、断网、首次设置。

## 4. 数据归属

这是整个设计的核心表格：

| 数据 | hub | daemon | 客户端设备 |
|---|---|---|---|
| 账号（邮箱、OAuth 身份 ID、passkey 公钥、登录 token 哈希）、设备列表、路由表 | ✅ | — | — |
| 离线暂存的密文（仅当开启队列） | ✅ 密文 | — | — |
| 配额计数、连接审计（元数据） | ✅ | — | — |
| 会话历史、消息、附件 | ❌ | ✅ SQLite | 本地缓存（IndexedDB，明文） |
| 搜索索引 | ❌ | ✅ FTS5 | — |
| Noise 静态密钥 | ❌ | ✅ 自己的 + 已配对客户端公钥 | ✅ 自己的 |
| 节点口令（只存 Argon2id 派生值）、接入失败计数 | ❌ | ✅ | ❌ 不保存 |
| 明文内容 | ❌ 不接触（前提见 § 5.6：客户端代码未被 hub 篡改） | ✅ | ✅ |
| Web 客户端代码（JS / wasm） | ✅ 下发 | — | 浏览器执行；能读到该 origin 下的私钥和明文缓存 |
| 预览（ADR-39） | label → 节点、令牌哈希 | label → 端口、票据（含密钥） | 预览 origin 的 IndexedDB 里存着令牌和票据；dev server 的内容只在隧道两端 |

**推论：**

- hub 数据库泄露 = 泄露「账号邮箱、谁有几台设备、什么时候在线、流量多少」，没有任何对话内容、代码、密钥。passkey 存的是公钥，OAuth 不存 token
- daemon 的数据目录是会话历史的**唯一副本**，备份是用户自己的事（导出 / 导入命令 `anycon export` 在 v0.2）
- daemon 离线时，客户端只能看到本地缓存过的历史，看不到全部。这是刻意接受的代价
- 客户端本地缓存是明文，在用户自己的设备上，这是合理的

**hub 对 payload 的纪律**：外层信封之外的所有字节都是不透明 blob。不解析、不索引、不写日志。这条纪律是整个隐私承诺的基础。

**hub 能看到的元数据**（说「hub 看不到内容」时，指的是这张清单之外的东西）：账号标识（v0.1 是管理员 / 受邀者的显示名；之后的邮箱、OAuth 身份）、每台设备的 id / 名称 / 类型 / 平台 / 版本 / origin、谁和谁配对、什么时候在线、来自哪个 IP；每个信封的 `from` / `to` / `kind` / `seq` / 大小 / 时间；`notify` 信封明文的 `category` 和不透明的 `session_id`；账号绑定的邮箱、每台浏览器的推送订阅（推送服务的 endpoint 和加密公钥）、通知偏好（[ADR-37](decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)）；预览（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)）：每台节点有哪些预览 label（不知道对应哪个端口）、谁什么时候申请了预览令牌、预览连接什么时候连上断开以及它们的流量。配对时 hub 只见到 `match_id`，且不记录。接入时 hub 见到谁在什么时候对哪台节点发起、有没有完成，见不到接入码、节点口令和二维码里的密钥。流量大小和时间模式本身也是信息，anycon 不做流量混淆。

## 5. 加密模型

### 5.1 密钥

每个 daemon 和每个客户端设备各有一对 Noise 静态密钥（X25519）。

- daemon：存在数据目录，文件权限 0600
- Web：IndexedDB，按 origin 隔离（见 § 3.3）
- CLI / SDK：各自的配置目录

**没有账号级密钥，没有恢复短语，没有跨设备密钥同步。** 密钥持有方是 daemon，客户端设备全丢了就在 daemon 所在机器上重新 `anycon pair`；设过节点口令的话，在任何设备上登录账号、用口令接入也行（§ 5.2.1）。历史仍在。

### 5.2 配对（首次）

1. 在 daemon 机器上 `anycon pair`，或在任一已配对客户端上点「添加设备」，daemon **在本地**生成一个短码（默认 16 位，如 `7K3P-Q2MX-4FWD-H8RT`，约 79 bit 熵，5 分钟有效；`anycon pair --short` 给 8 位、约 40 bit，只适合在自建 hub 上手输）。短码**从不发给 hub**
2. 新客户端输入短码，或打开 `https://<hub>/pair#code=…` 链接（短码在 URL fragment 里，浏览器不会把它发给 hub，反代的访问日志里也没有；未登录时的跳转也只把它留在浏览器本地）。两端各自对短码做一次 Argon2id（64 MiB、3 轮，约 0.1–1 s），输出的一半是 `match_id`、另一半是 PSK。hub 只凭 `match_id` 把两条连接撮合到一起，之后只做转发
3. 两端执行 **Noise XX + PSK**：XX 模式带内交换静态公钥；PSK 来自上一步。hub 手里只有 `match_id`，没有短码，不猜中短码就做不了中间人
4. 握手成功、hub 发出 `pair_done` 之后，daemon 经刚建立的加密传输再发一条 `pair.ack`（daemon 与新设备的正式 id、节点名、公钥指纹）。新客户端校验这些都与自己看到的一致才落盘，30 秒内没收到就失败、不落盘。hub 能伪造 `pair_done`，伪造不了 `pair.ack`
5. 双方记住对方静态公钥。hub 记录「设备 A 和设备 B 已配对」用于路由，但不持有任何密钥。已有同 id 的节点但公钥不同时，新客户端拒绝覆盖并提示

**残余风险（[ADR-29](decisions.md#adr-29-短码不经过-hub)）**：PSK 的熵就是短码的熵，而 `match_id` 是短码的确定函数、hub 在配对开始时就拿到它。恶意 hub 可以**离线**穷举短码直到 `match_id` 对上，猜中就有 PSK，就能对这次配对做完整的中间人（`pair.ack` 挡不住：它用的就是那把 PSK）；它甚至可以提前为所有 8 位短码预计算一张表。Argon2id 让每次猜测都要 64 MiB 内存和约 0.1 s，8 位短码的全空间约 2^40 次，一张 GPU 量级是数年；16 位（2^79）不可行。因此：

- 经**自己的 hub** 配对（自建）：没有这个威胁，`--short` 的 8 位短码也够用
- 经**不信任的 hub**（比如公共 hub）配对：用默认的 16 位短码（链接、二维码、Web「添加设备」都是它）。不要用 `--short`
- 被动窃听者（拿不到临时私钥）不能用握手记录验证猜测；这个风险只针对主动的 hub
- 彻底消除要换成 PAKE。[ADR-36](decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门) 为账号内接入引入了 CPace（§ 5.2.1），之后由 6 位接入码取代 `--short`，这条残余风险随 8 位短码一起去掉

在线猜测（往 hub 上不停 `pair_join`）受 hub 限速（10 次 / 分钟 / IP）和短码一次性、5 分钟有效约束。

### 5.2.1 账号内接入

短码配对要求手边有 daemon 机器或一台已配对设备。公共 hub 的用法是随时随地换设备，所以另有一条路（[ADR-36](decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)）：**登录账号 → 看到自己名下的节点 → 解锁其中一台**。原则是两层各自验证：

- **账号**由 hub 验证，只决定请求能不能送到这台 daemon 门口。账号登录本身不授予任何节点的访问权
- **能不能成为这台 daemon 的设备**由 daemon 验证。hub 不经手任何秘密

解锁有三种方法，选一种：

| 方法 | 用户做什么 | 秘密 | 适用 |
|---|---|---|---|
| 扫码批准 | 新设备显示二维码，用已配对的手机在 Web UI 里扫一下，确认 | 新设备生成的 256 bit 随机数，在二维码里，经已配对设备的加密通道交给 daemon | 手机在手边 |
| 接入码 | 新设备点「请求批准」；已配对设备收到推送，点批准后显示 6 位码；在新设备上输入。也可以在已配对设备上「添加设备」时直接生成 | daemon 生成的 6 位一次性码，只交给批准的那台设备 | 手机在手边但不方便扫码 |
| 节点口令 | 输入给这台节点设的口令 | 口令；daemon 只存 Argon2id 派生值 | 手边什么已配对设备都没有 |

三种方法的握手后半段都是 § 5.2 的 Noise XX + PSK 和 `pair.ack`，只差 PSK 的来源：扫码批准直接用二维码里的随机数；接入码和节点口令是低熵秘密，双方先跑一轮 **CPace**（PAKE），输出当 PSK。PAKE 的性质是：hub 即使主动参与，每次在线交互至多验证一个猜测，拿不到能离线穷举的东西；而在线猜几次由 **daemon** 决定——接入码每个最多 3 次，节点口令连续失败满 3 次后指数退避、连续 10 次停用，要在本机或已配对设备上重新启用。每次接入成功、口令连续失败、被锁定、口令被改动，所有已配对设备都会收到通知。

节点口令默认没有。`anycon init` 结束时和 Web 的 `/setup` 向导配对完成后都会提示设置，之后在本机 `anycon passwd` 或已配对设备上改。它不是账号密码，客户端也从不保存它。

不能用接入的场景：daemon 的首次身份（还不属于任何账号，第一台设备只能在机器旁边配），以及不登录账号的 SDK。这两种仍走 § 5.2 的短码。

### 5.3 重连

已配对的双方用 **Noise IK**，一个往返建立会话。每次 WebSocket 重连都重新握手，前向安全。

### 5.4 多 daemon

客户端和每台 daemon 单独配对，UI 按节点展示。

### 5.5 吊销

从任一已配对设备吊销另一台设备：

- daemon 删掉该设备公钥 → 该设备再也握不上手
- hub 删掉该设备路由 → 它的连接被断开

两边都要删，缺一个都不干净。daemon 换机器或重装时旧密钥作废，所有客户端需重新配对，UI 要明确提示。

被吊销的客户端再次连接会看到「需要重新配对」；它用新短码配对或重新接入（§ 5.2.1）即可（daemon 的公钥没变时，客户端那份旧的节点记录被原样复用，不算「公钥冲突」）。浏览器上有两种退出：**退出登录**只清内存里的登录态，本地的 Noise 私钥、device token 和明文缓存**保留**在 IndexedDB 里；**退出并移除本设备**会先吊销这台设备（hub 和 daemon 两边），再清空 IndexedDB。公用 / 借来的电脑用后者。

### 5.6 威胁模型

先说结论，避免过度解读「端到端加密」：

- E2E 保护的是**会话内容在 hub 上不可见、转发过程中不可改**。hub 看得到元数据（§ 4）
- **Web UI 的代码由 hub 下发**，所以用 Web UI 的人信任的是「这个 hub 的运营者不会下发恶意代码」。用 CLI / SDK 没有这个前提
- 配对经不信任的 hub 时，8 位短码（`--short`）有被 hub 离线穷举的残余风险（§ 5.2），用默认的 16 位

| 威胁 | 防护 / 现状 |
|---|---|
| hub 运营者窥探会话内容 | E2E：Noise 密钥只在 daemon 和客户端手里，hub 无密钥。**限定**：用 Web UI 时见下一行 |
| **hub 运营者（或入侵者）下发恶意 Web 代码** | **协议层无法防护**，见下面「Web 客户端代码的信任问题」。缓解：自建 hub；敏感场景用 CLI / SDK；CSP；钉发布版本。 |
| hub 被入侵 / 数据库泄露 | 同上，只泄露元数据；被入侵的 hub 同样能下发恶意 Web 代码 |
| hub 在配对时做中间人 | 短码由 daemon 本地生成、不经 hub；`match_id` 和 PSK 由短码经 Argon2id 派生，hub 只拿到 `match_id`；新客户端要等经加密传输送达的 `pair.ack` 才落盘。残余：hub 可以拿 `match_id` 离线穷举短码，Argon2id 把 8 位短码的代价推到数年 GPU、16 位不可行；经不信任的 hub 配对不要用 `--short` |
| hub 在配对时只伪造 `pair_done`（不知道短码） | 新客户端得不到匹配的 `pair.ack`，配对失败且不落盘；已有节点的公钥不会被静默覆盖 |
| hub 在接入时做中间人，或猜接入码 / 节点口令 | 扫码批准的 PSK 是 256 bit 随机数，不经 hub。接入码和节点口令走 CPace：hub 每次在线交互至多验证一个猜测，没有可离线穷举的东西；猜几次由 daemon 定（接入码每个 3 次；口令连续失败 3 次后指数退避，连续 10 次停用）。hub 的限速只防滥用，不是这里的边界 |
| 诱导用户批准攻击者的设备 | 接入请求里新设备自报的名称和平台未经认证，批准界面写明「只批准你眼前这台设备」；接入码只显示在批准的那台设备上，提示「不要告诉任何人」；二维码内容不是链接，只有 Web UI 内的扫码能读，做不成「点链接就批准」 |
| 节点口令泄露（公共 hub 的 Web UI、钓鱼页面） | Web UI 里输入的口令，hub 下发的 JS 能拿到——和设备私钥同一个信任前提（下面「Web 客户端代码的信任问题」）。口令挡不住钓鱼，同步 passkey 接入（路线图）才防。客户端不保存口令；怀疑泄露就改口令，每次接入成功都有通知 |
| 被动网络窃听 | TLS（用户反代）+ Noise 双层 |
| 客户端设备丢失 / 公用电脑 | 从其他设备吊销。浏览器里的 Noise 私钥、device token 和会话缓存是**明文**放在 IndexedDB 的，「退出登录」不删它们；公用电脑要用「退出并移除本设备」。建议设备本身开磁盘加密 |
| daemon 机器被入侵 | 不在范围内——它本来就是跑 agent 的机器 |
| 已配对设备滥用权限 | 已配对设备能让 agent 在 daemon 机器上干活，权限就是 agent 的权限，只受审批和 workspace 边界约束，不受 OS 沙箱约束。危险权限模式受 `permissions.allow_dangerous_modes` 约束，会话重启 / resume 时重新检查；会话级 `env` 默认被拒（`permissions.allow_session_env = false`，[ADR-31](decisions.md#adr-31-会话-env-默认关闭)），开启后仍有黑名单；`set_agent_defaults` 走同样的校验 |
| 提示注入的 agent 借浏览器外泄数据 | Markdown 里的远程图片默认不加载（点击才加载）；hub 给 Web UI 加 CSP（`connect-src 'self'`、`img-src 'self' data: blob:`、`frame-ancestors 'none'`），页面不能向第三方发请求。agent 自己的网络请求走权限审批 |
| 预览页里的 dev app 攻击 Web UI | dev app 是 agent 写的任意代码。它跑在独立的预览 origin 上（另一个可注册域名，hub 启动时检查），读不到 Web UI 的 IndexedDB 和 cookie；预览运行时和 Web UI 分开构建、分开嵌入，Web UI 的 origin 上注册不了预览的 service worker；内嵌预览的 iframe 不给 `allow-top-navigation`，Web UI 不处理来自它的消息（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)） |
| 经预览访问 daemon 机器上的其他服务 | daemon 只转发到回环地址，默认只放行会话自己起的服务监听的端口，其余端口要在那台机器上写进 `preview.allow_ports`；本机 hub 的端口永远不行。票据由 daemon 签发、用 Noise PSK 校验，hub 伪造不了；hub 的预览令牌另外保证只有和这台节点配对过的设备能申请。残余：预览 origin 里的令牌和票据对 dev app 的 JS 可见，拿到它们的人在过期前能访问这个端口 |
| 账号被盗 | 盗号者能看元数据、能发起接入请求，但成为设备需要短码（只能从已配对设备或 daemon 机器获得）、已配对设备的批准，或节点口令，三样都不经 hub（§ 5.2.1）。他能做的坏事：用接入请求反复推送骚扰你、把节点口令接入试到锁死；已配对设备不受影响，每次都有通知 |
| 邮箱或 GitHub / Google 账号被盗 | 等同账号被盗，权限同样小 |
| 局域网明文 HTTP 下 token 或 cookie 被嗅探 | 同网段攻击者能拿到登录态，权限同账号被盗；会话内容仍是 Noise 密文。介意就家里也走 https 公网域名（§ 3.3） |
| 用未验证邮箱的 OAuth 身份冒领他人账号 | 只有 provider 声明已验证的邮箱才自动关联，否则先验证码确认（§ 3.2） |
| 验证码暴力破解 / 邮件轰炸 | 6 位码 10 分钟、错 5 次作废；按 IP 和邮箱限速。限速依赖真实 IP，`ANYCON_TRUSTED_PROXIES` 配错就失效 |
| OAuth 回调 CSRF / 授权码注入 | `state` + PKCE |

「账号被盗」那几条值得强调：**账号本身的权限很小**。账号能做的是管理设备列表和看元数据；能和 daemon 说话的是设备，不是账号。账号内接入（§ 5.2.1）不改变这一点：账号只能把请求送到 daemon 门口，开门要已配对设备的批准或节点口令。这也是登录门槛可以比一般 SaaS 低、不做密码和二次验证的原因。

还有一条相反方向的边界：账号里的节点不一定都是你的。攻击者可以用自己的 daemon 生成一条配对链接发给你，你登录着点开，如果页面自动开始配对，他的节点就被静默加进了你的账号和浏览器（hub 对「没有账号的新 daemon」就是归入 joiner 的账号）。所以从 URL 带进来的短码，Web UI 先显示确认步骤，点了才开始配对。

#### Web 客户端代码的信任问题

浏览器里跑的 JS 和 Noise wasm 是**打开页面时由 hub 下发的**。Noise 的静态私钥、device token 和会话缓存都在这段代码所在 origin 的 IndexedDB 里，代码能读到它们。所以：

- 如果 hub 的运营者（或者入侵了 hub 的人，或者在你和 hub 之间终止 TLS 的 CDN / 隧道，比如 Cloudflare Tunnel）想读你的会话，他不需要破解 Noise，只要给你一份改过的 JS，让它把私钥或解密后的明文发走。你会毫无察觉
- 这是「服务器下发前端代码」这类方案的固有限制，不是实现上的疏漏，E2E 在这里保护不了你。**用公共 hub 的 Web UI = 信任公共 hub 的运营者；自建 hub 的 Web UI = 信任你自己**
- CLI 和 SDK 不受影响：它们的代码是你从发布渠道装的，hub 不下发任何可执行代码

现在能做的缓解：

1. **自建 hub**，Web UI 和 hub 同源同主。隐私敏感的使用推荐这样做
2. **用 CLI / SDK**。对公共 hub，隐私敏感的操作走 `anycon connect`，不走浏览器
3. **CSP**：hub 给 Web UI 下发严格的 `Content-Security-Policy`（`default-src 'self'`；`script-src 'self' 'wasm-unsafe-eval'`，不允许内联脚本；`connect-src 'self'`；`img-src 'self' data: blob:`；`frame-ancestors 'none'`，完整策略见 [hub.md § 2.1](spec/hub.md#21-静态与健康)）。它挡的是被注入内容、第三方脚本和外泄通道，**挡不住 hub 自己下发的恶意脚本**，别把它当成这个问题的解
4. **钉发布版本**：自建时钉镜像 tag / digest，升级前读 changelog；PWA 的新版本不会自动生效，要用户确认刷新，这是缓冲，不是边界（service worker 本身也来自 hub）
5. 反代 / CDN 同样在这条路径上：它们终止 TLS，能改 JS。用 Cloudflare 这类服务时，信任范围包括它

后续可以做、但 v0.1 没做的：**签名的发布包和 SRI**——发布包由项目签名，浏览器端的外壳（浏览器扩展、桌面壳，或钉住版本的 PWA 壳）按 SRI 校验它加载的 JS / wasm，代码不再由 hub 现场决定；以及给自建 hub 的运维一个「校验 `web/dist` 与官方 release 哈希一致」的命令。要说清楚的一点：**光在 hub 自己下发的 `index.html` 里写 SRI 没用**——HTML 和子资源来自同一个 hub，SRI 只防 CDN 篡改子资源，防不了下发 HTML 的那一方；SRI 要配合独立于 hub 的分发渠道才有意义。见 [ADR-32](decisions.md#adr-32-如实声明web-客户端代码由-hub-下发)。

## 6. Workspace 模型

### 6.1 文件夹即项目

daemon 配置一个 workspace 根目录（v0.1 只支持一个，多根是后续配置项）。根目录下**每个一级子文件夹就是一个项目**：

- 不需要注册。用户 `mkdir` 一个文件夹，客户端几秒内看到
- 元数据全部推导：是否 git 仓库、当前分支、remote、是否有未提交改动、最近修改时间。不存数据库，不会有幽灵项目
- 沙箱边界天然存在：agent 的 cwd 在根目录内（关联项目是关联目录本身），cwd 外的写操作由 agent 的权限模式决定是否审批

另外可以把本机**已有的目录**关联为项目（ADR-33）：客户端在选择器里挑一个 `workspace.link_roots`（默认 `~`）之内的目录，daemon 记下它的路径，不移动、不复制；解除关联也不动文件。关联项目的边界是那个目录本身。

### 6.2 客户端的项目操作

这些是 daemon 原生操作，不经过 agent，不弹 agent 权限审批，但进审计日志：

| 操作 | 实现 |
|---|---|
| 新建空项目 | `mkdir` + 可选 `git init` |
| 关联已有目录 | 记下路径（`linked_projects` 表），选择器经 `workspace.browse` 列目录名 |
| 从 Git 克隆 | daemon 执行 `git clone`，凭证用机器上已有的 ssh key / credential helper，daemon 自己不碰凭证 |
| 会话的 worktree | 新建会话时可选：`git worktree add` 到 `<项目>/.anycon/worktrees/<slug>`，分支 `<前缀>/<slug>`；同步 base、只读 diff、归档时清理（§ 6.5） |
| 重命名 | `mv` |
| 删除 | 移到 `<root>/.trash/`，不 `rm -rf`；关联项目只解除关联 |
| 从模板创建 | 后续：clone 模板仓库再删 `.git` |

克隆这类长操作返回 job id，进度走流式通道。

### 6.3 临时会话

不绑定项目的会话在 `<root>/.scratch/<session-id>/` 下运行，适合「帮我写个脚本试试」。产出满意可一键转成项目（`mv`）。

### 6.5 每会话 worktree

ADR-34：git 项目里新建会话时可以选「新 worktree」，daemon 在项目里面建 `.anycon/worktrees/<slug>` 和一条新分支，agent 的 cwd 就是它，几个会话并行不互相踩。worktree 在项目边界之内，路径复核和关联项目一样；`.worktreeinclude` 列出的被忽略文件（`.env` 之类）只复制、不执行脚本。会话页能看相对 base 的只读 diff、同步 base；提交和开 PR 让 agent 做（走审批），daemon 只读 PR 状态。归档时干净的 worktree 被删掉、分支保留，有未提交改动的保留。细节见 [spec/daemon.md § 10.1](spec/daemon.md#101-每会话-worktree)。

### 6.4 fs watch

只 watch 根目录这一层，只关心项目的增删改名。不监听项目内部——`node_modules`、`.git/objects` 会产生海量事件。macOS 用 FSEvents，Linux 用 inotify，通过 `notify` crate 统一。

## 7. 会话与 adapter

### 7.1 会话

每个会话绑定：一个 daemon、一个项目路径（或 scratch）、一个 agent。daemon 维护并发上限。会话事件流（文本增量、工具调用、权限请求、完成、错误）持久化到 SQLite，客户端连上时按 seq 拉增量。

### 7.2 adapter trait

```rust
#[async_trait]
trait Adapter {
    fn capabilities(&self) -> Capabilities;   // 是否支持 resume、权限交互、附件等
    async fn spawn(&self, cwd: &Path, prompt: &str) -> Result<AgentHandle>;
    async fn resume(&self, cwd: &Path, agent_session_id: &str) -> Result<AgentHandle>;
}

trait AgentHandle {
    fn events(&mut self) -> impl Stream<Item = AgentEvent>;
    async fn send(&mut self, input: UserInput) -> Result<()>;
    async fn respond_permission(&mut self, id: &str, decision: Decision) -> Result<()>;
    async fn cancel(&mut self) -> Result<()>;
}
```

所有目标 agent 都有结构化事件流，**不走 PTY**。adapter 的语义对齐 ACP，使 ACP agent 零成本接入。

### 7.3 v0.1 的 adapter

| agent | 驱动方式 |
|---|---|
| Claude Code | `claude -p --output-format stream-json --input-format stream-json`，双向 JSONL |
| Codex | `codex app-server` JSON-RPC（`codex exec` 在无头模式下不询问审批，不用） |
| OpenCode、Pi、其他 ACP agent | ACP v1 client adapter |
| 自定义 | Python / TS SDK，agent 本身就是一个 daemon 节点 |

各 CLI 的具体接口变化很快，实现前以各自当时的文档为准。每个 adapter 还要实现 `probe()`：探测是否安装、版本、认证状态、模型列表（含每个模型的思考等级）、权限模式集合，细节见 [spec/daemon.md § 5](spec/daemon.md#5-agent-探测与能力声明)。

## 8. 通知

hub 看不到会话内容，但要能推「agent 在等你审批」。做法：

- daemon 发 `notify` 信封，**外层只带一个类别枚举**：`permission_requested` / `turn_finished` / `error` / `session_idle`，以及账号内接入的 `enroll_requested` / `security`（这两类不带会话 id，§ 5.2.1）
- 详情在内层密文里
- hub 把类别转成 Web Push；配置了 SMTP 时也可发邮件（后续：Telegram）
- 用户点开后客户端解密看详情

泄露面是一个类别词，加一个不透明的随机会话 id。

## 9. 离线行为

| 场景 | 行为 |
|---|---|
| daemon 离线，客户端发消息 | hub 丢弃并回 `undeliverable`；消息留在客户端本地标记「未发送」，可编辑可删除；`peer_online` 后客户端自动重发 |
| daemon 离线，客户端传附件 | 客户端不发起，提示节点离线 |
| daemon 回来，短时间内收到多条 `session.send` | 同一会话内在 agent 开始处理前到达的消息**合并成一个 turn**，避免 N 倍 token 开销和「做了又撤回」 |
| 客户端离线，daemon 有事件 | daemon 本地持久化；客户端回来按 seq 拉增量 |

默认 hub **不暂存任何消息字节**（只有账号和设备表）。管理员可选开启 hub 侧离线队列（见 [protocol.md § 7.2](protocol.md#72-可选hub-离线队列)），让手机发完锁屏后 Mac 醒来也能收到；代价是 hub 要暂存密文，所以默认关闭。
