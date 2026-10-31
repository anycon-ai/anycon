# codex adapter

驱动 `codex app-server`（stdio，**换行分隔的 JSON-RPC，省略 `jsonrpc` 头**）。**不用 `codex exec`**：它在无头模式下写死
`approval_policy = never`，无法把审批交给人，而审批是 anycon 的核心场景（ADR-26）。v0.1 **每个 anycon 会话一个 `codex app-server`
子进程、进程里一个 thread**（ADR-26 修订）：env 按会话生效、一个会话卡死不连累别的、和 `AgentHandle` 一一对应；空闲回收与
Claude Code 相同（`sessions.idle_timeout` 后进程退出，下次 `send` 用 `thread/resume`）。规格见 `docs/spec/daemon.md` § 5–9。

下面的「实测」除特别注明外都来自本机的 `codex-cli 0.157.1`（2026-10-12）；子 agent、MCP elicitation、上下文压缩、用量口径那一节
（「子 agent、MCP elicitation、压缩上下文」）是 `codex-cli 0.164.0`（2026-10-26）的实测。app-server 官方标注 experimental，字段会变，所以：
线上格式只手写用到的子集（`wire.rs`，未知字段忽略、未知通知 / item 类型丢弃并 `debug!`）；CI 用
`codex app-server generate-json-schema` 生成的 schema 校验全部 fixture 和 adapter 实际发出的每一条消息（见「测试」）。

| 模块 | 职责 |
|---|---|
| `probe.rs` | 找二进制、版本门槛（≥ 0.157.0）、`codex login status`、拉起 app-server 取模型和认证（`initialize` → `model/list` → `config/read` → `account/read`）、cheap 探测沿用认证判定（`AuthMemo`）、诊断字段 |
| `models.rs` | `model/list` / `config/read` → 模型表（**不内置任何模型 id**） |
| `process.rs` | 会话设置 → `thread/start` / `thread/resume` / `turn/start` 的参数；模式映射；值校验 |
| `rpc.rs` | 行分类（响应 / 服务端请求 / 通知）、请求 / 响应对应（`RpcClient`，探测与会话共用）、超长行的「预览」分类（`classify_oversize`） |
| `wire.rs` | 线上格式的 serde 子集 |
| `translate.rs` | 通知 / 服务端请求 → `AgentEvent`（纯同步状态机，直接回放 JSONL 测试）；按 `threadId` 把子 agent 的通知和主 turn 隔开 |
| `permissions.rs` | 审批 / 提问 / MCP elicitation ↔ `PendingPermission` / `PermissionDecision`（含表单答案的类型转换）；不支持的服务端请求怎么答 |
| `tools.rs` | thread item → category / 标题 / `ToolCallDetail`；补丁的 unified diff |
| `handle.rs` | `AgentHandle`：进程 + 读 stdout 的任务 + 事件转发任务；握手、send / steer / respond / cancel / set_settings / shutdown |

## 命令行与环境

```
codex app-server        # 默认 --listen stdio://，不加别的参数
```

cwd = 会话 cwd；环境 = `SpawnRequest.env` **原样**（daemon 已合并登录 shell 环境、`agents.codex.env`、白名单过滤后的会话 env）。
`config.agents.codex.command` 可以是绝对路径或带前置参数的字符串（`codex --profile x`，前置参数放在 `app-server` 之前）。
子进程在**自己的进程组**里，stdout 单行上限 8 MiB，stderr 只留尾巴（报错用），关闭 = 关 stdin → 等 300 ms → SIGINT → 5 秒 → SIGKILL（整个进程组），
leader 一退出就清整个进程组——都来自 `common/child.rs`，和 Claude Code adapter 一样。实测 app-server 对 stdin EOF 和 SIGINT 都是 20 ms 内退出码 0。

找二进制与 Claude Code 相同（`common/bin.rs`）：`config.command` → `which -a codex`（daemon 环境的 `PATH`，只认绝对项）→ 已知目录
（含 `~/.nvm/versions/node/*/bin`）；逐个跑 `--version`（2 秒，`ANYCON_VERSION_TIMEOUT_MS` 可调）取第一个能跑的。`codex --version` 输出
`codex-cli 0.157.1`。

## 版本

最低 **0.157.0**（`MIN_CLI_VERSION`，本机验证过的版本），没有上限。更老 → `unsupported_version`，hint `npm i -g @openai/codex@latest`；
没装 → `not_installed`，hint `npm i -g @openai/codex`。版本解析不出来只 warn，不挡。

## 认证判定（`account/read` 为准，`codex login status` 兜底）

**`codex login status`**（文字在 **stderr**，stdout 是空的）本机实测：

| 情况 | 退出码 | stderr |
|---|---|---|
| 已登录（API key） | 0 | `Logged in using an API key - sk-test-***l-key`（codex 自己打码） |
| 已登录（ChatGPT） | 0 | `Logged in using ChatGPT`（本机没有 ChatGPT 登录，未实测） |
| 未登录 | 1 | `Not logged in` |
| `auth.json` 损坏 | 1 | `Error checking login status: expected ident at line 1 column 2` |

退出码 0 → 已登录；退出码 1 **且**文字是 `Not logged in` → 未登录；其余（超时、`auth.json` 损坏、不认识的输出）→ unknown。

**权威判定是 app-server 的 `account/read`**（稳定版 schema，不带 `refreshToken` 就只读本地状态、不联网）：响应 `{ account?, requiresOpenaiAuth }`。
它会解析 profile、`agents.codex.command = "codex -c model_provider=…"` 这些「靠名字猜」猜不到的配置，也分得清「自定义提供方但经代理走 OpenAI 登录」。
本机三种情形都实测了（录制在 `tests/fixtures/codex/account_read_*.json`）：

| 情形 | `account/read` | 判定 |
|---|---|---|
| 空 `CODEX_HOME` | `{ account: null, requiresOpenaiAuth: true }` | `not_authenticated`（code `login_required`，hint `codex login`） |
| 配了自定义提供方（`requires_openai_auth = false`） | `{ account: null, requiresOpenaiAuth: false }` | 不挡 |
| API key | `{ account: { type: "apiKey" }, requiresOpenaiAuth: true }` | 不挡 |
| ChatGPT 登录 | `{ account: { type: "chatgpt", email, planType }, requiresOpenaiAuth: true }`（按 schema 手写，未实测） | 不挡 |

规则：`requiresOpenaiAuth = false` → 不挡；`account` 为空且 `requiresOpenaiAuth = true` → `not_authenticated`；其余按已登录。**不再按 `config.toml` 里 `model_provider` 的名字猜。**

**`account/read` 用不了时**（app-server 起不来、请求失败或超时），完整探测按这个顺序取判定：

1. `account/read`（权威）；
2. **同一个二进制路径上一次权威的判定**（`AuthMemo.authoritative`，来自 `account/read`），**且当时记下的 `login status` 和现在的相同**——`login status` 不认自定义提供方，一次 `account/read` 超时就退回它，
   会把原来好好的会话（`requiresOpenaiAuth = false`，`login status` 却说未登录）一下子挡成 `not_authenticated`。沿用时**不改记录**（还是原来那条权威判定），所以
   连着几次 `account/read` 失败也不会「渐渐变成 `login status` 的判定」，cheap 探测照旧比较 `login status` 有没有变；二进制路径不同的记录不算；
