# claude-code adapter

驱动 `claude -p --output-format stream-json --input-format stream-json` 的双向模式：stdin 常开，一行一条 JSON；
权限走 `control_request { subtype: can_use_tool }` / `control_response`。规格见 `docs/spec/daemon.md` § 5–9。
下面的「实测」都来自真实 CLI **2.1.293**（本机已登录，2026-10-11；workflow / 本地命令 / elicitation / 上下文用量那几节是 **2.1.308**，2026-10-26，haiku 5.5），录制（已脱敏）在 `tests/fixtures/claude/real_*.jsonl`。
录制入库前必须清洗（仓库公开）：`system/init` 里的 `slash_commands` / `skills` / `plugins` / `mcp_servers` / `tools` 换成 `example-*` 占位（保留结构和数量），
模型输出和流式 delta 里的本机路径换成 `/work/proj`、`/home/user`（delta 被切碎了，要按 content block 拼起来再替换；workflow 的转录目录、任务输出文件、`memory_paths`、`messaging_socket_path` 同理，后两个直接删掉），`rate_limit_event` 的用量比例换成占位，
thinking 块的 `signature` 换成短占位，`/context` 输出里的技能名换成 `example-skill-N`，技能库的长文本（`# Workflow authoring reference`）换成一句说明，MCP server 名换成 `example-*`。
`tests/claude_translate.rs::real_recordings_carry_no_local_environment` 兜底：init 里的名字必须是 `example-*`，全文（含拼回去的 delta）不能有 `/Users/`、`/private/`、`/tmp/`、
其它 `/home/<名字>`、`@`、常见密钥前缀。

| 模块 | 职责 |
|---|---|
| `probe.rs` | 找二进制（config → `which -a` → 已知目录，逐个 `--version`）、版本门槛（≥ 2.1.0）、`claude auth status`、诊断字段 |
| `models.rs` | 内置模型表（按模型挂思考等级）+ `~/.claude/settings.json` / 环境里的模型 + `additional_models` |
| `process.rs` | 会话设置 → 启动参数（`--flag=value` + 值校验）/ 环境；找要执行的程序 |
| `translate.rs` | stream-json 一行 → `AgentEvent`（纯同步状态机，直接回放 JSONL 测试） |
| `permissions.rs` | `can_use_tool` ↔ `PendingPermission`，`PermissionDecision` → `control_response` |
| `tools.rs` | 工具名 → category / 标题 / `ToolCallDetail` |
| `input.rs` | 用户输入 → stdin 上的 stream-json 用户消息；图片附件读文件、转 base64、拼 `image` block |
| `handle.rs` | `AgentHandle`：进程 + 读 stdout 的任务；send / respond / cancel / set_settings / shutdown |

## 启动参数

```
claude -p --output-format=stream-json --input-format=stream-json --verbose --include-partial-messages
       --permission-prompt-tool=stdio
       --model=<m> [--effort=<e>] --permission-mode=<mode> [--dangerously-skip-permissions]
       [--append-system-prompt=S] [--max-turns=N] [--max-budget-usd=X] [--settings={"fastMode":true}]
       (--resume=<agent_session_id> | --session-id=<新 uuid>)
```

cwd = `req.cwd`；环境 = `req.env` **原样**（剔除 `ENV_STRIP` 即 `CLAUDECODE` 等「嵌套会话」标记现在只由 daemon 的 `env.rs` 做一次，adapter 不再剔；
adapter 唯一的改动是关思考时加 `MAX_THINKING_TOKENS=0`）；stdin 保持打开；子进程在**自己的进程组**里。

**不加 `--add-dir`**：argv 只由会话设置决定，和 cwd 无关；cwd 是 git worktree（`<项目>/.anycon/worktrees/<slug>`，ADR-34）时和普通目录完全一样。
不把主仓库的 `.git` 放进可写目录（ADR-34 第 9 条）：那样 agent 不经审批就能往 `hooks/` 里写脚本，用户之后在终端里 `git commit` 会执行它们；adapter 不预先放行，写到 cwd 之外的动作走 agent 自己的权限流程。
回归测试 `argv_never_adds_extra_directories`（`process.rs`）。

**进程组总会被清掉**：leader（claude）一退出——自己退、被 SIGKILL、崩溃都一样——等待任务立刻对整个进程组 `killpg(SIGKILL)` 一次，
所以 Bash 工具派生的后台进程（dev server 之类）不会在 claude 退出后残留；`kill()` / `Drop` / `shutdown()` 在 leader 还没被回收时同样 `killpg`。
leader 被回收之后不再对这个 pid / pgid 发信号（pid 可能被复用，再 `killpg` 会误杀别的进程组）：回收发生在 `Child::wait()` 被 poll 的那一刻，
`common::child::Reaped` 把每次 poll 和「查标志 → 发信号」放在同一把锁里，所以两者之间 leader 不可能被回收；`run_capture`（`--version` 等）的超时分支同理，
已经回收就不再 `killpg`。唯一剩下的窗口是回收之后那一次清理 `killpg` 本身（macOS 没有 `waitid(WNOWAIT)` 可以先清后收）：进程组里还有幸存者时 pgid 不会被复用，
空了的话要误杀必须在这几微秒里让 pid 空间整整转一圈。SIGINT 只发给 claude 本身，让它自己收尾。

**stdout 单行上限 8 MiB**：超过就是协议错误——杀进程组、关事件通道，turn 进行中会先发 `Error { code: agent_exited }`，
`Exited.error` 是 `claude protocol error: a stdout line was longer than …`。stderr 单行只留前 8 KiB。一次性调用（`--version`、`auth status`、`type -a`）的 stdout / stderr 各只留前 1 MiB。

**argv 安全**：每个带值的选项都是**单个 argv 元素** `--flag=value`（用真实 CLI 验证过 commander 对 `=` 形式的解析，包括 `--resume=` / `--session-id=` / `--settings=` /
`--max-budget-usd=`），所以任何值——以 `-` 开头、带空格——都不可能被解析成另一个选项。值本身也校验（`process.rs` 的 `check_*`，`LaunchOptions::resolve` / `validate` 和
`Adapter::validate_settings` 都会调，daemon 在 `session.create` / `set_settings` 时就得到 `invalid_settings{field}`）：

| 值 | 规则 |
|---|---|
| `model` | 字母数字开头，之后 `[A-Za-z0-9._:/@\[\]+-]`，≤ 200 字符（别名、`name[1m]`、Bedrock ARN、Vertex `name@date` 都在内；以 `-` 开头 / 含空白 / 控制字符 → 拒绝） |
| `thinking`（`--effort`） | 字母数字开头，之后 `[A-Za-z0-9_-]`，≤ 32；另外仍须是所选模型支持的等级 |
| `mode` | 必须是内置的 6 个模式之一 |
| 会话 id（`--resume` / `--session-id`） | 字母数字开头，之后 `[A-Za-z0-9_-]`，≤ 128（resume 的 id 来自 daemon 的库，同样要过） |
| `system_prompt_append` | ≤ 32 KiB，不含 NUL 等控制字符（换行 / 制表符可以） |
| `max_budget_usd` | 有限的正数（NaN / ∞ / ≤ 0 → 拒绝；CLI 本身也要求 > 0） |

