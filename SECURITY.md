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
- `anycon mcp`（ADR-49）：能否经它的工具批准或拒绝权限请求、在未带 `--allow-dangerous-modes` 时选 dangerous 模式或向已处在这类模式的会话发消息、绕过 `allow_dangerous_modes`、设置会话 `env`，能否让经 hub 的连接改写审计身份（`local.set_client` 只在本机通道，固定枚举），能否让 daemon 拉起的 agent 环境里没有或被改写 `ANYCON_AGENT` / `ANYCON_SESSION_ID`；清单（`todo.*`，ADR-54）：agent 经 `add_todo` 或 anycon 拉起的 agent 里的 `anycon todo add` 能否修改、关闭或删除清单项，经 `anycon mcp` 加的项、或 anycon 拉起的 agent 里 `anycon todo add` 加的项，能否在调用方没有刻意隐藏来源的情况下不被标成 agent 加的 / 未收下，能否经 MCP 工具或嵌套 CLI 的正常路径清掉 `unreviewed`（自报了 `mcp` 的连接重新自报成 `cli`、清掉 `ANYCON_AGENT` 这类刻意绕过是已声明的护栏边界，不算），能否绕过未收下的项的上限（100）或总上限（1 000），清单内容（标题、正文）能否进入 hub、日志或审计；派发（`todo.dispatch`，ADR-54）：自报了 `mcp` 的连接或 anycon 拉起的 agent 里的 `anycon todo run` 能否在没有刻意绕过护栏的情况下派发，第一条消息能否被调用方指定（不能），「经过」的文字（agent 写的结果）能否进入 hub、日志或审计，`no_push` 在声明了它的 agent 上能否被一次正常的 `git push` / `gh pr create` / `gh pr merge` 绕过（脚本、`git -c k=v push` 这类刻意的绕过是已声明的边界，不算）
- 决策档案的规则（ADR-54）：agent 或自报了 `mcp` 的连接能否经协议的正常入口（不是存心绕开的本机进程）让一条规则不经人确认就生效，能否改或删别人写的规则；规则能否让权限请求被自动批准、放宽权限模式或「不 push、不开 PR」；规则文字能否进到日志、审计或 hub 看得到的地方
- 判断后端和提炼规则（ADR-54 第 3、4 条）：没配后端、或者 `judge.daily_calls = 0` 时 daemon 能否发出任何模型请求；`api` 后端的请求能否带出决策点摘录和已有规则之外的会话内容；密钥、提示或回应能否进到日志、审计、`node.get_config`、`node.info` 或 hub 看得到的地方；已配对设备能否远程设置后端（地址、密钥、要跑的 agent）；提炼出的规则能否不经人确认就生效；Claude Code 的一次性调用能否用上任何工具
- 被提示注入的 agent 能否借 Web UI 把数据发往第三方
- 更新检查（ADR-57）：请求里能否带出账号、设备、节点、安装的标识或会话内容；`[update] check = false` / `ANYCON_UPDATE_CHECK=0` 之后 daemon 是否仍联系 anycon.ai；已配对设备能否远程打开它；CLI 的其他命令、`anycon mcp`、SDK 是否发这个请求；伪造的回应能否让终端、日志或 Web UI 显示校验过的版本号以外的文字，或让提示里的链接指向 `https://github.com/anycon-ai/anycon/releases/` 以外的地方
- dev server 预览（ADR-39）：预览页里的代码能否读到 Web UI 的密钥或登录态；hub 能否在不持有票据的情况下打开隧道或看到隧道内容；能否经预览访问 daemon 机器上不该放行的端口（会话之外、`preview.allow_ports` 之外、本机 hub）；吊销设备或关闭预览后隧道是否仍能使用

## 自建用户如何获知安全更新