3. `login status`：**只有「已确认未登录」才挡**，unknown 不挡（这一档的判定不权威，下一次 cheap 探测会再问 `account/read`）。

`login status` 变了（用户刚 `codex login` / 退出）时**不用第 2 档**：旧判定很可能就是错的（刚登录后点「刷新」而 `account/read` 恰好超时，还显示未登录；反方向则显示可用、拉起时才失败），
直接用刚读到的 `login status`，并把记录换成这次的（不权威，下一次 cheap 探测会再问 `account/read`）。cheap 探测就是因为 `login status` 变了才来问 `account/read` 的，所以同理。

**完整探测**在 `config/read` 之后调 `account/read`（同一个 app-server 会话里，顺手的事）。**cheap 探测**（`probe_interval` 的周期探测）只跑 `login status`：
结果（已登录 / 未登录 / unknown）和上一次一样 → 沿用上一次的判定，不拉起 app-server；不一样（登录了 / 退出了）→ 拉起 app-server **只补跑 `account/read`**（不拉模型列表）。
「上一次」存在 **adapter 实例里的 `AuthMemo`**（`CodexAdapter.auth_memo`：二进制路径、`login status` 的结果、判定、判定是否来自 `account/read`）——`anycon_adapters::all()` 里的实例在 daemon
的整个生命周期里是同一个，所以不需要 `previous` 里的诊断字段。没有记录（daemon 刚启动、换了 adapter 实例）、`previous` 为空、二进制路径变了、或上一次的判定不是来自 `account/read`（退回 `login status` 得出的判定不权威），
都按「不一样」处理，再问一次 `account/read`。已知取舍：`login status` 没变、而 `config.toml` 改了提供方（`account/read` 的答案会变）时，cheap 探测不会发现，要等下一次完整探测
（`agent.list { refresh: true }`、二进制路径 / 版本变了、`agents.codex` 配置变了）。

## 模型与默认值（探测第 4 步）

在 `ctx.scratch_dir` 下拉起 `codex app-server`：`initialize` → `initialized` → `model/list`（翻页，`includeHidden: false`，再跳过 `hidden`）→
`config/read` → `account/read`，然后关掉；全程受 `ctx.deadline` 约束。

- 每个 `model/list` 项 → `ModelInfo`：`id` = `model`（缺省 `id`），`label` = `displayName`，`default` = `isDefault`；
  `supportedReasoningEfforts` → 该模型的 thinking 选项（`low … ultra` 的子集，**挂在模型上**，不同模型不同），`defaultReasoningEffort` 为默认。
- `config/read` 的 `model` 成为默认模型（列表里没有就追加一项，没有思考等级信息），`model_reasoning_effort` 成为该模型的默认等级（须在它的选项里）。
- `config.agents.codex.additional_models` 追加（不能抢默认）。`allow_custom_model` 恒为 true（Codex 的模型淘汰很快，自建网关的模型 id 更是随便）。
- **`model/list` 不需要登录**（本机实测：空 `CODEX_HOME` 也返回完整列表）。失败（没网、app-server 起不来、超时）时沿用 `previous` 的模型；
  都没有就是空列表 + `allow_custom_model`，**不会**因此把 status 打成 error。`config/read`、`account/read` 单独失败不影响模型列表（`account/read` 失败见上，退回 `login status`）。
- `cheap` 探测不拉起 app-server，模型来自 `previous`；modes / options / capabilities 每次都按当前代码重新给出。

## 权限模式与选项

四档权限模式（id 是 adapter 自己的，不翻译不归一）映射到 `approvalPolicy × sandbox × approvalsReviewer`：

| 模式 | risk | `approvalPolicy` | 沙箱（`thread/start.sandbox` / `turn/start.sandboxPolicy`） | `approvalsReviewer` |
|---|---|---|---|---|
| `read_only` | safe | `on-request` | `read-only` / `{type: readOnly}` | `user` |
| `workspace`（默认） | normal | `on-request` | `workspace-write` / `{type: workspaceWrite}` | `user` |
| `auto_review` | normal | `on-request` | `workspace-write` / `{type: workspaceWrite}` | `auto_review`（Codex 自己的审查子代理放行） |
| `full_access` | dangerous | `never` | `danger-full-access` / `{type: dangerFullAccess}` | `user` |

注意两处线上写法不同：`thread/start` 的 `sandbox` 是短横线字符串，`turn/start` 的 `sandboxPolicy` 是驼峰的 tagged 对象。

**不发 `writableRoots`**：`sandboxPolicy` 只带 `type`，`thread/start` / `thread/resume` 也不传任何可写目录，cwd 是 git worktree（`<项目>/.anycon/worktrees/<slug>`，ADR-34）时也一样，参数里除 `cwd` 之外和普通目录完全相同。
理由（ADR-34 第 9 条）：worktree 的 `.git` 指向主仓库的 `.git/worktrees/<name>`，把主仓库 `.git` 加进可写目录，agent 就能不经审批往 `hooks/` 里写脚本，用户之后在终端里 `git commit` 会执行它们；
`workspace` 模式下 agent 在 worktree 里 `git commit` 会写到沙箱之外的 `.git`，由 Codex 自己的提权审批处理（用户逐次批准），adapter 不预先放行。回归测试 `worktree_cwd_adds_no_writable_roots`（`process.rs`）。

其他选项：

| id | 类型 | 映射 |
|---|---|---|
| `fast_mode` | bool，缺省 false | `serviceTier`：开 = `priority`（`model/list` 里叫 "Fast" 的那档），**明确关** = `default`（标准速度），没指定 = 不传（沿用 Codex 自己的配置） |
| `model_reasoning_summary` | select：`auto` / `concise` / `detailed` / `none`，缺省 `auto` | `turn/start.summary` |

**没有声明的**（本机 0.157.1 的稳定版 schema 不支持）：

- `plan_mode`：`turn/start.collaborationMode` 只出现在 `generate-json-schema --experimental` 的输出里，用它要在 `initialize` 里声明
  `experimentalApi: true`。v0.1 只用稳定版接口，不声明；`kind = plan` 的权限请求因此也不会出现。`item/tool/requestUserInput` 本来只在 plan 模式下可用，
  但仍然按 `question` 实现了（实测打开 `default_mode_request_user_input` 特性时在默认模式下也会来）。
- `max_turns` / `max_budget_usd`：Codex 没有对应项，设置带它们 → `invalid_settings`。
- `structured_output` 能力标 false：`turn/start.outputSchema` 存在，但 `SessionSettings` 里没有东西可以喂给它。
- `rewind` false。

## 握手与 thread

`spawn` **握手做完才返回**（任何一步失败都在 `spawn` 里报错，进程被清掉）：

1. `initialize { clientInfo: { name: "anycon", title: "anycon", version }, capabilities: { experimentalApi: false, optOutNotificationMethods: [...] } }`，然后通知 `initialized`。
   `optOutNotificationMethods` 让 app-server 别发不需要又可能很大的通知：`turn/diff/updated`（整个 turn 的累计 diff，每改一次文件就重发一次）、`item/fileChange/patchUpdated`、
   `account/rateLimits/updated`、`remoteControl/status/changed`（真 app-server 上实测生效）。