找二进制：`PATH` 里的**相对项**（`.`、`./node_modules/.bin`、空项）一律丢掉——`which` 会按 cwd 解析它们，cwd 是项目目录时就会执行克隆下来的仓库里自带的 `claude`；
相对的 `HOME` 同理；`config.command` 里带 `/` 的路径必须是绝对的；spawn 时的现找用中性 cwd（系统临时目录），不是项目目录；一次性调用的 cwd 不存在时也退到系统临时目录，不会继承 daemon 当前所在的目录。

**`--permission-prompt-tool stdio` 必须带**（事实表里没有）。实测不带的话，需要确认的工具（比如 `Write`）被 CLI 直接自动拒绝，
stdout 只出 `system/permission_denied`，**根本不发 `can_use_tool`**。带上之后才有控制请求。

模型、思考、权限模式总是**显式**传：缺省时取 `config.default_*`，再取 `AgentInfo` 里标 default 的（opus 5.5 / medium / `default`），
这样 UI 显示的当前值就是 CLI 实际用的值，不会悄悄继承用户 `~/.claude/settings.json` 里更宽松的设置。
**拉起（含 resume）时 `SpawnRequest.settings` 是完整的目标设置**（daemon 传的就是合并、清除、回填默认之后的整份），没给的字段 = 清除，
和 `set_settings` 同一套语义（`LaunchOptions::resolve` 与 `resolve_desired` 共用 `build`）：`system_prompt_append` / `max_turns` / `max_budget_usd` 为 `None`、
`options.fast_mode` 缺省 → 不传对应参数；模型 / 思考 / 模式缺省 → 配置默认 / `AgentInfo` 默认。**`ResumeHandle.launch_args` 不参与解析**：
daemon 在没有存活进程时（idle 关闭、daemon 重启、ended）改设置只改库里的 `settings`，`launch_args` 可能是过期的快照；回落到它，用户清掉的系统提示追加 / 预算 / 轮数上限
就会在 resume 时悄悄复活（系统提示的复活方向是不安全的）。`launch_args` 里每一项（模型 / 思考 / 模式 / `system_prompt_append` / `max_*` / `fast_mode`）在 `SessionSettings`
里都有对应字段，没有「只有进程才知道」的东西，所以从 resume 里**只取 `agent_session_id`**（拼 `--resume=<id>`）。进程起来后 `Started.resume.launch_args` 是这次实际生效的设置，daemon 据此落库。
`set_settings` 在进程活着时也会更新句柄的 `launch_args`（见下面「取消、改设置、关闭」）。daemon 侧在 `driver == None` 时同步改写 `launch_args` 是另一条独立的保险，两边各自成立。

| 设置 | 映射 |
|---|---|
| `model` | `--model`；会话中 `set_model` |
| `thinking` | `--effort`（`low`…`max`）；`off`（仅 4.6 系列）= 不传 `--effort` + 环境 `MAX_THINKING_TOKENS=0`；会话中无法改 → `restart_required` |
| `mode` | `--permission-mode`；`bypassPermissions` 另加 `--dangerously-skip-permissions`；会话中 `set_permission_mode` |
| `options.fast_mode` | `--settings '{"fastMode":true}'`（**未验证**：`fastMode` 是 settings.json 的键，init 里 `fast_mode_disabled_reason: sdk_opt_in_required` 说明 SDK 模式下可能还要别的开关）；改它 → `restart_required` |
| `system_prompt_append` / `max_turns` / `max_budget_usd` | `--append-system-prompt` / `--max-turns` / `--max-budget-usd`；改它们 → `restart_required` |
| `env` | 子进程环境（daemon 已合并进 `req.env`）；改它 → `restart_required` |
| 附件 | 只收图片（ADR-35）：`capabilities.attachments = ["image/png", "image/jpeg", "image/gif", "image/webp"]`。`send` 读 `attachment_paths[i]`（daemon 落盘的文件）→ base64 → stdin 消息 `content` 数组里的 `image` block，图在前、文字在后；见下面「图片附件」 |

### 图片附件

stdin 上的用户消息，**没有附件时**和以前完全一样（`content` 是字符串）：

```json
{"type":"user","message":{"role":"user","content":"hello"},"parent_tool_use_id":null}
```

**有附件时** `content` 是数组：`image` block 按附件顺序在前，`text` block 在后，文字为空就不放 `text` block：

```json
{"type":"user","message":{"role":"user","content":[
  {"type":"image","source":{"type":"base64","media_type":"image/png","data":"<base64>"}},
  {"type":"text","text":"what is this?"}]},"parent_tool_use_id":null}
```

- 校验在改任何状态**之前**做（出错没有半截的 turn，下一条正常消息仍是同一个 turn 计数）：`attachments` 与 `attachment_paths` 数量不一致 → `Protocol`；mime 不在上面四种里 → `Unsupported`；文件读不了（不存在、是目录、权限）→ `Io`（保留 `ErrorKind`，信息里带附件名，不带路径；路径写在 `tracing` 的 warn 里），绝不 panic。
- `interject = true` 照旧 `Unsupported`（带不带图都是），turn 进行中收到的消息也是 `Unsupported`，两种情况都不读文件。
- **一行可能有好几 MiB**（base64 比原文件大 1/3）：stdin 这一侧没有行长限制（`JsonlChild` 的写任务对整行 `write_all`），只有 **stdout** 单行有 8 MiB 上限。用例 `a_multi_mib_image_goes_through_stdin_in_one_line`（6 MiB 文件 → 8 MiB 的一行，fake 收到后字节完全一致）。一条输入里的图片 base64 合计超过 30 MiB（`MAX_IMAGES_BASE64`，按文件大小算、先于读内容）→ `Unsupported`，不写 stdin：Anthropic 的请求体上限是 32 MB，超了的图一旦进了 Claude Code 的会话历史，之后每个 turn 都会带着它失败。上游还有两层（ADR-35「2026-10 修订」）：daemon 每个附件默认 ≤ 10 MiB、每条消息合计 ≤ 20 MiB、积压合并成一个 turn 时附件累计超过 20 个或 20 MiB 就分到下一个 turn；Web 把图缩到长边 ≤ 2 000 px（一次请求里超过 20 张图时 Anthropic 要求每张 ≤ 2 000 px）、单张通常 ≤ 1.5 MiB、一条消息合计 ≤ 15 MiB。
- **已知限制**：图进了 Claude Code 的会话历史之后，每个 turn 都会随历史重发。一个会话里累计的图很多时（按单张约 1 MiB 算几十张），整段历史可能超过请求体上限，之后的 turn 失败，只能新开会话——这一点 anycon 控制不了（Claude Code 自己的压缩 / 裁剪策略决定）。
- **不开 `--replay-user-messages`**，所以 CLI 不会把用户消息（连同 base64）再从 stdout 回显出来；就算回显，`translate` 的 `on_user` 也只处理 `tool_result` block，`image` / `text` 一概跳过，不会变成事件。
- **工具结果里的图片**（比如 `Read` 读了一张 PNG，`tool_result.content` 里夹 `image` block）：事件的 `output` 保留 block 的结构和 `media_type`，但 `source.data` 换成 `[omitted: N bytes of base64]`——不让几 MiB 的 base64 进事件流、历史页和所有设备的缓存。`ToolResult` 之外的纯文本（`tool_output` 的第二个返回值，用来取退出码）只含 `text` block。
- 真实 CLI 测试 `real_image_attachment_is_understood`（`#[ignore]`，见下）：自己编一张 32×32 纯红 PNG，问「What color is this image? Answer with one English word.」，断言回复含 `red`。PNG 编码器（stored deflate block，不引入图片库）有不花额度的自检用例 `solid_png_encoder_produces_a_well_formed_png`（签名、每个 chunk 的 CRC、Adler-32、像素），并用 `file` / `sips` / Python `zlib` 手工核对过。

