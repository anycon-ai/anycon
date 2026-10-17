# mock agent

确定性的假 agent，给 E2E、web、daemon 测试用。**不碰网络，只写一个文件、只读 daemon 给的附件文件**：`write` 关键词在 cwd 里写 `anycon-mock.txt`（步骤 6，唯一会写的地方，见下面「`write`」）；带附件的消息会读 daemon 交给它的附件文件（步骤 2，见下面「附件」）；除此之外不碰文件系统。其它关键词的工具调用只是发事件，`echo hi` 不会真的执行。
行为表是契约：`crates/adapters/tests/mock_matrix.rs` 逐条测过，E2E 与 web 测试可以依赖。改行为要同步改这里和那个测试。

## 启用

`MockAdapter::probe` 在 `ctx.config.enabled == false` 时返回 `status: disabled`，issue 提示
`set agents.mock.enabled = true or ANYCON_MOCK_AGENT=1`。daemon 负责把 `config.agents.mock.enabled` 或环境变量
`ANYCON_MOCK_AGENT=1` 折算进传给 adapter 的 `AdapterConfig.enabled`（`AdapterConfig` 的 serde 默认是 `true`，
所以 daemon 对 mock 必须显式写 `false` 作为默认）。`spawn` 在未启用时也会拒绝。

## 探测结果

| | |
|---|---|
| id / label / driver | `mock` / `Mock Agent` / `mock` |
| 模型 | `mock-large`（默认；thinking `low` / `high`，默认 `high`）、`mock-small`（无 thinking）；`config.additional_models` 追加在后面（按 id 去重），**探测得到的，`spawn` / `set_settings` 也能真的选用**（用它自己的 `thinking` 选项和默认值） |
| 权限模式 | `ask`（normal，默认）、`plan`（safe）、`yolo`（dangerous） |
| 选项 | `verbose`（bool，默认 false） |
| capabilities | `resume`、`interrupt`、`thinking_stream`、`usage` 为 true；`interject`、`env` 等为 false；`attachments` = `["image/png", "image/jpeg", "image/gif", "image/webp"]`（ADR-35） |
| `allow_custom_model` | false |

## 一个 turn 的事件序列

下面按顺序，每一步在对应条件满足时才发。`prompt` 的关键词匹配不区分大小写（子串）。

| 步骤 | 条件 | 事件 |
|---|---|---|
| 1 思考 | 当前模型有 thinking（`mock-large`） | `thinking_delta "Considering the request. "`、`thinking_delta "Planning the answer. "`、`thinking_done { duration_ms: 20 }` |
| 2 回显 | 总是 | `text_delta "Mock reply: "`、`text_delta "[model=<m> thinking=<t> mode=<mode>]"`、（`verbose` 为 true 时 `text_delta " verbose=on"`）、（设了 `system_prompt_append` / `max_turns` / `max_budget_usd` 中任何一个时 `text_delta " prompt_append=<前 32 字符> max_turns=<n> budget=<x>"`，没设的项整项不显示，见下）、`text_delta " You said: <prompt，最多 200 字符>"`、（**消息带附件时**多一个 `text_delta " Attachments: <name> (<mime>, <bytes> bytes, sha256 <前 12 位>)[; …]"`，见下面「附件」）、`text_done`。`<t>` 在没有 thinking 的模型上是 `none` |
| 3 计划 | prompt 含 `plan` | `plan`（3 项：`Inspect the project` completed、`Implement the change` in_progress、`Run the tests` pending）→ 权限请求 `kind=plan`（见下）→ 允许：`text_delta "Plan approved."`；否则 `"Plan rejected: <message>"`（无 message 时 `"Plan rejected."`）；然后 `text_done` |
| 4 提问 | prompt 含 `question` | 权限请求 `kind=question`（见下）→ 有 `answers`：`text_delta "You answered: q1=b"`（多个 key 按字典序用 `; ` 连接，数组值（多选）用 `", "` 连接，和 claude-code adapter 一致）；否则 `"Question skipped."`；然后 `text_done` |
| 5 工具 | prompt 含 `permission`，**或**（mode 是 `ask` 且 prompt 含 `run`） | 权限请求 `kind=tool`（见下）→ 选 `allow` / `allow_session`：`tool_call` → `tool_output_delta "hi\n"` → `tool_result`；其它（`deny` 等）：`text_delta "skipped"` + `text_done`，不发 tool 事件 |
| 5' 工具（免询问） | mode 是 `yolo` 且 prompt 含 `run`（且没触发 5） | 直接 `tool_call` → `tool_output_delta` → `tool_result`，不发权限请求 |
| 5'' 大输出 | prompt 含 `big`（任何模式，不要权限请求） | `tool_call { name: "Bash", category: shell, input: {command: "cat big.log"}, id: "<session_id>-tool-<n>" }` → `tool_result { output: <约 300 000 字节的字符串>, is_error: false, exit_code: 0 }`；**没有** `tool_output_delta`。输出开头是 `BIGSTART\n`，然后 6000 行定长 50 字节的 `line NNNNN abcdefghijklmnopqrstuvwxyz0123456789ab\n`，结尾是 `BIGEND`（总长 300 015 字节，全 ASCII）。e2e 用它覆盖分片和 daemon 的 64 KiB 输出截断（保留前 48 KiB + 后 16 KiB） |
| 6 写文件 | prompt 含 `write`（任何模式，**不发权限请求**） | 在 cwd 里创建 / 追加 `anycon-mock.txt`：`tool_call { name: "Write", category: file_edit, detail: {path: "anycon-mock.txt", edit_kind: write} }` → `tool_result { detail.unified_diff }` → `file_changed { path: "anycon-mock.txt", kind: created \| modified }`；写不了时只有 `tool_call` + `tool_result { is_error: true }`。见下面「`write`」 |
| 7 错误 | prompt 含 `error` | `error { code: "mock_error", message: "Mock error requested by the prompt", recoverable: true }`，`turn_done.reason = error` |
| 8 结束 | 总是 | `turn_done { turn_id, reason, usage }`，`reason` 默认 `completed` |