2. 新会话 `thread/start { cwd, model?, approvalPolicy, approvalsReviewer, sandbox, developerInstructions?（= system_prompt_append）, serviceTier? }`；
   续接 `thread/resume { threadId, cwd, model?, approvalPolicy, approvalsReviewer, sandbox, excludeTurns: true, serviceTier? }`
   （`excludeTurns`：不要把整段历史回给我们，daemon 自己有一份）。thread id（`result.thread.id`）作为 `agent_session_id`，随 `AgentEvent::Started` 发出。
3. **续接不上就改开新 thread，并明说**：错误里有 `no rollout found` **或 `thread not found`** 就回退到 `thread/start`。前者是 thread 在记录第一条用户消息之前（比如第一个 turn 立刻因为没登录
   而失败）没有 rollout 文件，之后再 `send` 走 resume 会失败（本机实测 `no rollout found for thread id …`）；后者是 rollout 被删 / 被 Codex 自己归档了。回退时 `Started` 带新 id，
   紧跟着发一条可恢复的 **`error { code: "agent_context_reset", recoverable: true }`**（不让 turn 失败），消息里带 Codex 的原话和「之前的上下文 agent 看不到了」——否则模型像第一次见面一样回答，用户还以为它记得。
   其它 resume 错误照常报错。
4. **`system_prompt_append` 只能在 thread 创建时给**。实测 `thread/resume { developerInstructions }` 被**忽略**（rollout 里持久化的赢，请求体里仍是旧指令）。
   所以后来的改动走 `thread/inject_items`，往 thread 的历史里追加一条 developer 消息（"从现在起按下面的指令，取代之前的附加指令：…"；清掉时是「之前的附加指令作废」）。
   `ResumeHandle.launch_args.instructions_applied` 记着 thread 已经知道的文本（私有键，daemon 的 `sync_launch_args` 不碰它）：续接时和期望值不同就注入一次；
   注入失败只 warn，下次续接再试。`launch_args` 其余键（`model` `thinking` `mode` `system_prompt_append` `fast_mode` `model_reasoning_summary`）和 `SessionSettings` 同名，
   续接时**不读**它们，只取 thread id 和 `instructions_applied`。

## turn

- `send`：`interject = false` → `turn/start { threadId, input, + 覆盖项 }`，**等响应**（错误当场作为 `send` 的错误返回，没有半截 turn）；
  `interject = true`（daemon 要把消息插进**正在进行的** turn，`capabilities.interject = true`）→ `turn/steer { threadId, expectedTurnId, input }`。
  **插话绝不自己开 turn**：没有进行中的 turn（包括 turn 刚结束、`turn_done` 还在事件队列里没被 daemon 读到），或者 `turn/steer` 报 `no active turn to steer` /
  `expected active turn id …`，一律返回 `AdapterError::TurnEnded`，daemon 把消息退回队列、作为下一个 turn 发（以前这里会自己 `turn/start`，daemon 不知道那个 turn，
  它的事件没有 turn 包着、`turn_done` 被丢掉，会话一直显示 idle）。**已经 `cancel`、turn 还没收尾时再插话也是 `TurnEnded`，一条 `turn/steer` 都不发**（原因见下一条）。
  `interject = false` 而 adapter 里还有 turn 在跑 → `Unsupported`（daemon 不会这样发）。
  附件（`capabilities.attachments` = 四种图片 + `*/*`）：图片 → `{type: "localImage", path}`（png / jpeg / gif / webp）；其它文件不读，`input` 的第一项是列出名字、类型、大小和 daemon 上路径的 `text`（`common::file_reference_text`），Codex 自己去读（沙箱只限制写）。
- **steer 之后 interrupt，steer 的消息会丢**（真实 codex 0.157.1 实测，本地假模型服务 + 临时 `CODEX_HOME`；模型请求在途时 `turn/steer` 一条消息、紧接着 `turn/interrupt`）：
  `turn/steer` 的响应是 `{ turnId }`（接受了），`turn/interrupt` 的响应是 `{}`，turn 以 `interrupted` 收尾；但那条 steer 的消息**既没有变成 `userMessage` 条目，也没有出现在下一个 turn 请求的历史里**
  （下一个 turn 的请求里只有第一条消息、一条 `<turn_aborted>` 提示和新消息），重复两次结果一样。codex 把 steer 的输入放进「待处理输入」，等当前模型请求结束后才并入历史，interrupt 把这一步直接丢了。
  反过来，interrupt 先到、steer 后到：steer 被拒绝，`-32600 no active turn to steer`（在 `turn/completed` 之后才回），已经映射成 `TurnEnded`。
  所以 `cancel` 发出之后 adapter 把插话当成「没有进行中的 turn」：`TurnEnded`、不发 `turn/steer`，daemon 把消息退回队列、作为下一个 turn 发出，不丢。
  `cancel` 之前**已经被接受**的 steer 追不回来（adapter 不知道 codex 有没有把它并入历史）——见「已知限制」。
- **每个 `turn/start` 都带当前的覆盖项**（对「本 turn 及之后」有效）：`model`、`effort`（= thinking）、`summary`、`approvalPolicy`、`approvalsReviewer`；
  `sandboxPolicy` 和 `serviceTier` **只在与线程当前值不同时才传**（`process::Sent`）——没动过它们的会话保留用户 `config.toml` 里
  `[sandbox_workspace_write]`（网络、可写根）和 `service_tier` 的细节，因为 `sandboxPolicy` 是整个策略、不带这些细节。
- `cancel`：先把未答复的审批回 `cancel`，再 `turn/interrupt { threadId, turnId }`；3 秒内没有 `turn/completed` 就补一个 SIGINT（app-server 退出码 0，
  读任务把它记为预期内的取消退出）。取消后新来的审批直接回 `cancel`，不摆给用户。
- **`set_settings`**（传入的是完整目标设置，没给的字段 = 清除并回到默认）：model / thinking / mode / fast_mode / model_reasoning_summary 都是 per-turn 覆盖项，
  **立刻生效**（`applied_now`，下一个 turn 起），不需要重启；`system_prompt_append` 走 `thread/inject_items`（同样 `applied_now`，注入失败 = 整个调用失败，daemon 会标记重启，
  续接时再试）；只有 **`env`** 变了才 `restart_required`（环境是进程启动时定的）。校验在做任何事之前。

## 事件队列与不阻塞约定

daemon 在等 `send` / `respond_permission` / `set_settings` / `cancel` 返回期间**不读** `events()`（`AgentHandle` 文档里的不阻塞约定）。所以读 stdout 的任务**只管读、分类、处理响应和服务端请求**，
事件进一个**不限长的内部队列**，由另一个任务往有界的 `events()` 通道（1024）里送：就算 daemon 一条都不读，`turn/interrupt` / `turn/steer` / `thread/inject_items` 的响应照样能被读到、
审批请求照样能被答复。（旧实现读任务自己 `send().await` 有界通道，输出密集时通道一满就读不到 `turn/interrupt` 的响应，`cancel()` 最多卡 30 秒；Claude Code adapter 的 `set_model` /
`set_permission_mode` 回执同理，也一并改了。）`TurnDone` 的 busy 标志是在读任务里、事件入队的那一刻清的，所以 `TurnDone` 还在队列里时到达的插话得到 `TurnEnded`。
代价：消费者长期不读时队列无界增长（daemon 不会这样；一个失控的 agent 往里灌也是 daemon 的问题）。