`--permission-mode default` 在 `claude --help` 的 choices 里写的是 `manual`，但 `default` 也被接受（实测），所以沿用规格里的 `default`。

## 探测

1. 解析命令：`config.command`（绝对路径直接用，可带参数）→ daemon 环境 `PATH` 的 `which -a claude` → `~/.local/bin ~/.bun/bin ~/.cargo/bin /opt/homebrew/bin /usr/local/bin ~/.nvm/versions/node/*/bin ~/.volta/bin ~/.npm-global/bin`。
   对候选逐个 `--version`（默认 2 秒超时），第一个能跑的胜出（nvm / volta 的坏 shim 被跳过）。冷启动很慢的 shim 可以把环境变量 `ANYCON_VERSION_TIMEOUT_MS` 放进 `agents.claude-code.env` 调大。
2. 版本：从输出里抠 semver（`2.1.293 (Claude Code)`），< 2.1.0 → `unsupported_version`（hint `claude update`）；解析不出来只警告，不挡。
3. 认证：`claude auth status`，退出码 0 已登录 / 1 未登录，stdout JSON 的 `loggedIn` / `authMethod` / `email` 尽力解析。
   未登录 → `not_authenticated`（hint ``run `claude` on the machine to log in``）。环境里有 API key / Bedrock / Vertex / Foundry 凭证时，`auth status` 说未登录也不挡（当 unknown）。超时 / 其它退出码 = unknown，不阻塞。
4. 模型：内置表 → 用户配置的（`ANTHROPIC_MODEL`、`ANTHROPIC_DEFAULT_*_MODEL`、`~/.claude/settings.json` 的 `model` 与 `env`，用 `ctx.env`）→ `additional_models`，按 id 去重；`allow_custom_model = true`。
   用户 / 配置追加的模型没有思考选项（等级因模型而异，不瞎猜）。没有慢步骤，所以 `cheap` 与否结果一样。整个探测遵守 `ctx.deadline`，超时错误里列出没跑完的步骤。

诊断：`anycon_adapters::diagnose("claude-code", &ctx)`（或 `ClaudeCodeAdapter::diagnose`）返回 `(文本, 字段)`：命令来源、daemon PATH / shell、`which -a`、`$SHELL -lc 'type -a claude'`、各候选的 `--version` 结果、解析路径、版本、认证、模型数、状态、issue。

## 事件翻译（实测要点）

- 非 `{` 开头的行忽略；不认识的 `type` / `subtype`（`commands_changed`、`thinking_tokens`、`task_summary`、`post_turn_summary`、`hook_started`…）忽略。认识的 `system/*`、`rate_limit_event` 见下面几节（Task / Activity / Notice）。
- **`system/init` 每个 turn 都会再发一次，第一条用户消息之前不发。** 所以 `Started` 不等 init：新会话我们自己生成 `--session-id <uuid>`、续接用 `--resume <id>`，spawn 时就发 `Started`；init 带同一个 id 就不重复发，不同才再发一次。
- 文本 / 思考来自 `stream_event` 的 `content_block_delta`；每个 content block 另有一条自己的 `assistant` 消息（同一个 `message.id`）。文本已流式发过的不再从 `assistant` 重发（没开 partial messages 时才用 `assistant` 兜底）。
- **思考内容是空的**：较新的 CLI 里 `thinking_delta.thinking` 是 `""`（只有 `estimated_tokens`，另有 `system/thinking_tokens`）。每个思考块发一条空的 `ThinkingDelta` 作「正在思考」标记，块结束发 `ThinkingDone`。`--effort low` 的简单问题往往根本没有思考块。
- `tool_call` 在 `assistant` 消息里发（那里有完整 `input`）；`user` 消息里的 `tool_result` → `ToolResult`（`is_error` 取自块；Bash 失败时从 `Exit code N` 取退出码；Edit/Write 从 `tool_use_result.structuredPatch` / `create` 生成 unified diff；Glob/Grep 取 `numFiles` 作 `match_count`），成功的 file_edit 再发 `FileChanged`（`tool_use_result.type == "create"` → created，否则 modified）。工具输出文本超过 256 KiB 截断并加标记。
- `TodoWrite` → `Plan`（不发 tool_call / tool_result）；`ExitPlanMode`、`AskUserQuestion` 由权限请求表达，也不发 tool_call / tool_result。
- 子 agent（`parent_tool_use_id` 非空）的文本 / 思考不混进主流，它们内部的工具调用照常发，带 `parent_id = parent_tool_use_id`，客户端据此把它们收进子 agent 的块里。
- 转到后台的调用：后台子 agent（`Agent` 的 `run_in_background`）马上返回 `tool_use_result.status = "async_launched"`；后台命令（Bash 的 `run_in_background`，或者跑着跑着被转到后台的）马上返回 `Command running in background with ID: …`，`tool_use_result.backgroundTaskId` 有值（实测 2.1.293）。另外 `system/task_started { is_backgrounded: true, tool_use_id }` 先于结果到达，也认。这些结果标 `detail.background`（后台命令不给 `exit_code`）；任务结束时 CLI 发 `system/task_notification { tool_use_id, status: completed | failed | stopped | killed, summary }`，翻译器给同一个调用补一条 `ToolResult`（`output` = `summary`；后台命令的 `summary` 形如 `Background command "…" completed (exit code 1)`，从中取 `exit_code`；`failed` 和退出码非 0 算错误，被停掉的不算）。前台子 agent 也会发这个通知，它不是 background 的，忽略。
- 子 agent 用掉的 token → `detail.total_tokens`：前台的取 `tool_use_result.totalTokens`，后台的取 `task_notification.usage.total_tokens`（实测 2.1.293：`usage: { total_tokens, tool_uses, duration_ms }`）。`result.usage` 只算主 agent 自己的调用，不含子 agent 的（`modelUsage` 和 `total_cost_usd` 含）。
- `result`：**`usage` 与 `duration_ms` 是本 turn 的，`total_cost_usd` 是进程内累计值**（实测第二个 turn 的 `total_cost_usd` 大于第一个、`modelUsage` 也是累计的）——翻译器把费用换成相邻两次的差值，所以 daemon 直接把各 turn 的 `cost_usd` 相加即可。`success` → completed；`error_max_turns` → max_turns；`error_max_budget_usd` → budget；其它 → error（先发 `Error` 事件：`api_error_status` 401/403 → `auth_failed`（不可恢复），429 → `rate_limited`，其它 `agent_error`）。我们刚发过 interrupt 的那个 turn，非 success 的 result 一律算 `cancelled`（不发 Error）。没有进行中的 turn 时的 error result（比如 `--resume` 不存在的会话）只发 `Error`，不发 `TurnDone`。
- `TurnDone.turn_id` 是 adapter 进程内的计数（从 1 递增）；daemon 自己发 `turn_start`，应该用自己的 turn id 覆盖它。