### 回显串（`step 2`）的格式

```
Mock reply: [model=<m> thinking=<t> mode=<mode>][ verbose=on][ prompt_append=<p>][ max_turns=<n>][ budget=<x>] You said: <prompt>
```

- **前缀 `Mock reply: [model=… thinking=… mode=…]` 不变**；新增的三项和 ` verbose=on` 一样都在 `]` 之后、` You said:` 之前，各自只在设置了时才出现，顺序固定：`prompt_append`、`max_turns`、`budget`。三项合在同一个 `text_delta` 里（一项都没设时不多这一个 delta，事件序列和以前完全一样）。
- `prompt_append=<p>`：`system_prompt_append` 的**前 32 个字符**（按字符，不按字节；换行 / 回车 / 制表符换成空格，保持单行；不加省略号）。
- `max_turns=<n>`：整数。`budget=<x>`：`max_budget_usd` 的 Rust `Display` 写法（`2.0` → `2`，`1.5` → `1.5`，`0.25` → `0.25`）。
- **附件**（ADR-35）：消息带附件（`UserInput.attachments` 非空）时，在 ` You said: …` 那个 delta **之后**、`text_done` **之前**多发**一个** `text_delta`（不是每个附件一个），其余事件和不带附件时完全一样；不带附件时事件序列与以前逐字节相同。见下面「附件」。
- e2e 断言「`clear` 真的生效」：设置前回显里有 `prompt_append=`，`set_settings` 清掉之后回显里不再有（见下面「改设置」）。
- `env` 不回显（mock 不跑进程，capabilities.env = false）。

- `plan` 模式下 prompt 含 `run`（不含 `permission`）：什么工具都不跑。
- `permission` 关键词在**任何**模式下都会发权限请求（E2E 的「run permission」用它）。
- 一旦用户选过 `allow_session`，同一会话后续的 `kind=tool` 请求自动放行（直接 `tool_call`…），不再询问。
- **`write` 和其它关键词**：各步骤互相独立，按表里的顺序依次执行（`write` 在 `big` 之后、`error` 之前）。`write` 不受 mode 影响（`plan` 模式下也写）、自己不发权限请求；要在写之前走一次审批，同时带 `permission`——那条请求是步骤 5 的 Bash，答复只管 Bash（拒绝只是跳过 Bash，`write` 照常写）。例：`write permission` = …回显…, permission(tool), tool_call(Bash), tool_output_delta, tool_result, tool_call(Write), tool_result, file_changed, turn_done；`write big error` = …回显…, tool_call(Bash `cat big.log`), tool_result, tool_call(Write), tool_result, file_changed, error, turn_done(error)。
- **`slow`**：prompt 含 `slow` 时，每个事件之前停 200 ms（用来测取消）。等权限答复的时间不算。