## 超长的行

stdout 单行上限 8 MiB。真实 codex 0.157.1 上实测（本地假模型服务 + 临时 `CODEX_HOME`）：

| 消息 | 实测的最大行 |
|---|---|
| `item/commandExecution/outputDelta` | ≈ 10 KB（一个输出 23 MB 的命令是 3033 条，不累积） |
| `item/completed` commandExecution（`aggregatedOutput`） | ≈ 1 MiB（Codex 自己截断，一个 23 MB 输出的命令只有 1.09 MB） |
| 新增 11 MB 文件的补丁：`item/started` / `item/completed` fileChange | **11.3 MB**（整个文件内容在 `changes[].diff` 里） |
| 同一个补丁的 `turn/diff/updated`（整个 turn 的累计 diff，每改一次重发一次） | **11.4 MB**，发了 3 次 |

以前超过 8 MiB 就杀进程，一次大补丁就让整个 turn 以 protocol error 结束。现在：

- `turn/diff/updated`（和另外几个不用的大通知）用 `optOutNotificationMethods` 让 app-server 根本不发。
- Codex 的 stdout 行用 `JsonlChild::spawn_events`：超长的行**整行丢弃**（只记方法名和长度，`warn!`），进程不杀，以 `LineEvent::Oversize { len, prefix }`（前 4 KiB）交给读任务分类：
  - **通知**（顶层有 `method`、没有 `id`）→ 丢弃。`item/started` / `item/completed` 的补丁 / MCP 结果等，用前缀里读得到的 `type` / `id`（和第一个文件的路径、MCP 的 server / tool）补一个
    「`<标题> (details too large to show)`」的 `tool_call` 和 `[result not shown: the message was N bytes, over the 8388608-byte limit]` 的 `tool_result`，客户端的转圈停得下来，
    也不会把 turn 结束时补的「出错」安到一个其实成功了的调用头上；超长的文本 / 思考消息补 `text_done` / `thinking_done`。
  - **待答复的服务端请求**（有 `method` 和 `id`）、**对我们某个请求的响应**、**前缀里认不出是什么** → 才是协议错误：杀进程，`Exited { error: "codex protocol error: …" }`（turn 进行中先发 `agent_exited`）。
- 没有提高行上限：8 MiB 之内的行不受影响，超过的有上面的降级，不值得为一个 11 MB 的补丁让每个行缓冲区都按 64 MiB 预留。
- Claude Code adapter 仍然是「超长 = 杀进程」（它的 `tool_result` 里的整个文件内容没有别的出路）。

## 事件翻译

| app-server | anycon 事件 |
|---|---|
| `item/agentMessage/delta` / `item/completed agentMessage` | `text_delta` / `text_done`（没收到过 delta 的整段补发） |
| `item/reasoning/summaryTextDelta`、`textDelta` | `thinking_delta`（摘要第 2 段起前面补空行）；`item/completed reasoning` → `thinking_done`；没有文本的思考块发一条空 `thinking_delta` |
| `item/started` commandExecution / fileChange / mcpToolCall / webSearch / collabAgentToolCall / subAgentActivity（`kind = started`）/ imageView / dynamicToolCall / imageGeneration | `tool_call`（category：shell / file_edit / mcp / search / subagent / file_read / other；命令的 `/bin/zsh -lc '…'` 壳拆掉再显示，`input` 原样）。`subagent` 只有启动子 agent 的那次调用（`spawnAgent` / `subAgentActivity`），`wait` 等其它 collab 调用是 `other` |
| `item/commandExecution/outputDelta`、`item/fileChange/outputDelta` | `tool_output_delta` |
| `item/completed` | `tool_result`（`declined` / `failed` / 退出码非 0 → `is_error`；命令带 `exit_code`；补丁带 `unified_diff`，并为每个 change 发 `file_changed`：add → created、update → modified、delete → deleted、带 `move_path` 的 update → deleted + created） |
| `turn/plan/updated` | `plan`（`pending` / `inProgress` / `completed`） |
| `thread/tokenUsage/updated` | 累积进 `turn_done.usage`：一个 turn 里每次更新的 `last` 之和（`total` 是整个线程的累计；resume 时 app-server 会重放上一个 turn 的用量，只认当前 turn 的）。`input_tokens` **不含**缓存命中（OpenAI 的 `inputTokens` 包含 `cachedInputTokens`，这里减掉，口径与 Claude Code 一致，`input + cache_read` 不重复计数），没有费用。`context_window` = `modelContextWindow`；`context_tokens` = 最近一次 `last` 的 `inputTokens`（已含缓存命中，不再加 `cachedInputTokens`；压缩后那条更新里 `inputTokens` 是 0，取 `totalTokens`）。只认主 thread 的更新；子 agent 的 `total.totalTokens` 进它的 `task.usage.total_tokens` |
| `turn/completed` | `turn_done`：`completed` → completed，`interrupted` → cancelled，`failed` → error（先发一条 `error` 事件，除非 `error` 通知已经发过）；`usage.duration_ms` 用 `turn.durationMs` |
| `error { willRetry: false }` | `error`（`unauthorized` / 401 → `auth_failed`，用量 / 限流 / 过载 / 429 → `rate_limited`，上下文超限 → `context_window_exceeded`，沙箱错误 → `sandbox_error`，其余 `agent_error`） |
| `error { willRetry: true }`（重连中） | 可恢复的 `error`（`agent_reconnecting`），不收尾 |
| `serverRequest/resolved` | 丢掉对应的未决审批（比如 turn 被打断时 Codex 自己了结的） |
| `subAgentActivity` / `collabAgentToolCall` / 子 thread 的通知 | `task`（`kind = subagent`），子 agent 里的调用带 `parent_id`；子 agent 的文本 / 思考 / 计划 / 错误 / `turn/*` 不碰主 turn。见「子 agent」 |
| `warning`、`guardianWarning` | `notice { agent_notice }`（warning） |
| `configWarning`、`deprecationNotice` | `notice { config_warning }`、`notice { deprecation }`（warning；`summary` 加 `details`） |
| `mcpServer/startupStatus/updated`（`failed`） | `notice { mcp_server_failed }`（warning；`error` 原文，需要重新授权的加一句），每个 server 每个进程一次 |
| `model/rerouted` | `notice { model_fallback, model_from, model_to }`（warning），之后 `usage.model` 是新模型（子 agent 的改道只提示不改） |
| `item/autoApprovalReview/completed`（`denied` / `timedOut`） | `notice { auto_review }`（warning；被拒绝的动作 + `review.rationale`） |
| `hook/completed`（`failed` / `blocked`） | `notice { hook_failed }`（warning；`statusMessage` 或 hook 的输出） |
| `item/started` / `item/completed` contextCompaction | `activity { compacting }` / `activity { none }` + `notice { context_compacted }`（info，`message` 为空）；`activity` 只在主 turn 进行中发 |
| 其余（`thread/status/changed` / `account/*` / `thread/compacted`（已废弃，被 `contextCompaction` item 取代）/ `item/autoApprovalReview/started` / …） | 只进日志 |