### 后台任务与 CLI 自己开的 turn

实测（2.1.293，haiku 5.5，2026-10-21）：subagent 默认在后台跑（`Agent` 工具的 `run_in_background`），Bash 也可以 `run_in_background`。
主 agent 派出后台任务后就输出 `result`，主 turn 结束；任务在同一个进程里继续，subagent 的 `assistant` / `user` 消息照常出现在 stdout 上（`parent_tool_use_id` 非空）。
每个任务结束时 CLI 先发 `task_updated { patch.status: completed }`、`task_notification`、`background_tasks_changed`，然后**不等用户消息**自己再跑一轮：
`system/init` → `system/status requesting` → assistant → `result`。stdout 上的后台任务相关消息：

| 消息 | 内容 |
|---|---|
| `system/background_tasks_changed` | `tasks: [{ task_id, run_id, task_type: local_agent / local_bash, description, … }]`，**当前还在跑的全量列表**，空数组 = 没有了 |
| `system/task_started` | `task_id`、`tool_use_id`、`is_backgrounded`、`task_type`、`description`、`prompt` |
| `system/task_progress` | 任务的进度（`usage { total_tokens, tool_uses, duration_ms }`、`last_tool_name`、`description` = 当前在做的事、`summary`？；workflow 另有 `workflow_progress[]`，见「后台任务快照」） |
| `system/task_updated` | `patch.status`（`completed` …） |
| `system/task_notification` | `status`、`summary`、`output_file`（任务结束，也是 CLI 接着自己跑一轮的触发） |

翻译：

- 后台任务数以 `background_tasks_changed` 为准；没有它的 CLI 按 `task_started`（`is_backgrounded: true`）加、`task_notification` / 结束状态的 `task_updated` 减。数目变化时发 `AgentEvent::Background { tasks }`，daemon 据此在任务跑完之前不按 `idle_timeout` 收进程（规格 § 7.3）。
- **没有 turn 在跑时出现 `system/init` = CLI 自己开始了一轮**：发 `AgentEvent::TurnStarted`，`busy` 置上（`cancel` 能打断它），这一轮的 `result` 照常变成 `TurnDone`。只认 `init`，不认 assistant 消息：中断之后迟到的消息不能开出一个永远等不到 `result` 的 turn。
- 这一轮进行中 `send` 的消息**暂存**（`Translator::submit_user_message` → `Deferred`），不写 stdin：写进去的话 CLI 会把它排到这一轮之后，先到的 `result` 会被当成用户 turn 的结束。这一轮的 `result` 到了，不发 `TurnDone`，以 `Out::Write` 把消息交给读任务写进去，用量记下来加到用户 turn 的 `TurnDone` 上（两轮合成一个 turn）。暂存期间 `cancel` 了的，消息丢掉，按 `cancelled` 收尾；进程在这期间退出，按「turn 中途退出」报。
- 新 turn 开始时清空按 turn 去重的表；工具调用的表（`tools` / `seen_tool_use` / `suppressed`）只在没有后台任务时清——subagent 的工具调用会跨 turn（`tool_use` 在上一轮，`tool_result` 在这一轮），清掉就丢了 diff / `file_changed` 要用的工具名和路径。
- 用例：`claude_translate.rs` 的 `background_tasks_and_the_turns_the_cli_starts_on_its_own`（手写的 `fixtures/claude/background_tasks.jsonl`，形状照录制）、`background_count_falls_back_to_task_lifecycle_messages`、`a_user_message_during_the_clis_own_turn_waits_for_it_and_takes_it_over`、`cancelling_while_the_message_waits_drops_it`；进程级 `claude_process.rs::a_message_sent_during_the_clis_own_turn_is_written_after_that_turn_ends`（fake-claude 的 `__AUTO__` 自己开一轮、`__PAUSE__` 停一下，日志里核对 stdin 消息排在这一轮的 `result` 之后）。

### 后台任务快照（`Event::Task`）

`system/task_started` / `task_progress` / `task_updated` / `task_notification` 都是增量；adapter 按 `task_id` 合并成整条快照，每次内容变化发一条 `Event::Task`（后一条整条取代前一条，daemon 节流，规格 § 8.3）。
后台任务数（`AgentEvent::Background`）和调用的 `tool_result` 仍按上一节的老逻辑，互不影响。

**Workflow 在 `-p` 下可用**（实测 2.1.308，`tests/fixtures/claude/real_workflow.jsonl`、`real_workflow_failed.jsonl`）：用户消息里要明说「use a workflow」，模型会先调 `Skill(workflow-authoring)` 取脚本 API，
再调 `Workflow({ script })`。流程：

1. `control_request can_use_tool`（`tool_name: Workflow`，`input.script`，`description` = 脚本 `meta.description`）——普通的工具权限，adapter 的标题是 `Run workflow: <description>`；
2. 允许后马上返回 `tool_result`（`Workflow launched in background. Task ID: …`，`tool_use_result { status: async_launched, taskType: local_workflow, workflowName, runId }`），主 turn 随后 `result`。这条结果**不**标 `detail.background`（它的类别是 `other`）：最终状态在 `task` 快照里，不靠第二条 `tool_result`；
3. `background_tasks_changed` → `task_started { task_type: local_workflow, workflow_name, description, prompt = 脚本全文, tool_use_id }` → 若干 `task_progress` → `task_updated { patch: { status, end_time, error? } }` → `task_notification { status, summary, usage, output_file }` → `background_tasks_changed []` →
   CLI 自己开一轮（`init` … `result`，`origin.kind = task-notification`）把结果告诉用户。

**`task_progress` 的节奏**（2 个 agent、各一次模型回复、全程约 4 秒的 workflow：5 条，相对 `task_started` 的时间 0.03 s、2.2 s、2.4 s、2.5 s、3.7 s）：来自二进制里 workflow 的进度发射器——agent 条目变化先按 16 ms 去抖成一批，
SDK 侧至少隔 250 ms 才发一次；批次里**全是 `state: progress` 的更新**（agent 在调工具）时 `workflow_progress` 最多每 10 秒带一次，其余帧只有 `usage` / `description` / `last_tool_name`，没有 agent 列表——所以 adapter 对没带 `workflow_progress` 的帧沿用上一份 `agents` / `phases`，带了的就是全量替换。
`usage.total_tokens` / `tool_uses` 是各 agent 条目的合计（agent 完成时才计入）；`duration_ms` 是任务开始到现在。

字段（`workflow_progress[]` 的两种条目，取自二进制里的 zod schema，实测 `start` / `done`；`progress`、`error` 来自 schema 和 CLI 自己的消费代码，没有在实录里见到）：

| 条目 | 字段 → 快照 |
|---|---|
| `{ type: workflow_phase, index, title, kind? }` | → `phases[]` |
| `{ type: workflow_agent, index, label, state, … }` | → `agents[]`（`index` / `label` / `state` / `phaseIndex` / `model` / `tokens` / `toolCalls` / `durationMs` / `lastToolName` / `error` / `attempt` / `cached`）；`phaseTitle` 没有对应的 `workflow_phase` 条目时用它补一个阶段；其余字段（`agentId`、`startedAt`、`queuedAt`、`promptPreview`、`resultPreview`、`lastToolSummary`…）不带 |

