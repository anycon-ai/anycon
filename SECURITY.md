# Security

## 报告漏洞

请不要在公开 issue 里报告安全问题。

- 通过 GitHub Security Advisories 私密报告：Security → Report a vulnerability
- 或发邮件到 security@anycon.ai

我们会在 72 小时内确认收到，并在修复发布前与你协商披露时间。

## 范围

关注的核心问题：

- hub 能否以任何方式获得消息明文或 Noise 密钥（下面「已知限制」里的 Web 代码下发是已声明的例外，但欢迎报告任何**超出**它的路径）
- 配对流程能否被 hub 或网络攻击者做中间人（含：不知道短码的 hub 能否让新客户端落盘它的公钥；见下面的「配对短码」）
- 设备吊销后是否仍能握手或收到消息
- daemon 的 workspace 沙箱能否被 agent 或客户端绕过（含关联目录：能否关联到拒绝表 / 隐藏目录 / 数据目录，或关联后借符号链接走出登记的目录；Windows 上含 junction）
- 同一台机器上的其他用户能否连上 daemon 的本机通道（Unix socket；Windows 上的命名管道，含抢先建同名管道冒充 daemon），或读到数据目录里的密钥（ADR-42）
- 账号被盗后攻击者能做什么（设计上应仅限于管理设备列表，无法与 daemon 通信；管理员账号另外能看全部账号的元数据、授予 / 取消管理员、删除账号，同样看不到会话内容、进不了节点）
- 能否通过未验证邮箱的 OAuth 身份、验证码流程、登录 token 或邀请链接接管他人账号（provider 没声明已验证的邮箱从不用于匹配账号，也不会成为账号邮箱）
- GitHub / Google 登录的回调能否被 CSRF 或授权码注入利用（`state` 与浏览器里的 `anycon_oauth` cookie 绑定、单次使用，换 token 用 PKCE；关联身份时登录态必须仍是发起关联的账号），回调的跳转能否被引到站外（开放重定向）
- `multi` 的开放注册：能否绕过账号配额（设备数、daemon 数、在线连接数、每日配对次数）和验证码限速
- 删账号：能否在没有最近登录（10 分钟内）的情况下删掉别人的账号或自己的账号，能否借别的路径让已删账号的设备继续连接，或删除后仍留下账号的设备、身份、审计数据
- 已配对客户端能否绕过 `allow_dangerous_modes`、会话 `env` 策略或 workspace 边界，在 daemon 机器上不经审批执行代码；能否经 `node.set_config`（ADR-43：只有一张热加载键的白名单，`allow_dangerous_modes` 和 `preview.enabled` 只能收紧）改到名单之外的键、放宽只能收紧的开关，或在被环境变量覆盖的键上写出不一致的状态；`node.logs` 能否带出会话内容
- `anycon mcp`（ADR-49）：能否经它的工具批准或拒绝权限请求、选到 `allow_dangerous_modes` 之外或未带 `--allow-dangerous-modes` 时的 dangerous 模式、设置会话 `env`，能否让经 hub 的连接改写审计身份（`local.set_client` 只在本机通道，固定枚举），能否让 daemon 拉起的 agent 环境里没有或被改写 `ANYCON_AGENT`
- 被提示注入的 agent 能否借 Web UI 把数据发往第三方
- dev server 预览（ADR-39）：预览页里的代码能否读到 Web UI 的密钥或登录态；hub 能否在不持有票据的情况下打开隧道或看到隧道内容；能否经预览访问 daemon 机器上不该放行的端口（会话之外、`preview.allow_ports` 之外、本机 hub）；吊销设备或关闭预览后隧道是否仍能使用

## 自建用户如何获知安全更新

- 订阅本仓库的 Security Advisories
- 关注 GitHub Releases（RSS：`https://github.com/anycon-ai/anycon/releases.atom`）
- 安全修复会同时发布补丁版本并在 release note 顶部标注

## 威胁模型