**提示（`notice`）同样的文字每个进程只发一次**：`configWarning` 和 `warning` 是同一段话的两条通知，每个子 agent 的 thread 启动时又把 `warning` 发一遍（实测）。

**被打断 / 失败的 turn 不会为进行中的 item 发 `item/completed`**（实测）：`turn/completed` 时 translator 补上 `text_done` / `thinking_done` 和带 `is_error` 的 `tool_result`，客户端的转圈才停得下来。
`userMessage` item 不翻译（用户消息由 daemon 生成）。`plan` item（plan 模式的计划全文）和 `thread/name/updated` 不翻译（标题归 daemon，§ 7.7）。

## 审批与服务端请求

| 请求 | 处理 |
|---|---|
| `item/commandExecution/requestApproval` | `kind = tool`（shell）；options `allow` / `allow_session` / `deny` → `{decision: accept \| acceptForSession \| decline}`。**请求自己带的非空 `command` 为准**（标题 / `detail.command` / `input.command`），`item/started` 里记的只用来补 cwd、`commandActions` 等：zsh-exec-bridge 的子命令审批（`approvalId` 非空）同一个 `itemId` 会有多次回调，每次的 `command` 是那条子命令，用户答复的也是那条；这种标题是 `Run sub-command` |
| `item/fileChange/requestApproval` | `kind = tool`（file_edit）；补丁内容在同一个 item 的 `item/started.changes` 里（请求本身只有 `itemId`、`reason`、`grantRoot`），标题 / 路径 / diff 从那里补；答复同上。显示规则见下 |
| `item/permissions/requestApproval` | `kind = tool`（other，tool = `requestPermissions`）；allow → `{permissions: <请求原样>, scope: "turn"}`，allow_session → `scope: "session"`，deny → `{permissions: {}}` |
| `item/tool/requestUserInput` | `kind = question`；选项 id = label（答复里要回 label），`isOther` / 没有选项 → `free_text`；答 `{answers: {<题 id>: {answers: [...]}}}`；跳过 → `{answers: {}}` |
| `mcpServer/elicitation/request`，`_meta.codex_approval_kind = "mcp_tool_call"` | MCP 工具调用的审批：`kind = tool`（mcp），标题 `Run MCP tool <server>/<tool>`，`input` = `_meta.tool_params`，`detail.description` = `_meta.tool_description`。options：`allow` → `{action: accept}`；`_meta.persist` 含 `session` 才有 `allow_session` → `{action: accept, _meta: {persist: "session"}}`；含 `always` 才有 `allow_always` → `… persist: "always"`；`deny` / 超时 / 取消 → `{action: decline}` |
| `mcpServer/elicitation/request`，`mode = form` | `kind = question`，schema 的每个属性一题（顺序是 app-server 给的字母序）：enum / `oneOf` / `anyOf` → choices（`enumNames` / `title` 做标签），数组 → 多选，boolean → choices `true` / `false`（Yes / No），string / number / integer → `free_text`（数值题的文字里写范围）；`required` 里没有的属性 `Question.optional = true`（必答是默认，题干里不另外标）；说明（`message`）进标题（多行 / 太长的并进第一题）。options `accept`（Submit）/ `decline`。答 `accept` + `answers` → `{action: accept, content: {…}}`，按 schema 类型转换（整数 / 小数 / 布尔 / 数组），没答的非必填项省略；必填缺失、类型不对、不在候选里、超出 `minimum` / `maximum` → 按 `decline` 答复，并发一条可恢复的 `error { elicitation_invalid_answer }` 说明哪一项；`decline`、超时、会话取消 → `{action: decline}`。没有属性的表单（别的确认）→ `kind = other`，只有接受 / 拒绝 |
| `mcpServer/elicitation/request`，`mode = url` | `kind = other`，`detail.text` = `message`，`detail.url` = `url`（只接受 `http` / `https` 且不含空白 / 控制字符，否则按 `decline` 答复并发 `error { elicitation_rejected }`）。options `accept`（I've completed it）→ `{action: accept}`、`decline` |

- 审批请求带的 **`reason` 放在 `detail.tool.detail.description` 的最前面**（web 的审批卡片在命令下面显示这一段），同时保留在 `detail.tool.input.reason`。
- **补丁审批的显示规则**（现在需要审批的往往恰好是写项目外的补丁，显示不能误导）：
  - cwd 里面的路径写相对路径，**cwd 之外的保持绝对路径**（标题、`detail.path`、diff 头一致；`/etc/hosts` 不会再被显示成 `etc/hosts`；含 `..` 的也算项目外）。diff 头：里面 `a/<相对>` / `b/<相对>`，项目外直接是绝对路径。
  - 多文件补丁：标题写清文件数和前 3 个名字（`Edit 3 files (src/a.rs, /home/me/.ssh/authorized_keys, new.txt)`，CLI 通知只显示标题）；`detail.description` 依次是 `reason`、全部路径清单（`- edit …` / `- create …` / `- delete …` / `- move … to …`）、`grantRoot` 的说明；
    全部路径也在 `input.changes[].path` 里；`detail.path` 仍是第一个。
  - `grantRoot` 非空（同时要求本会话写这个根目录）：标题加 `and allow writing under <root> for this session`，说明里加一句 `This also grants write access under <root> for the rest of the session.`。
  - 拿不到 `item/started`（先到的是请求）：标题 `Edit files (patch contents unknown)`，说明以 `The patch contents are unknown: …` 开头，`input.contentUnknown = true`；请求里有的（`reason`、`grantRoot`）照常显示。
- **会话取消时**未答复的审批回 **`cancel`**（`decline` 会让 agent 接着干，`cancel` 同时中断 turn）。实测 `acceptForSession` 即使不在请求的 `availableDecisions` 里也被接受；
  `decline` 之后 agent 继续同一个 turn。
- 子 agent 里的请求（`threadId` 不是主 thread）和主 agent 的走同一条路，标题后加 `(sub-agent <名字>)`，让用户看得出是谁在问。
- 不能交给用户的 elicitation（`openai/form`、`openaiForm` 模式——`requestedSchema` 不限形状，我们不知道它的语义；读不懂的 schema；超过 50 个属性；URL 不是 http(s)）按 `{action: decline}` 答复，并发一条可见的可恢复 `error { code: elicitation_rejected }`（和 Claude Code adapter 同一个 code）。
  MCP server 给的文字 / 候选有长度上限（单段 2000 字节、单个枚举 100 项），免得它撑爆请求。
- **v0.1 不支持的服务端请求按 schema 回拒绝，并发一条可见的 `error { code: unsupported_request }`**（不能挂着不回，否则 turn 卡死）：
  `item/tool/call`（动态工具，我们不注册）→ `{success: false, contentItems: [...]}`；
  旧版 `applyPatchApproval` / `execCommandApproval` → `{decision: "abort"}`；`account/chatgptAuthTokens/refresh`（我们不管 token）和 `attestation/generate` → JSON-RPC 错误 `-32603`；
  任何不认识的方法 → `-32601`。参数形状不对的审批请求 → `-32602`。