`state`：`start` 带 `queuedAt` 而没有 `startedAt` = **排队**（`queued`：并发上限、等限额恢复；实录里 beta 先是这样，几十毫秒后才有 `startedAt`），有 `startedAt` 的 `start` 和 `progress` = `running`，`done` = `done`（`cached: true` = 命中缓存、没真跑），
`error` = `failed`，`skipped: true`（用户跳过）= `skipped`；`error` + `blocked: true` 是被安全分类器拦下，同样是 `failed`。schema 之外的 `state` 整条丢掉。CLI 自己的消费端对终态的任务忽略后续帧，adapter 一样。

其余约定：

- `title`：workflow 取 `workflow_name`（没有就是 `description`），其它取 `description`；`summary`：workflow 是 `description`（`meta.description`），子 agent 是 `task_progress.summary` 或 `description`（当前在做的事，如 `Editing src/lib.rs`），结束时换成通知的 `summary`（子 agent 的结果、`Background command "…" completed (exit code 0)`）；
  workflow 正常结束时通知的 `summary` 只有 `Dynamic workflow "…" completed`，不换；失败时换成通知的（含错误），`task_updated.patch.error` 先到先用。
- `last_tool`：子 agent 取 `last_tool_name`；**workflow 不取**——它的 `last_tool_name` 是最近有动静的那个 agent 的 `label`，工具在各 agent 条目的 `lastToolName` 里。
- `task_type` → `kind`：`local_workflow` / `local_agent` / `local_bash` → `workflow` / `subagent` / `shell`，其余（`mcp_task`、`remote_agent` …）`other`；没有 `task_type` 的老 CLI 看 `workflow_name` / `subagent_type`。
- **只有后台任务发快照**：`is_backgrounded: false`（前台子 agent）和 `awaited: true`（被前台调用等着的续跑）只记状态，转到后台（`task_updated { patch.is_backgrounded: true }`）之后才开始发；`skip_transcript` / `ambient`（CLI 的杂务）不发。
  理由：前台调用本来就有 `tool_call` / `tool_result`，规格里的「N 个后台任务在运行」也只数后台的。
- 终态：`task_updated.patch.status`（`completed` / `failed` / `killed`；`pending` / `running` / `paused` 还没结束）和 `task_notification.status`（`completed` / `failed` / `stopped`）。两者先后到，终态不一致时先到的算数；通知带最终用量和摘要，并进去再发一次。
- 续跑：任务续跑时 `task_id` 不变、`run_id` 变（字符串比较递增）；比当前小的 `run_id` 的帧是迟到的，忽略；新的 `task_started` 从头开始。
- 状态只留最近 32 个已结束的任务（迟到的通知还要并进去），进程结束时全忘；daemon 负责给进程退出时还没结束的任务补 `cancelled`。

### Activity 与 Notice

| CLI 消息（字段取自 2.1.308 二进制里的 SDK schema 和发射代码） | 事件 |
|---|---|
| `system/api_retry { attempt, max_retries, retry_delay_ms, error_status (连接错误为 null), error (类别), no_response? }`（由内部的 `api_error` 转换而来；用量限额等待用 `429 rate_limit`） | `Activity { retrying, attempt, max_attempts, delay_ms, reason }`；`reason` = `<状态> <类别>`（`529 overloaded`）、连接错误 `connection error`、首字节超时 `no response from the API within Ns`。下一个响应的 `message_start` 到了发 `Activity none`。没有 turn 在跑时（后台 agent 在重试）不发 |
| `system/status { status: "compacting", compact_output_chars? }`（自动压缩期间会反复发） | `Activity compacting`（只发一次）；`status: null`（带 `compact_result`）或 `compact_boundary` 到了发 `Activity none`。`status: "requesting"` 是每次请求开始时发的，与压缩无关 |
| `system/compact_boundary { compact_metadata { trigger, pre_tokens, post_tokens? } }` | `Notice context_compacted`：`Context compacted (auto): 168000 → 12000 tokens` |
| `rate_limit_event { rate_limit_info { status: allowed \| allowed_warning \| rejected, resetsAt (秒), rateLimitType, utilization? } }` | `status != allowed` 时 `Notice rate_limit`（warning，`resets_at_ms = resetsAt × 1000`），按限额类型记（状态，resetsAt），同一组合只发一次，回到 `allowed` 后重新计。这个事件在用量每变一点就发一次（每个 turn 好几条），所以去重必须有 |
| `system/notification { key, text, priority: low \| medium \| high \| immediate, color? }` | `Notice agent_notice`：`color` 为 `warning` / `error` → warning，否则 info；`low`（REPL 的闲置提示）不转；同一 turn 同 key 同文本只发一次 |
| `system/informational { content, level: info \| notice \| suggestion \| warning }` | `Notice agent_notice`：`warning` → warning，`notice` / `suggestion` → info，`info` 不转（schema 的说明：只在转录视图里显示） |
| `system/model_fallback`、`model_refusal_fallback`、`model_refusal_no_fallback`、`model_consent_fallback`（`content`、`original_model`、`fallback_model?`） | `Notice model_fallback`（warning，`model_from` / `model_to`） |
| `system/permission_denied { tool_name, decision_reason?, message }` | `Notice permission_denied`（info）：`<tool_name>: <decision_reason 或 message>` |
| `system/hook_response { hook_name, hook_event, outcome: success \| error \| cancelled, exit_code?, stderr, output }` | `outcome = error` → `Notice hook_failed`（warning）。CLI 默认只让少数 hook 事件进输出流（`--include-hook-events` 打开全部，adapter 不加），多数会话里看不到 |
| `system/init.mcp_servers[] { name, status: connected \| failed \| needs-auth \| pending \| disabled }` | `status = failed` → `Notice mcp_server_failed`（warning），每个进程每个 server 一次（`init` 每个 turn 都发） |

`api_retry` 和 `rate_limit_event` 很难在实测里触发，`tests/fixtures/claude/api_retry.jsonl`、`rate_limit_rejected.jsonl`、`compaction.jsonl` 是照上面的 schema 手写的（形状和字段名来自二进制，没有真实录制）；`rate_limit_event` 的 `allowed` 形态在多数 `real_*` 录制里都有。

### 本地斜杠命令（`/context`、`/cost` …）

实测 2.1.308：`/context`、`/cost`、`/usage`、`/status` 都是本地命令，不调模型（`result { num_turns: 0, modelUsage: {}, local_command: "context" }`）。`-p` stream-json 下输出是一条
`assistant` 消息：`model: "<synthetic>"`、content 是一个 text block（markdown）、顶层带 `local_command_source: "<local-command-stdout>…</local-command-stdout>"`（以及 `context_usage` / `usage_report` / `local_command_run` 之类的结构化附带信息）。
以前这条消息被当成模型的文本发出去；现在带 `local_command_source` 的 assistant 消息发 `Notice { code: command_output, message: <text block 原文> }`（`<local-command-stderr>` 形式 warning）。
schema 里还有 `system/local_command_output { content }`，实测没见到，同样处理。同样是 `<synthetic>` 的 API 错误提示（`isApiErrorMessage`，没有 `local_command_source`）仍是普通文本。录制：`real_local_command.jsonl`（`/context`，技能名已换成占位）。

### 上下文用量