- 订阅本仓库的 Security Advisories
- 关注 GitHub Releases（RSS：`https://github.com/anycon-ai/anycon/releases.atom`）
- 安全修复会同时发布补丁版本并在 release note 顶部标注
- daemon 的更新检查（缺省开，[ADR-57](docs/decisions.md#adr-57-daemon-每天向-anyconai-检查一次新版本只提示不下载)）大约每天一次向 anycon.ai 询问最新版本，有新版本时 `anycon doctor` 和 Web UI 节点设置的「关于」里会提示，并附 GitHub 发布页的链接。它只告诉你有新版本，不说明是不是安全修复，也不下载、不安装；关掉了它的（关法见下面「已知限制」里的「更新检查让项目方看到你的 IP」），靠上面三条

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

**浏览器里的明文缓存和密钥。** 会话缓存、清单缓存（清单项的标题和正文，以及还没送达节点的待发改动）、Noise 私钥和 device token 以明文存在 IndexedDB，清单缓存和会话缓存的暴露面一样。「退出登录」只清内存里的登录态，不删它们；公用电脑请用「退出并移除本设备」（吊销设备并清空本地数据，包括还没被取走的分享暂存）。忘记一个节点只删这个节点的清单缓存和待发改动；换账号登录清掉所有本地数据（包括没被取走的分享暂存）。「设置 → 本机存储 → 清空缓存」只删会话历史和附件的缓存（草稿和未发送消息还要用的附件留下），密钥、草稿、未发送消息和清单缓存都保留，代替不了「退出并移除本设备」。会话历史缓存只留这个浏览器最近打开的 20 个会话，每个最多 2000 条事件。

**分享到 Web UI 的文字。** PWA 的分享目标由 service worker 处理：内容存在它自己的小库里、不进 URL，页面取出后删掉，用户确认后才记到清单。没有这个处理的 service worker 在管页面时（还没有已激活的 service worker；管着页面的仍是没有这个处理的旧版本，升级前就装着应用的用户在接受更新、或关掉这个应用的所有窗口和标签页之前都是这样（重新加载不会让等待中的新 service worker 生效）；浏览器里 service worker 不可用）那一次 POST 会发到 hub，hub 回 303 到 `/capture`，把请求体丢弃（不解析、不存、不进日志），但分享的文字经过了 hub，也经过了它前面终止 TLS 的反代 / CDN / 隧道，它们看得到请求体。被浏览器停掉的 service worker 不在其中，`fetch` 事件会唤醒它。

**设备 token 的生命周期。** hub 签发的 device token 默认每 30 天轮换一次，轮换后旧 token 再有效 60 秒，同一设备 1 分钟内最多轮换一次（[docs/spec/hub.md § 5](docs/spec/hub.md#5-设备与设备-token)）。这缩短的是**泄露了的 token** 还能用多久，不能让已经拿到有效 token 的人立刻失效：他在被发现之前可以冒充该设备连 hub（读设备列表等元数据；进不了加密通道，那还需要设备的 Noise 私钥），也可以抢在设备之前轮换，这时设备手里的 token 60 秒后失效，要重新配对并吊销旧设备。**旧 token 的持有者做不到这一点**：只有用当前 token 认证的连接才能轮换，设备自己轮换之后，拿着旧 token 连上来的连接（哪怕活过了 60 秒宽限）得不到新 token，泄露的旧 token 抢不走设备。已知限制：轮换的回执要是没送到客户端、客户端又在宽限内用旧 token 重连，它不能再请求轮换，宽限一过就要重新配对。怀疑泄露就直接吊销设备。

**不是沙箱。** agent 以 daemon 用户的身份运行，anycon 提供审批和 workspace 路径边界，不提供 OS 级隔离。已配对设备等于能在那台机器上让 agent 干活。**关联的本机目录**（`workspace.link`，ADR-33）同样交给了已配对设备：里面的文件可以不经审批读取（`fs.read`，进审计），agent 在里面按所选权限模式改文件（normal 风险的模式不逐个审批）。daemon 拒绝关联隐藏目录、常见的系统 / 凭证 / 自启动目录和 anycon 数据目录，但这张拒绝表是启发式的，挡不住所有「写进去就会被执行」的位置；显式放行一个拒绝项会放开它下面的一切。**建议把 `workspace.link_roots` 收窄到代码目录**（比如 `["~/work"]`），设成 `[]` 就关闭关联。 已知窗口：turn 进行中关联目录被换掉（比如换成指向别处的符号链接），daemon 每约 2 秒复核一次、发现后收掉 agent 进程，在这之前 agent 不经审批执行的动作仍可能落到新目标上；报告时请说明是否超出了这个窗口。

**dev server 预览。** 预览运行时（引导页、service worker、shim）同样由 hub 下发，和 Web UI 同一个信任前提。预览 origin 的 IndexedDB 里存着这个预览的 hub 令牌和票据密钥，预览页里的 dev app（agent 写的代码和它的依赖）读得到它们：拿到的人在过期前（默认 24 小时）能访问这一个端口，关闭预览（`preview.close`）或吊销签发它的设备立即失效。dev app 里写死的 `localhost` 地址、自己的 service worker 在预览里不工作。见 [ADR-39](docs/decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)。

**清单项派发的交接是提示，不是防注入的手段。** `todo.dispatch`（`anycon todo run`、SDK）开一个会话，每一轮结束时 daemon 把 agent 的最后一段话（≤ 4 KiB）记进这一项的「经过」；同一项再派发时，最近 5 次派发各自的报告（合计 ≤ 8 KiB）写进新会话的第一条消息，标明是之前 agent 会话的报告、不是指令。被提示注入的会话写下的话因此会进到下一个 agent 的输入里，标注（含每份报告每行的 `> ` 前缀）挡不住注入。第一条消息由 daemon 生成，调用方不能另给；daemon 拒绝自报了 `mcp` 的连接发 `todo.dispatch`，`anycon todo run`（CLI 自己做）在 anycon 拉起的 agent 里拒绝执行，都是护栏，不是边界：有 shell 的 agent 清掉环境变量、自报成 `cli` 或直接发 RPC 仍然能派发。「经过」里的文字是会话内容：不进日志和审计，只在端到端通道里传输，hub 只看到元数据。**「不 push、不开 PR」是按命令模式匹配的护栏，不是边界，也只在声明了它的 agent 上有效**：现在只有 Claude Code（启动时 `--settings` 的 `permissions.deny` 拒绝 `git push`、`gh pr create`、`gh pr merge` 这几条命令的写法）；写个脚本再 push、`git -c k=v push`、`gh api` 都挡不住；规则在 Claude Code 2.1.325 的 `default`、`bypassPermissions`、`auto` 三种权限模式下实测都生效（`bypassPermissions` 只测了带规则的一次，没有不带规则的对照；只有这一个 CLI 版本，别的版本没验证）；Codex 和 ACP agent 没有声明它，派发给它们必须明确放弃。

**决策档案里生效的规则是写进派发会话系统提示的指令，不是防注入的手段，也不改权限。** 生效的规则（`rule.*`，`anycon rule`，ADR-54）在建会话时写进 `todo.dispatch` 开出的会话的 `system_prompt_append`；`rules.inject_manual_sessions = true`（缺省 `false`）时手动开的会话也带。人写下的规则直接生效；不是人直接写下的规则（agent 提的，之后提炼出来的）是待确认，不注入，要人确认才生效。agent 的入口只有 `rule.add`：带 `agent: true`（anycon 拉起的 agent 里的 `anycon rule add` 自动带）或来自自报了 `mcp` 的本机连接时，状态强制为待确认、来源记成 agent；daemon 拒绝这样的连接 `rule.update` / `rule.delete`，`anycon rule confirm` 等在 anycon 拉起的 agent 里拒绝执行，`anycon mcp` 没有规则的工具。**这些是护栏，不是边界**：「agent 写的」是自报的标记，存心绕开的本机进程或有 shell 的 agent（清掉 `ANYCON_AGENT`、自报成 `cli`、直接发 RPC）可以写出一条直接生效的规则。真正的闸是「待确认的要人确认」——不看就确认，这道闸就没有了——和注入的字数上限（一个会话最多 4096 字节，超了拒绝，不截断）。agent 提的待确认规则最多 50 条，满了 agent 再提会被拒绝，所以它占不满规则的名额；规则里不能有双向控制符和零宽字符（看不见的字符没法审阅）；确认之前用 `anycon rule show` 看全文，`confirm` 也会打印被激活的那条的全文。规则只是 agent 系统提示里的一段话：不自动批准或拒绝权限请求，不改权限模式，不放宽「不 push、不开 PR」，agent（包括被提示注入的）不一定遵守。规则的文字是明文存在 daemon 数据库里的用户内容，不进日志和审计，只在端到端通道里传输，hub 只看到元数据；`anycon export` 不含规则。ACP agent 没有系统提示的入口，收不到规则（派发不失败，「经过」里记着）。

**提炼规则会把会话的摘录发给你配置的判断后端。** 只有在 `config.toml` 里配了判断后端（ADR-54 第 4 条）才会发生：提炼时 daemon 把一个会话里的决策点摘录（你答复、插话、取消之后说的话，以及前后一小段 agent 写的文字）和已有规则的文字发给后端。`api` 后端是 daemon 自己经 HTTPS 把它们发给那个模型服务，不经 hub；`agent` 后端是一次性调用本机的 agent CLI，由它发给它自己的模型服务。Codex 的一次性调用在只读沙箱里还能跑只读命令，摘录里被注入的指令可能让它读这台机器上的文件并写进回答。提炼出的规则一律待确认，要人看过才生效；这道闸只有在你确认之前真的读过全文才有用。

**Bot 会在你允许的项目里派发会话，使用 agent 声明的自动审批模式（没有时回退到其缺省模式），固定带「不 push、不开 PR」，不能批准权限。** 没有可用的分拣后端或允许项目为空时不运行。分拣会把交给 Bot 的项、之前的结果、问题和回答及生效规则发给判断后端：`api` 由 daemon 经 HTTPS 直接发给配置的模型服务、不经 hub；`agent` 由本机 agent CLI 发给它的模型服务。就 daemon 能执行的动作而言，被注入的内容最多让它对清单里的一项选错动作，不能让它改正文或批准权限；固定动作的校验不限制判断后端自身能访问什么（Codex 一次性调用仍可运行只读命令）。「不 push」是命令模式的护栏，不是沙箱边界。问题和交回推送只新增类别和不透明的清单项落点，内容仍经 E2E 通道取得。Bot 派出的会话的 PR 状态由 daemon 用本机的 `gh` 取：只有 PR 号、状态、检查汇总、评审条数和最新时间进入分拣输入，评审和评论的正文不进，因为公开仓库里任何人都能发评论；需要处理评审意见时 Bot 派发一个会话，由那个会话里的 agent 自己用 `gh` 读（受 agent 自己的权限约束）。

**Bot 连接让一个节点的 Bot 在另一个节点（B）上记项并派发，B 因此多了一台受限设备（`twin` 种类）。** 只能由人建立：在 B 上用 `node.twin_link_code` 发出一个只接受 Bot 设备的 16 位短码（10 分钟有效、同一时间一个），普通短码、接入码和节点口令一律不接受 `twin` 种类的设备。限制由 B 的 daemon 按设备种类强制，不是靠对方自觉：`twin` 设备只能调一张方法白名单，而且只能碰它自己建的项和它自己派出的会话（名单之外、别人的项和会话一律 `forbidden`，`forbidden` 先于 `not_found`，探测不出别人的东西存不存在）；推给它的通知只有它自己的项和会话的四种，会话列表、权限请求、清单、接入、安全事件、项目和 agent 的变化都不推，也不发 `notify`；`node.info` 不给它目录路径和判断后端。范围（开放哪些项目、同时几个会话、每天几次、用哪个 agent 和模式）存在 B 上、由 B 的人定，缺省什么都不开放，派发的会话固定带「不 push、不开 PR」和 worktree。B 的 `[twin] accept_links = false`（远程只能收紧）随时断开所有 Bot 连接。**A 被入侵、A 的判断被注入，都能在 B 对这条连接开放的项目里让 B 的 agent 干活**；上限是 B 的范围设置、B 上 agent 的审查器和「不 push、不开 PR」（命令模式的护栏，不是沙箱边界）。A 上的标题、正文和 Bot 的说明会写进 B 的数据库，B 上会话的结果会取回 A；经 hub 的仍是端到端密文，hub 多看到的元数据是「A 的 Bot 这台设备和 B 配了对」，以及两者之间流量的大小和时间。Bot 设备的密钥只在 A 的 daemon 进程里，没有给判断后端或 A 上别的本机进程用的代理，也没有「经 A 调 B」的 RPC 方法：B 上的动作只由 A 的 Bot 的计划（A 的 daemon 确定性地执行）和 daemon 后台对远程项做的只读同步发出，A 上别的本机进程没有直接用 B 的入口，只能像用户一样往 A 的清单里记项、交给 Bot（agent 记的项要人先收下）。建立和吊销时，B 给它所有已配对的设备发安全事件和 `security` 推送并记审计。已有 Bot 设备的 B 降级到不认识 `twin` 种类的旧版本之前，先吊销这些设备。

**`anycon mcp` 的护栏约束的是接口，不是那个 agent。** 把 `anycon mcp` 配置给某个 agent 的本机用户，等于让它以本机 CLI 的权限操作节点上的会话，只受接口上的约束：没有批准 / 拒绝权限请求的工具、默认拒绝 dangerous 权限模式（选这类模式，或向已经处在这类模式的会话发消息）、没有会话 `env`、在 anycon 拉起的 agent 里默认拒绝会改状态的工具（环境变量 `ANYCON_AGENT`）。这四条里有开关或标记的是第二条（`--allow-dangerous-modes`）和第四条（`ANYCON_AGENT` / `--allow-nested`），它们都在 agent 能碰到的地方：它能改自己的环境和 MCP 配置，有些 MCP 客户端启动 stdio server 时也只传精简的环境，这时标记传不到 `anycon mcp`，嵌套护栏就不起作用；有 shell 能力的 agent 本来就能直接运行 `anycon`（包括 `anycon approve`）。审计里的 `local-mcp` 是客户端自报的。**清单**（ADR-54）：`add_todo`（以及 anycon 拉起的 agent 里的 `anycon todo add`）不需要 `--allow-nested`，agent 能往节点的清单里加项；这样的项标成 agent 加的、未收下，最多 100 个，agent 经这些入口不能修改、关闭或删除清单项。「agent 加的」是调用方自报的标记（`todo.add` 的 `agent: true`；自报了 `mcp` 的本机连接由 daemon 直接标记），存心隐藏来源的本机进程（比如先清掉 `ANYCON_AGENT`）或这台机器上配对的 CLI 设备可以加出看起来像人加的项、也能改清单；自报了 `mcp` 的连接清不掉 `unreviewed` 也只到它再自报成 `cli` 为止。这是护栏不是边界。清单的标题和正文是明文存在 daemon 数据库里的用户内容，只在端到端通道里传输，hub 只看到元数据（消息大小和时间）。真正的上限是 `sessions.max_concurrent`、审批、`permissions.allow_dangerous_modes` 和 `config.toml`。见 [ADR-49](docs/decisions.md#adr-49-anycon-mcpstdio-mcp-server-是又一个客户端入口不暴露审批不做编排)。

**元数据。** hub 看得到谁连谁、什么时候、多大的信封、连接来自哪个 IP（库里只存前缀：每台设备最近一次被看到时的 IPv4 /24 或 IPv6 /48，浏览器登录会话和审计记录里也是这种前缀）、设备名和账号标识（显示名、绑定的邮箱，以及关联的 GitHub / Google 身份的提供方和稳定用户 ID——不存用户名、头像和 access token）、账号的配额用量；开着预览时还有预览 label、令牌的申请和预览连接的流量。anycon 不做流量混淆。账号被删除后，hub 上与它有关的设备、身份和审计记录一并删除，只留一条没有 IP 的删除记录。

**更新检查让项目方看到你的 IP。** 除了连 hub 和替用户做的事（克隆仓库、用 `gh` 查 PR 状态；agent 自己的网络请求不算 daemon 的），daemon 自己发起的请求只有更新检查（缺省开，ADR-57）和配置了 `api` 判断后端时对那个模型服务的请求（见上）：打开时每天一次 `GET https://anycon.ai/releases.json`，请求里只有 `User-Agent: anycon/<版本> (<os>; <arch>)`，没有账号、设备、节点或安装的标识，也没有会话内容。anycon.ai 的服务器和它前面的网络服务商（目前是 Cloudflare）看得到请求的 IP 地址、时间和 User-Agent（版本、系统、架构）；项目把这些请求记在访问日志里最多 30 天，用来按国家、版本和平台数活跃的 daemon，之后只留计数。自建用户想和 anycon.ai 零联系，要在 `config.toml` 里设 `[update] check = false` 或在 daemon 的环境里设 `ANYCON_UPDATE_CHECK=0`（不在远程修改的白名单里，已配对设备打不开它）。回应不可信：被入侵的 anycon.ai 能显示一条假的「有新版本」或压下真的那条，显示出来的只有校验过的 `X.Y.Z` 和由它拼出来的 GitHub 发布页链接，daemon 不会因此下载或运行任何东西。
