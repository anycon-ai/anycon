# 决策记录

每条记录：做了什么决定、为什么、否掉了什么。新决定追加在末尾，推翻旧决定时在旧条目加「已被 ADR-N 取代」而不是删除。

---

## ADR-1 定位：连接层，不是开发环境

**决定**：anycon 是 agent 的连接层（"Tailscale for agents"），不做 IDE、终端、diff、PR、issue 看板、worktree 编排。

**原因**：完整的 agent 开发环境已经有成熟的开源方案，正面竞争没有意义。「薄、协议优先、agent 无关」这个定位目前没有人把它做成核心。

**否掉**：做成大而全的开发环境。

**已被 [ADR-34](#adr-34-web-ui-是开发者客户端体验对齐-claude-桌面版每个会话可以有自己的-worktree) 部分修订**：「薄」只约束连接层（hub + 协议）；Web UI 面向开发者，体验对齐 Claude 桌面版——每会话 worktree、只读 diff、同步 base、经 agent 开 PR。终端、远程 shell、编辑器、经 hub 反代 dev server 仍然不做。

---

## ADR-2 自建优先，公共托管为辅

**决定**：鼓励自建。anycon.ai 提供有资源限制的公共 hub 作为试用入口，不以盈利为目标。

**原因**：目标用户是开发者，自己有服务器。公共 hub 降低第一次上手门槛。

**否掉**：以 SaaS 为主的模式；带「禁止他人托管同名服务」等附加条款的开源协议。选 Apache-2.0。

---

## ADR-3 容器不含 TLS / 反代

**决定**：hub 容器只监听明文 HTTP 端口。ACME、nginx、域名由用户处理。

**原因**：用户已有反代方案，内置会冲突。保持容器单一职责。

**否掉**：内置 ACME（Caddy 式）。曾考虑过，后放弃。

---

## ADR-4 一个 hub 二进制，两种模式

**决定**：`single` 和 `multi` 是同一份代码的配置项。数据模型从第一天是多租户形态。

**原因**：避免维护两套代码；公共 hub 跑的和用户自建的是同一份 release，本身就是信用。

---

## ADR-5 会话历史存 daemon，hub 只中继

**决定**：hub 不存任何内容，只存账号、设备、路由和密文队列。历史、附件、搜索索引都在 daemon 的 SQLite 里。

**原因**：
- hub 手里没有值得偷的东西，公共托管的法律与运维风险最小
- E2E 的复杂度大幅下降：密钥持有方是 daemon，不需要账号级密钥、恢复短语、跨设备密钥同步
- 目标用户的机器本来就常开，历史放家里合理

**代价**：daemon 离线时客户端只有本地缓存，看不到全部历史。文档明确写出。

**否掉**：
- hub 存明文历史——功能最全（服务端搜索、看板、多端同步都容易）但公共托管时 hub 持有用户的代码和密钥
- hub 存密文历史 + 账号级密钥——曾作为方案讨论，需要扫码传密钥 + 恢复短语两套机制，复杂度过高

---

## ADR-6 E2E 在 v0.1 就做

**决定**：Noise 协议端到端加密从 v0.1 开始，不做可选。

**原因**：在 ADR-5 的架构下 E2E 的增量成本很低（一次配对握手）；公共 hub 要开，没有 E2E 不敢让人用；「hub 看不到内容」是最容易讲清楚的卖点。（精确表述和例外——Web 代码由 hub 下发、hub 看得到元数据——见 [ADR-32](#adr-32-如实声明web-客户端代码由-hub-下发)。）

**曾否掉又捡回来**：中间一度决定 v0.1 不做 E2E、只做「payload 当不透明 blob」的纪律，理由是 hub 存历史模式下 E2E 太重。换成 daemon 存历史后重新评估，成本降到可接受。

---

## ADR-7 两层协议

**决定**：外层信封（hub 可见，MessagePack）+ 内层 JSON-RPC 2.0（Noise 密文）。

**原因**：hub 的可见性边界就是分层边界；内层随便改 hub 不用发版；JSON-RPC 的 request/response/notification 三种语义正好够用，不自己发明信封。

**否掉**：自定义 RPC 格式；gRPC（浏览器支持差，且 hub 不需要理解内层）。

---

## ADR-8 配对用 Noise XX + PSK，重连用 IK

**决定**：首次配对 `Noise_XXpsk3`，PSK 由带外短码派生；重连 `Noise_IK`。

**原因**：XX 允许带内交换公钥，psk3 让没有短码的 hub 无法完成握手，从而无法做中间人。IK 一个往返，重连快。（短码的来源和残余的离线穷举风险见 [ADR-29](#adr-29-短码不经过-hub)：最初的实现里短码由 hub 生成，这句话当时并不成立。）

**否掉**：
- 账号级身份密钥（见 ADR-5）
- 自己拼 x25519 + chacha——用 snow，不手写握手
- TLS 客户端证书——浏览器体验差

---

## ADR-9 文件夹即项目

**决定**：daemon 的 workspace 根目录下每个一级子文件夹就是一个项目，不需要注册。客户端可直接新建、克隆、重命名、回收站。

**原因**：「先在机器上打开/注册项目才能远程操作」是明确不想要的体验——用户在手机上应该能从零克隆一个仓库并开始工作，全程不碰那台机器。推导元数据而不是存储，不会有幽灵项目。沙箱边界天然存在。

**否掉**：项目注册表；允许 workspace 外的任意路径。

**已被 ADR-33 部分修订**：根目录下的文件夹仍然不需要注册；另外允许把 `workspace.link_roots` 之内的已有目录关联为项目。

---

## ADR-10 Monorepo

**决定**：Rust workspace + bun workspace + uv workspace 共存于一个仓库。官网和社区 adapter 单独仓库。

**原因**：协议类型是唯一真源，所有消费者必须和它在一起；hub 与 daemon 版本耦合，一次 tag 出所有二进制；兼容矩阵在一个 CI 里跑。同类的多组件项目普遍采用 monorepo。

**否掉**：按语言或组件拆仓库。

---

## ADR-11 axum 而非 actix-web

**决定**：axum。

**原因**：tower 中间件生态（限速、超时、压缩、trace）对公共 hub 直接可用；tokio 团队维护；生态示例默认按 axum 写。性能差距对本项目无意义。

---

## ADR-12 sqlx 双后端，trait 抽象

**决定**：`Store` trait，`SqliteStore` 和 `PostgresStore` 两个实现，两套 migration。v0.1 只实现 SQLite，trait 从第一天就有。

**原因**：公共 hub 可能需要 Postgres；`sqlx::Any` 没有编译期检查且功能受限。

**否掉**：`sqlx::Any`；只做 SQLite；sea-orm（抽象层用不上）。

---

## ADR-13 Web UI 不依赖 TS SDK

**决定**：Web UI 和 `@anycon/sdk` 只共享 `packages/protocol`（类型 + 编解码），各自写传输和状态层。

**原因**：避免 SDK 的 API 被 UI 需求牵着走。共享类型足以防止协议漂移。

**否掉**：Web UI 用 SDK 作为唯一通信层。

---

## ADR-14 自己写组件，行为层用 Radix

**决定**：视觉组件自己写，Tailwind 样式，Radix Primitives 承担可访问性行为。

**原因**：不喜欢 shadcn 的视觉；可访问性细节（焦点陷阱、键盘导航、定位）不值得自己踩坑。

---

## ADR-15 Python ≥ 3.12

**决定**：`requires-python = ">=3.12"`，CI 跑 3.12 / 3.13 / 3.14。

**原因**：3.10 于 2026-10 EOL；3.11 的 TaskGroup / ExceptionGroup / tomllib 要用；3.12 新泛型语法，生命周期到 2028-10；目标用户用 uv，系统 Python 版本不构成门槛；abi3 wheel 一份覆盖所有版本。

---

## ADR-16 crypto 一份实现，三种产物

**决定**：`crates/crypto` 编译为 Rust crate、wasm、Python wheel。

**原因**：Noise 握手三端都要跑，一份实现握手永远对得上。

**否掉**：TS 用 `@noble/*`、Python 用 `pynacl` 各写一遍——省构建复杂度但多两处可能出错的地方。

---

## ADR-17 daemon 与 CLI 合一

**决定**：`anycon` 一个二进制，`anycon daemon` 启动守护进程。

**原因**：用户少装一个东西；daemon 和 CLI 共享配置、密钥和协议代码，分开反而要处理两者的版本一致性。

---

## ADR-18 不走 PTY

**决定**：所有 adapter 走 agent 的结构化事件流（JSONL / JSON-RPC / ACP），不模拟终端。

**原因**：目标 agent 全部提供结构化输出；PTY 会把 anycon 拖向终端模拟器，违反 ADR-1。

---

## ADR-19 私有开发期要短

**决定**：仓库在 v0.1 happy path 跑通、基础文件齐全后即转 public，不等稳定版。公开与宣发分开。

**原因**：核心卖点是可信可自建，代码早公开本身是信用；安全设计越早被挑毛病越好；早期 issue 是最便宜的需求验证。

---

## ADR-20 积压消息合并为一个 turn

**决定**：daemon 重新上线后，同一会话内积压的 `session.send` 合并成一条用户消息交给 agent，只触发一个 turn。离线队列限制改小（50 条 / 256 KiB / 1h），定位为扛短暂中断。

**原因**：逐条喂给 agent 的 token 开销是 N 倍，且「先做 X」「算了」这种序列会被真的执行。合并后 agent 看到完整语境，自然处理撤回与重复发送。成本落在用户的 LLM 账单上，不是 hub 上，所以必须在 daemon 侧解决，与队列放在哪无关。

**否掉**：
- 逐条交付——token 浪费
- 取消 hub 队列、改为客户端本地排队——hub 更薄，但手机发完锁屏后 Mac 醒来也不会有动作，牺牲了「随时调用」的体验（**此项已被 ADR-21 部分推翻**：客户端本地排队成为默认，hub 队列降为可选）
- 大队列、长 TTL——队列不是为长时间离线设计的，积压越多合并后的 turn 越大

---

## ADR-21 hub 离线队列默认关闭

**决定**：hub 默认不暂存任何信封。对端离线时回 `undeliverable`，消息留在客户端本地标记「未发送」，对端上线后客户端自动重发。hub 侧队列保留为可选功能（`ANYCON_QUEUE_ENABLED`），公共 hub 不开启。

**原因**：
- 「手机发完锁屏、Mac 醒来自动执行」这个场景实际需求很小，不值得为它让 hub 带存储
- 关闭后 hub 不碰任何消息字节，隐私承诺从「只暂存密文」变成「连密文都不存」
- 用户发消息时看得到节点离线状态，自己决定等还是走
- 合并 turn 的逻辑（ADR-20）对客户端重发的突发消息同样生效，token 问题不因队列关闭而回来

**取代**：ADR-20 中「否掉客户端本地排队」的部分。

---

## ADR-22 账号登录方式与身份模型

> 其中密码、TOTP、二次验证和 `single` 模式登录集合的部分已被 ADR-23 取代；公共 hub 登录集合、身份模型、关联规则、SMTP 仍有效。

**决定**：

- 公共 hub（`multi`）的登录方式：**邮箱验证码、GitHub、Google、passkey**。不提供密码
- 自建 hub（`single`）：密码 + passkey/TOTP、passkey 免密登录；配置了 SMTP / OAuth 后同样可用邮箱验证码、GitHub、Google（密码 + TOTP 已被 ADR-23 取代为 token）
- 登录方式由配置决定，不由模式决定。模式只管注册是否开放、配额是否启用、是否允许密码（补充 ADR-4；「是否允许密码」已被 ADR-23 改为「是否允许 token」）
- 二次验证：密码登录**必须**绑定 passkey 或 TOTP；passkey、GitHub、Google 登录不再要求；邮箱验证码登录可选。公共 hub 不默认强制（已被 ADR-23 取代：不再有二次验证）
- 一个账号挂多个身份：`identities(provider, subject, account_id)`，`subject` 是 provider 的稳定用户 ID。表结构从 v0.1 就有，登录流程 v0.2 实现
- OAuth 首次登录的关联规则：按 `(provider, subject)` 匹配；没有则看 provider 是否声明邮箱已验证——已验证且与现有账号邮箱相同就自动关联，未验证先走邮箱验证码
- 邮件只走 SMTP（lettre），不接任何厂商的 HTTP API
- 验证码：6 位数字、10 分钟、错 5 次作废、同邮箱 60 秒一次、按 IP 和邮箱双重限速

**原因**：

- 只有 GitHub 一个入口，不想关联 GitHub、或 GitHub 账号出问题的人就进不来。邮箱是唯一通用的身份，也是合并多个登录方式的主键
- 公共 hub 面向全球用户，Google 是 GitHub 之外覆盖最广的身份。OAuth 框架搭好之后多一个 provider 成本很低
- 账号权限很小（architecture.md § 5.6），登录门槛不必高。GitHub 已经强制 2FA，hub 再强制一次是双重摩擦
- 公共 hub 不要密码：没有密码就没有重置流程、撞库、泄露库比对这些事
- 自建 hub 可能没有邮件服务，密码是零配置的兜底；passkey 则是不想要密码的人的免密路径（已被 ADR-23 取代：零配置兜底改为 token）
- 按 provider 稳定 ID 而不是邮箱或用户名匹配，因为后两者可以改。未验证邮箱自动关联是经典的账号接管漏洞
- SMTP 是所有邮件服务商的公分母——Resend、SES、Postmark 都提供 SMTP 端点；接 HTTP API 等于每家写一份
- 登录方式和模式解耦，`single` 模式的自建用户想用 GitHub 登录也能用，不用为此开 `multi`

**否掉**：

- 只做 GitHub OAuth（原 v0.2 方案）
- 公共 hub 提供密码登录
- 魔法链接（magic link）——跨设备、跨浏览器打开会丢会话，邮件客户端预抓取会把链接消耗掉；验证码在哪里输都行
- 在 OAuth 登录之上再强制 passkey/TOTP
- 解析 Google 的 `id_token`——用 access token 调 userinfo 就够，省掉 JWKS 和 JWT 校验
- 账号级密钥、把登录身份和 E2E 密钥绑定（见 ADR-5，登录和加密是两件事）
- 厂商邮件 HTTP API

---

## ADR-23 token 登录与局域网双 origin

**决定**：

- 删除密码和 TOTP。`single` 模式的零配置登录方式改为 **token**：设置了 `ANYCON_ADMIN_TOKEN` 就用它；没设置则首次启动生成一个，写入 `/data/admin-token`（0600）并打印到日志。它取代原来的「一次性 setup 链接」
- 登录页粘一次 token 换长期 session cookie，之后这个浏览器不用再粘
- 账号设置里可为任何账号生成 token，只展示一次，库里存哈希。配置里的那个只是管理员的 bootstrap token；受邀者也可以走 token
- 丢了所有 passkey 的恢复出口就是改 `ANYCON_ADMIN_TOKEN` 重启。不需要 reset-admin 命令
- token 登录只在 `single` 模式；`multi` 不提供。模式管的三件事变为：注册是否开放、配额是否启用、是否允许 token 登录
- 不再有「二次验证」概念。passkey 是免密登录方式之一，不是第二因素
- `single` 模式下只用 token 的账号可以没有邮箱；邮箱在绑定邮箱验证码或 OAuth 身份时才需要
- **局域网 IP 访问是一等场景**：hub 跑在家里，家里走 `http://<局域网 IP>:8080`，出门走公网域名。两个 origin 都要能用
- hub 侧对双 origin 的处理：cookie 的 `Secure` 按请求协议决定，不设 `Domain`；WebSocket Origin 校验接受 `ANYCON_PUBLIC_URL` 和请求自身的 Host；私有网段和 localhost 的 http 自动放行，不加配置项；`ANYCON_PUBLIC_URL` 仍是公网域名，OAuth 回调、配对链接、WebAuthn RP ID 只用它，Host 头不用于生成任何链接
- 接受的代价：IndexedDB 按 origin 隔离，同一浏览器在两个 origin 上是两台设备，配对两次；passkey、GitHub、Google 只在 https 公网域名可用；PWA 和 Web Push 按 origin
- 文档推荐家里也用公网域名（一个 origin），局域网 IP 作为支持但有代价的方式

**原因**：

- token 是自建工具的通行做法（notebook 服务、家庭自动化网关、各类自托管面板都这么登录）：零依赖、零向导、明文 HTTP 下可用
- 密码只剩「零配置兜底」一个角色，token 做得更好，顺带删掉 argon2、totp-rs、改密码、忘记密码、二次验证规则一整套
- passkey 在局域网 http 下不可用——WebAuthn 要求安全上下文，RP ID 也不能是 IP——所以局域网必须有 token 这类方式
- 局域网明文下 token 会被同网段看到，但内容层仍是 Noise E2E，泄露的只是登录态，账号权限本来就小
- 两个 origin 是浏览器同源策略决定的，hub 绕不过去；手机在家也走公网域名就没这个问题，双 origin 只影响家里的电脑
- 自动放行私有网段而不加配置项：局域网访问是主场景之一，不应该要用户多配一个变量才能用

**否掉**：

- 密码 + TOTP（ADR-22 的 `single` 方案）
- 为局域网签自签名证书或 mkcert 让 passkey 可用——每台设备装 CA，摩擦太大；想要一个 origin 的人直接用公网域名
- 跨 origin 共享 Noise 密钥（导出导入）——多一套机制换少配对一次，不值
- 从 Host 头生成任何链接——经典的 Host 头投毒；Host 只用于 Origin 校验和 cookie 属性
- `reset-admin` 命令——改环境变量重启就是重置
- `multi` 模式提供 token 登录——公共 hub 上粘 token 的 UX 奇怪，邮箱验证码已经覆盖同等强度的场景

**取代**：ADR-22 中关于密码、TOTP、二次验证和 `single` 模式登录集合的部分。

---

## ADR-24 客户端不内置任何 agent 知识

**决定**：

- 「本机装了哪些 agent、哪个能用、能选什么模型、每个模型能开什么思考等级、有哪些权限模式、还有什么选项」全部由 daemon 探测后以 `AgentInfo` 声明（`agent.list`），Web / CLI / SDK 按 schema 渲染
- **思考等级挂在模型上**，不是全局枚举：不同模型支持的等级不同，有的不能关
- **权限模式是各 agent 自己的集合**（Claude Code 六种、Codex 收成四档、ACP agent 自定义），只附一个 `risk: safe | normal | dangerous`；不归一成固定几个值
- 其他 agent 特定选项走通用 `ConfigOption`（select / bool / text）透传
- 结构见 [spec/daemon.md § 5–6](spec/daemon.md#5-agent-探测与能力声明)

**原因**：agent 和模型每月都在变，客户端不应该为「多了一个模型」发版；各 agent 的权限语义本来就不同，强行归一会丢信息或误导；「按模型的思考等级、agent 自己的模式集合、通用的选项开关」这个形态在同类工具里已经验证可行；ACP 的 `configOptions` 也是这个思路。

**否掉**：Web 内置模型表；全局 `effort` 枚举；把权限模式归一成 ask / accept_edits / plan / auto 四个值。

---

## ADR-25 Claude Code 走 stream-json 的控制通道，不做 Agent SDK sidecar

**决定**：Claude Code adapter 用 `claude -p` 的双向 stream-json，权限请求、中断、改模型、改权限模式走 `control_request` / `control_response`。备选方案是文档化的 `PreToolUse` http hook，实现时若控制通道不稳则切换并记 ADR。

**原因**：控制通道是官方 Agent SDK 和 CLI 之间的实际协议，SDK 依赖它，不会无迁移路径地 break；用 Agent SDK 做 sidecar 要用户装 Node，违反 ADR-17；http hook 要往用户的 settings 文件写配置，侵入性更大。风险对策：锁定支持的 CLI 版本区间、用 `system/init.capabilities` 做特性检测、用录制流回放测试。

**否掉**：Agent SDK sidecar；PTY（ADR-18）；只用 `--permission-mode` 不做交互审批。

---

## ADR-26 Codex 走 app-server，不用 exec

**决定**：Codex adapter 只驱动 `codex app-server`（stdio JSON-RPC）。一个常驻进程服务所有 Codex 会话（v0.1 已修订为每会话一个进程，见下面的修订）。

**原因**：`codex exec` 在无头模式下写死 `approval_policy = never`，无法把审批交给人，而「审批」是 anycon 的核心场景；app-server 有 `model/list`（含每个模型的 reasoning effort）、`item/*/requestApproval`、`turn/interrupt`、`turn/steer`、`thread/resume`、`thread/list`，控制面完整。app-server 官方标 experimental，对策是 CI 用 `codex app-server generate-json-schema` 按版本生成 schema 做编译期校验。

**否掉**：`codex exec --json`；经 `codex-acp` 封装走 ACP（少一层控制，多一个 npm 依赖）。

**2026-10 修订（v0.1 实现）**：v0.1 改为**每个 anycon 会话一个 `codex app-server` 进程、进程里一个 thread**，生命周期和 Claude Code 的进程一样由 `sessions.idle_timeout` 管（空闲退出、下次 `thread/resume`）。原因：① 进程的 env 是整个进程共享的，共用进程就没法给每个会话不同的 env（`capabilities.env`）；② 一个会话的 app-server 卡死或崩溃不连累别的会话；③ 和现有 `AgentHandle` 一一对应，daemon 的并发上限、空闲回收、关停逻辑不用为 Codex 开特例。代价是多个会话同时活着时多占几个进程的内存（`sessions.max_concurrent` 默认 3）。共用进程留到确有需要时再做（adapter 内部的事，不影响协议）。

---

## ADR-27 本机 CLI 走 Unix socket

**决定**：daemon 开 `~/.anycon/daemon.sock`（0600），本机 CLI 通过它调用和内层协议相同的 JSON-RPC，不经 hub、不做 Noise。CLI 在别的机器上时作为普通设备经 hub 连接。

**原因**：在 daemon 所在机器上断网也要能 `anycon send`；本机管理命令（`pair`、`devices`、`doctor`、`export`）不该依赖 hub 在线；socket 权限就是认证，和 Docker、Tailscale 的做法一致。

**否掉**：CLI 永远经 hub；daemon 开本机 HTTP 端口（要处理 token，且暴露面更大）。

---

## ADR-28 会话事件只发给 watcher

**决定**：客户端 `session.watch` 后才收该会话的 `session.event`；列表级变化（标题、状态、待审批数）以 `session.changed` 发给所有在线客户端；`permission.request` 发给所有在线客户端。

**原因**：daemon 对每个客户端单独加密，事件不是广播而是逐个发，N 个客户端 × M 个活跃会话的 token 级增量会白白消耗手机流量和 daemon CPU；审批要能在列表页完成，所以权限请求例外。

**否掉**：所有事件广播给所有在线客户端。

---

## ADR-29 短码不经过 hub

> 「否掉：PAKE」已被 [ADR-36](#adr-36-账号内接入账号负责路由daemon-负责开门) 推翻：CPace 用在账号内接入；短码配对本身不变，8 位短码之后由接入码取代。

**决定**：配对短码由 daemon 本地生成，永远不发给 hub。两端各自做**一次** `Argon2id(短码, salt = "anycon-pair-v2", m = 64 MiB, t = 3, p = 1)`，输出的前 8 字节是 `match_id`（交给 hub 撮合），后 32 字节是 PSK（`derive_pair_secrets`）。链接里的短码放 URL fragment。握手完成且 `pair_done` 之后，daemon 经新传输发 `pair.ack`，新客户端校验后才落盘。**默认 16 位短码**（`anycon pair`、链接、二维码、Web「添加设备」都是）；8 位（`--short`）只给自建 hub 上手输用。

**原因**：第一版实现里短码由 hub 生成、joiner 又把短码明文交给 hub，hub 因此知道 PSK，能对每次配对做中间人——与 README 的核心承诺直接矛盾。match_id 让 hub 只能撮合不能直接拿到 PSK；`pair.ack` 让 hub 伪造 `pair_done` 也骗不到客户端落盘。

**2026-10 修订**：第一版 match_id 是 HKDF-BLAKE2s，几微秒一次。match_id 是短码的确定函数，hub 在 `pair_begin` 时就拿到它，可以**离线**穷举 8 位短码（2^40，GPU 上分钟级）甚至预计算整张表，猜中就有 PSK，`pair.ack` 挡不住（它用的就是那把 PSK）。所以：① KDF 换成 Argon2id（每次猜测 64 MiB 内存、约 0.1 s，8 位全空间约数千核·年；wasm 端一次约 0.1–1 s，可接受）；② 默认 16 位短码（2^79，不可行）；③ 文档如实写：8 位只适合自建 hub。盐仍是全局常量——两端只有短码这一个共享秘密，没有地方传随机盐，这是 Argon2 代价必须付的原因。

**残余风险**：拿到 match_id 的恶意 hub 对 8 位短码仍可穷举，只是贵；而且盐是全局常量，所以一次约十几 GPU·年的投入可以预计算整张「match_id → 8 位短码」表，之后对该 hub 上**所有**用 8 位短码的配对都是查表——这就是默认改成 16 位、8 位只留给自建 hub 的原因。被动窃听者不行（握手记录没有临时私钥就无法验证猜测）。彻底解决要换 PAKE，v0.2 评估 CPace。

**否掉**：PAKE（CPace / SPAKE2，三端都要再实现一套，v0.1 不做）；短码含 hub 可见的路由前缀（hub 仍可见部分熵，不如 hash）；每次配对随机盐（没有带外通道传它）。

---

## ADR-30 内层分片与大小上限

**决定**：`data` 信封 body 加 1 字节框架头：整条 / 分片 / 重置。一条 RPC 的 Noise 密文超过约 200 KiB 时按 `msg_id / index / count` 分片，接收方拼完再解密；任何一片丢失即传输失步，接收方发重置、重新握手。hub 丢弃超限信封时回 `dropped { too_large }`。daemon 同时限制单条消息：`session.history` 按字节分页（≤ 192 KiB，`has_more`）、`tool_result.output` 超过 64 KiB 截断保留头尾。

**原因**：hub 对单信封有上限（防御性，默认 256 KiB），而真实 Claude Code 会话的一页历史轻易超过，导致会话页永远打不开（已复现）。分片让上限只约束信封不约束消息；daemon 侧限制避免无意义的巨型消息。

**否掉**：把 hub 上限调到几 MiB（背压缓冲 4 MiB 就没意义了）；只做 daemon 侧截断（任何一条大事件都会复现问题）。

---

## ADR-31 会话 env 默认关闭

**决定**：`session.create.settings.env` 默认被拒绝（`permissions.allow_session_env = false`）。开启后，一个键要同时满足三条才接受（实现按 ②→①→③ 的顺序检查，所以黑名单键即使写进了允许列表也先按「黑名单」报错）：① 在运维配置的允许列表 `permissions.session_env_allow`（精确键名或 `PREFIX_*`，默认空；`anycon_agent_api::env_key_in_allowlist`）；② 通过 `anycon_agent_api::env_key_allowed`（合法标识符、不在 `ENV_PROTECTED`、不命中黑名单）；③ 不覆盖 `agents.*.env` 里运维配置的键。值 ≤ 8 KiB、≤ 64 个。每次设置都进审计（只记键名）。

**原因**：已配对客户端本来只能在审批下行动；能随意设 env 就能绕过审批执行代码（`BASH_ENV`、`LD_PRELOAD`）或把 prompt 和凭证改发到别处（`ANTHROPIC_BASE_URL`）。

**2026-10 修订**：第一版只有黑名单，复验实测 `SHELLOPTS=xtrace` + `PS4='$(…)'`、`LESSOPEN='|…'` 都能执行命令，而它们都不在名单上——黑名单显然枚举不全，「合理需求无法枚举」的理由反过来也成立。所以改成**允许列表为主、黑名单兜底**：运维明确写出允许哪些键，黑名单只防手滑（并补上了复验列出的那批）。对绝大多数部署，正确配置就是不开。

**否掉**：只有黑名单（枚举不全）；完全不过滤；按值过滤（值的语义取决于键）。

---

## ADR-32 如实声明：Web 客户端代码由 hub 下发

**决定**：威胁模型、README、SECURITY.md 都明说两件事：（1）E2E 保护的是会话内容对 hub 不可见，hub 看得到元数据（设备、时间、大小、谁连谁）；（2）Web UI 的代码由 hub 下发，所以用 Web UI 的人信任 hub 的运营者。去掉「hub 永远看不到内容」「hub 什么都看不到」这类绝对表述，改成「hub 只看到元数据」。缓解办法：自建 hub；隐私敏感场景用 CLI / SDK；hub 下发严格 CSP；自建时钉住发布版本。8 位短码经不信任的 hub 配对时有离线穷举的残余风险（ADR-29），用 16 位长码。

**原因**：用公共 hub 或被入侵的 hub 访问 Web UI 时，hub 可以下发改过的 JS 读走 IndexedDB 里的私钥和明文缓存，E2E 对这类用户实际失效。「服务器下发前端」是这类方案的固有限制，不是能在协议层修的东西，但文档不该让用户以为不存在。

**否掉**：
- 把 Web UI 与 CLI / SDK 说成同等可信
- v0.1 就上 SRI 当解决办法：hub 下发的 `index.html` 里的 SRI，HTML 和子资源来自同一个 hub，只防 CDN 篡改子资源，防不了下发 HTML 的那一方；SRI 要配合独立于 hub 的分发渠道才有意义
- 只做浏览器扩展：体验代价太大，手机上用不了

**后续**（不在 v0.1）：签名的发布包 + SRI，配合独立分发（浏览器扩展 / 桌面壳 / 钉住版本的 PWA 壳）；自建 hub 提供「校验 `web/dist` 与官方 release 哈希一致」的命令；PAKE 取代短码 PSK。

---

## ADR-33 关联本机已有目录为项目

**决定**：除了「根目录下的一级子文件夹」（ADR-9），客户端还可以用 `workspace.link { path, name? }` 把本机一个**已有目录**关联为项目（`Project.kind = linked`），不移动、不复制。daemon 只记「名字 → canonicalize 后的绝对路径」（SQLite `linked_projects` 表）。

- **范围**：只能关联 `workspace.link_roots`（默认 `["~"]`，热加载）之内的目录；列表为空 = 关闭这个功能（`workspace.link` / `workspace.browse` 返回 `forbidden`）。不能关联：workspace 根目录里面的目录（它们已经是项目）、包含 workspace 根目录或 anycon 数据目录（`ANYCON_HOME`）的目录、anycon 数据目录里面的目录、文件系统根。所以默认配置下关联 `~` 本身会被拒绝（它包含 `~/.anycon`）。
- **选择器**：`workspace.browse { path?, show_hidden? }` 只列 `link_roots` 之内的**目录名**（不列文件、不读内容），给手机上的选择器用。
- **命名**：和根目录下的项目共用一个命名空间。关联、新建、克隆、改名、从回收站恢复、scratch 转项目遇到已占用的名字都返回 `exists`。根目录里事后被手工建出同名文件夹时，关联的项目优先（它是显式登记的，会话按名字绑定），那个文件夹不出现在列表里，daemon 记一条 warn。
- **生命周期**：`workspace.unlink { name }` 只从列表去掉，不动目录；对关联项目调 `workspace.trash` 返回 `invalid`（提示用 unlink），anycon 永远不移动、不删除关联目录里的文件。`workspace.rename` 只改关联的名字。目录被移走 / 删除后项目以 `missing: true` 留在列表里，不能新建会话，可以解除关联。fs watch 不监听关联目录（项目的增删只来自 link / unlink）。
- **边界**：`fs.list` / `fs.read` 的路径第一段是关联项目名时，其余部分相对那个目录解析、canonicalize 后不能走出它。agent 的 cwd 是关联目录本身。

**原因**：用户机器上已经有大量项目（`~/work/...`），只能用根目录下的项目意味着要么挪目录、要么在根目录里放符号链接（而指向根外的符号链接按 ADR-9 的路径安全规则是被拒绝的）。ADR-9 反对的是「先在那台机器上注册项目才能远程用」——关联仍然完全在客户端完成，不需要碰那台机器；推导元数据的做法不变，只多存一张「关联了哪些路径」的表。

**风险与对策**：关联一个目录 = 把它交给已配对设备：`fs.list` / `fs.read` 不经审批就能读里面的任何文件（只进审计），agent 在里面按所选权限模式工作——normal 风险的模式（Claude Code `acceptEdits`、Codex `workspace`）会**不经审批**改 cwd 里的文件。所以「能关联哪些目录」本身就是安全边界，规则如下（2026-10 修订）：

1. **范围**：`workspace.link_roots`（默认 `["~"]`）之内；空列表 = 关闭。**每一项都不能经过符号链接**：`~` 按 daemon **启动时**解析并固定下来的 canonical HOME 展开（之后 HOME 的 canonical 路径变了，`~` 开头的项按被忽略处理并 warn），再按系统别名（`/var` ↔ `/private/var`、`/tmp` ↔ `/private/tmp`、`/etc` ↔ `/private/etc`、macOS 的 firmlink `/System/Volumes/Data/X` ↔ `/X`，后者只对按原样大小写写出的词法路径生效，见 daemon.md § 10 的已知限制）规范化，然后**逐段 `lstat`**：任何一段是符号链接就整项忽略并 warn（「请写真实路径」）；只是大小写或 Unicode 规范化（NFC / NFD）和盘上写法不同不算链接，以 canonical 路径作为这个 root。每次请求都按这条重判，不依赖快照什么时候拍——所以事先把某个 root（或它的上级）换成符号链接，重启或热加载之后也不会生效。被忽略的项和原因（经过符号链接 / 不存在）要让客户端看得到：没有可用的 root 时错误消息里列出它们，`anycon config set workspace.link_roots …` 之后打印 daemon 的判定。
2. **隐藏目录**：从所在的 root 往下，路径里任何一段以 `.` 开头（`~/.ssh`、`~/work/.cache/x`）就拒绝（`LinkBlock::Hidden`）。要关联隐藏目录，运维把它（或它所在的隐藏目录）显式写进 `link_roots`——那一项之下的段才不算。
3. **内置拒绝表**（`LinkBlock::Sensitive`）：系统、凭证、自启动目录——`~/Library`、`~/Library/LaunchAgents`、`~/Library/Keychains`、`~/bin`、`~/go/bin`、`~/.ssh`、`~/.gnupg`、`~/.aws`、`~/.azure`、`~/.kube`、`~/.docker`、`~/.config`、`~/.config/autostart`、`~/.config/systemd`、`~/.config/gcloud`、`~/.config/gh`、`~/.config/fish`、`~/.local`、`~/.password-store`、`~/.codex`、`~/.claude`，以及 `/etc` `/private` `/System` `/Library` `/usr` `/bin` `/sbin` `/var` `/dev` `/opt` `/proc` `/sys` `/boot` `/root` `/Applications`。等于、在里面、或包含其中任何一个都拒绝；只有 `link_roots` 的某一项本身就在拒绝项 D 里面时，那一项之下不再受 D 限制（更深的拒绝项仍然生效，例如放行 `~/.config` 不放行 `~/.config/systemd`）。「本身就在 D 里面」按第 1 条规范化后的路径判断（第 1 条已经保证它和 canonical 路径一致）。拒绝表是**启发式**的：它挡常见的凭证 / 自启动位置，挡不住所有「写进去就能执行」的地方；显式放行一个拒绝项就放开了它下面的一切（表里列出的更深项除外）。推荐把 `link_roots` 收窄到代码目录（如 `["~/work"]`）。拒绝表里的目录也不能 `workspace.browse` 进去（`forbidden`），列表里显示为不可关联
4. **绝对拒绝**（不能放行）：文件系统根；workspace 根目录里面（`invalid`）或包含它；anycon 数据目录（`ANYCON_HOME`）、在它里面或包含它。「在里面 / 包含」按目录身份 `(dev, ino)` 沿祖先链判断，不按路径字符串——同一个目录的另一种写法（firmlink、大小写变体）也绕不过去。
5. **使用时复核**：每次用到关联项目（`workspace.list`、`fs.*`、新建会话、拉起 / 续接 agent、**每个 turn 开始前——包括复用还活着的 agent 进程时**；turn 进行中在**放行权限请求之前**、**插话之前**，以及每 2 秒一次的周期检查）都重新 canonicalize，结果必须**等于登记的路径**，并重跑第 2–4 条（`link_roots` 收紧后不自动解除已有关联，但第 2–4 条按当前配置判断）；不满足就当作 `missing`，这个项目所有会话的 agent 进程都被收掉（空闲的直接关，正在跑 turn 的取消后关；待答复的权限请求按拒绝处理，插话不送出），turn 以 `not_found` 结束。周期检查约每 2 秒一次，发现之后还要收进程（SIGINT 后最多 5 秒宽限再 SIGKILL），所以目录被换掉之后，agent 不经审批执行的动作最多还能持续「一个检查间隔 + 收进程的时间」；复核通过之后、答复送到 agent 之前换目录同样拦不住——这是纵深防御，不是隔离。这挡住「关联之后把目录换成指向 `~/.anycon` 的符号链接」。`fs.read` 从边界目录起逐段 `openat`（每段 `O_NOFOLLOW`），中途任何一段被换成符号链接就失败；`fs.list` 打开目录之后按句柄的真实路径复核一次。这样「检查之后、打开之前换目录」的窗口就关上了（读文件）或很小（列目录）。

`workspace.browse` 让已配对设备额外看得到 `link_roots` 之内（拒绝表之外）的**目录名**。关联和解除关联进审计（`workspace.link` 记路径）。

**ADR-34 补充**：「永远不移动、不删除关联目录里的文件」有一个例外——anycon 在关联目录里建、删它自己的 `.anycon/worktrees/<slug>`（只删库里记录过的），往 `info/exclude` 写一行 `/.anycon/`，以及 `git worktree` / `git branch` 对仓库元数据的修改。worktree 目录按本条第 5 条同样的时机复核。

**否掉**：
- 多个 workspace 根（`workspace.roots = [...]`）：只能在配置里加整个父目录，不能从客户端挑一个目录
- 放开符号链接：会把「符号链接指向根外的一律拒绝」这条路径安全规则整个打开，而且符号链接要在那台机器上建
- 不限范围的关联：已配对设备就能把 agent 指到任意目录（包括 `~/.anycon` 里的设备密钥）

---

## ADR-34 Web UI 是开发者客户端：体验对齐 Claude 桌面版，每个会话可以有自己的 worktree

**决定**：

ADR-1 拆成两层来看：

- **连接层（hub + 协议）不变**：薄、不碰内容、只看到元数据。本条没有任何 hub 改动
- **客户端（Web UI，以及 daemon 为它提供的 git 能力）面向开发者**，体验对齐 Claude 桌面版（Claude Code 的 Code 页）：每个会话可以在自己的 git worktree 和分支里工作、看得到相对 base 的改动、能同步 base、能开 PR

守住的三条线（是安全边界，不只是范围）：

- **不做终端、不做远程 shell**：已配对设备能直接执行命令就绕过了审批（ADR-18、cli.md § 7）。要执行命令让 agent 做，走审批
- **不经 hub 反代 dev server**：hub 会看到明文。预览本地服务只能做成 Noise 隧道里的端口转发，单独立项
- **不做编辑器**：远程场景下改代码交给 agent；diff 只读

**每会话 worktree**：

1. **入口**：`session.create.workspace = { mode: "worktree", base?, branch? }`；不带 `workspace` 就是 `local`（在项目目录本身工作，和以前一样）。线路上不猜默认值；客户端的初始选择 = 用户在这个项目上次的选择，没有就用节点配置 `worktree.default`（默认 `worktree`）。只有 `GitMeta.worktree_ready`（项目目录就是仓库顶层，且至少有一个提交）的项目可以；scratch 会话不行
2. **daemon 自己建 worktree，不用 agent 的 `--worktree`**：Claude Code 和 Codex 都有这个参数，但位置、分支命名、清理方式各不相同，ACP agent 没有。daemon 建好后把 worktree 当 agent 的 cwd，三种 adapter 一样对待，diff / 同步 / 清理只有一套
3. **位置在项目里面**：`<项目>/.anycon/worktrees/<slug>`，分支 `<worktree.branch_prefix>/<slug>`（前缀默认 `anycon`）。slug 由 daemon 从第一条消息的英文单词生成（daemon 不调模型），取不到就随机两个词；客户端可以直接给分支名。daemon 在 `<git common dir>/info/exclude` 里加一行 `/.anycon/`，不改仓库里的 `.gitignore`。放在项目里，fs 边界和 ADR-33 第 5 条的复核自然覆盖到它；放 `~/.anycon` 下会撞上「agent 不能在数据目录里工作」这条绝对拒绝
4. **被忽略的本地文件**：新 worktree 里没有 `.env`、`node_modules`、`target/`。仓库根的 `.worktreeinclude`（gitignore 语法）列出要从项目目录复制过去的未跟踪文件；只复制普通文件，不跟随符号链接，上限 10 000 个文件 / 1 GiB。**不执行任何 setup 脚本**：仓库里定义、由 daemon 直接执行的脚本是不经审批的代码执行。装依赖让 agent 做，走审批
5. **只读改动视图**：`session.diff`（相对 base 和本分支的 merge-base，或只看未提交的）、`session.git_status`（ahead / behind / 未提交 / 冲突 / 上游 / PR）。daemon 的 diff 一律 `--no-ext-diff --no-textconv`
6. **同步 base**：`session.sync_base` = base 是远端跟踪分支时先 fetch，再 `git merge --no-edit`；有未提交改动、turn 在跑时拒绝；冲突时合并停在中途，客户端一键把「解决冲突」发给 agent，或 `abort`
7. **归档 / 删除**：`WorktreeDisposal`。`auto`（默认）= 干净就删 worktree（分支上没有自己的提交时连分支一起删），有未提交改动就保留；`keep` / `remove` 显式。取消归档时按分支重建。删掉会话但保留下来的 worktree 进「没有会话的 worktree」列表，在节点设置里清理（`workspace.worktrees` / `remove_worktree`）
8. **PR**：不在 daemon 里写 PR。「提交」「创建 PR」「解决冲突」是客户端发给 agent 的快捷消息——推送和 `gh pr create` 走 agent 的审批，提交信息和 PR 描述由 agent 写，正好守住「daemon 不调模型」。daemon 只用 `gh pr view` 读 PR 状态（`include_pr`），可选在 PR 合并 / 关闭后自动归档（`worktree.auto_archive_on_pr_close`，默认关）
9. **安全**：worktree 目录和关联项目一样在每次拉起 / 每个 turn / 放行权限 / 插话前、以及周期检查里复核（逐段 `lstat` 不能有符号链接，`(dev, ino)` 等于创建时记下的）；不一致就当作 `missing`，会话停下，不自动重建（目录还在就不碰）；目录已经不存在时，下一次发消息按分支重建。**不把主仓库的 `.git` 加进 agent 的可写目录**：那样 agent 能不经审批写 hooks，用户之后在终端里 `git commit` 就会执行它们；Codex 在 worktree 里提交走它自己的提权审批
10. **关联项目**：ADR-33 的「anycon 永远不移动、不删除关联目录里的文件」补一条例外：anycon 会在关联目录里建、删它自己的 `.anycon/worktrees/<slug>`（记在库里，只删记录过的），往 `info/exclude` 写一行，以及 `git worktree` / `git branch` 对仓库元数据的修改。其余文件仍然不动

**原因**：

- 用户要的是「在手机和浏览器里像用 Claude 桌面版一样用自己的机器」。hub 负责连通性，客户端体验是另一件事；ADR-1 把两者绑在一起，结果是客户端功能被「薄」这个连接层原则误伤
- 手机上的典型用法是一口气派几个任务。`sessions.max_concurrent = 3` 允许同一项目同时跑多个会话，它们在同一个目录里互相踩；每会话一个 worktree 是 Claude 桌面版验证过的解法（它的分支前缀设置作用于新会话，说明「每会话一条分支」是默认形态）
- worktree、diff、git 操作都在 daemon 上执行，经 E2E 到浏览器，hub 照样只看到元数据，隐私模型不变
- PR 交给 agent：daemon 里实现 push / PR 要处理凭证、PR 文本、各家 forge，等于在 daemon 里重写一遍 agent 已经会做的事

**否掉**：

- 用 agent 自带的 `--worktree`：各家不一致，ACP agent 没有
- worktree 放在 anycon 数据目录或项目旁边：前者撞绝对拒绝，后者在项目边界之外，要为它另立一套路径规则
- daemon 执行仓库里的 setup / teardown 脚本：不经审批的代码执行
- 把主仓库 `.git` 加进 Codex 的 `writableRoots`：见上面第 9 条
- daemon 自己 push、调 forge API 建 PR：凭证、PR 文本、多 forge 都要 daemon 管
- 终端面板：和 Claude 桌面版体验差得最明显的一处，但它是安全边界（见上）

**已知限制**：daemon 运行 git / gh 时（包括 Web 在 turn 结束后自动刷新的状态和改动，不只是用户明确操作时）会读仓库自己的配置，仓库配置里定义的外部命令——clean / smudge filter、merge driver、`core.sshCommand` / `core.askPass` / credential helper、`remote.*.uploadpack`——会被执行（`core.fsmonitor`、hooks、外部 diff / textconv、合并提交的签名已关）。和 `workspace.list` 的 clean filter 限制同类。另外，worktree 模式没有扩大、但也没有消除「agent 改仓库元数据」这条面：agent 能改的 cwd 里有 worktree 的 `.git` 指针文件（「当前目录」模式下是整个 `.git/`），改了它可以让用户之后在这个目录里跑的 git 读到 agent 写的配置；daemon 在 worktree 里跑 git 或动手前都按语义核对 `.git` 指针仍指向它自己的管理目录（变了按 `missing` 处理，提示 `git worktree repair`），但这是纵深防御，不是隔离（见 SECURITY.md「不是沙箱」）。

**部分修订**：ADR-1（「不做 diff、PR、worktree 编排」改为本条的范围）、ADR-33（关联目录里 anycon 自己的 worktree 例外，见第 10 条）。

**2026-10 修订**：

- **已归档的会话不能直接发消息**（所有会话，不只 worktree 会话）：`session.send` 返回 `invalid`（`data.reason = "archived"`），先 `unarchive`。归档时 worktree 可能已经被删掉或保留着未提交的改动，归档状态下跑 turn 会绕开复核。`workspace.remove_worktree` 对还有 turn 或 agent 进程的会话返回 `busy`，不管是否归档
- **创建失败的 worktree 继续占着它的名字**：`failed` 的记录保留分支名和目录名，新会话拿 `-2`；处置 `failed` 的记录只改库，不碰磁盘和分支（失败当时已经清理过）。崩溃残局（`creating`）清理前核对没有别的记录用同一路径 / 分支
- **base 规范化**：分支写法记成完整引用名，其他表达式（`HEAD~1` 之类）只记解析出的提交，不在 worktree 里按名字重新解析
- **冷缓存**：`workspace.list` 在没有缓存时等 git 元数据算完（每个项目最多 3 秒）；超时的项目带 `git_pending: true`（`git` 为空不代表不是 git 项目，客户端保持占位、不让会话悄悄建成「当前目录」），算完之后发 `workspace.changed { updated }`，客户端重新拉
- 合并提交 `--no-gpg-sign`；`gh pr view` 用记录的分支名、参数前加 `--`

**2026-10 修订**：等审批时也不能归档（`busy`），已归档的会话不开新 turn；daemon 自己发起的归档（PR 自动归档）遇到有 turn / 等审批 / 有排队消息的会话跳过。改名时先 repair 再让会话看到新名字；`.git` 指针按语义比较（接受相对路径）；处置时发现 worktree 被换掉不再报「已删除」。base 另存展示名，合并提交信息用展示名。

---

## ADR-35 图片附件：经 blob 通道传到 daemon，消息只带引用

**决定**（附件从 v0.2 提前；目前只有图片）：

1. **上传**：客户端把图片分块传给 daemon（`blob.put_begin` → `blob.put_chunk` × N → `blob.put_end`），daemon 落盘到 `<数据目录>/blobs/<blob_id>.<扩展名>`、记进 `blobs` 表，`put_end` 返回 `AttachmentRef`。`session.send` / `session.create` 只带 `AttachmentRef`，事件流里的 `user_message.attachments` 也只带引用。hub 没有任何改动，看到的仍然只是密文 `data` 信封
2. **分块是请求，不是通知**：每块都有答复，客户端据此显示进度、只重发失败的那一块；块按 `index` 定位（偏移 = `index × chunk_size`），可以乱序、并发（客户端最多 4 块在途）、重发
3. **下载是拉取**：`blob.get { blob_id, offset }` 分页（每段 ≤ 512 KiB），不做 daemon 推送的 `blob.chunk`。请求 / 响应天然有流控和重试；客户端只在要显示时才拉（懒加载）；daemon 不为每个客户端维护下载状态
4. **daemon 是唯一的校验点**：大小（≤ `sessions.max_attachment_bytes`）、SHA-256、文件头（magic bytes 前缀必须和声明的 mime 一致；不解码图片）、上传时的固定白名单（`image/png` `image/jpeg` `image/gif` `image/webp`）、发送时该会话 agent 的 `capabilities.attachments`、每条消息至多 20 个。消息里的 `AttachmentRef` 以 daemon 的记录为准：客户端给的 `name` / `mime` / `bytes` 和记录不一致 → `invalid`。宽高由 daemon 读文件头得出
5. **生命周期**：blob 第一次被消息引用时绑定到那个会话，随会话删除（`session.delete`）；一个 blob 只属于一个会话。没被引用的 blob 24 小时后回收；进行中的上传 10 分钟没有新块就作废；`blob.discard` 让客户端提前丢掉（用户在输入区移除了附件）
6. **交给 agent**：Codex 用 `localImage { path }`（daemon 的文件，扩展名与 mime 一致）；Claude Code 由 adapter 读文件转 base64，放进 stream-json 用户消息 `content` 数组的 `image` block（图在前、文字在后）。积压的多条消息合并成一个 turn 时，附件按消息顺序拼接
7. **Web 在上传前规整图片**：长边缩到 ≤ 2 000 px、单张通常 ≤ 1.5 MiB（硬上限 3.75 MiB，base64 后约 5 MB，Claude 的单图上限）、一条消息合计 ≤ 15 MiB，带 EXIF 的 JPEG 重编码；浏览器能解码而 agent 不收的格式（HEIC、BMP…）转成 agent 收的格式（具体规则见下面「2026-10 修订」，最初的版本只按单张算、缩到 2 048 px）。附件在选中的那一刻就开始上传，输入区显示进度；节点不在线时不能添加附件

**原因**：

- 内容只在 daemon 和客户端之间流动，hub「不碰内容」不变（ADR-1 / ADR-30）
- 消息只带引用：`session.send` 的 1 MiB、历史一页 192 KiB 的上限都不受图片影响，事件表里没有大段 base64，多台设备回放历史时只拉自己要看的图
- 只做图片：两个 adapter 都只认图片；别的文件要给 agent，放进项目目录就行

**否掉**：

- 把 base64 直接塞进 `session.send`：撞 1 MiB 上限，历史和 FTS 里都是大块数据，重发要整张重传
- hub 存 blob（或经 hub 的 HTTP 上传）：hub 会看到明文，违背 ADR-1
- daemon 推送下载（`blob.chunk` 通知）：每个客户端都要维护下载状态和流控，客户端也没法按需拉

**代价 / 限制**：

- hub 对客户端的入站限速默认 10 MiB / 分钟，连续发几张大图会被限速——传输层自动退避重发，只是慢
- 别的设备第一次看到历史里的图要整张拉下来（没有缩略图）；Web 缓存在 IndexedDB（LRU，上限 200 MiB）
- 离线很久的「未发送」消息里的附件可能已经被 daemon 回收，重发得到 `not_found`，要重新添加

**2026-10 修订**：

- **总量上限**（原来只按单张算）。Anthropic 的限制：请求体 ≤ 32 MB，一次请求里超过 20 张图时每张长边 ≤ 2 000 px；而图一旦进了 Claude Code 的会话历史，之后每个 turn 都会带着它重发。所以三层都按总量限：Web 把图缩到长边 ≤ 2 000 px（不再是 2 048），单张超过 1.5 MiB 就有损重编码（WebP / JPEG 0.9），一条消息合计 ≤ 15 MiB；daemon 每个附件默认 ≤ 10 MiB（`sessions.max_attachment_bytes`，原来 50 MiB）、每条消息合计 ≤ 20 MiB，积压合并成一个 turn 时附件累计超过 20 个或 20 MiB 就停，剩下的作为下一个 turn；Claude adapter 一条输入的图片 base64 合计 > 30 MiB 直接拒绝、不写 stdin。**残余**：一个会话里累计的图很多时（单张约 1 MiB 的几十张），整段历史仍可能超过请求体上限，之后的 turn 失败，只能新开会话——取决于 Claude Code 自己怎么压缩历史，anycon 控制不了
- **元数据**：带 EXIF 的 JPEG 在 Web 上一律重编码（方向转正、去掉 GPS 等元数据）
- **上传让路**：每个节点同时 1 张在上传、2 块在途，「允许」「停止」之类的请求最多排在约 1.4 MB 后面（同一设备到 daemon 是一条有序的传输）
- **校验的边界**：文件头只是前缀比对（magic bytes），不解码图片；`put_end` 收尾时上传进入「收尾中」，之后的块被拒绝、已经在写的块先写完再算 SHA-256，所以校验过的就是落盘的；并发的 `put_end` 拿到同一个结果
- **磁盘**：完成但没被引用的 blob 全节点合计 ≤ 1 GiB，数据目录所在卷剩余空间不够 `size + 1 GiB` 时拒绝新上传
- **名字**：拒绝控制字符和双向控制符；扩展名和 mime 对不上时 daemon 补上正确的扩展名（`x.html` + `image/jpeg` → `x.html.jpg`）

---

## ADR-36 账号内接入：账号负责路由，daemon 负责开门

**背景**：公共 hub 的用处是随时随地换一台设备连上自己的节点。现在新设备只能走短码配对（ADR-29）：短码要从 daemon 本机或另一台已配对设备上拿，手边一台已配对设备都没有就进不去；公共 hub 上又只能手敲 16 位。

**决定**：

1. **两层，各自验证**。账号由 hub 验证，只决定「请求能不能送到这台 daemon 门口」；能不能成为这台 daemon 的设备，由 daemon 自己验证。账号登录本身不授予任何节点的访问权，任何一方单独失守都接入不了
2. **接入（enroll）**：登录了账号 A 的设备（A 的正式设备，或升级请求带 A 的 web 登录态的临时连接）对 A 名下的一台 daemon 发 `enroll_begin { node }`。hub 校验归属、在线、限速后分配 `enroll_id`，通知 daemon，之后像配对撮合一样只转发两者之间的 `handshake` 信封。握手的后半段就是现有配对：`Noise_XXpsk3` → `pair_confirm` → `pair_done` → `pair.ack`，落盘和钉公钥的规则一字不改，区别只在 PSK 从哪来：

   | 方法 | PSK 来源 | 需要什么 |
   |---|---|---|
   | `scan` 扫码批准 | 新设备生成 32 字节随机数 r，和节点 id、`enroll_id` 一起放进二维码；已配对设备在 Web UI 里扫码，经 E2E 把 r 交给 daemon。PSK = `HKDF-BLAKE2s(r, info = "anycon-enroll-scan-v1" ‖ node_id)` | 一台带摄像头的已配对设备 |
   | `code` 接入码 | daemon 生成 6 位数字一次性码，经 E2E 只交给批准的那台已配对设备；用户在新设备上输入，双方跑 CPace，输出作 PSK | 一台已配对设备 |
   | `password` 节点口令 | 用户给节点设的口令；双方跑 CPace（口令先经 Argon2id 加节点盐），输出作 PSK | 只要知道口令 |

   接入码有两个来源：批准一个 `code` 接入请求时生成（只对那个请求有效）；或者在已配对设备上「添加设备」时主动生成（5 分钟内本账号任一不请求批准的 `code` 接入都可以用）
3. **PAKE 用 CPace**（CFRG 选定的平衡 PAKE，ristretto255 + SHA-512 套件），放在 `crates/crypto`，和 Noise 一样一份实现编译成 Rust / wasm / Python wheel。CPace 的会话标识由双方各出 16 字节随机数（不用 hub 给的 `enroll_id`），信道标识绑定 `"anycon-enroll-v1"`、节点 id 和方法；输出直接当 XXpsk3 的 PSK，不新增握手模式
4. **在线猜测由 daemon 限**。hub 的限速只防滥用，不是安全边界——hub 本身可能就是攻击者：
   - `password`：从 daemon 发出自己的 CPace 份额起，没有以一条通过校验的 XXpsk3 第 3 条消息结束的，都算一次失败（错口令、超时、中途断开）。连续失败满 3 次后，每次失败要等 2^(n−3) 分钟才接受下一次（n 是连续失败次数）；连续 10 次停用口令接入，要在本机 `anycon passwd --enable` 或在已配对设备上重新启用。成功清零。计数在发出 CPace 份额之前先落盘（悲观计数，崩溃和中途断开也算失败）、重启不清零、全节点一个计数，不按来源分；同时只允许一个口令尝试，否则并发能绕过退避
   - `code`：一次性、5 分钟有效、每个码最多 3 次尝试；主动生成的接入码同时最多一个
   - `scan`：r 是 256 bit，不计数；请求 5 分钟有效
   - 每台 daemon 同时最多 3 个进行中的接入，多出来的直接拒绝

   CPace 保证每次在线交互至多验证一个猜测，hub 手里没有能离线穷举的东西。这是它和现有短码的根本区别：短码的 `match_id` 是短码的确定函数（ADR-29 的残余风险）
5. **节点口令**：默认没有。`anycon init` 结束时提示（daemon 起来后用 `anycon passwd` 设），Web 的 `/setup` 向导配对完成后也提示，都可跳过；之后用本机 `anycon passwd` 或已配对设备上的 `node.set_enroll_password` 改。至少 8 个字符，不做复杂度规则（在线猜测有硬上限，长度够就行）。daemon 只存 16 字节随机盐和 `Argon2id(口令, 盐, m = 64 MiB, t = 3, p = 1, out = 32)`，口令本身不存；客户端从不保存口令。设置、修改、清除、停用、重新启用、被锁定都通知所有已配对设备并进审计。节点口令不是账号密码，hub 不经手，ADR-22「公共 hub 没有密码」不受影响
6. **通知**：`code` 请求到达时，daemon 给所有在线的已配对设备发 `node.enroll_requested`，并发 `notify { category: enroll_requested }`（Web Push，不带 `session_id`，不带任何细节）。新设备经接入加入、口令连续失败满 3 次、口令接入被锁定、口令被改动，发 `node.security_event` 和 `notify { category: security }`。`notify.category` 新增这两个值
7. **请求里的设备信息只是提示**：PSK 生效之前，新设备自报的名称和平台没有经过认证（hub 能改）。批准界面写明「只批准你眼前这台设备」；认证过的 `ClientInfo` 仍以 XXpsk3 第 3 条为准。接入码只显示在批准的那台设备上，旁边提示「不要告诉任何人」。二维码内容不是 URL（`anycon-enroll:1:<node_id>:<enroll_id>:<r>`），只有 Web UI 内的扫码能读，不能被做成「点这个链接就批准」的钓鱼
8. **CLI**：`anycon connect <hub-url> --node <节点>` 先临时登录账号（邮箱验证码；`single` 模式也可以用 token），再选方法接入；拿到的登录态只放在内存里，用于那一次 WebSocket 升级，接入结束就 `/auth/logout`。接入完成后 CLI 和以前一样是持 device token 的设备，不常驻账号登录态。SDK 不做账号登录，仍用短码配对
9. **不变的**：短码配对保留，用于 daemon 的首次身份（daemon 还不属于任何账号，第一台设备只能在机器旁边配）、不登录账号的 CLI / SDK、以及二维码 / 链接。吊销不变，被吊销的设备可以重新配对或重新接入。hub 不持有任何密钥和秘密，只多一种撮合：按「账号 + 节点 id」，而不是按 `match_id`

**原因**：

- 账号登录直接放行，等于 hub（运营者、入侵者、数据库加邮箱一起泄露）能往任何账号里塞一台设备；邮箱或 GitHub 被盗等于电脑被远程控制。anycon 不做密码和二次验证，前提正是「账号权限很小」（architecture.md § 5.6），这个前提要保住
- 现有配对的缺口有两个：手边没有已配对设备就进不去；公共 hub 上要敲 16 位
- CPace 让低熵秘密（6 位码、人记得住的口令）在恶意 hub 面前也安全：只能在线猜，在线猜几次由 daemon 决定
- 复用 XXpsk3 和 `pair.ack`：握手、钉公钥、落盘的规则一行不改，三种方法只差 PSK 的来源
- `crates/crypto` 本来就是一份实现编译三端，ADR-29 当时「三端各实现一遍」的顾虑不成立

**否掉**：

- 账号登录即放行、由 hub 为设备公钥背书：见原因第一条
- 邮箱验证码直接当接入凭证：码由 hub 发出，hub 知道它
- 「两边各显示一个数字，让用户比对是否一致」（SAS）：hub 同时坐在两条握手中间，它的临时密钥是后选的，可以反复试，直到两边的 6 位数相同（约 2^20 次，秒级）。要防就得加承诺轮次，不如扫码或 PAKE
- 账号级密钥、恢复短语、跨设备密钥同步：architecture.md § 5.1 刻意不做；节点口令已经覆盖了「设备全丢了」的恢复
- 口令由 hub 验证（包括在 hub 上跑 OPAQUE）：hub 验证就等于 hub 开门
- daemon 上用 OPAQUE（增强型 PAKE）：它的好处是服务端被攻破时不直接泄露口令等价物，而这里的服务端是 daemon，daemon 被入侵本来就不在威胁模型内；CPace 更简单
- 口令只能在本机设置：已配对设备本来就有 agent 的全部权限，限制它不增加安全，只让人出门在外时没法补设口令

**代价 / 限制**：

- 节点口令是长期秘密。在公共 hub 的 Web UI 里输入时，hub 下发的 JS 能拿到它——和设备私钥同一个信任前提（ADR-32）。口令也挡不住钓鱼页面（passkey 方案才防钓鱼）。怀疑泄露就改口令；每次接入成功都有通知
- 拿到账号的人能把口令接入锁死、能用错的码把某个接入码试满 3 次（每次都得真的跑一轮 CPace），也能用接入请求反复推送骚扰你。影响的只是接入，已配对设备照常可用，而且每次都有通知
- hub 多看到一些元数据：谁在什么时候对哪台节点发起了接入、有没有完成。看不到接入码、口令、二维码里的密钥；方法本身在 `handshake` 体里，hub 不解析，但能从消息节奏推测
- CLI 只支持邮箱验证码和 token 两种账号登录；只配了 OAuth 的 hub 上，CLI 仍用短码配对
- Web UI 要加应用内扫码（摄像头权限 + 二维码解码）

**后续**（不在本条）：

- 同步 passkey 接入：从已配对设备把一个 passkey 的公钥登记到 daemon，新设备对 daemon 出的挑战签名（挑战绑定新设备的公钥），由 daemon 验签。防钓鱼、不用输入，等 passkey 登录做完再做
- 用接入码取代 `anycon pair --short` / `node.pair_begin { long: false }` 的 8 位短码，之后删掉 8 位短码。ADR-29 的残余风险只在 8 位短码上，到那时就没有了

**部分修订**：ADR-29（「否掉：PAKE」改为采用 CPace，用在账号内接入；短码配对本身不变）、architecture.md § 3.1（CLI 在接入时临时登录账号）、architecture.md § 5.1（设备全丢了也可以用节点口令接入）。

---

## ADR-37 Web Push、邮箱验证码登录和邮件通知

**背景**：v0.1 只有 token 登录，通知只在页面打开时出现（应用内 toast）。ADR-36 之后，「有设备请求接入」「口令被锁定」这类事件恰恰发生在用户不在已配对设备前的时候，没有推送就没有意义；新设备上登录账号也只能粘 token。这一条把 Web Push、邮箱验证码登录和邮件通知一起做掉（[spec/hub.md § 2.2 / § 2.3 / § 6](spec/hub.md#6-通知)、[spec/web.md § 4.1 / § 4.9 / § 7](spec/web.md#7-通知)）。

**决定**：

1. **SMTP**：`ANYCON_SMTP_URL` 配了才有邮件（验证码和通知走同一条通道），用 lettre。`smtp://` 是 STARTTLS（必须成功）、`smtps://` 是隐式 TLS；另有 `smtp+plain://` 不加密，**只允许本机和内网**：IP 字面量必须是回环 / 私有 / 链路本地地址；主机名（比如 compose 的服务名）在每次发信前由 hub 自己解析，所有地址都必须是本机 / 内网，然后直接连解析出的 IP（本机的 postfix、Mailpit 一类中继，以及测试）。用户名密码写在 URL 里（要百分号编码）。`ANYCON_EMAIL_FROM` 是发件人（`Anycon <noreply@example.com>` 或裸地址），配了 SMTP 就必填，否则启动报错。TLS 用内置的 webpki 根证书，不依赖镜像里的系统证书
2. **邮箱验证码登录（`single` 模式）**：只给**已经绑定了邮箱的账号**用。管理员和受邀用户先用 token / 邀请登录，在「账号设置」里绑定邮箱（发码 → 验证），之后在任何设备上都能用邮箱登录。`single` 模式不因为邮箱创建账号（加人仍然走邀请）；`multi` 的「邮箱即注册」随 `multi` 一起做
   - `POST /auth/email/send { email }` **总是** 200，邮箱没绑定就不发；邮件在后台任务里发，响应时间不随邮箱是否注册变化（防枚举）
   - 6 位数字码，10 分钟有效，每个码尝试 5 次作废；同一邮箱 60 秒内只发一次、每天最多 10 个码；每个邮箱每天累计验证失败 30 次后当天不再接受（换 IP 绕不过去，一天最多约 30 次猜测）；另按 IP 限速（发码 5 次 / 小时，验证 20 次 / 分钟）。失败的验证至少 250 ms 才回（响应时间不暴露邮箱是否绑定）。登录码每个邮箱最多同时 3 个有效（别人请求新码顶不掉你手里的）。库里只存码的哈希
   - 邮箱规范化：去首尾空白、整体小写；`accounts.email` 唯一
   - 绑定：`POST /api/me/email { email }` 发码（邮箱已被别的账号占用 → `409 email_taken`），`POST /api/me/email/verify { email, code }` 生效；`DELETE /api/me/email` 解绑（账号没有别的登录方式时 `409 last_login_method`）
3. **Web Push**：
   - VAPID 密钥对（P-256）首次启动生成，存 `${ANYCON_DATA_DIR}/vapid.json`（0600），公钥经 `/api/config.vapid_public_key` 下发（base64url，未压缩点）。JWT 的 `sub` 取 `ANYCON_VAPID_SUBJECT`，没设时 `ANYCON_PUBLIC_URL` 是 https 就用它，否则用 `mailto:` + 发件人地址
   - 订阅按**设备**存：浏览器把自己的 web 设备 id 和 `PushSubscription` 一起 `POST /api/push/subscriptions`，hub 校验这台设备属于当前账号、是 web、没吊销；同一设备只留一条（新的顶掉旧的）；吊销设备时删掉它的订阅
   - **推给谁**：daemon 本来就给每台已配对设备各发一封 `notify` 信封，所以 hub 只推给**这封信的收件设备**，并且只在它**没有在线连接**时推（在线时信封照常经 WebSocket 送到，页面在后台就由页面自己弹系统通知）。再叠加账号的通知偏好（§ 5）
   - 加密按 RFC 8291（`aes128gcm`），鉴权按 RFC 8292（VAPID，ES256）。载荷只有 `{ category, node, session }`——和 `notify` 信封外层一样的元数据，没有正文。`TTL` 1 小时（`enroll_requested` 5 分钟，和接入会话一样长），`permission_requested` / `enroll_requested` 用 `Urgency: high`。推送服务回 404 / 410 就删订阅
   - **只往已知的推送服务发**：订阅的 endpoint 必须是 https，主机落在内置白名单里（FCM、Mozilla、Apple、Microsoft 的推送域名），否则 `400 push_endpoint_not_allowed`。hub 是替用户发 HTTP 请求的一方，不能被当成访问任意地址（包括内网）的跳板
4. **邮件通知**：`permission_requested`、`error`、`enroll_requested`、`security` 四类可以发邮件。条件：配了 SMTP、账号绑定了邮箱、偏好里开了邮件、**账号此刻没有任何 web 设备在线**（用户正在看的时候不发；常驻的 SDK / CLI 设备不算）；同一账号同一类别 10 分钟最多一封（daemon 给每台设备各发一封 `notify`，这条去重同时把它们合成一封）。正文是固定模板（节点名 + 类别 + 链接），中英双语，不含任何会话内容
5. **通知偏好**：`notification_prefs(account_id, category, push, email)`，`GET` / `PUT /api/me/notifications`。默认：`permission_requested`、`error`、`enroll_requested`、`security` 推送和邮件都开，`turn_finished` 只开推送，`session_idle` 全关

**原因**：

- 推送要在用户「不在」的时候才有用；邮件是最后一道（手机没装 PWA、推送订阅过期）
- 只给绑定过的邮箱发码：`single` 模式是管理员自己的 hub，不该有「任何邮箱都能收码」的入口；绑定这一步也顺便证明了邮箱归属
- 推送只给收件设备、且只在它离线时：daemon 已经做了扇出，hub 再扇出一次就会重复；在线的设备已经有 WebSocket，再推一次就是同一件事响两遍，Chrome 还会因为「收到推送却不显示通知」降级
- endpoint 白名单：推送订阅是浏览器给的、用户可控的 URL，hub 每次 notify 都会去 POST 它，不设限就是 SSRF

**否掉**：

- 用第三方推送库（`web-push` 等）：要么拖 OpenSSL，要么 HTTP 客户端和我们不一致；RFC 8291 / 8292 用 RustCrypto 自己拼不到两百行，测试里对 RFC 8291 附录的向量
- 邮件走厂商 HTTP API：ADR-22 已否（只走 SMTP）
- 推送的 HTTP 客户端用 reqwest：reqwest 0.13 一旦打开任何 rustls 特性，同一个构建里所有 `reqwest::Client::new()`（cli / daemon 的测试在用）都会因为没装默认 crypto provider 而 panic。改用 hyper + hyper-util + hyper-rustls（`ring`，内置 webpki 根证书）
- 推送给账号下所有 web 设备：会重复（见原因）；没和这台 daemon 配对的浏览器点开通知也打不开节点
- 用户在线时也发邮件：太吵

**代价 / 限制**：

- 推送服务（Google、Mozilla、Apple）能看到「某个时刻有一条推送」和密文大小；载荷本身端到端加密，内容也只有类别和不透明 id
- iOS 只有加到主屏的 PWA 能收推送（Safari 的限制）
- 邮件通知不带正文，点开链接才能看到是什么；邮件服务商能看到节点名和类别
- 白名单之外的推送服务（自建的 autopush 之类）要改代码

**部分修订**：ADR-22（`single` 模式的邮箱登录只给已绑定的账号，不能用邮箱注册）。

---

## ADR-38 发布：原生 runner 上的矩阵构建，不用 cargo-dist

**背景**：v0.1.0 要发四样东西：hub 镜像、`anycon` 二进制（含安装脚本和 Homebrew formula）、两个 npm 包，每样都要带第三方许可证。[tech-stack.md](tech-stack.md) 原先计划二进制用 cargo-dist。依赖里有 aws-lc-sys 和 ring，C 代码要用目标平台的工具链编译。

**决定**（`.github/workflows/release.yml`，`packaging/`）：

1. 推 `v*` 标签触发。先全部构建、自检（镜像起得来、二进制 `--version`、npm tarball 装进空项目能加载 wasm），都通过之后才发布；手动触发只构建不发布，用来在打标签前试跑
2. **二进制**：一个手写的 GitHub Actions 矩阵。macOS arm64 原生构建，Intel 版在同一个 arm64 runner 上交叉编译（Apple 的 clang / ld 本身支持两种架构）；Linux x64 / arm64 各用原生 runner，在 Ubuntu 22.04 上构建，所以要求 glibc ≥ 2.35。产物是 `anycon-<target>.tar.gz` + `.sha256`，GitHub 的构建来源证明，安装脚本 `packaging/install.sh`（校验 sha256、装到 `~/.local/bin`、不用 sudo），formula 由 `packaging/homebrew-formula.sh` 生成，在 macOS 上从刚建好的 Release 装一遍、跑 `brew test` 再推到 `anycon-ai/homebrew-tap`
3. **镜像**：amd64 / arm64 各在原生 runner 上构建，按 digest 推送，再用 `docker buildx imagetools create` 合成多架构清单并打 `<版本>`、`<主>.<次>`、`latest` 标签（预发布版只打 `<版本>`）
4. **npm**：`bun pm pack` 打包（prepack 重新构建 dist 和 wasm；bun 把 `workspace:*` 换成实际版本），再用 `npm publish <tarball> --provenance` 发布，protocol 先于 sdk，已发布的版本跳过
5. **第三方许可证**：Rust 依赖用 cargo-about（`packaging/about.toml`，按发布的目标平台过滤，忽略 build / dev 依赖，离线运行只读 crate 源码包里的文件）；Web UI 里的 npm 包由 vite 构建时收集（`web/scripts/third-party-licenses.ts`），生成的清单随 Web UI 由 hub 下发。压缩包、镜像（`/usr/share/doc/anycon-hub/`）、`@anycon/protocol` 的 wasm 各带自己的那份

**原因**：

- cargo-dist 生成并接管自己的 release workflow，改动要用它的工具重新生成。我们的发布还有镜像、npm、许可证这几块，写进同一个 workflow 比把它们挂在生成的文件外面更直接；四个平台的矩阵本身不复杂
- 原生 runner 避开了交叉编译：不需要为 aws-lc-sys / ring 准备目标平台的 C 工具链和 cmake；镜像里的 Rust 构建在 QEMU 下要慢一个数量级
- 先构建后发布：GHCR、npm、Homebrew 任何一处先发出去而别处失败，都会留下一个不完整的版本；npm 的版本号发了就不能复用
- npm 发 bun 打好的 tarball：发布的就是自检过的那个文件；npm CLI 能附带 provenance，`bun publish`（1.4）没有这个选项

**否掉**：

- cargo-dist：见上
- QEMU 构建 arm64 镜像：太慢
- musl 静态二进制：能覆盖 Alpine 和老 glibc，但 aws-lc-sys 在 musl-gcc 下的构建没有验证过，daemon 在 musl 的默认分配器下的表现也没测过。先发 glibc 版，musl 放进 roadmap
- 用 `bun publish` 直接发：没有 provenance；在包目录里发布时还会再跑一遍 prepack，发出去的和自检的不是同一个 tarball

**代价 / 限制**：

- Linux 二进制要求 glibc ≥ 2.35（Ubuntu 22.04、Debian 12 及更新）；更老的发行版和 musl 从源码构建
- macOS 二进制没有开发者签名和公证：curl / Homebrew 下载不受 Gatekeeper 拦截，浏览器下载的要手动放行
- `ubuntu-22.04` runner 下线时要换成更老 glibc 的构建环境（比如容器），否则 glibc 下限会跟着升高
- 首次发布前要手动准备 GHCR 包的可见性、npm 账号 `anycon`（`@anycon` scope）的发布 token、tap 仓库和两个 secret（[CONTRIBUTING.md「发布」](../CONTRIBUTING.md#发布)）

**取代**：[tech-stack.md](tech-stack.md) 里「Rust 跨平台 release 用 cargo-dist」的计划。
