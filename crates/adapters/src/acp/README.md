# acp adapter

驱动任何说 [Agent Client Protocol](https://agentclientprotocol.com)（ACP）v1 的 agent：stdio，换行分隔的 JSON-RPC 2.0（**带 `jsonrpc: "2.0"` 头**）。
OpenCode、Gemini CLI、Goose、Kimi Code、Qwen Code、Cursor、GitHub Copilot、Pi 在内置注册表里（`registry.rs`），用户可以覆盖命令、也可以加自己的。规格见 `docs/spec/daemon.md` § 5–9。

**依据的协议版本**：ACP **v1**（`initialize` 的 `protocolVersion: 1`；agent 回别的版本 → `unsupported_version`）。字段名、方法名以官方 JSON Schema
`schema/v1/schema.json`（schema crate **1.28.0**，2026-10-30）和同日的 `docs/protocol/v1/*` 为准，不凭记忆；同一份 schema 入库在
`tests/fixtures/acp/schema.v1.json`，`tests/acp_schema.rs` 用它校验 adapter 发出的每一条消息。**只用稳定的接口**：草案里（`schema.unstable.json`）的东西——`PromptResponse.usage`、
`session/fork`、`session/set_title`、提供方管理、子 agent、MCP-over-ACP、plan 操作…——一律不用；稳定的 `session/list` / `session/delete` / `logout` 也没有用到（`session.import` 是 v0.3）；v2 仍是草案，不支持。

每个 ACP agent 是一个独立的 adapter 实例（`AcpAdapter::new(<名字>)`），id 就是名字，配置在 `config.agents.acp.<名字>`，在 `agent.list` 里是独立的一项（`driver = "acp"`）。

| 模块 | 职责 |
|---|---|
| `registry.rs` | 内置注册表（八个 agent 的 id / 名字 / 命令 / 安装提示）、agent 名字的规则、生效命令（配置优先，其次注册表） |
| `probe.rs` | 找命令（第一个词在 `PATH` / 已知目录里）、`--version`、完整探测（拉起 agent → `initialize` → `session/new` 取选项 → 关掉）、cheap 探测、诊断字段 |
| `options.rs` | `configOptions` / `modes` ↔ `AgentInfo` 的 models / modes / options；每个会话的选项状态（`ConfigState`） |
| `rpc.rs` | JSON-RPC 行分类、请求 / 响应对应（`Rpc`）、错误码常量 |
| `wire.rs` | `initialize` 响应的宽松解析（类型不对的字段当没有，对应 schema 里的 `x-deserialize-default-on-error`） |
| `conn.rs` | **进程**（一个 agent 一个）、进程池、会话（`Session`）、读任务；新开 / 续接 / 脱离会话；进程崩溃的收尾 |
| `handle.rs` | `AgentHandle`：send / respond / cancel / set_settings / shutdown；把设置应用到 agent；图片附件 → 内容块 |
| `translate.rs` | `session/update` 等 → `AgentEvent`（纯同步状态机，直接回放 JSON 测试） |
| `tools.rs` | 工具调用的归一化：`kind` → category、`rawInput` / `locations` / `diff` → `ToolCallDetail`、`content` / `rawOutput` → `tool_result` |
| `diff.rs` | `diff` 内容块（整个文件前 / 后的文本）→ unified diff（Myers，带上限） |
| `permissions.rs` | `session/request_permission` / `elicitation/create` ↔ `PendingPermission` / `PermissionDecision` |

## 命令与进程

命令 = `config.agents.acp.<名字>.command`（可以是绝对路径，或带参数的字符串），没有就用注册表里的：

| id | 名字 | 命令 | 没装时的提示 |
|---|---|---|---|
| `opencode` | OpenCode | `opencode acp` | `install from https://opencode.ai` |
| `gemini` | Gemini CLI | `gemini --acp` | `npm i -g @google/gemini-cli` |
| `goose` | Goose | `goose acp` | `install from https://block.github.io/goose/` |
| `kimi` | Kimi Code | `kimi acp` | `install from https://moonshotai.github.io/kimi-cli/` |
| `qwen` | Qwen Code | `qwen --acp` | `npm i -g @qwen-code/qwen-code` |
| `cursor` | Cursor | `cursor-agent acp` | `install from https://cursor.com/docs/cli/acp` |
| `copilot` | GitHub Copilot | `copilot --acp` | `npm i -g @github/copilot` |
| `pi` | Pi | `pi-acp` | `npm i -g pi-acp` |

命令和包名取自 [ACP 官方注册表](https://github.com/agentclientprotocol/registry)（只收录用户机器上已经有二进制的启动方式）；id 按命令名取，和注册表里的 id 不一定一样。
Claude Code 和 Codex 也有 ACP 封装（`claude-agent-acp`、`codex-acp`），anycon 不用它们：原生 adapter 的控制面更完整。

**安装判定**：命令的第一个词在 daemon 环境的 `PATH`（再加 `~/.local/bin`、`~/.bun/bin`、`~/.cargo/bin`、`/opt/homebrew/bin`、`/usr/local/bin`、nvm / volta / npm-global 的目录）里
找得到才算安装，**不跑 `npx` / `uvx`**（那会在用户机器上下载并执行东西）。写绝对路径的直接用，必须是绝对路径（相对路径会按子进程的 cwd，也就是项目目录解析）。
`<程序> --version` 尽力而为：ACP agent 不一定认它，能起来就算装了；输出里解析得出 semver 才给版本，完整探测里 `initialize` 返回的 `agentInfo.version` 优先。

**自己加的 agent** 必须写 `command`（没有默认命令），名字只能是小写字母 / 数字 / `-` / `_`、最多 32 个字符，不能是保留字（`claude-code`、`codex`、`mock`、`acp`、`env`、…）；
写成 `[agents.<名字>]` 而不是 `[agents.acp.<名字>]` 会在加载配置时报错。自己加的 agent 的名字取自它 `initialize` 返回的 `agentInfo`（`title`，没有就是 `name`），没有就用 id；注册表里的用注册表的名字。

**拉起**：cwd 是一个中性目录（系统临时目录）——会话的目录在 `session/new` 里给，规格要求 agent 按它而不是进程 cwd；环境 = `SpawnRequest.env` **原样**（daemon 环境 + `agents.env` +
`agents.acp.<名字>.env`）；进程在**自己的进程组**里，stdout 单行上限 8 MiB（来自 `common/child.rs`，和别的 adapter 一样）：超长的**通知**（一个很大的工具输出）整行丢弃并警告，
超长的响应 / 待答复的请求 / 认不出的才是协议错误——整个进程停掉（`Exited { error: "… protocol error …" }`）。关闭 = 关 stdin → 等 300 ms → SIGINT → 5 秒 → SIGKILL（整个进程组），
leader 一退出就清整个进程组。

## 一个 agent 一个进程，服务它的所有会话

`conn::Pool`（每个 `AcpAdapter` 实例一个）管这个进程：

- 第一个会话 `spawn` 时拉起进程并 `initialize`；之后的会话直接用同一个进程，各自 `session/new`（或续接）。起进程是串行的（第二个会话等第一个把进程起好）。
- 进程按**挂着的句柄数**计数。daemon 回收一个会话（`idle_timeout`、删除、归档、重启会话、daemon 退出）是调那个句柄的 `shutdown()`：会话脱离，其它会话照常跑；
  **最后一个句柄走的时候才关进程**（等它退完，daemon 据此知道旧进程收尾了）。所以「按 `idle_timeout` 退出」是该 agent 的会话**都**回收完了进程才退。
- agent 声明了 `sessionCapabilities.resume` 或 `loadSession` 时 `capabilities.resume = true`，daemon 才会按 `idle_timeout` 回收会话，下次 `send` 走续接；
  **两者都没有的 agent，daemon 不回收它的会话**，进程一直活到会话被删除 / 归档（规格：进程保活到所有会话 `ended`）。
- 脱离时 agent 有 `session/close` 就关掉那个会话（取消还在跑的活、释放资源）；没有就让它留在进程里（只要进程还活着，下次续接直接接上，不发请求），除非它正忙（留着一个还在跑 turn 的会话没法再接，摘掉）。
- 启动方式变了（命令 / 环境改了，指纹不同）：下一个会话另起一个进程，旧的在它的句柄都走完后退出。
- **没有会话级的环境**：进程是共享的，`SessionSettings.env` 在校验时就被拒绝（`invalid_settings { field: "env" }`），要给 agent 环境变量写 `agents.acp.<名字>.env`。`capabilities.env = false`。
- `AgentHandle::pid()` 是共享进程的 pid。预览（ADR-39）按 pid 判断监听端口是不是会话起的，所以同一个 agent 的所有会话互相看得到对方起的 dev server。

**进程崩溃 / 被杀**：stdout 关闭，每个还挂着句柄的会话各自收尾：还开着的文字 / 思考 / 工具调用补上结果（工具调用补 `is_error` 的 `tool_result`），中途崩溃的先发一条可恢复的
`error { agent_exited }`，然后 `Exited { error }`（没有 turn 时同样 `Exited`；不补 `turn_done`，daemon 处理 `Exited` 时收尾 turn）。daemon 据此把各个会话标成 `error`，下一条消息走续接；
下一个 `spawn` 发现旧进程死了就起新的。

## client 能力（`initialize`）

| 声明 | 为什么 |
|---|---|
| `fs: { readTextFile: false, writeTextFile: false }`，`terminal: false` | **不声明**：agent 用它自己的工具读写文件、跑命令，权限走 `session/request_permission`。agent 还是发 `fs/*` / `terminal/*` 的话答 `-32601`；自定义（`_` 开头）的方法同理 |
| `elicitation: { form: {}, url: {} }` | 规格 § 9.1：agent 向用户要结构化输入走权限通道（`question` / `other`）。注意 ACP 里 `{}` 不等于表单支持，两个模式要各写一个 |
| `session.configOptions.boolean: {}` | 规格 § 5.4：`select` 和 `boolean` 两种选项都透传；不声明的话 agent 不会给布尔选项 |
| **不声明** `auth.terminal`、`session.notices`、`session.compaction` | 我们没有交互式终端；规格说 ACP 不产生 notice / task / activity 事件，agent 没有声明就不会发（用一条普通消息代替） |

`clientInfo` = `{ name: "anycon", title: "anycon", version }`。

## 认证

**adapter 不调 `authenticate`**：登录方式可能要打开浏览器或读终端输入，daemon 做不了。探测里 `session/new` 报 `-32000`（`auth_required`）→ `not_authenticated`（`login_required`），消息里列出
`authMethods[].name`；`type = terminal` 的登录方式（用户要在机器上跑一次命令）给出命令：`<配置的命令> <方法的 args>`（带上方法的 `env`），其余的提示用 agent 自己的 CLI 在机器上登录一次。
会话里 `session/new` / `session/resume` 报 `-32000` → `AdapterError::NotAuthenticated`（daemon 立刻重探测）；`session/prompt` 报 `-32000` → `error { auth_failed }` + `turn_done { error }`。
登录之前拿不到模型列表，沿用上一次探测的（没有就是空）。

## 探测

1. **命令**：见上。没有命令 → `error`（`no_command`，自己加的 agent 没写 `command`）；命令解析不了 / 相对路径 → `error`（`invalid_command`）；找不到 → `not_installed`（`binary_not_found`，hint 是安装方式）；
   找到但起不来（没有执行权限…）→ `error`（`binary_failed`）。
2. **版本**：`<程序> --version`，2 秒超时（`ANYCON_VERSION_TIMEOUT_MS` 可调），第一个**能起来**的候选。
3. **完整探测**（首次、`agent.list { refresh }`、配置变了、cheap 探测发现变化）：在 `ctx.scratch_dir` 下拉起 agent → `initialize`（上限 20 秒）→ `session/new { cwd: scratch, mcpServers: [] }`（上限 40 秒）→ 有 `session/close` 就关掉探测用的会话
   → 关进程。失败：握手失败（进程退出 / 超时 / 不是 ACP）→ `error`（`handshake_failed`，带 agent 的 stderr 尾巴）；协议版本不是 1 → `unsupported_version`（`protocol_version`）；`session/new` 失败 → `error`（`session_new_failed`）；总预算（`agents.probe_timeout`）用完 → `error`（`probe_timeout`）。
4. **cheap 探测**（`probe_interval` 的周期探测）只跑第 1、2 步：路径和 `--version` 的输出都和上一次完整探测一样（记在 adapter 实例的 `ProbeMemo` 里）、上一次是 `ready` → 沿用上一次的结果，不拉起 agent；不一样、或者上一次不是 `ready`（登录状态 cheap 探测看不出来）→ 做完整探测。

探测结果里的能力：

| `capabilities` | 值 |
|---|---|
| `resume` | `sessionCapabilities.resume` **或** `loadSession` |
| `interrupt` | `true`（`session/cancel`） |
| `interject` | `false`（ACP v1 没有往进行中的 turn 里插话） |
| `attachments` | `promptCapabilities.image` 为真 → `common::IMAGE_MIMES`（png / jpeg / gif / webp），否则空 |
| `thinking_stream` / `usage` | `true`（事件有定义；agent 不发就没有） |
| `structured_output` / `env` / `rewind` | `false` |

## 会话选项（`configOptions` / `modes`）

模型、思考等级、模式、其余选项全部来自 agent 在 `session/new`（`session/load`、`session/resume`、`session/set_config_option`）响应里给的 `configOptions`，每个会话有自己的一份状态，
agent 随时可以用 `config_option_update` / `current_mode_update` 改它。同一个类别有多个选项时取排在前面的（规格要求按数组顺序）。

| `AgentInfo` | 来源 | 设置时 |
|---|---|---|
| `models` | `category = model` 的 select 的各项（分组的取组名作 `tags`）；`default` = 探测时的 `currentValue`。**没有这个选项就不提供模型选择**（旧的 `models` 字段 / `session/set_model` 不在 v1 稳定 schema 里，不支持）。`allow_custom_model = false`：ACP 只接受选项列表里的值 | `session/set_config_option { configId: <那个选项的 id>, value }` |
| `models[].thinking` | `category = thought_level` 的 select（`default` = 探测时的 `currentValue`）。ACP 的等级**不按模型区分**：探测时看到的是默认模型的那一组，所有模型共用；换模型后 agent 可能给出别的等级 | 同上，`configId` 为 `thought_level` 那一项 |
| `modes` | `category = mode` 的 select；没有就用旧的 `modes.availableModes`。ACP 不给风险等级：全部 `normal`，`config.agents.acp.<名字>.dangerous_modes` 标 dangerous（daemon 叠加） | 有 `mode` 选项用 `session/set_config_option`，否则 `session/set_mode { modeId }` |
| `options` | 其余的 `select` / `boolean`（`model_config` 和自定义类别都在这里），`default` = 探测时的 `currentValue`；别的 `type` 忽略（规格：收到不认识的 type 就当没有这个选项） | `session/set_config_option`；布尔带 `type: "boolean"` |

**设置怎么应用**（`handle::apply_settings`）：顺序是模型 → 思考等级 → 模式 → 其余选项（换模型会让 agent 改思考等级的候选，每一步都看最新的选项状态）。
**拉起时**把会话的完整设置和 agent 现在的值比，不一样的才发请求（daemon 给的默认值就是探测时的 `currentValue`，所以通常一个请求都不发）；**会话中 `set_settings`** 只发和**上一次请求的值**不一样的
——agent 自己切了模式（比如批准计划之后）不会被 daemon 记着的旧值冲掉（代价：daemon 记着的 `settings.mode` 和 agent 实际的模式可能不一致，下次拉起时按 daemon 记的重新设）。
所有设置都**随时生效**（`applied_now`），不需要重启进程；turn 进行中 daemon 本来就锁定设置。
应用不上的：**拉起时**不让会话失败，作为可恢复的 `error { agent_setting_failed }` 告诉用户，会话照常用 agent 当前的值；**`set_settings`** 返回 `invalid_settings { field, reason }`。
`options` 里被清掉（`null`）的键不会还原已经设下去的值（下次拉起才回到默认）。

**不支持的设置**（校验时 `invalid_settings`）：`env`（见上）、`system_prompt_append`、`max_turns`、`max_budget_usd`（ACP v1 没有对应项）。

## turn

- `send`：`session/prompt { sessionId, prompt: [ContentBlock] }`。请求写出去就返回（一个 turn 可以跑很久，`AgentHandle::send` 不能等它），等响应的任务在响应到达时收尾 turn。
  附件：图片 → `image` 内容块（`{ type, data: base64, mimeType }`，图在前、文字在后；只有图片时不发空文本）；agent 没声明 `promptCapabilities.image` 或不是图片 → `Unsupported`。
- `interject = true` → `Unsupported`（`capabilities.interject = false`，daemon 不会这样发）。
- `cancel`：先把未答复的权限请求 / elicitation 按取消答复（`{ outcome: "cancelled" }` / `{ action: "cancel" }`——规格要求取消时必须这样答复所有未决请求），再发 `session/cancel` 通知；
  agent 以 `stopReason: cancelled` 结束 prompt → `turn_done { cancelled }`。agent 在取消之后回的任何 JSON-RPC 错误也按取消算（规格说 agent 应当捕获，但 API 库被中止时常有漏网的）。
  **不发 SIGINT 兜底**：进程是这个 agent 所有会话共用的，杀了会连累别的会话；agent 迟迟不收尾由 daemon 的取消宽限处理（它会让这个句柄 `shutdown`）。取消不杀进程。
- 事件在 daemon 等 `send` / `cancel` 返回期间不被读：每个会话的事件先进一个不限长的内部队列，由另一个任务往有界的 `events()` 通道（1024）里送，读 stdout 的任务不会被一个没人读的会话拖住（`AgentHandle` 的不阻塞约定）。
  代价：消费者长期不读时队列无界增长（daemon 不会这样）。

## 事件翻译

| ACP | anycon 事件 |
|---|---|
| `agent_message_chunk`（`text`） | `text_delta`；`messageId` 变了、被别的内容打断、turn 结束时 `text_done` |
| `agent_thought_chunk`（`text`） | `thinking_delta`（空文本也发）；收口时 `thinking_done { duration_ms }` |
| `tool_call` / `tool_call_update` | 按 `toolCallId` 合并成一次调用。**`tool_call` 事件要等到有内容才发**（很多 agent 先发空壳的 `pending`，`rawInput` 随后才补上，而事件一旦发出就改不了）：第一次有输入 / 内容 / 位置、状态不再是 `pending`、权限请求引用它、后面出现别的内容、或 turn 结束时才发，发的时候先补上排在前面还没发的。`completed` / `failed` → `tool_result`（`failed` → `is_error`） |
| `kind` | `execute` → `shell`；`edit` / `delete` / `move` → `file_edit`；`read` → `file_read`；`search` → `search`；`fetch` → `web`；`think` / `switch_mode` / `other` / 不认识的 → `other`（ACP 没有 `subagent` / `mcp` 这两个 category） |
| `tool_call.name` / `title` / `input` | `name`（没有就用 `kind`）/ `title`（没有就是 name）/ `rawInput`（没有就是 `{}`） |
| `tool_call.detail` | 按 category 在 `rawInput` 里找常见的键名（尽力而为，找不到就留空）：shell → `command`（字符串或 `["bash","-lc",…]` 数组）、`cwd`；file_edit → `path`（`locations` 优先，其次 diff、`rawInput.path`…）、`edit_kind`；file_read → `path`；search → `query` / `path`；web → `url` |
| `tool_result.output` | 内容里的文字（`content` 文本块、内嵌文本资源）优先，其次 `rawOutput`，最后是 diff 的一句话概述（`Edited a.txt`） |
| `tool_result.detail` | shell 的 `exit_code`（`rawOutput` 的 `exit_code` / `exitCode` / `metadata.exit`…）；有 `diff` 内容的编辑 → `unified_diff`（路径在 cwd 里写相对，之外写绝对） |
| `content[].type = diff` | 调用成功完成时每个 diff 一个 `file_changed`（`oldText` 为空 → created，`delete` 类调用 → deleted，其余 modified）；编辑类调用没有 diff 块时按 `locations`（`edit` → modified，`delete` → deleted） |
| `in_progress` 时内容文字在末尾增长的部分 | `tool_output_delta`（内容被换成别的文字就不发，最终 `tool_result` 带完整输出） |
| `content[].type = terminal` | 不翻译：我们没有声明 `terminal`，agent 不该给 |
| `plan` | `plan`（`pending` / `in_progress` / `completed`；`priority` 丢掉） |
| `session_info_update.title` | `AgentEvent::Title`（同一个标题只发一次；agent 起的标题优先于 daemon 的默认标题；`null` 不清掉） |
| `usage_update` | 累积进 `turn_done.usage`：`context_tokens` = `used`、`context_window` = `size`（取最近一次）；`cost_usd` = 累计费用相对上一个 turn 结束时的**增量**（只认 `currency = USD`；续接的会话不知道基线，第一个 turn 不报费用）；`model` = 当前模型选项的值；`duration_ms` = prompt 发出到响应的耗时。ACP v1 稳定版没有 token 数，所以 `input_tokens` / `output_tokens` 为空 |
| `session/prompt` 的 `stopReason` | `end_turn` → `completed`；`cancelled`；`max_tokens` / `max_turn_requests` → `max_turns`（daemon 把它当会话「正常结束」：状态 `ended`，进程还活着、能继续发消息）；`refusal` → `error`（先发一条可恢复的 `error { agent_refusal }`）；不认识的 → `completed` |
| `session/prompt` 的 JSON-RPC 错误 | `-32800`（或要求过取消之后任何错误）→ `cancelled`；`-32000` → `error { auth_failed }`；其余 `error { agent_error }`；都以 `turn_done { error }` 收尾 |
| turn 结束时还没收尾的工具调用 | 补一条 `tool_result`：取消 / 出错的标 `is_error`（输出 `Cancelled before it finished` / `Interrupted`），正常结束的不标 |
| `user_message_chunk`、`available_commands_update` | 不翻译（用户消息由 daemon 生成；斜杠命令列表没有对应的事件） |
| `current_mode_update`、`config_option_update` | 更新会话的选项状态（见上），不进事件流（`settings_changed` 归 daemon 管） |
| `notice`、`compaction_*` | 不翻译：我们没有声明这些能力 |
| 没有 turn 在跑时到达的消息 / 工具更新 | 丢弃（`session/load` 重放的历史就是这样被吞掉的） |

## 权限与 elicitation

| 请求 | kind | 选项 | 答复 |
|---|---|---|---|
| `session/request_permission` | `tool`（`toolCall.kind = switch_mode` 时是 `mode_change`，`detail.text` 是计划文字） | **透传 agent 给的 options**：`optionId` → `id`，`name` → `label`，`kind` → `allow_once` / `allow_always` / `reject_once` / `reject_always`（不认识的当 `allow_once`：只影响颜色，daemon 自动拒绝时只挑 `reject_*`）。`needs_message` 恒为假（ACP 答复里没地方放理由） | `{ outcome: { outcome: "selected", optionId } }`，`optionId` 原样回；没带选项（自动拒绝）→ 挑 `reject_once`，再 `reject_always`，都没有就按取消答复（绝不当放行）；会话取消 → `{ outcome: "cancelled" }` |
| `elicitation/create`，`mode = form` | `question`（`requestedSchema` 的每个属性一题；没有属性的表单是 `other`） | `accept`（Submit）/ `decline` | `{ action: "accept", content }`（答案按 schema 转成 JSON 类型：integer / number / boolean / enum 的原值 / 多选数组，没答的可选项不出现）/ `{ action: "decline" }`；回答和 schema 对不上 → 按 `decline` 答复并发可恢复的 `error { elicitation_invalid_answer }`；会话取消 → `{ action: "cancel" }` |
| `elicitation/create`，`mode = url` | `other`（`detail.url`，只接受 http / https） | `accept`（I've completed it）/ `decline` | `{ action: "accept" }` / `{ action: "decline" }` / `{ action: "cancel" }` |

- 表单的转换和 Codex 共用（`common::elicit`）；agent 给的文字不可信，标题只取第一行，单段文字 2000 字节、枚举 100 项、表单 50 个属性封顶，超了按 `decline` 答复。
- 不认识的模式、读不懂的 schema、链接不是 http(s) → 按 `decline` 答复并发可恢复的 `error { elicitation_rejected }`。**请求级**的 elicitation（没有 `sessionId`，不属于任何会话）没人能答复 → `decline`。
- 没有 turn 在跑 / 正在取消 / 会话在重放历史时到达的请求没人能答复，直接答复免得 agent 一直等：权限请求按 `cancelled`，elicitation 按 `decline`（正在取消时是 `cancel`）。一个选项都读不出来的权限请求 → `cancelled`，并发可恢复的 `error { unsupported_request }`。
- agent 取消了它自己发的请求（`$/cancel_request`）→ 按取消答复，从未决里摘掉（daemon 那边的权限卡片留着直到 turn 结束，之后再答复是空操作）。
- `session/request_permission` 的 `toolCall` 和 `tool_call_update` 同一个形状：合进已知的那次调用，并且这个调用此刻就会出现在事件流里（卡片引用它）。
- `allow_always` / `reject_always` 的记忆由 agent 自己管（ACP 规定），anycon 不替它记。

## 会话的新开、续接、脱离

- **新会话**：`session/new { cwd, mcpServers: [] }`（不传 MCP server：规格建议客户端不要把工作区里读到的 stdio server 配置发给 agent）。`Started` 带 agent 的 `sessionId` 作 `agent_session_id`，`launch_args` 是当前设置的快照
  （`model` / `thinking` / `mode` / `options`，只是留痕，续接时**不读**它，设置按 daemon 给的完整设置重新应用）。
- **续接**（daemon 带着记下的 `agent_session_id`）：进程里还有这个会话 → 直接接上，不发请求；否则有 `session/resume` 用它（不重放），其次 `session/load`（重放全部历史：先登记会话、期间的更新一律吞掉，
  只取最后的选项状态，响应回来才放行）。**接不上就开新会话并明说**：`session/resume` / `load` 报 `-32002` 或消息里说找不到会话，或者 agent 两个能力都没有 → `session/new`，`Started` 带新 id，
  紧跟一条可恢复的 `error { agent_context_reset }`（之前的上下文 agent 看不到了；daemon 的事件历史还在）。其它续接错误照常报错（`-32000` → 未认证）。
- **脱离**：见「一个 agent 一个进程」。`shutdown` 幂等（daemon 会调两次），先发 `Exited { error: None }` 再关事件流。

## 已知限制

- **不支持 `session/set_model` / `models` 字段**（v1 稳定 schema 里没有）：agent 只在 `configOptions` 里给模型选择器才有模型选择。
- **思考等级不按模型区分**（见上）：换模型后 agent 给了别的等级，daemon 用探测时的那组校验，agent 不接受的值在 `set_settings` 时报 `invalid_settings`、拉起时是 `agent_setting_failed`。
- **没有 token 数**（`input_tokens` / `output_tokens` 为空）：`PromptResponse.usage` 在草案里，是单 turn 还是累计还没定，所以不读；只有上下文占用和（USD 的）费用。
- **不支持**：`fs` / `terminal`（agent 用自己的工具）、终端登录（`auth.terminal`）、`authenticate`、notice / compaction、斜杠命令列表、MCP server 配置、`additionalDirectories`、`session/list` / `fork` / `delete`（`session.fork` / `session.import` 是 v0.3）、`logout`。
- **没有取消的 SIGINT 兜底**（共享进程）：不理 `session/cancel` 的 agent 只能靠 daemon 的取消宽限（它让句柄 `shutdown`，有 `session/close` 的 agent 会因此停掉那个会话；没有的会话从池里摘掉，agent 里的活可能还在跑）。
- **agent 自己切了模式** daemon 不知道（`settings_changed` 归 daemon 管，adapter 不能发）：`settings.mode` 和实际模式可能不一致；`switch_mode` 类的权限请求没有 `detail.mode`（每个选项对应的目标模式 ACP 没规定），所以批准计划不会让 daemon 跟着改模式。
- **预览的端口归属**按进程组，同一个 agent 的所有会话共用一个组（见上）。
- 脱离但没关掉的会话（agent 没有 `session/close`）占着 agent 的内存，直到进程退出。
- 事件的内部队列无界（不阻塞约定的代价，同 Codex）。
- 没有实机验证：真实 agent 只有 OpenCode 的冒烟测试（见下），本机没有装任何 ACP agent，所以那条测试没有跑过；其余 agent 的命令来自注册表，行为差异（比如各家 `rawInput` 的形状、有没有 `session/close`）靠宽松解析兜底。

## 测试

- `tests/bin/fake-acp-agent`（Python，一个进程多个会话）：`initialize`（可以失败 / 挂住 / 回 v2 / 不回版本）、登录要求（`agent` / `terminal` 两种方式）、`session/new` / `resume` / `load`（状态存在 `FAKE_ACP_STATE` 里，跨进程）/ `close`、
  `session/set_mode` 和 `set_config_option`（换模型会改思考等级的候选）、按 prompt 里的关键词流式输出 / 工具调用（含先空壳后补参数）/ 编辑（diff）/ 计划 / 标题 / 权限请求 / elicitation / 等待取消 / 崩溃 / 各种错误和 stop reason / 要 `fs` 的请求 / 自己切模式；
  `FAKE_ACP_LOG` 记下收发的每条消息（带 pid）。开关见脚本头。
- `tests/acp_probe.rs`：命令查找、没装的 hint、坏命令、禁用、`--version` 不认、登录要求、握手失败 / 版本不对 / 超时、各种配置形态、能力、cheap 探测沿用与重探、诊断。
- `tests/acp_process.rs`：握手和参数、设置、事件、权限（透传、取消、自动拒绝）、elicitation、会话中改设置、附件、**一个进程服务多个会话**、续接（`resume` / `load` / 会话不在了 / 没有能力）、崩溃、关闭、丢弃句柄。
- `tests/acp_schema.rs`：用入库的官方 v1 JSON Schema（`tests/fixtures/acp/schema.v1.json`，来源和更新办法见同目录 README）校验 adapter 发的每一条消息（请求 / 通知的 `params`、对 agent 请求的答复），以及假 agent 发的每一条消息（它得是合法的 ACP，adapter 才算对着真实形状的流量测过）。
- `src/acp/*` 里的单元测试：翻译、工具归一化、diff、选项、权限、注册表、`initialize` 解析。daemon 侧见 `crates/daemon/tests/acp_agent.rs`（假 agent 经 daemon 跑一遍：列表、会话、权限、取消、选项、共用进程、idle 回收后续接、崩溃、配置热增减）。
- `tests/real_acp.rs`（`#[ignore]` + `ANYCON_REAL_AGENT_TESTS=1`）：OpenCode。`real_opencode_probe` 只探测（不发消息，不花 token）；`real_opencode_turn` 会发一条很短的消息，**花一个真实请求**，需要时 `--skip real_opencode_turn`。没装 `opencode` 就跳过。

```text
ANYCON_REAL_AGENT_TESTS=1 cargo test -p anycon-adapters --test real_acp -- --ignored --nocapture
```