详见 [docs/architecture.md § 5.6](docs/architecture.md#56-威胁模型)。一句话版本：端到端加密保护的是会话内容（和 dev server 预览的内容）在 hub 上不可见；hub 看得到元数据；Web UI 和预览运行时的代码由 hub 下发。

## 已知限制（已声明，不算漏洞，但欢迎讨论缓解办法）

**Web UI 的代码由 hub 下发。** 浏览器里的 JS 和 Noise wasm 来自你连接的 hub。恶意或被入侵的 hub（包括在你和 hub 之间终止 TLS 的 CDN / 隧道）可以下发改过的代码，读取该 origin 的 IndexedDB 里的 Noise 私钥、device token 和明文缓存，并以你的设备身份驱动 agent。这是「服务器下发前端」的固有限制，E2E 在这里保护不了 Web 用户。

缓解：

- 隐私敏感的使用**自建 hub**（你信任的 hub = 你自己的）
- 对公共 hub，用 **CLI / SDK**：代码由你从发布渠道安装，hub 不下发可执行代码
- hub 给 Web UI 下发严格的 CSP，限制页面能向哪里发请求，挡被注入内容和第三方脚本；它挡不住 hub 自己下发的脚本
- 自建时钉住镜像版本（tag / digest），升级前读 changelog
- 后续：签名的发布包 + SRI，配合独立于 hub 的分发渠道（浏览器扩展 / 桌面壳 / 钉住版本的 PWA 壳）。注意单靠 hub 下发的 HTML 里的 SRI 没用（HTML 和子资源来自同一个 hub）

**配对短码。** 短码由 daemon 本地生成、不发给 hub；两端对它做一次 Argon2id（64 MiB、3 轮），一半输出做撮合用的 `match_id`、另一半做 Noise 的 PSK；链接里的短码放在 URL fragment（不进 hub 的请求日志）；新客户端要等 daemon 经加密传输送来的 `pair.ack` 才落盘。残余风险：`match_id` 是短码的函数，**恶意 hub** 拿到它就能离线穷举短码，猜中就能对这次配对做中间人。默认 16 位短码（约 79 bit）穷举不可行；`anycon pair --short` 的 8 位（约 40 bit）在 Argon2id 下仍要数年 GPU 量级，只适合自建 hub。彻底解决需要 PAKE，在路线图上。见 [ADR-29](docs/decisions.md#adr-29-短码不经过-hub)。

**浏览器里的明文缓存和密钥。** 会话缓存、Noise 私钥和 device token 以明文存在 IndexedDB。「退出登录」只清内存里的登录态，不删它们；公用电脑请用「退出并移除本设备」（吊销设备并清空本地数据）。

**设备 token 的生命周期。** hub 签发的 device token 默认每 30 天轮换一次，轮换后旧 token 再有效 60 秒，同一设备 1 分钟内最多轮换一次（[docs/spec/hub.md § 5](docs/spec/hub.md#5-设备与设备-token)）。这缩短的是**泄露了的 token** 还能用多久，不能让已经拿到有效 token 的人立刻失效：他在被发现之前可以冒充该设备连 hub（读设备列表等元数据；进不了加密通道，那还需要设备的 Noise 私钥），也可以抢在设备之前轮换，这时设备手里的 token 60 秒后失效，要重新配对并吊销旧设备。**旧 token 的持有者做不到这一点**：只有用当前 token 认证的连接才能轮换，设备自己轮换之后，拿着旧 token 连上来的连接（哪怕活过了 60 秒宽限）得不到新 token，泄露的旧 token 抢不走设备。已知限制：轮换的回执要是没送到客户端、客户端又在宽限内用旧 token 重连，它不能再请求轮换，宽限一过就要重新配对。怀疑泄露就直接吊销设备。

**不是沙箱。** agent 以 daemon 用户的身份运行，anycon 提供审批和 workspace 路径边界，不提供 OS 级隔离。已配对设备等于能在那台机器上让 agent 干活。**关联的本机目录**（`workspace.link`，ADR-33）同样交给了已配对设备：里面的文件可以不经审批读取（`fs.read`，进审计），agent 在里面按所选权限模式改文件（normal 风险的模式不逐个审批）。daemon 拒绝关联隐藏目录、常见的系统 / 凭证 / 自启动目录和 anycon 数据目录，但这张拒绝表是启发式的，挡不住所有「写进去就会被执行」的位置；显式放行一个拒绝项会放开它下面的一切。**建议把 `workspace.link_roots` 收窄到代码目录**（比如 `["~/work"]`），设成 `[]` 就关闭关联。 已知窗口：turn 进行中关联目录被换掉（比如换成指向别处的符号链接），daemon 每约 2 秒复核一次、发现后收掉 agent 进程，在这之前 agent 不经审批执行的动作仍可能落到新目标上；报告时请说明是否超出了这个窗口。

**dev server 预览。** 预览运行时（引导页、service worker、shim）同样由 hub 下发，和 Web UI 同一个信任前提。预览 origin 的 IndexedDB 里存着这个预览的 hub 令牌和票据密钥，预览页里的 dev app（agent 写的代码和它的依赖）读得到它们：拿到的人在过期前（默认 24 小时）能访问这一个端口，关闭预览（`preview.close`）或吊销签发它的设备立即失效。dev app 里写死的 `localhost` 地址、自己的 service worker 在预览里不工作。见 [ADR-39](docs/decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)。

**`anycon mcp` 的护栏约束的是接口，不是那个 agent。** 把 `anycon mcp` 配置给某个 agent 的本机用户，等于让它以本机 CLI 的权限操作节点上的会话，只受接口上的约束：没有批准 / 拒绝权限请求的工具、默认拒绝 dangerous 权限模式、没有会话 `env`、在 anycon 拉起的 agent 里默认拒绝会改状态的工具（环境变量 `ANYCON_AGENT`）。后两条的开关和标记都在 agent 能碰到的地方：它能改自己的环境和 MCP 配置，一些 MCP 客户端启动 server 时也只传精简的环境；有 shell 能力的 agent 本来就能直接运行 `anycon`（包括 `anycon approve`）。审计里的 `local-mcp` 是客户端自报的。真正的上限是 `sessions.max_concurrent`、审批、`permissions.allow_dangerous_modes` 和 `config.toml`。见 [ADR-49](docs/decisions.md#adr-49-anycon-mcpstdio-mcp-server-是又一个客户端入口不暴露审批不做编排)。

**元数据。** hub 看得到谁连谁、什么时候、多大的信封、设备名和账号标识（显示名、绑定的邮箱，以及关联的 GitHub / Google 身份的提供方和稳定用户 ID——不存用户名、头像和 access token）、账号的配额用量；开着预览时还有预览 label、令牌的申请和预览连接的流量。anycon 不做流量混淆。账号被删除后，hub 上与它有关的设备、身份和审计记录一并删除，只留一条没有 IP 的删除记录。