### 权限请求

`request_id = "<session_id>-perm-<n>"`（n 从 1 递增，同一会话内唯一）。

| kind | title | detail | options |
|---|---|---|---|
| `tool` | `Run command` | `tool: { tool: "Bash", category: shell, input: {command: "echo hi"}, cwd: <spawn cwd>, detail: {command: "echo hi", cwd} }` | `allow`（allow_once，"Allow"）、`allow_session`（allow_always，"Allow for this session"）、`deny`（reject_once，"Deny"，`needs_message`） |
| `plan` | `Plan ready` | `text: "## Plan\n\n1. Inspect the project\n2. Implement the change\n3. Run the tests"` | `allow`（allow_once，"Start implementing"）、`deny`（reject_once，"Keep planning"，`needs_message`） |
| `question` | `Agent has a question` | `questions: [{ id: "q1", text: "Which option do you prefer?", choices: [{id: "a", label: "Option A"}, {id: "b", label: "Option B"}], multi: false, free_text: true }]` | `answer`（allow_once，"Submit answers"）、`deny`（reject_once，"Skip"，`needs_message`）。答复可以只带 `answers`（不带 option_id）也算回答 |

工具事件 id：`<session_id>-tool-<n>`（`Bash` 和 `Write` 共用同一个计数）。Bash 工具固定是 `echo hi`，输出 `hi\n`，`exit_code: 0`。

### `write`（步骤 6）：mock 唯一会写文件的地方

关键词和别的一样，不区分大小写、按子串匹配，所以 `Overwrite it`、`rewrite` 也会触发。

**写什么、写在哪**：

- 只写 **cwd 里的 `anycon-mock.txt`**（常量 `anycon_adapters::mock::WRITE_FILE_NAME`），别的路径一概不碰；不建目录。
- 文件不存在：创建，内容 `mock write turn <turn_id>\n`。文件存在：追加一行 `mock write turn <turn_id>\n`（原内容末尾没有换行就先补一个）。`<turn_id>` 是 adapter 自己的 turn 号（本进程内从 1 递增，和 `turn_done.turn_id` 同一个数；daemon 会用自己的 turn id 覆盖事件里的，所以进程重启 / 续接之后两边可能对不上，文件里记的永远是 adapter 的；一行的文本是 `anycon_adapters::mock::write_line(turn_id)`）。
- **不跟随符号链接**：目标存在且是符号链接（含悬空链接）→ 报错，不写（创建用 `O_EXCL`、追加用 `O_NOFOLLOW`，检查之后被换成链接也写不进去）。目标不是普通文件、已有内容超过 64 KiB、不是 UTF-8 文本、cwd 不存在或不可写，同样报错。
- 不受 mode 影响（`ask` / `plan` / `yolo` 都写），自己不发权限请求。

**事件**（在回显、`big` 之后、`error` 之前）：

| 事件 | 内容 |
|---|---|
| `tool_call` | `id: "<session_id>-tool-<n>"`、`name: "Write"`、`category: file_edit`、`title: "Write anycon-mock.txt"`、`input: {"path": "anycon-mock.txt", "content": "mock write turn <n>\n"}`、`detail: { path: "anycon-mock.txt", edit_kind: "write" }`。`path` 是**相对 cwd 的路径**（真实 agent 给的是绝对路径；mock 固定相对，和 tmp 目录名无关） |
| `tool_result` | 同一个 `id`；`output: "Created anycon-mock.txt"` 或 `"Modified anycon-mock.txt"`；`is_error: false`；`detail.unified_diff`（见下）。没有 `tool_output_delta` |
| `file_changed` | `{ path: "anycon-mock.txt", kind: "created" }`（新建）或 `"modified"`（追加，含已存在的空文件） |

`unified_diff` 是能被 `git apply` 接受的标准格式，hunk 头的行数是准确的：