`Usage.context_window` = `result.modelUsage.<模型>.contextWindow`（实测 haiku 5.5 是 1 000 000）：`modelUsage` 按模型分项，子 agent 用别的模型时不止一项，先按本 turn 最近一条顶层 assistant 消息的 `message.model` 取，再按 `system/init.model`，最后只有一项时取它，否则缺省。
`Usage.context_tokens` = 本 turn 最近一条顶层（`parent_tool_use_id` 为空）assistant 消息的 `usage.input_tokens + cache_read_input_tokens + cache_creation_input_tokens`——同一条请求的每个 content block 各带一条 assistant 消息，输入用量相同；
`<synthetic>`（本地命令、API 错误）的不算。本地命令没有模型调用，两个字段都缺省。

## 权限

CLI 发 `control_request { request_id (uuid), request: { subtype: can_use_tool, tool_name, display_name, input, description?, permission_suggestions?, tool_use_id, requires_user_interaction? } }`。

| 来源 | kind | options | 答复 |
|---|---|---|---|
| 普通工具 | `tool`（title 如 `Write /path`、`Run command`） | `allow` / `allow_session` / `deny`（需要 message） | allow：`{behavior: allow, updatedInput: <input 原样>}`；allow_session：再加 `updatedPermissions: [{type: addRules, rules: [{toolName}], behavior: allow, destination: session}]`；deny：`{behavior: deny, message}` |
| `ExitPlanMode` | `plan`（`detail.text` = `input.plan`） | `allow`（开始实施）/ `deny`（继续规划，可带意见） | allow 把 `input`（含 `plan`、`planFilePath`）原样回传 |
| MCP elicitation（`control_request { subtype: elicitation }`） | 表单 → `question`；URL → `other`（`detail.url`）。见下面「MCP elicitation」 | `accept` / `decline` | `{ action: accept, content? }` / `{ action: decline }` |
| `AskUserQuestion` | `question`（`input.questions[]` → `Question`：id `q1..qn`、choices 的 id = label、`multi` = `multiSelect`、`free_text = true`） | `answer` / `deny`；也可以只带 `answers` | 选项优先：`deny` → `{behavior: deny, message}`（**即使带着 `answers`**）；`answer`（或没给 `option_id`）+ `answers` → `{behavior: allow, updatedInput: {...input, answers: {"<题目全文>": "<label>"}}}`，多选用 `", "` 连接；`answer` 却没有 `answers`、或选项不认识 → 报错（请求仍然未决）；既没选项也没 `answers` → deny |

`AskUserQuestion` 与 `ExitPlanMode` 的答复形状**已用真实 CLI 验证**（tool_result 回显 `Your questions have been answered: "Which color do you prefer?"="blue"`，以及 `User has approved your plan`）。多选的 `", "` 连接来自 Agent SDK 文档，没单独验证。

注意：
- 实测默认模式下 Bash `echo hi` 被 CLI 自己按规则放行（`permission_decision.source: config`），不发 `can_use_tool`；`Write` 会发。
- `permission_suggestions` 里是 `{type: setMode, mode: acceptEdits, destination: session}` 这类；我们的 `allow_session` 按规格用 `addRules` 按 `toolName` 放行本会话，不采用这些建议（建议里可能有写入 settings 文件的 destination）。
- ExitPlanMode 被允许后，CLI 自己退出 plan 模式（stdout 出 `system/status { permissionMode: "default" }`）。adapter 没有事件可以报告这个模式变化，由 daemon 按规格 § 9.1 切 mode（走 `set_settings` → `set_permission_mode`）。
- CLI 撤回未决请求时发 `control_cancel_request`；turn 结束时 adapter 也清空未决表。之后对这些 id 的 `respond_permission` 是 no-op（`Ok`），所以 daemon 在取消后自动 deny 不会报错。
- 不认识的 `control_request` 子类型（如 `hook_callback`）回一个 `error` 的 `control_response`，免得 CLI 干等。

### MCP elicitation

MCP server 在工具调用中途可以向用户要输入（`elicitation/create`）。实测 2.1.308（`real_elicitation_form.jsonl`、`real_elicitation_url.jsonl`；MCP server 是一个只用标准库的 Python 脚本 `tests/bin/elicit-mcp-server`，
用项目里的 `.mcp.json` 注册，`-p` 下不需要额外批准）：stream-json 宿主**默认**就会收到 `control_request { subtype: elicitation }`（只有 `--permission-prompts none` 时 CLI 自己回 cancel），不需要 `initialize`；
不答 CLI 一直等。以前 adapter 对它回 error（= 对所有 elicitation 回了 cancel）。

```
→ { "type": "control_request", "request_id": "<uuid>", "request": { "subtype": "elicitation", "mcp_server_name": "example-elicit",
    "message": "Please enter your details", "mode": "form",                       // server 没写 mode 时 CLI 补 "form"
    "requested_schema": { "type": "object", "properties": { "name": {…}, "age": {…}, … }, "required": ["name"] },
    "property_order": ["name", "age", "subscribe", "color"] } }                   // 属性顺序（JSON 对象不保序）；URL 模式没有
→ URL 模式：{ "subtype": "elicitation", "mcp_server_name", "message", "mode": "url", "url": "https://…", "elicitation_id": "elic-1" }
← { "type": "control_response", "response": { "subtype": "success", "request_id": "<uuid>",
    "response": { "action": "accept", "content": { "name": "Ada", "age": 36, "subscribe": true, "color": "green" } } } }
```

三种答复（`accept` + `content`、`accept` 不带 content（URL 模式）、`decline`）都实测过：MCP server 收到的就是 `{action, content?}`，工具结果回显 `elicitation result: {"action": "accept", "content": {…}}`，模型据此继续。`cancel` 是第三个取值，adapter 不用（用户拒绝、取消、超时都答 `decline`）。
`content` 不合法的情况没有试过，所以 adapter 先按 schema 把答案转成 JSON 类型。

- 表单 → `kind = question`，标题 `<server>: <message>`，每个 `requested_schema.properties` 一道题（id = 属性名，顺序按 `property_order`）：enum / `oneOf`/`anyOf`（`{const, title}`）/ 旧的 `enumNames` → 选项（id = 取值的字符串形式，答复时换回原值，数字枚举回数字）；
  boolean → `true` / `false` 两个选项（标签 Yes / No）；`array` + enum 的 `items` → 多选；string / number / integer → 自由输入（number / integer 在题干里带类型和 `minimum` / `maximum`）；必答是默认；schema 的 `required` 里没有的属性 `Question.optional = true`（题干里不另外标，客户端自己显示「可选」），`description` 接在后面。options `accept`（Submit，allow_once）/ `decline`（reject_once）。
- 答案转换：整数 / 数字解析，boolean 认 `true` / `false` / `yes` / `no`，enum 必须是允许的取值；可选项留空就不出现在 `content` 里，必填项为空是错误。**转换失败 → 改按 decline 答复**，并发一个可恢复的 `error { code: elicitation_invalid_answer }` 说明原因：
  daemon 在转给 adapter 之前就把请求记成了已答复（`permission_resolved`），adapter 如果像别的请求那样「放回去等用户重答」，CLI 会一直等下去。
- URL → `kind = other`，标题 `<server> needs you to open a link`，`detail.text` = `message`，`detail.url` = 链接，options `accept`（I've completed it）/ `decline`。链接不是 http / https（含 `javascript:`、`file:`、`data:`、相对地址、带控制字符）的不问用户：
  translator 直接回 `decline`（`Out::Reply`）并发 `error { code: elicitation_rejected, recoverable: true }`。