## 子 agent、MCP elicitation、压缩上下文（`codex-cli 0.164.0` 实测，2026-10-26）

办法：用和 adapter 一样的 `initialize` 参数（`experimentalApi: false`、同一份 `optOutNotificationMethods`）驱动真 `codex app-server`，逐行记下两个方向的全部消息；
临时 `CODEX_HOME`（`config.toml` 的副本，不碰 `~/.codex`），reasoning effort 调到 low，一共 9 次很短的真实模型调用（子 agent 3 次、MCP 4 次、压缩 1 次、再加一次经过 adapter 的端到端）。
MCP 用一个只靠标准库的 stdio MCP server（一个工具在 `tools/call` 里回头发 `elicitation/create` 的表单 / URL 模式，一个 `readOnlyHint: false` 的工具）。
入库的 fixture 清洗过（`real_subagent_*`、`real_mcp_*`、`real_context_compaction`、`real_startup_notices`）。

**子 agent**

- 启用：`features.multi_agent` 在 0.164.0 是 stable 且默认开（`codex features list`），不用配置；`multi_agent_v2`（stable、默认关）打开后事件形状一样。
- **子 agent 的通知走同一条连接**，各带自己的 `threadId`：`thread/status/changed`、`turn/started`、`item/*`（含 `agentMessage`、`reasoning`、`commandExecution`）、`item/*/delta`、
  `thread/tokenUsage/updated`、`warning`、`turn/completed`。子 thread 的 `turn/started` / `turn/completed` 里 `turn.rootTurnId` 是**主 turn** 的 id（主 turn 的是它自己）。没有子 thread 的 `thread/started`。
  子 agent 的审批请求（`item/commandExecution/requestApproval` 等）也走这条连接，`threadId` 是子 thread、`itemId` 是子 thread 里的 item（item id 全局唯一，所以主 thread 的 `tools` 表照样查得到）。
- **启动子 agent 不是 `collabAgentToolCall`**，而是主 thread 里的 `subAgentActivity { kind: "started", id: <模型那次调用的 id>, agentThreadId, agentPath: "/root/<名字>", model, reasoningEffort }`
  （`item/started` 和 `item/completed` 同一毫秒，在子 thread 的 `turn/started` 之前）。结束时又一个 `subAgentActivity { kind: "completed", id: "subagent-completed-<子 turn id>" }`，紧挨着子 thread 的 `turn/completed`
  （schema 里 `kind` 还有 `interacted`、`interrupted`，没有触发）。主 agent 等子 agent 用 `collabAgentToolCall { tool: "wait", receiverThreadIds: [], agentsStates: {} }`——
  **完成时 `receiverThreadIds` 和 `agentsStates` 也是空的**，子 agent 的结果不在这里。`spawnAgent` 带 `receiverThreadIds` / `agentsStates` 的形状（其它版本 / 工具）只按 schema 手写进 `subagent_collab.jsonl`。
- 旧代码的后果：子 agent 的 `turn/completed` 之所以没结束主 turn，只是因为 turn id 对不上；它的文本、思考、计划会混进主流，工具调用没有 `parent_id`，`subAgentActivity` 被丢弃。
- **现在的处理**（`translate.rs`）：
  - 主 thread 的 id 由 `handle` 在握手后告诉翻译器（回放时从第一个我们自己的 turn 的 `turn/started` 学）。其它 thread 的通知**绝不**碰主 turn：不结束它、不进它的用量 / 计划 / 错误、文本和思考不进主流（和 Claude Code 一致：子 agent 的文本不进主流）。
  - **只有被明确宣布为子 agent 的 thread 才会露面**：`subAgentActivity`、`collabAgentToolCall` 的 `receiverThreadIds`（`spawnAgent`）和 `agentsStates` 的键。别的 thread（比如自动审批的审查 thread，它有自己的 `item/autoApprovalReview/*`）发来的通知一律忽略。
  - `subAgentActivity(started)` → `tool_call`（`spawnAgent`，category `subagent`，`parent_id` 为空；嵌套时指向上一层）+ `task`（`id` = 子 thread id，`tool_id` = 这次调用的 id，`title` = `agentPath` 去掉 `/root/`）；`item/completed` → `tool_result`（`Started sub-agent <名字> (<模型>)`）。
    `collabAgentToolCall(spawnAgent)` 的接收方登记为子 agent，`tool_id` 是这次调用，名字取 prompt 的第一行；`tool_result` 的 `output` 是每个子 agent 一段「名字: 状态」加它的消息，不再是 `agentsStates` 的原始 JSON。
  - 子 agent 里的工具调用 → `tool_call` / `tool_result`，`parent_id` = 启动它的那次调用。**只在主 turn 进行中展示**（turn 之外发出去的 `tool_call` 没有地方收尾）；子 agent 的 turn 结束时它没完成的调用补一条出错的 `tool_result`。
  - `task` 快照（整条取代前一条，没变不重发）：`status` 由 `subAgentActivity` 的 `started` / `interacted`（running）、`completed`、`interrupted`（cancelled）、子 thread 的 `turn/started` / `turn/completed`（`completed` / `interrupted` / `failed`）、
    `agentsStates` 的状态（`pendingInit` / `running` → running，`completed`，`errored` → failed，`interrupted` / `shutdown` / `notFound` → cancelled）和 `thread/closed` 决定；`usage` = 子 thread 累计的 `totalTokens`、工具调用数、最近一个 turn 的耗时；
    `last_tool` = 最近一次调用的名字；`summary` = 它最近一条完整的回复（失败时是错误信息），最多 4 KiB。一个已经 completed 的子 agent 收到 `interacted` 或开始新 turn 会重新变成 running（task 是对子 agent 当前状态的快照，不是一次性的）。
  - 子 agent 在主 turn 结束之后还在跑（后台）：`task` 照常更新，进程退出时 daemon 把没收尾的补成 cancelled。**`AgentEvent::Background { tasks }` = 还在跑（`task` 快照是 `running`）的子 agent 数**，每次通知处理完对一次数、变化就发（含降到 0）：不发的话 daemon 会在 `idle_timeout` 到期时回收进程，把还在干活的子 agent 一起杀掉（回收的条件见 `docs/spec/daemon.md § 7.3`）。降回 0 靠：子 thread 的 `turn/completed`（任何状态）、`subAgentActivity` 的 `completed` / `interrupted`、`agentsStates` 里的终态、`thread/closed`；另外用户取消主 turn（`turn/completed { interrupted }`）时，还在跑的子 agent 一并标成 cancelled（实录里没有观察到「主 turn 被打断时子 agent 一定会收到收尾通知」——`subagent_collab` 里被打断的子 agent 有，是按 schema 手写的——所以不依赖它；子 agent 其实还在跑的话，它之后自己的通知会把状态改回来）。主 turn 失败 / 正常结束时不动子 agent。Codex 的父 agent 不会被子 agent 的完成唤醒，所以不会有 `TurnStarted`；daemon 靠 `turn_finished` 的兜底补发通知。

