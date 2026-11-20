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
- 附件（ADR-35、[ADR-44](decisions.md#adr-44-非图片附件落到会话工作目录)）：图片原生交给模型；其它文件复制进会话工作目录，消息里附上路径
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
- `fs.*` 写入
- CLI / SDK 上传图片时去掉 EXIF（含 GPS）并按方向转正——现在只有 Web UI 会重编码
- 管理员状态页显示邮件 / 推送的配置和订阅数
- scratch 过期前 7 天的提示；plan 批准时「开始实施，回到之前的模式」选项
- 浏览器侧：本地缓存淘汰（LRU / 清空缓存按钮）、多标签页共享一个 hub 连接、`noise_failed` 自动重试
- 配对：浏览器里的 Argon2id 派生移到同源 module Worker；之后用接入码取代 `anycon pair --short` 的 8 位短码，ADR-29 的残余风险随之消失
- 传输：hub 给「已转发到某 seq」的正向回执，去掉持续大流量约 1.6 MiB/s 的上限；RESET 帧带会话标识；粘住记录的时间兜底（[protocol.md § 3.1](protocol.md#31-消息大小) 的已知限制）
- 关联目录选择器：进不去的目录（拒绝表、数据目录）在点击前就不提供「进入」
- Web 客户端的信任：发布包签名、独立于 hub 的分发渠道（浏览器扩展 / 桌面壳）、自建 hub 的 `web/dist` 哈希校验命令（ADR-32）
- 同一 CLI 设备多进程同时连 hub（现在后来的会顶掉先到的，[spec/cli.md § 3.3](spec/cli.md)）
- worktree 的低优先级项：改名 repair 期间再改名、`.git` 指针按 git 的规则解析、重新排队路径也不给已归档会话开 turn、归档时的待发队列对客户端可见 / 可丢弃、Web 初次同步不被冷缓存拖住、名字被 `removed` / `kept` 记录占着时的提示文案、worktree 根 fd + `openat`；项目里有活着的 agent 进程（含空闲）时改名被拒（要等空闲回收）
- 小项：日志按天清理（现在只在启动时清一次）、探测超时的提示写出哪一步没完成、daemon 的 notification 发送端和会话 backlog 加上限、协议允许时收紧「谁能发 `pair_begin`」
- 移动端 PWA 的后台行为：页面被系统直接冻结时不会按时断开；Android / iOS 推送的真机验证

## v0.2 — 公共 hub 与更多 agent

已完成（v0.2.0 发布，见 [CHANGELOG](../CHANGELOG.md#020---2026-11-01)）：

- `multi` 模式：开放注册、配额与限速、`auth_ok.quota` / `quota_update`（[ADR-40](decisions.md#adr-40-multi-模式开放注册管理员配额账号删除和-oauth-绑定)）
- 登录方式：`multi` 的邮箱注册、GitHub、Google、OAuth 身份关联、设置页管理登录方式（ADR-22、ADR-40）
- 账号删除：自己删（要求 10 分钟内登录过）、管理员删；管理员页的账号搜索、授予 / 取消管理员
- dev server 预览：Noise 隧道 + 独立的预览域名（公共 hub 用 `aiplace.app`），会话页内嵌或新标签页打开（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)）
- hub 的 Postgres 存储（ADR-12），和 SQLite 同一套行为、同一套测试
- ACP client adapter：OpenCode、Gemini CLI、Goose、Kimi Code、Qwen Code、Cursor、GitHub Copilot、Pi 等说 ACP 的 agent 一次覆盖（[ADR-47](decisions.md#adr-47-acp-adapter每个-agent-一个-adapter一个进程服务它的所有会话)）
- 任意文件附件：agent 不原生收的文件复制进会话工作目录（[ADR-44](decisions.md#adr-44-非图片附件落到会话工作目录)）；CLI 的 `anycon send` / `new --file`
- Web 的会话全文搜索（⌘K 面板搜 daemon 的全文索引，命中直接定位到消息）
- Web 的只读文件浏览（`/n/:node/files/*`）
- 远程改节点配置（白名单，安全相关的只能收紧，[ADR-43](decisions.md#adr-43-远程修改节点配置白名单安全相关的只能收紧)）、远程读日志
- 设备 token 轮换（30 天自动轮换，旧 token 60 秒宽限）
- 多个 workspace 根（[ADR-45](decisions.md#adr-45-多个-workspace-根)）
- `anycon export` / `import`：会话历史和附件的备份、换机器（[ADR-46](decisions.md#adr-46-导出--导入一个-zip只走本机通道导入不带-agent-会话-id)）
- 公共 hub 上线：[hub.anycon.ai](https://hub.anycon.ai)（`multi`，邮箱验证码登录，每个账号一个节点，预览在 `*.aiplace.app`）；服务条款和隐私政策在 anycon.ai（`/terms`、`/privacy`），登录页和 `anycon connect` 经 `ANYCON_TERMS_URL` / `ANYCON_PRIVACY_URL` 链过去

还没做：

- 公共 hub 的 GitHub / Google 登录（要先注册 OAuth 应用）
- 英文文档：之后英文为主、中文同步

## v0.3 — 体验

已完成（v0.3.0 发布，见 [CHANGELOG](../CHANGELOG.md#030---2026-11-11)）：

- `anycon mcp`：stdio MCP server，让别的 agent 通过 MCP 工具（共十一个，其中 `list_todos` / `add_todo` 属于清单，[ADR-54](decisions.md#adr-54-清单决策档案和分身都在-daemon-上分身不能批准权限)，`add_todo` 不受 `--allow-nested` 限制）查看和驱动节点上的会话；不暴露权限审批、默认拒绝危险模式和嵌套使用（[ADR-49](decisions.md#adr-49-anycon-mcpstdio-mcp-server-是又一个客户端入口不暴露审批不做编排)）

还没做：

- Telegram 通知
- 语音输入
- 从模板创建项目
- 多客户端同时在线的状态同步细节
- TS SDK 节点方向（用 TS 写自定义 agent）
- daemon 自动更新与回滚
- Prometheus metrics

## v0.4 — 清单与分身

设计见 [ADR-54](decisions.md#adr-54-清单决策档案和分身都在-daemon-上分身不能批准权限)；Web UI 随之重排（「待处理」首页、手机底部标签栏、随手记一项、空间切换），见 [ADR-55](decisions.md#adr-55-web-ui-以待处理为首页手机用底部标签栏分身不是一个聊天窗口) 和 [spec/web.md § 14](spec/web.md#14-加入清单和分身之后的界面)，每一步的界面范围在 § 14.12。四步，每一步单独发布，第 1 步已实现，后三步还没做；之后的第 5 步（分身使用别的节点）见 [ADR-56](decisions.md#adr-56-分身可以使用别的节点daemon-作为受限设备配对范围由被使用的节点定)：

1. **清单**：按节点存的个人清单（[spec/daemon.md § 21](spec/daemon.md#21-清单)）。已实现：daemon、`anycon todo`、`anycon mcp` 的两个工具、TS SDK 和 Web
   - `todo.*` 方法、`capabilities.todo`、存储和审计；agent 环境里的 `ANYCON_SESSION_ID`
   - Web：首页换成「待处理」（权限请求、计划、提问、接入请求、标了待处理的会话、出错合成一个队列），手机用底部标签栏，随手记一项的各个入口，清单页和单项详情（本地缓存、节点离线时的改动回来后重放）
   - 多个节点：节点分组名（比如「家里」「公司」）；节点过滤器升级为空间切换，待处理、清单、会话树都跟着它；把一项移到另一个节点
   - `anycon todo` 子命令
   - `anycon mcp` 的 `add_todo` / `list_todos`：会话里的 agent 能记下之后要处理的事
2. **手动派发**：在清单的某一项上让 agent 处理
   - 从一项开一个 worktree 会话（标题和正文作为第一条消息），一项可以关联多个会话；也能反过来把会话里的一段话记成一项
   - 派发是 daemon 的一个方法（开会话、关联、回写、套用「不 push、不开 PR」都在 daemon 里），不由客户端拼 `session.create` 再回写：第 5 步里别的节点的分身调用的是同一个方法（[ADR-56](decisions.md#adr-56-分身可以使用别的节点daemon-作为受限设备配对范围由被使用的节点定) 第 8 条）
   - daemon 按会话状态回写：在跑 → `working`，等审批 / 等回答 / 出错 → `waiting`，这一轮结束 → `review`；`done` 由人点
   - 清单项是交接文档：每个会话一轮结束时，daemon 把 agent 的最后一段话记进这一项的「经过」；在同一项上再派会话，第一条消息除了标题和正文，还带上之前各次的结果。agent 没有往清单项里写的工具
   - 单项详情里看这件事的全部经过和关联会话的状态；验收页先给结论再给 diff，通过 / 再改改 / 不要了
   - 会话选项「不 push、不开 PR」（adapter 声明，翻译成 agent 自己的拒绝规则）：改动停在 worktree 的分支上等验收
3. **决策档案**：带出处的规则
   - 手写规则；回答权限请求和问答时「记成规则」；规则页（待确认 / 已生效）
   - 提炼：daemon 先从事件流里挑出决策点（审批的选择和理由、计划的修改意见、插话、取消后的改口），再交给判断后端归纳成待确认的规则
   - 判断后端：agent 会话，或直接调用模型 API（地址和密钥只在 `config.toml`）；分拣、提炼、顾问三个位置分开配
   - 多个节点：规则缺省只在本节点；范围设成「同组节点」的，由确认它的那台设备发给同组的每个节点，另有「对齐」比较差异。不同分组之间不同步
   - 注入有字数上限，超了提示用户合并；长期没用到的规则标成过期、不再注入，不自动删除
   - 用户确认后经 `system_prompt_append` 注入从清单派发的会话；是否也注入手动开的会话是节点的一个选项，默认不注入
4. **分身**：自动唤醒
   - 触发器：清单变化、定时、它派出的会话有了结果或在等人、用户回答了它的问题；短时间内的多个触发合并成一次
   - 周期性的清单项（「每个工作日早上」），到点生成这一次要处理的项
   - 派出的会话对应的 PR 检查失败、被合并或关闭、有了评审意见时唤醒（扩展已有的 `gh` 轮询，要补上评审意见）
   - 每次唤醒分拣清单：范围内的派发（第 2 步的派发，权限模式用 agent 自己的自动审批模式），技术上拿不准的问更强的模型，偏好上拿不准的问用户；用户回答时勾了「以后都这样」的直接成为规则，没勾的分身可以据此提一条待确认的规则
   - 范围和上限：能进哪些项目（默认一个都没有，逐个打开）、用哪个 agent / 模型 / 模式、同时派出几个会话、每天的预算；一键暂停；agent 加的项要人确认后才处理。范围、预算和运行约束按请求方存，不假设请求方只有本机分身（[ADR-56](decisions.md#adr-56-分身可以使用别的节点daemon-作为受限设备配对范围由被使用的节点定) 第 8 条）
   - 没有功能开关：没配判断后端或范围为空就是未配置，不起定时器、不调模型；`config.toml` 的 `[twin] enabled` 是本机判断的机器级硬关，远程只能收紧（[ADR-56](decisions.md#adr-56-分身可以使用别的节点daemon-作为受限设备配对范围由被使用的节点定) 第 1 条）
   - 运行约束：同一项连续失败到上限就停、会话长时间没进展算卡住，都转成「等你决定」；同时处理的项有上限，给人留并发名额
   - 运行记录：每次唤醒因为什么、对每一项做了什么决定和理由、花了多少，在 Web 的分身页上能看；没事可做的唤醒不发通知；审计里有分身自己的身份（不写死成本机的一个固定名字，第 5 步里还有别的节点的分身）
   - 用户在分身派出的会话里发了消息就算接手，分身不再处理这一项
   - 试跑：只给出它打算怎么处理当前清单，不派发、不提问
   - 新的通知类别「分身在等你回答」：整个 v0.4 里 hub 唯一的改动，只多看到一个类别词
   - 分身自己不能批准权限
   - 每个节点一个分身，各有各的清单、规则、预算；不做跨节点的主分身，分身也不在节点之间移动
5. **分身使用别的节点**（[ADR-56](decisions.md#adr-56-分身可以使用别的节点daemon-作为受限设备配对范围由被使用的节点定)）
   - 分身连接：节点 A 的 daemon 持有一个独立的客户端设备身份（设备种类 `twin`），作为节点 B 的一台已配对设备；由人在同时配对了两个节点的设备上建立（B 生成 16 位短码，经 E2E 交给 A），单向，可以单独吊销
   - B 按设备种类限制方法：`twin` 设备只能读节点和 agent 的信息、读对它开放的项目、在清单里记项并派发、读和取消它派发的会话；不能答复权限请求、改配置、管理设备、直接开会话
   - 跨节点的动作只有「在 B 的清单里记一项并派发」（第 2 步的派发）：B 不需要判断后端，结果取回 A 上那一项的「经过」；来源是分身的项不能再转派
   - 范围由 B 定：对每条分身连接开放哪些项目（缺省一个都没有）、并发、预算、agent 和模型，存在 B 上、由 B 执行
   - `config.toml` 的 `[twin] accept_links` 是「接受别的节点的分身」的机器级硬关，和 `[twin] enabled` 分开，远程只能收紧：只关 `enabled` 的机器自己不跑分身但给别的节点干活
   - 分身每次唤醒拿到一张已连接节点的表（名称、分组、是否在线、开放的项目、可用 agent、剩余名额）；工具多一个 `node` 参数，由 A 的 daemon 代理，密钥不出 daemon 进程
   - hub 只多认识一个设备种类词；威胁模型的改动（A 被入侵或判断被注入时能在 B 开放的项目里让 agent 干活）随这一步写进 architecture.md § 5.6、两份 README 和 `SECURITY.md`

## 之后再说

- 可选 P2P 直连（WebRTC/ICE），同一局域网或打洞成功时绕过 hub；dev server 预览的流量最受益（ADR-39 的隧道协议和传输无关）
- CLI 端口转发（`anycon forward <节点> <端口>`，类似 `ssh -L`），复用预览隧道
- 移动原生 App（先看 PWA 够不够）
- 只读分享会话
- 同步 passkey 接入（ADR-36「后续」）：已配对设备把 passkey 公钥登记到 daemon，新设备对 daemon 的挑战签名，防钓鱼、不用输入；依赖 passkey 登录
- 多账号 / 团队
- 清单在 hub 上的密文副本（节点离线时各设备之间也能同步清单）：依赖 hub 离线队列，条件见 [ADR-54](decisions.md#adr-54-清单决策档案和分身都在-daemon-上分身不能批准权限)「否掉」
- 社区 adapter 仓库

## 明确不做

通用的多 agent 编排框架（`anycon mcp` 只是又一个客户端入口，给调用方和 CLI / SDK 同等的原语，不调度、不在会话之间转发消息，[ADR-49](decisions.md#adr-49-anycon-mcpstdio-mcp-server-是又一个客户端入口不暴露审批不做编排)；daemon 上唯一的调度者是 v0.4 的分身，它只处理用户自己的清单，[ADR-54](decisions.md#adr-54-清单决策档案和分身都在-daemon-上分身不能批准权限)）、让 agent 批准权限请求、IDE / 终端 / 远程 shell、经 hub 明文反代 dev server（预览走 Noise 隧道，[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)）、issue 看板（标签、里程碑、看板视图、和外部 issue 系统同步；个人清单见 v0.4）、E2E 之外的任何加密选项。