- `system/elicitation_complete { mcp_server_name, elicitation_id }`（server 确认 URL 模式完成）不处理：请求靠用户的答复结束。

## 取消、改设置、关闭

- `cancel`：发 `control_request interrupt`（实测回执 `{still_queued: []}`），**3 秒内没有 `result` 就补 SIGINT**。实测的 interrupt 结果：一条 `user` 消息 `[Request interrupted by user]`，随后 `result { subtype: error_during_execution, is_error: true, terminal_reason: aborted_streaming }`，用量和费用都是 0；进程不退出，之后还能继续发消息。我们据此发 `TurnDone { reason: cancelled }`。SIGINT 兜底若让进程退了，补一个 cancelled 的 `TurnDone` 再 `Exited`（不带错误）。
- `set_settings(settings)`：**`settings` 是完整的目标设置**（daemon 一直传合并后的整份设置，不是增量），没给的字段 = 清除、回到 `config.default_*` / `AgentInfo` 默认（或「没有」）：
  `model: None` → `set_model` 回默认模型（没有任何默认可回就发 `model: null`，让 CLI 用它自己的默认）；`thinking: None` → 配置默认 / 模型默认；`mode: None` → 配置默认 / `default`；
  `system_prompt_append` / `max_turns` / `max_budget_usd` 为 `None`、`options.fast_mode` 缺省、`env` 为空 → 清除（`restart_required`）。步骤：
  1. 先把目标解析 + 校验（和 spawn 同一套规则，含上面的值校验）——非法就在发任何控制请求之前返回，不留半截状态；
  2. 只能启动时给的字段（思考、`system_prompt_append`、`max_turns`、`max_budget_usd`、`fast_mode`、`env`）记进当前状态 → `restart_required`；
  3. 可热生效的按固定顺序 **model → mode** 逐个发控制请求（`set_model` / `set_permission_mode`，等回执，上限 15 秒，CLI 冷启动要几秒），每成功一个就记进当前状态。
  中途失败就停：已应用的保留、失败的和后面的不动，错误信息里写明 `(applied: model; not applied: mode)`。模型被拒 → `InvalidSettings{model}`（回执 `error_code: catalog_unknown`）。
  切到 `bypassPermissions` 而启动时没带 `--dangerously-skip-permissions`（CLI 回 `bypass_not_launched`）→ 不发请求，直接 `restart_required`。
  状态变了之后 `resume_handle().launch_args`、翻译器、以及（补发的一个 `Started` 事件，daemon 会更新 `launch_args` 并落库）都跟着更新——所以 daemon 随后的 `shutdown` + `spawn(resume)` 不会让旧值复活。
- 每个控制请求的 `request_id` 都是新 uuid；CLI 的 `control_response` 按 id 匹配，没人等的回执忽略。
- **不阻塞约定**：daemon 在等 `set_settings`（要等 `set_model` / `set_permission_mode` 的回执）之类调用返回期间不读 `events()`。回执和事件在同一条 stdout 上，
  所以读任务只管读、分类、处理回执，事件进一个**不限长的内部队列**，由另一个任务往有界的 `events()` 通道（256）里送。旧实现读任务自己 `send().await` 有界通道，
  输出密集时通道一满就读不到回执，`set_settings` 要等到 15 秒超时（`cancel` 不等回执，不受影响）。用例：`claude_process.rs::control_requests_are_not_blocked_by_thousands_of_unread_events`、
  `cancel_under_a_flood_still_ends_with_a_cancelled_turn`（fake 输出 5000 条 delta、测试不读事件）。
- **插话**：`capabilities.interject = false`，`UserInput.interject = true` 一律 `Unsupported`（也绝不借此开新 turn）；turn 进行中收到 `interject = false` 的消息同样 `Unsupported`（daemon 自己排队），只有 CLI 自己开的那一轮例外：消息暂存到这一轮结束（见「后台任务与 CLI 自己开的 turn」）。
- `shutdown`：关 stdin → 等 300 ms → SIGINT → 5 秒 → SIGKILL（进程组）；claude 退出后残留在进程组里的进程也会被清掉。实测关 stdin 后有时 100 ms 内就退出，有时 1.5 秒还没退，所以需要后面两步。进行中的 turn 会先补 `TurnDone(cancelled)`。`Exited.error` 只在非预期退出时有值，带 stderr 末尾；预期内（我们要求关闭 / 取消）为空。turn 中途非预期退出会在 `Exited` 之前发一个 `Error { code: agent_exited, recoverable: true }`。
- 句柄不经 `shutdown` 被丢弃时（`Drop`）会关 stdin 并对整个进程组 SIGKILL，不留孤儿。
- `--resume` 一个不存在的会话：stderr `No conversation found with session ID: …`，stdout 一条 `error_during_execution` 的 `result`，退出码 1；对应 `Error` + `Exited{code: 1, error: …stderr…}`。

## `initialize` 控制请求

事实表建议「CLI 不发事件就试一下 `initialize`」。结论：

- **不需要。** CLI 启动后到第一条用户消息之前什么都不发，但 `interrupt` / `set_model` / `set_permission_mode` / `can_use_tool` 都不依赖 `initialize`（只依赖 `--permission-prompt-tool stdio`）。
- 发了也能用：`{"type":"control_request","request_id":"…","request":{"subtype":"initialize"}}` 约 5 秒后返回 `control_response`，`response` 里有 `commands`、`agents`、`output_style`、`models[]`（`value` / `resolvedModel` / `displayName` / `description` / `supportsEffort` / `supportedEffortLevels` / `supportsFastMode` / `supportsAutoMode`）、`account`、`pid`、`current_permission_mode`、`fast_mode_state`、`capabilities`。
- 其中 `models[]` 其实就是 CLI 自己的模型清单（规格里「CLI 没有列模型的命令」已经过时）。**没有用它做探测**：要起完整的 CLI（会拉起用户配置的 MCP server / 插件），对「周期性探测」太重。如果以后要用，可以在非 cheap 探测里发一次 `initialize` 并合并 `models[]`；`supportedEffortLevels` 可以直接校准内置表。
- 冷启动：进程起来到能回应控制请求约 4–5 秒；第一个 turn 的 `duration_ms` 也因此偏大。

## 未验证 / 假设

- 内置表里 4.6 系列的 `off` 与 `MAX_THINKING_TOKENS=0`、`fast_mode` 的 `--settings` 写法、各模型的 `min_cli_version`（目前都是 2.1.0，机制已就位）。
- 多选题的 `", "` 连接。
- `workflow_progress` 里 `state: progress` / `error` 的真实样子（实录里 agent 只经历了 `start` → `done`，`progress` 只出现在 CLI 对「全是 progress 更新」批次限频的那种帧里，那种帧不带 `workflow_progress`）。
- elicitation 的 `cancel` 取值、`content` 不合法时 CLI 的反应；`system/elicitation_complete` 的实际时序。
- `hook_response` / `permission_denied` / 模型回退 / `compact_boundary` 的实际样子：字段取自 SDK schema，没有实录。
- 标题 / 摘要类的 `Title` 事件：CLI 没有对应输出，不发。

## 真实 CLI 冒烟（`ANYCON_REAL_AGENT_TESTS=1`）

