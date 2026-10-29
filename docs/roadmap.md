# 路线图

## v0.1.0 — 第一个版本

主线：`docker run` 起 hub → `anycon pair` → 网页里给自己机器上的 Claude Code / Codex 发消息 → 审批权限请求 → 看到结果和改动。全程不碰那台机器。

**协议与加密**
- 外层信封 + 内层 JSON-RPC；`crates/protocol` 导出 JSON Schema，TS 类型由它生成，CI 检查一致性
- Noise XX+psk3 配对握手、IK 重连握手；配对短码在 daemon 本地生成，两端用 Argon2id 派生 PSK（ADR-29）；同一份实现编译成 wasm 给浏览器和 SDK
- 大消息分片、`session.history` 按字节分页、工具输出截断（ADR-30）
- CPace（ristretto255 + SHA-512），用于账号内接入（ADR-36）

**anycon-hub**
- `single` 模式：管理员 token 登录、邀请链接；局域网 + 公网双 origin（ADR-23）
- 设备认证、路由、配对撮合、账号内接入撮合；对端离线时回 `undeliverable`
- Store trait + SQLite 实现；嵌入 Web UI；`/healthz` `/readyz`；CSP 等安全头
- Web Push、已绑定邮箱的验证码登录、SMTP 邮件通知（ADR-37）
- 自包含的 Docker 镜像构建；反向代理示例 ×4、compose 示例（[deploy.md](deploy.md)）