- 新建：`--- /dev/null\n+++ b/anycon-mock.txt\n@@ -0,0 +1,1 @@\n+mock write turn 1\n`
- 追加：`--- a/anycon-mock.txt\n+++ b/anycon-mock.txt\n@@ -<起始>,<k> +<起始>,<k+1> @@\n` + 最多 3 行上下文（每行前面一个空格）+ `+mock write turn <n>\n`；例如第 2 个 turn：`@@ -1,1 +1,2 @@\n mock write turn 1\n+mock write turn 2\n`。已存在的空文件：`@@ -0,0 +1,1 @@\n+mock write turn <n>\n`。原内容末尾没有换行：最后一行按 git 的写法改成 `-<末行>\n\\ No newline at end of file\n+<末行>`，再加新行。

**写失败**（符号链接、目录、过大、非 UTF-8、cwd 不可写等）：`tool_call` 照发，然后 `tool_result { is_error: true, output: <原因> }`，**没有** `file_changed`，turn 照常以 `completed` 结束（不发 `error` 事件），目标一个字节都不改。`output` 是 `refusing to write anycon-mock.txt: <原因>`（符号链接是 `it is a symbolic link`，目录 / 特殊文件 `it is not a regular file`，`it is larger than 64 KiB`，`it is not UTF-8 text`），或 `anycon-mock.txt: <系统错误>`（比如 `No such file or directory (os error 2)`）。

**取消**：写文件之前取消 = 没写；`tool_call` 之后才取消时，文件可能已经写了但没有 `tool_result` / `file_changed`（和真实 agent 被打断时一样）。

### 附件（步骤 2 的一部分）

`capabilities.attachments` 是四种图片 mime，daemon 据此放行；mock 自己不再按 mime 拦（不认识的类型照样回显）。消息带附件时多出来的那个 `text_delta` 是：

```
 Attachments: <name> (<mime>, <bytes> bytes, sha256 <前 12 位小写 hex>); <name2> (<mime2>, …)
```

- 以 ` Attachments: ` 开头（前面有一个空格，紧接在 ` You said: <prompt>` 后面），多个附件按 `UserInput.attachments` 的顺序用 `; ` 连接。
- `<name>` / `<mime>` 取自 `AttachmentRef`；**`<bytes>` 和 sha256 是读 `attachment_paths[i]` 那个文件得出的**（不是 `AttachmentRef.bytes`），所以 e2e 能据此确认 daemon 把对的文件交给了 adapter：sha256 前 12 位 = 文件内容 SHA-256 的前 6 个字节。空文件是 `0 bytes, sha256 e3b0c44298fc`。
- 文件读不了（被回收、权限…）：这一项写 `<name> (unreadable)`，不报错，其它附件照常。
- `attachments` 和 `attachment_paths` 数量不一致：`send` 返回 `Protocol` 错误，不开 turn（`is_busy` 仍为 false）。
- 只读：mock 不写、不删、不移动附件文件；附件在 cwd 之外也无所谓，cwd 里不会因此多出任何东西。`out_chars` / `output_tokens` 把这个 delta 的字符也算进去（`input_tokens` 仍是 prompt 字符数）。
- 与关键词无关：`plan` / `permission` / `write` 等照常按各自步骤执行，附件的 delta 只影响步骤 2。`slow` 下它和别的事件一样先停 200 ms。

### 用量（确定性）

`turn_done.usage`：`input_tokens` = prompt 字符数（至少 1）、`output_tokens` = 本 turn 所有 `text_delta` 的字符数、
`cache_read_tokens = 0`、`cache_write_tokens = 0`、`cost_usd = (input + output) × 1e-6`、`duration_ms = 100`（固定）、
`model` = 本 turn 生效的模型。被取消的 turn 带的是截至取消时的数字。

## 其它行为