```
ANYCON_REAL_AGENT_TESTS=1 cargo test -p anycon-adapters -- --ignored real_ --nocapture
```

`real_probe_is_ready`（不花 token）、`real_pong_permission_cancel_and_resume`（haiku 5.5、low，4 个很短的 turn，约几美分）和 `real_image_attachment_is_understood`（haiku 5.5、low，1 个带图的 turn，约一美分以内；只跑它：`ANYCON_REAL_AGENT_TESTS=1 cargo test -p anycon-adapters --test real_claude -- --ignored real_image --nocapture`）。2026-10-11 本机结果（下面是前两个用例；图片用例 `real_image_attachment_is_understood` 通过的标准：输出里 `image turn events` 一行的 `text=` 含 `red`，`image turn done` 一行是 `reason=Completed`）：

- 探测 Ready：2.1.293，路径 `~/.nvm/…/bin/claude`，认证 `claude.ai`，14 个模型。
- pong：`reply with the single word pong` → `text_delta "pong"`、`turn_done completed`，usage `in 2 / out 4 / cache_read 11946 / cache_write 9942`，cost ≈ 0.0021；无思考事件（low）。测试自己按 daemon 的做法剔除 `CLAUDECODE` 等标记（adapter 不再剔）。
- `set_permission_mode` / `set_model` 都得到回执；乱写的模型 id → `InvalidSettings`；改思考 → `restart_required`。
- 「用 Write 工具创建 hello.txt」→ `can_use_tool`（kind tool，title `Write /…/hello.txt`）→ allow → `tool_result`、`file_changed(created)`，文件内容正确。
- 长文 prompt：首个 `text_delta` 后立刻 `cancel()`，12 ms 内拿到 `turn_done cancelled`，进程仍在。
- `shutdown` → `Exited`；用 `resume_handle()` 再 spawn（`--resume`）：同一个 `agent_session_id`，问「我第一条消息让你回复哪个词」→ 回答 `pong`，会话历史保留。
- 另外手工验证（Python 脚本，非测试）：默认模式下 Bash `echo hi` 无需确认；`--effort high` 时出现思考块但内容为空；interrupt 的 `result` 形状；`AskUserQuestion` / `ExitPlanMode` 的往返。

## 测试

- `translate.rs` 的单元测试 + `tests/claude_translate.rs`：对 `tests/fixtures/claude/*.jsonl` 回放（`real_*` 是录制，其余手写）。
- `tests/claude_process.rs`：用 `tests/bin/fake-claude`（Python 3，按 `FAKE_CLAUDE_*` 环境变量回放 fixture、记录收到的 stdin、模拟 interrupt / SIGINT / 崩溃 / resume 失败 / 超长行 / 后台进程 / 拒绝某个模式；`--flag=value` 和 `--flag value` 都认）端到端测 spawn / send / respond / cancel / set_settings（含清除值、部分失败）/ shutdown / 超长行 / 进程组清理 / argv 校验。
- 「resume 不读 `launch_args`」的进程级用例（`claude_process.rs`）：`resume_after_clear_does_not_resurrect_values_from_a_stale_launch_args`（带值拉起 → 退出 → 手里是「清掉之后的目标设置 + 过期的 launch_args」→ resume，argv 里没有旧值；
  对照：resume 时 settings 仍带值 → argv 有值，取的是 settings 的而不是 launch_args 的）和 `resume_passes_resume_flag_and_keeps_session_id`。
- 图片附件：`input.rs` 的单元测试（消息形状、读文件、各种坏输入）；`claude_process.rs` 的 `plain_text_messages_keep_the_string_content`、
  `images_go_into_the_stdin_message_as_content_blocks_images_first`、`an_image_only_message_has_no_text_block`、`a_multi_mib_image_goes_through_stdin_in_one_line`、
  `bad_attachments_are_refused_before_anything_is_sent`、`interjecting_with_images_is_still_unsupported`（fake 把收到的 stdin 行记进日志，测试对照形状和 base64）；
  `claude_translate.rs` 的 `echoed_user_messages_with_images_never_become_events`、`tool_result_images_are_replaced_by_a_placeholder_in_the_event`；`claude_probe.rs` 断言 `capabilities.attachments`。
- Task / Activity / Notice / 上下文用量 / elicitation（`claude_translate.rs`）：`real_recording_workflow`、`real_recording_workflow_that_fails`（`real_workflow*.jsonl`，整条回放，逐条核对快照）、
  `workflow_agents_are_mapped_state_by_state_and_merged_across_frames`（各种 `state`、乱序、缺 `workflow_phase`、没带 `workflow_progress` 的帧、终态后的帧、终态不一致的通知）、`task_kinds_titles_and_sources`、
  `subagent_progress_carries_usage_last_tool_and_summary`、`foreground_tasks_only_get_snapshots_once_they_move_to_the_background`、`a_resumed_task_starts_over_and_late_frames_of_the_previous_run_are_ignored`、`only_the_latest_finished_tasks_are_remembered`；
  `api_retry_stream_*`、`compaction_stream_*`、`rate_limit_stream_*`（手写的整条流：`api_retry.jsonl`、`compaction.jsonl`、`rate_limit_rejected.jsonl`）和逐条的 `api_retries_are_a_temporary_activity_*`、`compaction_is_an_activity_*`、`rate_limit_notices_*`、
  `notifications_and_informational_lines_*`、`model_fallbacks_permission_denials_and_hook_failures_*`、`failed_mcp_servers_*`、`local_command_output_is_a_notice_*`（`real_local_command.jsonl`）、`usage_carries_the_context_window_*`；
  `real_recording_elicitation_form` / `_url`、`an_elicitation_link_that_is_not_http_is_declined_without_asking`，以及 `permissions.rs` 里 elicitation 的单元测试（schema → 题目、答案 → 类型、转换失败、URL 校验）。
- `claude_process.rs` 的 `mcp_elicitation_*`：拿 `real_elicitation_*.jsonl` 当 fake-claude 的脚本，走完 spawn → 权限请求 → 答复 → stdin 上的 `control_response`（类型转换后的 `content`、decline、答案不合法时的 decline + `error`、非 http 链接直接 decline）。
- `tests/claude_probe.rs`：PATH 里放假的 `claude`，测探测与诊断。
- `tests/real_claude.rs`：真实 CLI，`#[ignore]`。`real_workflow_and_mcp_elicitation`（haiku 5.5，几次很短的调用，几美分以内）：一个两 agent 的 workflow（权限 → `task` 快照 → `Completed`，两个 agent 都 `Done`，CLI 自己开的那一轮带着 `context_window` / `context_tokens`），
  再用 `tests/bin/elicit-mcp-server`（项目 `.mcp.json` 注册）依次测表单提交（工具结果里能看到 `age: 36`、`subscribe: true`）、URL 确认、表单拒绝。2026-10-26 通过。

**重新录制**：`real_*.jsonl` 是用一个小驱动脚本（Python，起 `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool=stdio --model=haiku`，
写一条用户消息，自动答复 `control_request`，把 stdout 原样存下来）录的；Workflow 的 prompt 要明说「use a workflow」，elicitation 用 `tests/bin/elicit-mcp-server`（`--mcp-config` 或项目 `.mcp.json`）。
录下来先按开头的规则清洗再入库，`real_recordings_carry_no_local_environment` 兜底。