**anycon（daemon + CLI）**
- 出站连接、断线重连、睡眠恢复；本机 Unix socket 通道（ADR-27；Windows 是命名管道）；launchd / systemd 用户服务、Windows 计划任务（ADR-42）
- `init` / `daemon` / `pair` / `passwd` / `doctor` / `connect` / `ls` / `new` / `send` / `attach` / `diff` / `sync` / `worktrees` / `archive` / `delete` 等子命令（[spec/cli.md](spec/cli.md)）
- 登录 shell 环境解析（launchd 下也找得到 nvm / bun 装的 agent）
- agent 探测与能力声明：状态、版本、认证、按模型的思考等级、权限模式（[spec/daemon.md § 5](spec/daemon.md#5-agent-探测与能力声明)）
- Claude Code 原生 adapter（stream-json 双向）、Codex 原生 adapter（`codex app-server`，ADR-26）、mock agent
- 会话存储（SQLite + FTS5）、权限请求转发与多客户端答复、会话 watch、本地审计；会话 env 默认关闭（ADR-31）
- workspace：文件夹即项目，新建 / 克隆 / 改名 / 移到回收站 / 恢复、scratch 会话；关联本机已有目录（ADR-33）
- 每会话 git worktree：创建与复核、`.worktreeinclude`、改动 / diff / 同步 base、归档与删除时的处置、PR 状态与自动归档（ADR-34）
- 图片附件（ADR-35）
- 账号内接入：节点口令、扫码批准、接入码（ADR-36）

**Web UI**
- 登录、首次向导、配对（短码或 `/pair#code=` 链接）、解锁页（账号内接入）
- 节点首页、项目页、新建 / 克隆 / 关联项目
- 会话页：流式 Markdown、工具调用按类别渲染、思考折叠块、diff 视图、模型 / 思考 / 权限模式选择、权限审批卡片、图片附件
- worktree：工作位置选择、分支、「改动」标签（diff、同步 base、快捷消息、PR 状态）
- 节点设置、账号设置（登录方式、设备、通知、外观）；IndexedDB 本地缓存、三种断面布局、PWA、Web Push
- 英文 + 中文

**发布与分发**
- 推版本标签自动发布（ADR-38）：GHCR 多架构镜像 `ghcr.io/anycon-ai/hub`（amd64 / arm64）；`anycon` 二进制（macOS arm64 / x64、Linux x64 / arm64）、安装脚本、Homebrew tap（`brew install anycon-ai/tap/anycon`）；npm 包 `@anycon/protocol`、`@anycon/sdk`
- 每个产物附带第三方许可证清单；二进制有 sha256 和构建来源证明

**SDK 与测试**
- `@anycon/protocol`、`@anycon/sdk`（客户端方向，含图片上传）
- Playwright 端到端（`just e2e`）：登录、配对、会话、审批、worktree、账号内接入、通知等；Rust 侧 hub + daemon + CLI 的 happy path；SDK 对真实 hub + daemon 的端到端

## v0.1.x — 接下来补

发布与分发：

- macOS 二进制的开发者签名与公证（现在没有：用 curl / Homebrew 下载不受 Gatekeeper 拦截，浏览器下载的要手动放行）
- musl 静态 Linux 二进制（覆盖 Alpine 和 glibc 2.35 以前的发行版）
- Windows：Authenticode 签名（现在浏览器下载会触发 SmartScreen 提示）、arm64 原生二进制（现在用 x64 版在模拟下跑）、winget / scoop 包；预览按工作目录归属的规则（拿别的进程的工作目录）；adapter 进程层测试的 Windows 版（假 CLI 现在是带 shebang 的 python 脚本）
- `anycon-hub backup`
- CI 加 `cargo deny` / `bun audit`

功能：

- passkey 登录（连同 `identities` / `passkeys` 表、`/api/me/passkeys`）
- `notify` 的密文详情
- hub 离线队列（默认关闭的那个）
- Python：crypto wheel、pydantic 类型生成、Python SDK 客户端方向、`@anycon.agent` 注册节点
- 文件浏览（Web UI、`fs.*` 写入）
- `anycon new --image`（协议的 `session.create` 已经支持附件）
- CLI / SDK 上传图片时去掉 EXIF（含 GPS）并按方向转正——现在只有 Web UI 会重编码
- 管理员状态页显示邮件 / 推送的配置和订阅数
- scratch 过期前 7 天的提示；plan 批准时「开始实施，回到之前的模式」选项
- 浏览器侧：本地缓存淘汰（LRU / 清空缓存按钮）、多标签页共享一个 hub 连接、`noise_failed` 自动重试
- 配对：浏览器里的 Argon2id 派生移到同源 module Worker；之后用接入码取代 `anycon pair --short` 的 8 位短码，ADR-29 的残余风险随之消失
- 传输：hub 给「已转发到某 seq」的正向回执，去掉持续大流量约 1.6 MiB/s 的上限；RESET 帧带会话标识；粘住记录的时间兜底（[protocol.md § 3.1](protocol.md#31-消息大小) 的已知限制）
- 插话后又取消的 turn：被 agent 接受过的插话消息可能丢（Codex 实测），客户端标出「可能没有送达」
- 关联目录选择器：进不去的目录（拒绝表、数据目录）在点击前就不提供「进入」
- Web 客户端的信任：发布包签名、独立于 hub 的分发渠道（浏览器扩展 / 桌面壳）、自建 hub 的 `web/dist` 哈希校验命令（ADR-32）
- 同一 CLI 设备多进程同时连 hub（现在后来的会顶掉先到的，[spec/cli.md § 3.3](spec/cli.md)）
- worktree 的低优先级项：改名 repair 期间再改名、`.git` 指针按 git 的规则解析、重新排队路径也不给已归档会话开 turn、归档时的待发队列对客户端可见 / 可丢弃、Web 初次同步不被冷缓存拖住、名字被 `removed` / `kept` 记录占着时的提示文案、worktree 根 fd + `openat`；项目里有活着的 agent 进程（含空闲）时改名被拒（要等空闲回收）
- 小项：日志按天清理（现在只在启动时清一次）、探测超时的提示写出哪一步没完成、daemon 的 notification 发送端和会话 backlog 加上限、协议允许时收紧「谁能发 `pair_begin`」
- 移动端 PWA 的后台行为：页面被系统直接冻结时不会按时断开；Android / iOS 推送的真机验证

## v0.2 — 公共 hub 与更多 agent

已完成（随下个版本发布，见 [CHANGELOG](../CHANGELOG.md#unreleased)）：

- `multi` 模式：开放注册、配额与限速、`auth_ok.quota` / `quota_update`（[ADR-40](decisions.md#adr-40-multi-模式开放注册管理员配额账号删除和-oauth-绑定)）
- 登录方式：`multi` 的邮箱注册、GitHub、Google、OAuth 身份关联、设置页管理登录方式（ADR-22、ADR-40）
- 账号删除：自己删（要求 10 分钟内登录过）、管理员删；管理员页的账号搜索、授予 / 取消管理员
- dev server 预览：Noise 隧道 + 独立的预览域名（公共 hub 用 `aiplace.app`），会话页内嵌或新标签页打开（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)）

还没做：

- Store 的 Postgres 实现
- ACP client adapter（OpenCode、Pi 等一次覆盖）
- 非图片附件
- 会话搜索 UI
- 多 workspace 根
- `anycon export` / 导入，支持从公共 hub 搬到自建
- 隐私页、服务条款
- anycon.ai 公共 hub 上线，成本与捐赠公开
- 英文文档：之后英文为主、中文同步

## v0.3 — 体验

- Telegram 通知
- 语音输入
- 从模板创建项目
- 多客户端同时在线的状态同步细节
- TS SDK 节点方向（用 TS 写自定义 agent）
- daemon 自动更新与回滚
- Prometheus metrics

## 之后再说

- 可选 P2P 直连（WebRTC/ICE），同一局域网或打洞成功时绕过 hub；dev server 预览的流量最受益（ADR-39 的隧道协议和传输无关）
- CLI 端口转发（`anycon forward <节点> <端口>`，类似 `ssh -L`），复用预览隧道
- 移动原生 App（先看 PWA 够不够）
- 只读分享会话
- 同步 passkey 接入（ADR-36「后续」）：已配对设备把 passkey 公钥登记到 daemon，新设备对 daemon 的挑战签名，防钓鱼、不用输入；依赖 passkey 登录
- 多账号 / 团队
- 社区 adapter 仓库

## 明确不做

多 agent 编排、IDE / 终端 / 远程 shell、经 hub 明文反代 dev server（预览走 Noise 隧道，[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)）、issue 看板、E2E 之外的任何加密选项。