**MCP elicitation 和 MCP 工具审批**

- **MCP 工具调用的审批走 `mcpServer/elicitation/request`，不是 `item/tool/requestUserInput`**。`readOnlyHint: true` 的工具不问，`readOnlyHint: false` 的在 `on-request` 下问（没标 `readOnlyHint` 的没有实测）。请求：
  `{ threadId, turnId, serverName, mode: "form", message: "Allow the demo MCP server to run tool \"wipe_cache\"?", requestedSchema: {type: "object", properties: {}},
  _meta: { codex_approval_kind: "mcp_tool_call", persist: ["session", "always"], tool_description, tool_params: {…}, tool_params_display: [{name, value, display_name}] } }`。
  请求里**没有 item id 也没有工具名**（工具名在 `message` 里）；adapter 先找参数一致的、正在进行的 `mcpToolCall` 取工具名，找不到再从 `message` 里的 `tool "…"` 取。
  答复 `{action: "accept"}` 放行；`{action: "accept", _meta: {persist: "session"}}` 放行并且**同一个 turn 里第二次调用不再问**（实测）；`persist: "always"` 是「以后都允许」，没有实测；
  `{action: "decline"}` → 该调用 `failed`，`error.message = "user rejected MCP tool call"`；`{action: "cancel"}` → `"user cancelled MCP tool call"`；两种之后 agent 都照常继续同一个 turn。
  adapter 取消时也回 `decline`（随后还有 `turn/interrupt`）。
- **MCP server 自己的 elicitation**：Codex 作为 MCP 客户端在 `initialize` 里声明 `capabilities.elicitation: { form: {}, url: {} }`。server 发 `elicitation/create` 时，app-server 转成同名的 `mcpServer/elicitation/request`：
  `mode: "form"`（`message` + `requestedSchema`，`_meta` 为 null；**属性顺序是字母序**，app-server 不保留 server 给的顺序）或 `mode: "url"`（`message`、`url`、`elicitationId`）。
  答复 `{action: "accept", content: {…}}`（表单）/ `{action: "accept"}`（URL）原样转给 server（server 收到的 `content` 就是我们发的；URL 模式 server 收到 `content: {}`）。schema 里还有 `openai/form`、`openaiForm` 两种 `mode`，没见到。
- 每个请求紧跟一条 `serverRequest/resolved`；请求发出时对应的 `mcpToolCall` item 已经 `inProgress`，答复后才完成。

**压缩上下文**

- `thread/compact/start { threadId }`（响应 `{}`）会开一个**自己的 turn**：`turn/started` → `item/started { type: "contextCompaction", id }` → 几条 `thread/tokenUsage/updated` → `item/completed { contextCompaction }` → `turn/completed`
  （turn 里没有别的 item）。自动压缩发生在我们自己的 turn 里时，同样的两个 item 出现在那个 turn 中间，翻译成 `activity { compacting }` → `activity { none }` + `notice { context_compacted }`。
  已废弃的 `thread/compacted` 通知在这次实测里没有出现。
- 用量口径：`last.totalTokens = inputTokens + outputTokens`，`inputTokens` **已含** `cachedInputTokens`（18150 输入里 3712 缓存命中）。压缩后的那条更新是 `last = { totalTokens: 5985, inputTokens: 0, … }`——
  压缩后的上下文估计只在 `totalTokens` 里，所以 `context_tokens` 取 `inputTokens`，为 0 时取 `totalTokens`。压缩 turn 开头还重复了一次上一个 turn 的 `last`（只在用户触发的压缩里见到，会多算进那个 turn 的累计；自动压缩没有实测）。
- `modelContextWindow` 随每条 `thread/tokenUsage/updated` 给出（实测 258400）。

**启动时的提示**：`configWarning`（没有 `threadId`）在 `initialize` 之后马上到，`warning` 把同一段话在 `thread/start` 之后再发一遍（带 `threadId`），每个子 agent 的 thread 又各发一遍；
`mcpServer/startupStatus/updated` 按 server 报 `starting` → `ready` / `failed`（`error` 是原因，如 `MCP client for \`broken\` failed to start: MCP startup failed: No such file or directory (os error 2)`），同一个 server 的失败只提示一次，子 agent 的 thread 要是再报一遍也不重复。

## 和 Claude Code adapter 的行为差异

- 一个进程的 **thread id 由 Codex 分配**（Claude Code 的 session id 是我们生成的），`Started` 要等握手完成；`spawn` 因此是异步握手，失败在 `spawn` 里报。
- **`interject = true`**：daemon 带 `interject` 发来的消息是 `turn/steer`，不是排队；turn 已经结束、或者已经 `cancel` 而还没收尾，则 `TurnEnded`（Claude Code 对 `interject = true` 一律 `Unsupported`）。
- **设置几乎都能热改**（Claude Code 的思考等级要重启进程）；只有 `env` 要重启。
- `system_prompt_append` 的后续改动走 `thread/inject_items`（追加，不是替换；见上）。
- 取消用 `turn/interrupt`，兜底 SIGINT；审批回 `cancel`。
- 用量没有费用，且 `input_tokens` 已扣除缓存命中。
- 权限模式不同（四档）；没有 `plan` 权限请求、`max_turns`、`max_budget_usd`。
- 重连中的错误是 `agent_reconnecting`（可恢复），Claude Code 没有等价物。

## 测试

- `tests/bin/fake-codex`（Python）：`--version`、`login status`（`FAKE_CODEX_AUTH=in|apikey|out|error|hang`）、`app-server`（回放 fixture，服务端请求要等客户端答复，`turn/steer` / `turn/interrupt` /
  `thread/inject_items` / `thread/resume` / `account/read` 的行为和真的一致，日志记录收到的每条请求；`FAKE_CODEX_*` 开关见脚本头；fixture 里的 `{"_hold": true}` 让 turn 一直开着等客户端打断）。
- `tests/codex_probe.rs`、`tests/codex_process.rs`、`tests/codex_translate.rs`：探测（含 `account/read` 判定、cheap 探测沿用、`account/read` 失败时沿用上一次权威判定）、进程级（握手、参数、审批、取消 / 转向 / 插话、不阻塞、超长行、设置、续接与 `agent_context_reset`、崩溃、关闭）、翻译。
  不阻塞用例：fake 连续输出 5000 条 delta 且不读 `events()` 时，`cancel()` / 插话 / `set_settings` 都要在 5 秒内返回，随后读到全部 5000 条和 `turn_done { cancelled }`。