- **turn_id**：本进程内从 1 递增。daemon 自己生成 `turn_start`，应当用自己的 turn id 覆盖 `turn_done.turn_id`（所有 adapter 都是这样）。
- **取消**：`cancel()` 立刻让进行中的 turn 结束，发 `turn_done { reason: cancelled, usage }`（含「等权限答复」的状态；未决请求作废，之后对它的 `respond_permission` 是 no-op）。空闲时是 no-op，可重复调用。
- **续接**：`Started.resume.agent_session_id` = `"mock-<session_id>"`；用 `req.resume` 再 `spawn` 时沿用同一个 id。设置只看新请求的 `settings`（完整目标设置），**不读** `resume.launch_args`——它里面记的是这次有效的设置（`model` / `thinking` / `mode` / `verbose` / `system_prompt_append` / `max_turns` / `max_budget_usd`），仅供参考，`resume_handle()` 会随 `set_settings` 更新（不额外补发 `Started`）。
- **改设置**（「完整目标设置」语义，和 claude-code 一致）：`set_settings(settings)` 里的 `settings` 就是**整份目标设置**（daemon 传的是合并 / 清除 / 回填默认之后的整份），**缺省字段 = 清除**，不会从当前值补：
  `model: None` → `mock-large`；`thinking: None` → 该模型的默认（`mock-large` 是 `high`，`mock-small` 是 `none`）；`mode: None` → `ask`；`options.verbose` 缺省 → 关；
  `system_prompt_append` / `max_turns` / `max_budget_usd` 为 `None` → 回显里不再出现。立刻生效（`applied_now = true`、`restart_required = false`），从下一个 turn 起回显新值；
  非法值（未知模型 / 模式 / 选项、模型不支持的 thinking）返回 `InvalidSettings` 且不改变现有设置。`spawn` 用同一套解析（所以 `SpawnRequest.settings` 里缺省的字段同样回到默认）。
- **并发**：turn 进行中再 `send` 返回 `Unsupported`（mock 不支持插话，daemon 自己排队）。
- **关闭**：`shutdown()` 先取消进行中的 turn（会收到 `turn_done cancelled`），再发 `Exited { code: Some(0), error: None }`，随后事件通道关闭；之后 `send` 返回 `Exited` 错误。可重复调用。
- **未知的权限请求 id**：`respond_permission` 返回 `Ok`（幂等，应对取消 / 重复答复的竞态）。

## 例子

| prompt | 设置 | 事件（简写） |
|---|---|---|
| `hello` | 默认 | thinking×2, thinking_done, text×3, text_done, turn_done |
| `run permission` | 默认 | …回显…, **permission(tool)**, → allow → tool_call, tool_output_delta, tool_result, turn_done |
| `run permission` | 默认，答 `deny` | …回显…, permission(tool), text "skipped", text_done, turn_done |
| `slow hello` | 默认 | 每个事件间隔 200 ms；中途 `cancel()` → turn_done cancelled |
| `hello` | `system_prompt_append="Be terse."`, `max_turns=3`, `max_budget_usd=1.5` | 回显 `Mock reply: [model=mock-large thinking=high mode=ask] prompt_append=Be terse. max_turns=3 budget=1.5 You said: hello`；之后 `set_settings` 不带这三项 → `Mock reply: [model=mock-large thinking=high mode=ask] You said: hello` |
| `look` + 1 张图（内容 `abc`，名 `shot.png`，`image/png`） | 默认 | thinking×2, thinking_done, text×4（第 4 个是 ` Attachments: shot.png (image/png, 3 bytes, sha256 ba7816bf8f01)`）, text_done, turn_done |
| `error` | 默认 | …回显…, error, turn_done(error) |
| `make a plan` | 默认 | …回显…, plan, permission(plan), text, turn_done |
| `a question` | 默认 | …回显…, permission(question), text "You answered: q1=…", turn_done |
| `run it` | `mode=yolo` | …回显…, tool_call, tool_output_delta, tool_result, turn_done（不询问） |
| `big output` | 默认 | …回显…, tool_call(`cat big.log`), tool_result(约 300 KB, `BIGSTART`…`BIGEND`), turn_done（不询问，没有 tool_output_delta） |
| `write` | 默认，cwd 里没有 `anycon-mock.txt` | …回显…, tool_call(Write), tool_result(`Created anycon-mock.txt`, diff), file_changed(created), turn_done（不询问）；文件内容 `mock write turn 1\n` |
| `write` | 第 2 个 turn | …回显…, tool_call(Write), tool_result(`Modified anycon-mock.txt`, diff), file_changed(modified), turn_done；文件内容 `mock write turn 1\nmock write turn 2\n` |
| `write permission` | 默认 | …回显…, permission(tool), → allow → tool_call(Bash), tool_output_delta, tool_result, tool_call(Write), tool_result, file_changed, turn_done |
| `write`（`anycon-mock.txt` 是符号链接） | 默认 | …回显…, tool_call(Write), tool_result(`is_error: true`, `refusing to write anycon-mock.txt: it is a symbolic link`), turn_done(completed)；链接和目标都不变 |