- `tests/fixtures/codex/`：`real_*.jsonl`、`initialize_response.json`、`model_list.json`、`config_read_*.json`、`thread_*_response.json`、`account_read_{out,custom,apikey}.json` 是用**本机真实的 `codex app-server` 0.157.1**
  录的——模型输出是脚本（本机的 OpenAI 登录不可用），办法是在临时 `CODEX_HOME` 的 `config.toml` 里配一个自定义提供方
  （`wire_api = "responses"`、`base_url = http://127.0.0.1:<port>/v1`），再起一个本地小服务（现在是 `tests/bin/fake-responses`）按 Responses API 的 SSE 格式回放脚本；
  所以协议层（通知顺序、审批请求的形状、`turn/completed` 的各种状态、`serverRequest/resolved`）是真的。录制入库前清洗：路径 → `/work/proj`、`/home/user/.codex`，
  thread / turn id → `FIXTURE_THREAD` / `FIXTURE_TURN`，去掉主机名 / 安装 id（`recordings_carry_no_local_environment` 兜底）。
  `real_subagent_spawn`、`real_subagent_command`、`real_mcp_elicitation`、`real_mcp_tool_declined`、`real_context_compaction`、`real_startup_notices` 是 **`codex-cli 0.164.0` 的真实录制**
  （这几个的模型输出也是真模型的一次很短的调用，不是脚本；模型名 → `fixture-model*`，子 thread / turn → `FIXTURE_CHILD` / `FIXTURE_CHILD_TURN`，item id → `call_1` / `exec_1` / `msg_1`…）。
  其余（`tools_and_plan`、`command_failed`、`reconnecting`、`unsupported_requests`、`long_text`、`two_turns`、`account_read_chatgpt`、`subagent_collab`（`collabAgentToolCall` 的 `spawnAgent` / `wait`、被打断的子 agent）、
  `notices`（每种提示各一条，含重复的）、`mcp_elicitation_forms`（各种类型的表单、非法 URL、`openai/form`、只有 `session` 持久化的审批、空表单））按 schema 手写，`unknown_things` 故意带着不认识的东西。
- `tests/real_codex.rs`（`#[ignore]` + `ANYCON_REAL_AGENT_TESTS=1`）：
  - `real_schema_validates_every_fixture` / `real_schema_validates_what_the_adapter_sends`：用本机 `codex app-server generate-json-schema`（稳定版，**不入库**）校验全部 fixture
    （服务端通知 / 请求、各种响应，含 `account/read`）以及 adapter 对着假 app-server 实际发出的每一条请求、通知和对服务端请求的答复；后者覆盖四档权限模式，包括 `auto_review`
    （线程一开始就是 `auto_review`，和中途切到 `auto_review`）。**不需要登录**，不联网。
  - `real_app_server_handshake`：对真 app-server 跑探测（空 `CODEX_HOME` → `account/read` 说要登录 → `not_authenticated` + 完整模型列表）、`thread/start`、`thread/inject_items`、`thread/resume` 的 "no rollout" 回退
    （连同 `agent_context_reset` 提示）。不碰 `~/.codex`。
  - `real_custom_provider_is_ready_and_survives_a_huge_patch` / `real_cancel_under_heavy_command_output_returns_promptly`：真 app-server + `tests/bin/fake-responses` + 临时 `CODEX_HOME`（**不走用户配置的提供方**，不联网）：
    自定义提供方 `login status` 说未登录、`account/read` 说不需要 → ready，cheap 探测沿用；11 MB 的补丁（真实 codex 会产生 11 MB 的 `item/started` / `item/completed`）不弄死会话；
    命令输出很密（`yes`）时 `cancel()` 毫秒级返回、随后读到 `turn_done { cancelled }`。
  - `real_a_steered_message_is_lost_when_the_turn_is_then_interrupted`：模型请求在途时 `turn/steer` 一条消息、再 `cancel`，下一个 turn 的请求历史里没有那条消息（把「会丢」固定下来，codex 改了行为它会失败）；
    假服务（`tests/bin/fake-responses`）可以把收到的每个模型请求写进日志文件。
  - `real_logged_in_turn`：真的 `pong` 一轮，**会通过用户配置的提供方发一个真实请求**；没登录（`login status` 说未登录、`account/read` 又说要登录）就跳过；留在用户 `~/.codex` 里的 thread 事后归档（可逆）。
    需要时用 `--skip real_logged_in_turn` 跳过。

```text
ANYCON_REAL_AGENT_TESTS=1 cargo test -p anycon-adapters --test real_codex -- --ignored --nocapture --skip real_logged_in_turn
```

## 已知限制

- `plan_mode` 没有（experimental，见上）。`auto_review` 模式下 Codex 的审查：被拒绝 / 超时的是 `notice { auto_review }`，放行的不单独展示（`item/autoApprovalReview/started` 只进日志）。
- `thread/resume` 回退到新 thread 时，如果旧 rollout 其实被删了，上下文会丢（daemon 的事件历史还在）。
- `system_prompt_append` 的后续改动是「追加一条新指令」，旧指令仍留在历史里。
- 不支持动态工具、ChatGPT token 刷新、`openai/form` 模式的 elicitation（都会被拒绝并提示）。
- 超长（> 8 MiB）的通知不看 `threadId`：子 agent 的超长 `item/*` 会被当成主 thread 的补一个「内容没显示」的调用（前缀 4 KiB 里读不到 `threadId`）。
- 子 agent 的 `subAgentActivity` / `collabAgentToolCall` 在子 thread 的第一条通知之后才到的话（`collabAgentToolCall(spawnAgent)` 完成时才带 `receiverThreadIds`），那之前的子 thread 通知被当成"没宣布的 thread"忽略。
  实测 `subAgentActivity` 总在子 thread 的 `turn/started` 之前到，`collabAgentToolCall(spawnAgent)` 的时序没有实测。
- 单个 app-server 进程服务多个会话（共用进程）留到确有需要时再做，那是 adapter 内部的事，不影响协议（ADR-26）。
- **cacheWrite 口径待确认**：`thread/tokenUsage/updated` 里的 `cacheWriteInputTokens` 和 `inputTokens` 是什么关系还不知道。现在 `input_tokens = inputTokens - cachedInputTokens`，`cache_write_tokens = cacheWriteInputTokens`（大于 0 才给）；
  如果 `inputTokens` 本身就包含 cache write，那 `input_tokens` 里还含着 cache write，和 `cache_write_tokens` 重复计数。本机见到的所有真实输出里 `cacheWriteInputTokens` 都是 0，确认不了。
  要确认需要：一个会报 cache write 的提供方（Bedrock / Anthropic 一类）的真实 `thread/tokenUsage/updated`，且 `cacheWriteInputTokens` 非零，最好再带上同一个请求的提供方侧计费用量（输入 / 缓存读 / 缓存写各多少 token）对照，看 `inputTokens` 等于哪几项之和。
- 事件的内部队列无界（不阻塞约定的代价，见上）。
- 超长（> 8 MiB）的 `item/*` 通知只留「内容没显示」的调用 / 结果；补丁的 `file_changed` 事件和 diff 在这种情况下没有。
- `login status` 没变而 `config.toml` 改了提供方时，cheap 探测不会发现（见「认证判定」），要等完整探测。
- 一条已经被 `turn/steer` 接受、但模型还没读到的插话，如果随后用户 `cancel`，会被 codex 连同 turn 一起丢掉（上面「steer 之后 interrupt」）。adapter 没有办法事后知道，
  也不能重发（可能已经并入了历史）；只有 `cancel` 之后才到的插话会被退回队列。
- 完整探测里 `account/read` 失败、而 `login status` 没变时，沿用的是同一个二进制上一次的权威判定：如果 `config.toml` 在这期间改了提供方而 `account/read` 又一直失败，要等它恢复才会更新（`login status` 看不出提供方）。
