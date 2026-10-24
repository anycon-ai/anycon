# Daemon 功能规格

> 细化 [architecture.md § 2.2 / § 6 / § 7](../architecture.md) 和 [protocol.md § 5](../protocol.md#5-内层json-rpc-20)。本文覆盖 daemon 的全部功能：进程模型、配置、**agent 探测与能力声明**、**会话选项（模型 / 思考 / 权限模式）**、会话生命周期、事件翻译、权限、workspace、存储、多客户端、审计、可靠性。
>
> § 5 / § 6 的形态——枚举本机 agent、按模型列思考等级、各 agent 自己的权限模式集合、结构化诊断输出——是远程控制 agent 这类工具里已经验证过的做法，这里不重新发明。

## 1. 职责边界

daemon 做的事，一句话各一条：

- 对 hub 保持一条出站连接，对本机 CLI 开一个 Unix socket
- 探测本机装了哪些 agent、能用什么模型和选项，告诉客户端
- 按客户端的要求拉起 agent 进程，把它的事件翻译成协议事件，把用户输入和权限答复送回去
- 把会话历史、附件、审计存在本机 SQLite
- 验证账号内接入（扫码批准、接入码、节点口令），自己限制在线猜测
- 管 workspace：文件夹即项目

daemon **不**做：自己调用任何模型 API（标题生成、摘要都不做）、沙箱、终端、文件编辑。

## 2. 进程模型

```
launchd / systemd ──► anycon daemon
                        ├── hub 连接（一条 WebSocket，Noise 会话 × N 个客户端）
                        ├── ~/.anycon/daemon.sock（本机 CLI）
                        ├── adapter: claude-code ──► claude -p … （每会话一个进程，空闲后退出）
                        ├── adapter: codex       ──► codex app-server（每会话一个进程、一个 thread，空闲后退出；ADR-26 修订）
                        ├── adapter: acp:<name>  ──► <agent> 的 ACP 模式（一个进程，多 session）
                        └── fs watch（workspace 根目录；关联的目录不 watch）
```

- 以登录用户身份运行，没有特权
- 单实例：启动时对 `~/.anycon/daemon.lock` 加锁，第二个实例直接退出并提示
- 崩溃由服务管理器拉起；会话状态在 SQLite 里，拉起后处于 `running` 的会话标为 `interrupted`，客户端下次发消息时走 resume
- daemon 退出时对每个活着的 agent 句柄调 `shutdown()`：先 SIGINT 让它收尾，5 秒后 SIGKILL。每个 agent 子进程各在**自己的进程组**里（SIGKILL 时连它拉起的工具进程一起清；leader 已经退出时也会对进程组补一次 `killpg`），不是共用 daemon 的进程组；daemon 被 SIGKILL 时 agent 读到 stdin EOF 会自己退出
- **配对**：短码由 daemon **本地生成**（`node.pair_begin { long? }`，CLI 是 `anycon pair [--short]`，默认 16 位），用一次 Argon2id（`derive_pair_secrets`，放在阻塞线程里，约 0.1–0.3 s）得到 `match_id` 和 PSK，只把 `match_id` 登记给 hub（`pair_begin`），hub 回 `pair_registered { expires_at_ms }`。握手（daemon 是 responder）成功、两边 `pair_confirm`、hub 发 `pair_done` 之后，daemon **经刚建立的 Noise 传输向新设备发 `pair.ack`**（通知，`PairAck { node_id, peer_id, name, version, fingerprint }`），新设备校验后才落盘。细节见 [§ 15](#15-连接与可靠性) 和 [protocol.md § 4.1](../protocol.md#41-配对流程)
- **接入**：已登录账号的设备不用短码，经 hub 请求接入；daemon 用扫码密钥、接入码或节点口令验证，握手后半段和配对相同（[§ 15.1](#151-账号内接入)）
- 每个已配对客户端一个 Noise 传输和一个分片重组器；一条 RPC 的密文超过一片（200 KiB）时分片发送，收到的分片重组后再解密（[ADR-30](../decisions.md#adr-30-内层分片与大小上限)）

## 3. 配置 `config.toml`

```toml
[hub]
url = "https://hub.example.com"
name = "mac-mini"                     # 节点显示名，可在客户端改

[workspace]
root = "~/anycon"
link_roots = ["~"]                    # 可以关联为项目的本机目录范围（ADR-33）；[] = 不允许关联。热加载
trash_retention_days = 30             # .trash 自动清理
scratch_retention_days = 7            # 已结束且未转项目的 scratch 会话目录

[sessions]
max_concurrent = 3                    # 同时运行的 agent 进程数；超出进入 queued
idle_timeout = "20m"                  # turn 结束（且后台任务都结束）后 agent 进程保活多久；"0" = 常驻不退出（省去 resume 但吃内存），§ 7.3
max_attachment_bytes = "10MiB"        # 单个附件（blob）的上限，§ 7.10
history_retention = "forever"         # 或 "90d"

[permissions]
allow_dangerous_modes = true          # 是否允许 risk = dangerous 的权限模式（bypass / full access）；每次拉起 / resume agent 时按当前值重新判断
request_timeout = "0"                 # 0 = 不超时；设了则到期自动 deny
allow_session_env = false             # 是否接受客户端在 session.create / set_settings 里带 settings.env（ADR-31）。默认关
session_env_allow = []                # 开启后允许的键：精确键名或 "PREFIX_*"（如 ["MY_FLAG", "FEATURE_*"]）。默认空 = 什么都不放行；键还要过黑名单（§ 5.5），并且不能覆盖 agents.env 里运维配置的键

[agents]
env_from_login_shell = true           # 启动时用登录 shell 解析 PATH 等环境
probe_interval = "10m"

[agents.claude-code]
enabled = true
command = "claude"                    # 可写绝对路径，或带参数的命令
default_model = "opus"
default_thinking = "high"
default_mode = "acceptEdits"
env = { }                             # 追加到子进程环境
additional_models = [                 # 探测列表之外再加的模型（公司代理、Bedrock 名字等）
  { id = "my-proxy-model", label = "Proxy" },
]

[agents.codex]                        # claude-code 和 codex 不写这一段也参与探测（§ 5.6）；写 enabled = false 关掉
enabled = true
command = "codex"
default_mode = "workspace"

[agents.acp.opencode]                 # ACP agent 按名注册；内置一张已知表（§ 5.7），用户可加 / 覆盖
command = "opencode acp"
dangerous_modes = []                  # ACP 不提供风险等级，由用户标记

[worktree]                            # 每会话 worktree（ADR-34，§ 10.1）。整段热加载
default = "worktree"                  # git 项目里新会话的初始选择：worktree | local。只影响客户端的初始选择；线路上不带 workspace 时总是 local
branch_prefix = "anycon"              # 新分支名 <prefix>/<slug>；空串 = 不加前缀。允许 [A-Za-z0-9._-]，1–40 个字符，不能是 main / master / HEAD
auto_archive_on_pr_close = false      # 分支的 PR 合并 / 关闭后自动归档会话（要装好并登录 gh）
include_max_files = 10000             # .worktreeinclude 复制上限
include_max_bytes = "1GiB"

[enroll]                              # 账号内接入（ADR-36，§ 15.1）。整段热加载
approval = true                       # 接受扫码批准和接入码；false = 只能短码配对或节点口令
password = true                       # 设了节点口令时接受口令接入；口令本身用 anycon passwd 设，不写在这里

[notify]
turn_finished = "when_unwatched"      # always | when_unwatched | never
session_idle_after = "30m"

[log]
level = "info"
retention_days = 7                     # 只在 daemon 启动时清理一次，长期不重启会累积
```

`anycon config set` 可改；标 **热加载** 的键改了立即生效：`workspace.link_roots`、`sessions.*`、`permissions.*`（含 `allow_dangerous_modes`、`allow_session_env`）、`agents.*.default_*`、`agents.*.enabled`、`enroll.*`、`notify.*`、`log.level`、`worktree.*`。其余需重启。`hub.url` 同时决定 `anycon pair` 给出的配对链接的地址（`${hub.url}/pair#code=…`）。

环境变量覆盖文件里的值，写法是 `ANYCON_<段>__<键>`（双下划线表示层级），比如 `ANYCON_SESSIONS__MAX_CONCURRENT=2`；数组按 TOML 写：`ANYCON_WORKSPACE__LINK_ROOTS='["/abs/path"]'`。键名里带连字符的（`agents.claude-code.*`）没法用环境变量设，用 `anycon config set`。`ANYCON_HOME`、`ANYCON_LOG`、`ANYCON_MOCK_AGENT` 不是配置键（`ANYCON_MOCK_AGENT=1` 等价于 `agents.mock.enabled = true`）。

## 4. 数据目录

见 [cli.md § 2](cli.md#2-目录与配置)。会话历史是**唯一副本**：备份就是停掉 daemon 复制数据目录，或者在线 `sqlite3 daemon.db ".backup …"` 再加上 `blobs/`（`anycon export` 在 v0.2）。

## 5. Agent 探测与能力声明

### 5.1 为什么要探测

客户端在手机上，看不到那台机器。它要知道：装了哪些 agent、哪个能用、不能用是为什么、能选什么模型、每个模型能开什么思考等级、有哪些权限模式。这些全部由 daemon 探测后通过 `agent.list` 给出，**客户端不内置任何 agent 知识**。

### 5.2 `AgentInfo`

```
AgentInfo {
  id:        "claude-code" | "codex" | "opencode" | "pi" | …   // adapter id；ACP agent 用自己的名字
  label:     "Claude Code"
  driver:    "claude-stream-json" | "codex-app-server" | "acp" | "sdk"
  enabled:   bool                      // config 里关掉的 agent 仍然列出，status = disabled
  status:    "ready" | "loading" | "disabled" | "not_installed" | "not_authenticated"
           | "unsupported_version" | "error"       // loading：首次探测还没跑完，UI 先画骨架
  version?:  "2.1.293"
  path?:     "/Users/me/.nvm/versions/node/v24/bin/claude"
  issue?:    { code, message, hint }   // status != ready 时的原因和修复建议

  models: [ {
    id:          "claude-opus-5-5"     // 传给 agent 的原样字符串
    label:       "Opus 5.5"
    description: "Latest release"
    default:     bool
    thinking?:   { options: [ { id: "low", label } … ], default: "medium" }   // 按模型不同
    tags?:       [ "1m" ]
  } ]
  allow_custom_model: bool             // 允许用户输入列表外的模型 id

  modes: [ {                           // 权限模式，agent 自己的集合
    id: "acceptEdits", label: "Accept File Edits", description, default: bool,   // id 是 agent 自己的，不翻译
    risk: "safe" | "normal" | "dangerous"
  } ]

  options: [ ConfigOption ]            // 其他 agent 特定选项，见 § 6.2

  capabilities: {
    resume:            bool            // 进程退出后能否续接
    interrupt:         bool
    interject:         bool            // turn 进行中能否插话
    attachments:       [ "image/png", "image/jpeg", "image/gif", "image/webp" ]   // 收哪些附件；空 = 不收（§ 7.10）
    thinking_stream:   bool            // 会不会发 thinking_delta
    usage:             bool            // turn_done 带不带 usage
    structured_output: bool
    env:               bool            // 支持每会话 env
  }
  probed_at: timestamp
}
```

### 5.3 探测流程

每个 adapter 实现 `probe()`，daemon 在启动、`agent.list { refresh: true }`、`probe_interval` 到期、会话以 `agent_not_found` / `auth_failed` 失败后各跑一次。结果缓存，变化时向所有在线客户端推 `agent.changed`。

```
1. 解析命令      config.agents.<id>.command → 绝对路径直接用；
                 否则 `which -a <cmd>`（用「daemon 环境的 PATH」，§ 5.5）列出全部候选，
                 再补已知位置：~/.local/bin ~/.bun/bin ~/.cargo/bin /opt/homebrew/bin /usr/local/bin
                 ~/.nvm/versions/node/*/bin ~/.volta/bin ~/.npm-global/bin
                 对每个候选跑 `--version` 探活（2 秒超时），取第一个能执行的——
                 nvm / volta 的坏 shim 在这一步被跳过
                 → 一个都没有：not_installed，hint 给安装命令
2. 版本          上一步的 `--version` 输出解析 semver，和 adapter 的支持区间比较
                 → 低于最低版本：unsupported_version，hint 给升级命令
                 → 命令挂了 / 非零退出：error，带 stderr 前 200 字节
3. 认证          adapter 特定（§ 5.4）。能确定就给 not_authenticated + hint；
                 不能确定标 unknown，不阻塞，真正失败时会话以 auth_failed 结束
4. 模型与选项    adapter 特定（§ 5.4）。这一步可能要起一个 agent 进程（Codex 的 model/list、
                 ACP 的 session/new），是整个探测里最慢的
5. 写缓存        agent_cache 表，含 probed_at；diagnose 文本另存
```

- 整个 `probe()` 跑在**一个总超时**下（默认 60 秒，`agents.probe_timeout`），超时错误提示运行 `anycon doctor`（错误里列出卡在哪一步——`version` / `auth` / `models`——还没做，见 roadmap）
- 同一时间每个 agent 最多一个 probe 在跑；同时探测的 agent 数不限（彼此独立）
- `probe_interval` 的周期性探测只跑第 1–3 步（便宜）；第 4 步只在首次、`refresh: true`、二进制路径或版本变了、`agents.<id>` 配置变了时跑。装了新 agent 几分钟内自动出现，而模型列表不会每 10 分钟拉起一次 app-server

所有子进程调用都带超时、都在 workspace 根目录外的临时 cwd 执行（避免触发 agent 读项目配置）。

### 5.4 各 agent 的探测方法

| | Claude Code | Codex | ACP agent | SDK 节点 |
|---|---|---|---|---|
| 驱动 | `claude -p --output-format stream-json --input-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio`，stdin 常开；少了 `--permission-prompt-tool stdio` CLI 会自动拒绝所有需确认的工具而不发请求 | `codex app-server`（stdio，换行分隔的 JSON-RPC，省略 `jsonrpc` 头；先 `initialize`）。**不用 `codex exec`**：它在无头模式下写死 `approval_policy = never`，无法把审批交给人 | ACP v1（`protocolVersion: 1`），stdio；v2 仍是草案，按协商结果启用 | 节点自己就是协议实现 |
| 命令 | `claude` | `codex` | § 5.7 注册表里的命令，如 `opencode acp` | 不探测，节点自己声明 |
| 版本 | `claude --version` | `codex --version`（输出如 `codex-cli 0.162.1`） | `<cmd> --version` 尽力而为；`initialize` 返回的 `agentInfo.version` 优先 | — |
| 认证 | `claude auth status`（JSON；退出码 0 已登录、1 未登录；`authMethod` 给出方式）。`--bare` 模式不读 OAuth，所以 daemon 不用 `--bare` | `codex login status`（文本在 stderr、stdout 为空；退出码 0 已登录，1 + `Not logged in` 未登录，1 + 其它文字（如 `auth.json` 损坏）按 unknown 不挡）。凭证在 `~/.codex/auth.json` 或系统钥匙串。**权威判定是 app-server 的 `account/read`**（完整探测里，`config/read` 之后调）：`requiresOpenaiAuth = false`（自定义提供方等）→ 不挡；`account` 为空且 `requiresOpenaiAuth = true` → `not_authenticated`（hint `codex login`）；其余按已登录。完整探测里 `account/read` 用不了（老版本、超时、app-server 起不来）时，依次退回：同一个二进制路径上一次**权威**（来自 `account/read`）的判定 → `codex login status`（只有确认未登录才挡）。cheap 探测只跑 `codex login status`：结果和上一次一样就沿用上一次的判定（不拉起 app-server）；不一样（登录了 / 退出了）才在 cheap 探测里补跑一次 `account/read`（只做这一步，不拉模型列表）。「上一次的判定」存在 adapter 实例里（daemon 一直用 `anycon_adapters::all()` 给的同一个实例）；`login status` 没变、只改了提供方配置时 cheap 探测发现不了，要等下一次完整探测。`model/list` 不需要登录 | `initialize` 返回的 `authMethods`；`session/new` 报认证错误 → `not_authenticated`，hint 列出 `authMethods[].name`；`type = terminal` 的方法要用户在机器上跑一次命令 | — |
| 模型列表 | CLI 没有独立的列模型命令（stream-json 的 `initialize` 控制请求会返回 `models[]`（含 `supportedEffortLevels`）、`commands`、`account`，约 5 秒回，但要拉起完整 CLI 含用户的 MCP server，周期探测不用它；`initialize` 本身不是必需的，interrupt / set_model / set_permission_mode / can_use_tool 不发它也能用）。adapter 内置一张表（完整 id + 别名 `opus` / `sonnet` / `haiku` / `fable`、`[1m]` 后缀），每项带 `min_cli_version`，按 `claude --version` 过滤；**按模型给出思考等级**；再把用户 `~/.claude/settings.json` 的 `model` 和环境里 `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_*_MODEL` 的值追加为可选项（Bedrock / Vertex / 代理用户靠这个）；`allow_custom_model = true` 让用户填任意 id。表随 adapter 版本更新 | `model/list` → 每项 `id / displayName / description / supportedReasoningEfforts[] / defaultReasoningEffort / isDefault / hidden / upgrade`；再 `config/read` 取用户 `~/.codex/config.toml` 里的 `model` 和 `model_reasoning_effort` 作为默认项（列表里没有这个模型——常见于自定义 `model_provider`——就追加一项，思考等级取列表里各模型共有的等级并上 `model_reasoning_effort`）。**不内置模型 id**（Codex 的模型淘汰很快，官方文档也说用返回值） | 在临时 cwd 下 `session/new`，取 `configOptions` 里 `category = "model"` 的 `options`（可能分组）；没有就不提供模型选择 | 节点声明 |
| 思考等级 | Claude Code 的 `--effort`：`low / medium / high / xhigh / max`；4.6 系列没有 `xhigh`；Opus 5.5 / Sonnet 5.5 / Haiku 5.5 不能关（无 `off`）。这就是为什么**等级挂在模型上** | `model/list` 的 `supportedReasoningEfforts`，按模型（`low / medium / high / xhigh / max / ultra` 的子集） | `configOptions` 里 `category = "thought_level"`；没有就不提供 | 节点声明 |
| 权限模式 | `default`(Always Ask) / `acceptEdits` / `plan` / `auto` / `dontAsk` / `bypassPermissions`；`bypassPermissions` 标 dangerous，`plan` 标 safe | adapter 把 `approvalPolicy × sandbox × approvals_reviewer` 收成四档：`read_only`（safe：`readOnly` + `on-request`）/ `workspace`（normal，默认：`workspaceWrite` + `on-request`，审批送客户端）/ `auto_review`（normal：`approvals_reviewer = auto_review`，Codex 自己的审查子代理放行）/ `full_access`（dangerous：`dangerFullAccess` + `never`） | `session/new` 返回的 `modes.availableModes`（v1，已标 deprecated）或 `configOptions` 里 `category = "mode"`。ACP 不给风险等级：默认全部 `normal`，`config.agents.acp.<name>.dangerous_modes` 可标 dangerous | 节点声明 |
| 其他选项 | `fast_mode`(bool，仅支持的模型) | `fast_mode`(bool → `serviceTier`：开 `priority`、明确关 `default`、没指定不传)、`model_reasoning_summary`(select: auto / concise / detailed / none → `turn/start.summary`)。`plan_mode` v0.1 **不提供**：`collaborationMode` 只在 experimental schema 里（要 `experimentalApi`），v0.1 只用稳定接口，所以 Codex 也不会发 `kind = plan` 的权限请求 | 其余 `configOptions` 原样透传：`select` 和 `boolean` 两种 `type`，`category = model_config` 或自定义 | 节点声明 |

各 CLI 变化快，adapter 里这些探测逻辑要和 CLI 版本一起维护；回放测试用录制的真实输出。Codex app-server 官方标注 experimental，CI 里用 `codex app-server generate-json-schema` 生成对应版本的 schema 做编译期校验。

### 5.7 ACP agent 注册表

daemon 内置一张来自 [ACP 官方注册表](https://github.com/agentclientprotocol/registry) 的快照（随 release 更新），只收录**用户机器上已经有二进制**的启动方式：

| agent | 命令 |
|---|---|
| OpenCode | `opencode acp` |
| Gemini CLI | `gemini --acp` |
| Goose | `goose acp` |
| Kimi Code | `kimi acp` |
| Qwen Code | `qwen --acp` |
| Cursor | `cursor-agent acp` |
| GitHub Copilot | `copilot --acp` |
| Pi | `pi-acp`（需用户自己装 `npm i -g pi-acp`） |

探测规则：命令的第一个词在 daemon 环境的 `PATH` 里能找到才算安装；**不自动跑 `npx` / `uvx`** 形式的分发（那会在用户机器上下载并执行东西）。用户在 `config.agents.acp.<name>.command` 里加的任意命令照单全收。Claude Code 和 Codex 也有 ACP 封装（`claude-agent-acp`、`codex-acp`），anycon 不用它们，原生 adapter 的控制面更完整。

### 5.5 子进程环境

launchd / systemd 起的 daemon 没有用户 shell 的 `PATH`，`nvm`、`bun`、`cargo` 装的 agent 都找不到。这是远程控制类工具最常见的「在终端能用、在 daemon 里不能用」问题。

- 启动时执行 `$SHELL -ilc 'printenv -0 2>/dev/null || env -0'`（15 秒超时；失败再试一次非交互的 `-lc`），把结果作为 **daemon 环境**：找 agent、拉起 agent 都用它。macOS 自带的 BSD `printenv` 没有 `-0`（报 `illegal option`，整个解析会失败），所以回落到 `env -0`。两次都失败则退回进程自身环境并在日志和 `doctor` 里警告。桌面应用通常在壳层做这件事，anycon 没有桌面壳，所以放在 daemon 里。用户的 rc 文件重时这一步要 1–4 秒、期间 socket 还没开；测试 / CI 里可设 `agents.env_from_login_shell = false`（环境变量 `ANYCON_AGENTS__ENV_FROM_LOGIN_SHELL=false`）跳过
- 从 daemon 环境里**剔除** `CLAUDECODE`、`CLAUDE_CODE_ENTRYPOINT`、`CLAUDE_CODE_SSE_PORT`、`CLAUDE_AGENT_SDK_VERSION` 这类「我正在某个 agent 会话里」的标记：用户常在 Claude Code 的终端里敲 `anycon daemon`，不剔除的话拉起的 Claude Code 会拒绝嵌套启动。剔除只在 daemon 构造环境时做一次，adapter 不再重复剔除
- `config.agents.env` 和 `agents.<id>.env` 追加 / 覆盖。这是运维配的环境，**最高优先级**：会话 `env` 不能覆盖里面出现的键
- 每会话可再传 `env`（`session.create.settings.env`），但**默认整体被拒绝**（`permissions.allow_session_env = false`，ADR-31）。开启之后一个键要同时满足三条才接受，拒绝时返回 `invalid_settings { field: "env.<KEY>", reason }`（实现按 2 → 1 → 3 的顺序检查，所以黑名单键即使写进了允许列表也先按「黑名单」报错；配置加载时对这种允许项会 warn）：
  1. 在运维配置的**允许列表** `permissions.session_env_allow` 里（`anycon_agent_api::env_key_in_allowlist`：每项是精确键名或 `PREFIX_*`，大小写敏感，裸 `*` 不是通配；默认空，所以只开开关不配列表等于什么都不放行）
  2. 通过 `anycon_agent_api::env_key_allowed`：键名是合法标识符（`[A-Za-z_][A-Za-z0-9_]*`），不在保护集合（`PATH`、`HOME`、`SHELL`、`USER`、`LOGNAME`、`TMPDIR`），不命中黑名单。黑名单是**兜底**不是主防线（复验实测 `SHELLOPTS` + `PS4`、`LESSOPEN` 都能执行代码而第一版名单没有它们——黑名单枚举不全），完整名单以 `crates/agent-api/src/lib.rs` 的 `ENV_DENY_PREFIXES` / `ENV_DENY_EXACT` 为准，大致覆盖：动态链接（`LD_*`、`DYLD_*`、`GCONV_*`）、shell 启动 / 调试钩子（`BASH_ENV`、`ENV`、`SHELLOPTS`、`BASHOPTS`、`PS0`–`PS4`、`PROMPT_COMMAND`、`ZDOTDIR`、`FPATH`、`IFS`）、会被工具拉起的外部程序（`EDITOR`、`VISUAL`、`PAGER`、`LESSOPEN`、`VIMINIT`、`GIT_SSH*`、`*_ASKPASS`）、编译器 / 构建器（`CC`、`CXX`、`LD`、`MAKEFLAGS`、`RUSTC*`、`CARGO_*`）、各语言运行时和包源（`PYTHON*`、`PIP_*`、`UV_*`、`NODE_*`、`npm_config_*`、`BUN_*`、`DENO_*`、`RUBY*`、`GEM_*`、`PERL*`、`LUA_*`、`JAVA_*`、`_JAVA_*`、`JDK_*`、`DOTNET_*`、`GO*` 的若干、`PHP*`、`R_*`）、容器 / 集群（`DOCKER_*`、`KUBE*`、`KUBECONFIG`）、模型 / 云厂商和本项目的变量（`ANYCON_*`、`CLAUDE*`、`ANTHROPIC_*`、`OPENAI_*`、`GEMINI_*`、`GOOGLE_*`、`AWS_*`、`AZURE_*`、`GH_*`、`GITHUB_*`）、证书 / 名字解析 / 本地化（`SSL_CERT_*`、`CURL_CA_BUNDLE`、`NODE_EXTRA_CA_CERTS`、`HOSTALIASES`、`LOCPATH`、`NLSPATH`、`TERMINFO*`）、代理变量（大小写两种）、`XDG_*`
  3. 不覆盖 `agents.env` / `agents.<id>.env` 里运维配置的键
  - 另外：`permissions.allow_session_env = false`（默认）时带任何非空 `env` 都得到 `invalid_settings { field: "env" }`；开启后每个值不含 NUL 且 ≤ 8 KiB，最多 64 个键。每次拉起 agent 前也会按当前配置重新校验，关掉开关或收紧允许列表后，已带 env 的会话下一个 turn 会失败
  - 为什么：这些变量能让子进程在审批之外执行代码（`BASH_ENV`、`LD_PRELOAD`、`NODE_OPTIONS`、`SHELLOPTS` + `PS4`），或者把提示词和凭据转发到别处（`ANTHROPIC_BASE_URL`、`*_PROXY`、`npm_config_registry`）。已配对客户端本来只能在审批下行动。对绝大多数部署，正确配置就是不开
  - 每次设置会话 env 都进审计，记**键名**，不记值
- `anycon doctor` 和 `agent.diagnose` 都打印：命令来源（config / PATH / 已知位置）、daemon 的 `PATH`、daemon 的 shell、`which -a <cmd>`、`$SHELL -lc 'type -a <cmd>'`、解析到的路径、版本、认证状态、模型数。用户一眼能看出「终端里的 claude 和 daemon 找到的 claude 不是同一个」

### 5.6 RPC

| 方法 | 说明 |
|---|---|
| `agent.list { refresh?, if_none_match? }` | `{ agents: [AgentInfo], etag }`。`refresh` 强制重探（含第 4 步）；`if_none_match` 等于当前 `etag` 时返回 `{ agents: [], etag, not_modified: true }`，客户端每次连上都可以便宜地问一次（客户端缓存上一次的列表）。返回的是**客户端视图**：探测结果叠加配置——`agents.<id>.default_*` 反映到各项的 `default` 标记（所以 `etag` 随默认值变化，配置变化时推 `agent.changed`）；`permissions.allow_dangerous_modes = false` 时 `risk = dangerous` 的模式不出现在 `modes` 里（校验仍按完整信息，直接指定得到 `forbidden`）；`agents.<id>.dangerous_modes` 里的模式被标为 dangerous；内置的真实 agent（`claude-code`、`codex`）**没有配置段也参与探测**（没装就是 `not_installed`，客户端据此显示「未安装」和安装命令），`mock` 只在有 `[agents.mock]` 段或 `ANYCON_MOCK_AGENT=1` 时出现；`enabled = false` 的以 `status: disabled` 列出且不调 `probe` |
| `agent.diagnose { id }` | § 5.5 的诊断文本 + 结构化字段 |
| `agent.changed` (D→C) | 探测结果变化（装了新 agent、登录了、版本升了） |
| `node.set_agent_defaults { agent, model?, thinking?, mode?, options? }` | 写 `config.agents.<id>.default_*`，所有客户端共享。和 `session.create` 走**同一套校验**（§ 6.1）：选项值要合法，危险模式在 `allow_dangerous_modes = false` 时返回 `forbidden`；写入进审计（`config.changed`） |

## 6. 会话选项

### 6.1 `SessionSettings`

```
SessionSettings {
  model?:    string           // AgentInfo.models[].id，或自定义
  thinking?: string           // 所选模型的 thinking.options[].id
  mode?:     string           // AgentInfo.modes[].id
  options?:  { [id]: value }  // AgentInfo.options[] 里的
  env?:      { [k]: v }
  system_prompt_append?: string
  max_turns?: u32
  max_budget_usd?: f64        // agent 支持才有效（Claude Code 支持）
}
```

`session.create` 和 `session.set_settings` 都收这个结构。daemon 校验：模型在列表里或 `allow_custom_model`；思考等级在该模型的选项里；mode 在列表里；否则 `invalid_settings { field, reason }`。`dangerous` 的 mode 还要 `permissions.allow_dangerous_modes = true`，不满足返回 `forbidden`（CLI 退出码 5），而不是 `invalid_settings`；`allow_dangerous_modes = false` 时 `agent.list` 本来就不列出这些模式，所以只有绕过 UI 直接指定才会撞上。这条检查**不只在 create / set_settings 时做**：会话每次拉起 agent 进程（idle 超时后的 resume、`restart_required` 重启、daemon 重启后的 interrupted、`session.restart`）前都按**当前配置**重新判断；配置在会话存续期间被收紧时，turn 以 `forbidden` 失败，不会悄悄带着危险模式再跑。`node.set_agent_defaults` 同样受这条约束

会话 `env`：`permissions.allow_session_env = false`（默认）时带 `env` 会被拒绝；开启后每个键过 `env_key_allowed`，规则见 [§ 5.5](#55-子进程环境)。拒绝返回 `invalid_settings { field: "env.<KEY>" }`

一个例外：只改 `model` 而原 `thinking` 不在新模型的选项里时，daemon **不报错**，而是回落到新模型的默认等级，并在 `settings_changed` 事件里给出实际生效的完整 settings（客户端据此提示「思考等级已调整为 medium」）。模型切换是高频操作，不该因为等级名对不上而失败。

### 6.2 `ConfigOption`（agent 特定选项）

```
ConfigOption {
  id: "fast_mode", label, description?,
  kind: "bool" | "select" | "text",
  choices?: [ { value, label, description? } ],
  default: value
}
```

客户端用通用控件渲染，放在「更多选项」里。Codex 的 fast mode、ACP agent 的自定义 configOptions 都走这里。

### 6.3 各 agent 的映射

| 设置 | Claude Code（`claude -p`） | Codex（app-server） | ACP |
|---|---|---|---|
| `model` | `--model <id>`；会话中 `set_model` | `thread/start { model }`；会话中 `turn/start { model }` 按 turn 覆盖 | `session/set_config_option { sessionId, configId: <model 项的 id>, value }` |
| `thinking` | `--effort <level>`；会话中无法直接改 → 下一 turn 前**重启进程并 resume**（§ 7.4） | `turn/start { effort }` 按 turn 传，天然 live | `session/set_config_option`，`configId` 为 `thought_level` 那一项 |
| `mode` | `--permission-mode <mode>`；会话中 `set_permission_mode` | `thread/start` / `turn/start` 的 `approvalPolicy` + `sandboxPolicy`（wire 上是 camelCase：`workspaceWrite`、`dangerFullAccess`） | `session/set_mode { sessionId, modeId }`，或 `set_config_option` 的 `mode` 项 |
| `env` | 子进程环境 | 子进程环境（v0.1 每会话一个 app-server 进程，ADR-26 修订） | 子进程环境（进程按 agent 共享时不支持，adapter 按实际情况声明） |
| `system_prompt_append` | `--append-system-prompt` | 新会话 `thread/start { developerInstructions }`；之后改动用 `thread/inject_items` 追加一条 developer 消息，立即生效（`thread/resume` 会忽略 `developerInstructions`，以 rollout 里的为准；旧指令留在历史里） | 不支持 |
| `max_turns` / `max_budget_usd` | `--max-turns` / `--max-budget-usd` | 不支持（带了 → `invalid_settings`） | 不支持 |
| 附件 | stdin 消息 `content` 数组里的 `image` block（`source: { type: "base64", media_type, data }`，adapter 读 daemon 的文件；图在前、文字在后） | `turn/start.input` / `turn/steer.input` 的 `{ type: "localImage", path }`（daemon 的文件，扩展名与 mime 一致） | `session/prompt` 的 image ContentBlock（`promptCapabilities.image` 为 true 才行） |
| 结构化输出 | `--json-schema` | `turn/start { outputSchema }` 存在，但 v0.1 的 `SessionSettings` 没有东西喂它，`capabilities.structured_output = false` | 不支持 |

### 6.4 会话中修改

对客户端只有一条规则：**turn 进行中锁定，其余时候都能改，改动从下一个 turn 生效。** adapter 内部是用控制消息热改还是重启进程再 resume，客户端不需要知道。`session.set_settings` 成功后 daemon 发 `settings_changed` 事件并持久化。

`session.set_settings` 的 `settings` 是增量：缺省或 `null` 的字段保持不变，`options` 里值为 `null` 表示删键。要把一个值**清掉**（回到未设置 / agent 默认）得在 `clear` 里点名（`model` `thinking` `mode` `env` `system_prompt_append` `max_turns` `max_budget_usd`），daemon 先合并 `settings` 再处理 `clear`，然后按清除后的完整设置重新校验（`model` 清掉时 `thinking` 按默认模型回落），把**完整设置**交给 `AgentHandle::set_settings`（adapter 把缺省字段视为清除，必要时 `restart_required`），持久化的也是清除后的完整设置，resume 不会复活旧值：没有存活进程时 `set_settings` 同步改写 `launch_args`（清掉的字段从里面删掉），adapter 拉起时也只从 resume 句柄取 `agent_session_id`、其余一律按 daemon 给的完整设置（两边各自独立成立）。校验细节：`max_budget_usd` 必须是有限的非负数，`system_prompt_append` ≤ 16 KiB；审计 detail 带 `cleared`（清掉了哪些字段）。set_settings 成功返回的是**生效后的完整设置**，`clear` 点名的字段在里面已经消失，客户端直接用它替换本地。

### 6.5 默认值

`session.create` 不传的字段按顺序取：`config.agents.<id>.default_*` → `AgentInfo` 里标 `default` 的项。客户端自己的「上次用的值」是客户端的事（[web.md § 5.4](web.md#54-默认值)）。

## 7. 会话生命周期

### 7.1 状态

```
                 ┌────────── queued（并发满）──────────┐
                 ▼                                      │
create ──► starting ──► running ──► idle ◄──────────────┘
                          │  ▲        │
                          │  │        ├── idle_timeout → 进程退出，状态仍是 idle（可 resume）；后台任务在跑时不回收（§ 7.3）
                          │  │        └── 后台任务结束，agent 自己跑一轮 → running（§ 7.5）
                          │  └── send
                          ├── permission.request ──► waiting_permission ──► running
                          ├── cancel ──► idle
                          ├── 正常结束（agent 退出；max_turns / 预算用完）──► ended（进程还活着的，idle_timeout 后同样回收）
                          └── 异常 ──► error（recoverable 时可 send 触发 resume）
归档：queued / starting / running / **waiting_permission** 时拒绝（`busy`，先停止或答复；归档状态下不跑 turn），其余状态都可 archive；archived 是 flag 不是状态。已归档的会话不开新 turn（排队的消息不会在归档之后作为新 turn 发出）。已知限制：turn 结束后积压消息走的重新排队路径还没有这道检查，插话竞态下仍可能开 turn；归档也不清队列——失败 turn 留下的消息会在取消归档之后随下一条消息一起发出，客户端目前看不到这个队列
```

- `starting`：进程已拉起、还没收到 agent 的 init / 第一个事件。超过 60 秒没动静 → `error { code: agent_start_timeout }`
- `idle` 和「进程是否活着」解耦。客户端只看到 idle；进程保活与否是 daemon 的优化
- `interrupted`：daemon 重启时发现的孤儿会话。下次 `send` 走 resume；adapter 不支持 resume 的转为 `ended` 并提示

### 7.2 并发

`max_concurrent` 限制的是**正在运行 turn** 的会话数，不是 idle 的（agent 自己开的 turn 不占名额，§ 7.5）。满了时 `session.create` / `send` 进入 `queued`，按 FIFO 启动，事件 `status { state: queued, position }`。客户端可以取消排队。

### 7.3 进程保活与 idle_timeout

- Claude Code：stream-json 双向模式，turn 结束后保持 stdin 打开，进程保活到 `idle_timeout`；之后 SIGINT 退出。下次 `send` 用 `--resume <agent_session_id>` 重新拉起，用户无感
- Codex：v0.1 每个会话一个 `codex app-server` 进程、一个 thread（ADR-26 修订），和 Claude Code 一样 turn 结束后保活到 `idle_timeout` 再退出；下次 `send` 重新拉起 app-server，`initialize` 之后 `thread/resume { threadId: agent_session_id }`
- ACP：一个 agent 进程服务该 agent 的所有会话。agent 声明 `sessionCapabilities.resume` 或 `loadSession` 的，和 Claude Code 一样按 `idle_timeout` 退出、下次 `session/resume`（不重放）或 `session/load`（重放，daemon 吞掉重放事件只取最后状态）；两者都没有的，进程保活到所有会话 `ended`
- **后台任务**：Claude Code 的 subagent（默认在后台跑）和 `run_in_background` 的命令在主 turn 结束之后还在同一个进程里干活。adapter 从 stream-json 的 `system/background_tasks_changed`（全量列表；`task_started` / `task_notification` / `task_updated` 兜底）得到还在跑的任务数，变化时发 `AgentEvent::Background { tasks }`。daemon 记在会话上（`SessionSummary.background_tasks`、`session.changed` 带出，不进事件流，进程摘掉时清零），大于 0 时：
  - 不按 `idle_timeout` 回收进程（`idle` 和回合数 / 预算用完的 `ended` 都一样）——回收会关掉整个进程组，连同后台任务一起杀掉。任务都结束时重新开始计 `idle_timeout`，而不是从上一个 turn 结束算起；
  - 不发 `session_idle`；
  - 改了需要重启进程才生效的设置（§ 6.4），下一个 turn 沿用当前进程，重启推迟到后台任务都结束之后的 turn。进程里的环境、系统提示追加等在这期间还是旧的；
  - 删除、归档、`session.restart`、目录被换掉、取消宽限超时这些本来就要收进程的操作照旧收，后台任务随之结束。
  - 后台任务没有上限：一直不结束的（比如让 agent 在后台起的 dev server）会让进程一直保活，直到上面这些操作收掉它

### 7.4 Resume

每个会话记录 `agent_session_id`（Claude Code 的 `session_id`、Codex 的 thread id、ACP 的 session id）和启动参数。resume 时**重新传**所有启动参数（Claude Code 的 `--add-dir`、`--mcp-config`、`--settings` 不会随 resume 恢复）。同一 agent 会话不允许两个进程同时 resume：旧进程还在收尾时，新的 `send` 先等它退完。resume 前还要按当前配置重新判断危险模式（§ 6.1）。

Codex：一个**从来没有记录过用户消息**的 thread 没有 rollout（比如第一个 turn 因为没登录立刻失败），之后 `thread/resume` 报 `no rollout found`。adapter 回退成 `thread/start` 新开一个，`Started` 带新的 `agent_session_id`。Codex 的 `set_settings` 只有 `env` 变了才需要重启进程；模型、思考、模式、`fast_mode`、`model_reasoning_summary` 都按 turn 传，`system_prompt_append` 用 `thread/inject_items`，都立即生效。

已知限制（Claude Code）：Claude Code 按 cwd 存会话，会话目录挪了之后 `--resume` 找不到原来的会话（`No conversation found`）——根目录项目改名、scratch 会话 `promote`、跟着项目改名移动的 worktree 都会这样，下一次发消息失败。关联项目（目录不动）和 Codex 不受影响。

### 7.5 Turn 与消息合并

- 每次 `send` 开一个 turn，`turn_id` 单调递增，事件 `turn_start { turn_id, message_id }`
- **agent 自己开的 turn**：Claude Code 的后台任务结束后，CLI 不等用户消息、自己再跑一轮处理结果（没有 turn 时出现的 `system/init`）。adapter 发 `AgentEvent::TurnStarted`，daemon 开一个 `turn_start { turn_id, message_id: "" }` 的 turn（空串 = 没有对应的用户消息），状态 `running`，照常以 adapter 的 `turn_done` 结束、计入用量；它不占并发名额（§ 7.2，进程已经在跑，daemon 拦不住）。期间到达的消息和普通 turn 一样入队，这一轮结束后作为下一个 turn 发出；可以 `session.cancel`。daemon 已经有 turn 时收到 `TurnStarted` 直接忽略；已归档的会话（归档不一定收进程，后台任务可能还在跑）也不开，这一轮的事件记在 turn 之外
- 竞态：daemon 发出 `send` 时 CLI 恰好自己开始了一轮（`TurnStarted` 还在通道里没被读到）。这时写进 stdin 的话，CLI 会把消息排到这一轮之后，先到的是这一轮的 `result`，会被当成用户 turn 的结束。所以 adapter 把消息**暂存**，这一轮的 `result` 之后再写进去，不为这一轮发 `turn_done`，两轮合成一个用户 turn（事件、用量都归它）。暂存期间取消的，消息不再写，以 `cancelled` 收尾
- 积压消息合并规则见 [protocol.md § 7.3](../protocol.md#73-daemon-侧突发消息合并)
- 带附件的积压合并时按顺序收消息，附件累计超过 20 个或 20 MiB 就停（第一条总是收），剩下的留在队列里，本 turn 结束后作为下一个 turn 发出，并立刻发 `status { queued_messages }`（§ 7.10）
- turn 进行中收到的 `send`：`capabilities.interject` 为 true 的 agent 直接送入（`UserInput.interject = true`）；否则入队，turn 结束后作为下一 turn 发出，事件 `status { state: running, queued_messages: n }` 让客户端显示「1 条待发送」
- **已知限制**（Codex 实测）：插话被 `turn/steer` 接受之后、turn 又被取消的，这条消息 agent 看不到，也不会出现在下一个 turn 里；daemon 无法区分它是已经被模型读到还是丢了，所以不重发（避免重复）。客户端上标出「可能没有送达」在 v0.1.x
- 关联项目的会话在插话之前、放行权限请求之前，以及每 2 秒（共享的周期任务，覆盖有 turn 在跑的会话和**没有 turn 但 agent 进程还活着**的会话）都复核一次项目目录（ADR-33 第 5 条）。目录变了：
  - 插话不送出，消息退回队列。队列在项目解析失败时**保留**（不清掉）：目录恢复之后再发消息，退回的和新的一起作为下一个 turn 发出；目录没恢复时再发，新消息也进队列、turn 以 `not_found` 失败，队列仍保留；`session.cancel` 清空队列；
  - `permission.respond` 放行时：这个请求被消耗，按拒绝答复 agent，发 `permission_resolved { by_device: "daemon", 拒绝选项 }`；发起答复的客户端得到 `not_found`，`data.reason = "project_dir_changed"`（不要重试，再答复是 `already_resolved`）；
  - 会话以 `error { code: not_found }` + `turn_done { reason: error }` 收尾，这个项目所有会话的进程收掉。
  - 窗口：周期检查约每 2 秒一次，发现之后还要收进程（关 stdin → 300 ms → SIGINT → 最多 5 秒宽限 → SIGKILL），所以 agent 不经审批执行的动作（包括 agent 自己记住的「本会话都允许」之后的工具调用）在目录被换掉后最多还能持续「一个检查间隔 + 收进程的时间」；复核通过之后、答复真正送到 agent 之前换目录同样拦不住。这是纵深防御，不是隔离
- 已经发出取消（turn 还没收尾）时到达的消息不插话，直接入队；有一条插话还没有结果时，后来的消息也只入队、不开新 turn（保证 agent 看到的顺序和用户发的一致）
- 插话失败**不是** turn 失败：adapter 返回 `TurnEnded`（turn 刚结束、`turn_done` 还没被 daemon 读到）或任何其它错误时，daemon 把这条消息放回队列，按正常流程作为下一个 turn 发出，不 retire 进程、不让当前 turn 失败。adapter 收到 `interject = true` 时绝不自己开新 turn

### 7.6 取消

`session.cancel`：没有 turn 在跑时，清空等待发送的消息（目录变了期间留在队列里的，§ 7.5），状态不变，不发事件；有 turn 时 → adapter 的中断（Claude Code：控制消息 `interrupt`，失败则 SIGINT；Codex：先把未答复的审批回 `cancel`，再 `turn/interrupt { threadId, turnId }`，3 秒没收尾补 SIGINT；ACP：`session/cancel`，agent 以 `stopReason: cancelled` 结束 prompt）。agent 收尾后发 `turn_done { reason: cancelled }`，状态 `idle`。取消不杀进程。

`capabilities.interject`（turn 进行中插话）：Codex 有 `turn/steer`，为 true；Claude Code 的 stream-json 在 turn 中收到的用户消息会排队到下一 turn，为 false；ACP v1 为 false。

### 7.7 标题

- 默认取第一条用户消息前 60 个字符（截到词边界）；只有图片没有文字的消息用 `[image] <第一个附件名>`（多张时加 ` (+N)`），会话列表的预览同理
- `session.rename` 改，事件 `title_changed`
- daemon 不调模型生成标题（§ 1）

### 7.8 归档、删除、重启

| 方法 | 行为 |
|---|---|
| `session.archive` / `unarchive` | flag，列表默认不显示 archived。**已归档的会话不能 `send`**（`invalid`，`data.reason = "archived"`，先 `unarchive`；ADR-34 修订）。有 worktree 的会话：`archive { worktree: auto \| keep \| remove }` 先收掉 agent 进程再按 § 10.1「归档、删除、取消归档」处理 worktree；`unarchive` 按分支重建被删掉的 worktree，重建失败则保持归档并返回错误 |
| `session.delete` | 删 SQLite 里的事件和附件、scratch 目录；agent 自己的会话文件（如 `~/.claude/projects/…`）**不动**，那是用户的。有 worktree 的会话：`delete { worktree, delete_branch }` 同上处理，被保留下来的 worktree 变成「没有会话的 worktree」 |
| `session.restart` | 杀进程、resume。用于 agent 卡死或改了 `thinking` 后想立即生效 |
| `session.rewind`（v0.3） | `{ message_id, scope: conversation \| files \| both }` 回退到某条用户消息之前。Claude Code 有文件检查点，Codex / ACP 看各自能力；`capabilities.rewind` 声明 |
| `session.fork`（v0.3） | 从当前历史分出新会话：Claude Code `--fork-session`、Codex `thread/fork`、ACP `session/fork`（尚不稳定） |
| `session.import`（v0.3） | 把 agent 原生会话（用户在终端里开的）导入为 anycon 会话：Codex `thread/list`、ACP `session/list`、Claude Code 读 `~/.claude/projects/<cwd>/` 下的会话 id（不解析内容，只拿 id 去 resume） |

### 7.9 历史分页

`session.history { session_id, after_seq?, limit?, before_seq? }` → `{ events, has_more }`。

- `limit` 默认 200、上限 1000。`after_seq` 拉增量（seq 大于它的事件，升序）；`before_seq` 向上翻页（seq 小于它的最近 `limit` 条）
- **按字节分页**：一页累计到编码后 **192 KiB** 就在这里截断并置 `has_more = true`，即使条数没到 `limit`。单个事件本身超过 192 KiB 时（正常情况下不会，工具输出有 § 8.2 的上限）它自己成一页，不拆分
- 客户端的做法：拿返回的最后一个 `seq`（向上翻页时是最小的 `seq`）继续拉，直到 `has_more = false`；不要假设一页一定拿满 `limit`
- 为什么：传输层会分片（[ADR-30](../decisions.md#adr-30-内层分片与大小上限)），所以不再有「超过 hub 上限就丢」的问题；按字节分页是为了让每页大小有界——一页 1000 条带大输出的事件能有几 MiB，手机上解析、渲染、解密都吃力
- `session.list` 没有字节分页：客户端应当传 `limit`（不传时返回全部，会话很多时响应会很大，靠传输层分片兜底）

### 7.10 附件（[ADR-35](../decisions.md#adr-35-图片附件经-blob-通道传到-daemon消息只带引用)）

方法和错误码见 [protocol.md § 5.6](../protocol.md#56-附件--blobadr-35)。daemon 这边：

- **存放**：`<数据目录>/blobs/<blob_id>.<ext>`（`ext` 按 mime：`png` `jpg` `gif` `webp`；Codex 按扩展名判断类型），上传中的写 `blobs/tmp/<blob_id>.part`；目录 0700、文件 0600。`blob_id` 是 32 位小写 hex，每个 RPC 入口都先按这个格式校验（它会进路径）。`put_end` 校验通过后 rename 成正式文件、写 `blobs` 表（`session_id` 为空 = 还没被引用；`path` 记的是 `blobs/` 下的文件名，不是绝对路径）。启动时清掉 `blobs/tmp/`，并删掉 `blobs/` 里没有记录的 `<blob_id>.<ext>` 文件（崩溃留下的）；记录在、文件不在的 blob 按不存在处理（`not_found` / `blob`）
- **上传状态**在内存里（daemon 重启会丢掉进行中的上传）：`blob_id` → 期望大小、mime、name、sha256、已收的块、发起设备、最后活动时间。同一设备同时进行中的上传 ≤ 8；10 分钟没有新块的上传作废（删 `.part`）。`put_end` 先在锁内把上传标成「收尾中」（之后的 `put_chunk` → `invalid` / `finalizing`），等已经在写的块写完（每个上传一把读写锁：块拿读锁、收尾拿写锁），再算整个文件的 SHA-256、读文件头核对 mime（只是前缀比对，不解码：PNG `89 50 4E 47 0D 0A 1A 0A`、JPEG `FF D8 FF`、GIF `GIF87a` / `GIF89a`、WebP `RIFF????WEBP`），并尽量读出宽高（读不出来就省略，不算错）；并发的第二个 `put_end` 等第一个的结果。名字的扩展名和 mime 对不上时补上正确的扩展名
- **发送**：`session.send` / `session.create` 的每个 `AttachmentRef` 按库里的记录核对（规则见 protocol.md § 5.6），事件 `user_message.attachments` 写 daemon 记录的元数据；消息被接受（写进事件流）时把 blob 绑定到会话（`UPDATE … WHERE session_id IS NULL OR session_id = 本会话`）。幂等重试：同一个 `message_id` 再来时附件的 `blob_id` 列表也要一致，否则 `invalid`
- **名字**：`put_begin` 拒绝控制字符（Cc）、双向控制符（U+200E U+200F U+202A–U+202E U+2066–U+2069）和 U+2028 / U+2029；`put_end` 记录的名字扩展名和 mime 对不上时补 `.<ext>`（`png` `jpg` `gif` `webp`；`photo` → `photo.png`，`x.html` → `x.html.jpg`，超过 255 字节先截断主名）。客户端要原样使用 `put_end` 返回的 `AttachmentRef`
- **交给 adapter**：`UserInput.attachments` 是这条（或合并后的几条）消息的附件，`attachment_paths` 是对应的正式文件路径，一一对应；积压合并（§ 7.5）时按消息顺序拼接；插话同样带附件
- **回收**：每小时的清理任务删掉 24 小时前完成、仍没被引用的 blob（文件和行）；`session.delete` 删会话的 blob。`blob.discard` 只删没被引用的
- **数量**：一条消息 ≤ 20 个附件、合计 ≤ 20 MiB；单个 ≤ `sessions.max_attachment_bytes`（默认 10 MiB）。积压合并（§ 7.5）时按顺序收消息，附件累计超过 20 个或 20 MiB 就停（第一条总是收），剩下的留在队列里，本 turn 结束后作为下一个 turn 发出。这些是 Claude 单次请求的上限（请求体 32 MB、一次超过 20 张图时每张 ≤ 2 000 px）倒推的，对所有 agent 一样
- **磁盘**：完成但没被引用的 blob 全节点合计 ≤ 1 GiB（`put_begin` → `busy` / `quota`）；数据目录所在卷剩余空间不够 `size + 1 GiB` → `busy` / `disk_full`（这两个 `busy` 不要自动重试：要发掉 / 移除附件或腾出磁盘）；配额里也算进行中的上传。`anycon doctor`（`node.doctor`）多一项 `blobs`：文件数、字节数、进行中的上传，剩余空间 < 1 GiB 时失败
- 审计：不记 blob 的上传 / 下载（量大、没有安全意义）；`session.send` 的审计照旧
- 同一个客户端的请求是串行处理的：客户端多块在途只是省掉往返，`put_end` 算 SHA-256 期间这个客户端的其它请求要等（10 MiB 约几十毫秒）

## 8. 事件翻译

adapter 把原生事件映射到 [protocol.md § 6](../protocol.md#6-会话事件) 的 `Event`。完整集合（含本次新增）：

```
turn_start        { turn_id, message_id }
text_delta        { text }
text_done
thinking_delta    { text }                   // text 可能为空串（模型只给「正在思考」不给内容）
thinking_done     { duration_ms }
tool_call         { id, name, category, title, input, detail?, parent_id? }   // detail 是按 category 归一化的渲染字段，见下；parent_id：子 agent 内部的调用指向启动它的调用
tool_output_delta { id, text }                // 命令输出流式（Codex、ACP terminal 有；Claude Code 没有）
tool_result       { id, output, is_error, detail? }
plan              { entries: [ { content, status: pending | in_progress | completed } ] }  // agent 的待办计划
file_changed      { path, kind: created | modified | deleted }   // 从 file_edit 类工具推导
permission        { request_id, kind, title, detail, options: [...] }
permission_resolved { request_id, option_id?, by_device, answers?, message? }   // answers / message：设备答复时带的原样记下（历史里显示问题和回答）；daemon 自己答复的没有
turn_done         { turn_id, reason: completed | cancelled | error | max_turns | budget,
                    usage?: { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                              cost_usd?, duration_ms, model } }
status            { state, position?, queued_messages? }
settings_changed  { settings }
title_changed     { title }
error             { code, message, recoverable }
```

`detail` 让客户端不用认识每个 agent 的工具参数名就能画出像样的块：

| category | `tool_call.detail` | `tool_result.detail` |
|---|---|---|
| `shell` | `{ command, cwd }` | `{ exit_code }` |
| `file_edit` | `{ path, kind: edit \| write \| delete \| move }` | `{ unified_diff }` |
| `file_read` | `{ path, range? }` | — |
| `search` | `{ query, scope? }` | `{ match_count? }` |
| `web` | `{ url }` | — |
| `subagent` | `{ description, child_session_id? }` | — |
| `mcp` | `{ server, tool }` | — |

`input` 和 `output` 仍然原样保留（受 § 8.2 的大小上限约束），`detail` 是补充不是替代。任何 category 的 `tool_result.detail` 另外可以带两个截断标记：`output_truncated_bytes`（`output` 被头尾截断时丢掉的字节数）和 `live_output_truncated`（流式输出超过累计上限后停止转发）。`tool_result.detail.background = true` 表示这次调用的任务被放到后台、还没结束（`subagent` 的后台子 agent，`shell` 的后台命令），结束时同一个 `id` 再来一条 `tool_result`（[protocol.md § 6](../protocol.md#6-会话事件)）。子 agent 结束的那条可以带 `detail.total_tokens`（agent 报的它一共用掉的 token）。

`tool_call.category` 的归一化：

| category | Claude Code 工具 | Codex item `type` | ACP `kind` |
|---|---|---|---|
| `shell` | Bash | commandExecution | execute |
| `file_edit` | Edit、Write、MultiEdit、NotebookEdit | fileChange（`changes[].kind: add / update / delete` → `file_changed`） | edit / delete / move |
| `file_read` | Read、Glob、LS | — | read |
| `search` | Grep、WebSearch | webSearch | search |
| `web` | WebFetch | — | fetch |
| `subagent` | Task / Agent | collabAgentToolCall | — |
| `mcp` | `mcp__*` | mcpToolCall | — |
| `other` | 其余 | dynamicToolCall、其余 | think / switch_mode / other |

### 8.2 事件大小上限

daemon 在**写库和发送之前**限制单个事件的体积，存储和发送是同一份（不会出现「库里是全量、传出去是截断」）：

| 字段 | 上限 | 超过时 |
|---|---|---|
| `tool_result.output` | 64 KiB | 保留前 48 KiB 和后 16 KiB，中间放 `\n…[truncated N bytes]\n`（N 是被丢掉的字节数；切在 UTF-8 字符边界上；已经截过的再截是空操作），并在 `detail.output_truncated_bytes` 里给出 N。例如 Claude Code 读大文件、命令输出很长时 |
| `tool_call.input` | 64 KiB | 字符串：同样的头尾截断。对象 / 数组（Write 工具写一个大文件时是 `{ file_path, content }`，对象形的 `tool_result.output` 同理）**保留结构**，值的类型不变：只把里面超长的字符串叶子依次缩到 8 KiB / 2 KiB / 512 B / 128 B 直到整体不超限，条目成千上万的容器只留前 256 / 64 / 16 项并补一条 `…[truncated N items]`；这样还装不下的极端情况才退成截断后的 JSON 文本 |
| `tool_output_delta`（一次工具调用累计） | 64 KiB | 超过之后不再转发 / 存储后续的 delta，并补一行提示 `…[live output truncated; the final result keeps the head and tail]`；最终的 `tool_result` 里仍是头尾截断的完整输出。并在最终 `tool_result.detail.live_output_truncated = true` 里标记 |
| `session.send` / `session.create` 的 `prompt` | 1 MiB | 超过返回 `invalid` |

旧数据库里已存的超大事件在读出时同样会被截。其余可能很大的字段（`unified_diff`、`text_delta` 合并后的长文本）不截断，由传输层分片兜底；一页历史的总字节数另受 § 7.9 约束。

三个 adapter 的翻译要点：

- **Claude Code**：`stream_event` 里 `content_block_delta.thinking_delta` → `thinking_delta`、`text_delta` → `text_delta`；`assistant` 消息里的 `tool_use` 块 → `tool_call`（消息的 `parent_tool_use_id` → `parent_id`；子 agent 的文本 / 思考不进主流）；转到后台的调用（子 agent 的 `tool_use_result.status = async_launched`、Bash 的 `tool_use_result.backgroundTaskId`，或者 `system/task_started { is_backgrounded: true }` 报过的调用）结果带 `detail.background`，`system/task_notification`（`completed` / `failed` / `stopped` / `killed`）给同一个调用补最终结果，`output` 是通知里的 `summary`，后台命令从 `summary` 末尾的 `(exit code N)` 取 `detail.exit_code`；`failed` 和退出码非 0 算 `is_error`，被停掉的不算；子 agent 用掉的 token 进 `detail.total_tokens`（前台取 `tool_use_result.totalTokens`，后台取通知里的 `usage.total_tokens`）；`user` 消息里的 `tool_result` → `tool_result`；TodoWrite 的输入 → `plan`；`result` → `turn_done`（`total_cost_usd`、`usage`、`duration_ms`、`num_turns` 进 `usage`）；`system/init` 的 `session_id` 存为 `agent_session_id`
- **Codex**：`item/agentMessage/delta` → `text_delta`；`item/reasoning/summaryTextDelta` 和 `textDelta` → `thinking_delta`；`item/started` → `tool_call`，`item/commandExecution/outputDelta` → `tool_output_delta`，`item/completed` → `tool_result`（`status: declined` 视为 `is_error`）；`turn/plan/updated` → `plan`；`turn/completed` → `turn_done`（`status: interrupted` → `cancelled`），usage 来自 `thread/tokenUsage/updated`：一个 turn 的用量 = 每次更新的 `last` 之和（`total` 是线程累计），`input_tokens` 扣除缓存命中（和 Claude Code 口径一致），没有费用；只认当前 turn 的更新（resume 后 app-server 会重放上一个 turn 的用量）；`thread/started.thread.id` 存为 `agent_session_id`。被打断 / 失败的 turn 不会为进行中的 item 发 `item/completed`，adapter 在 `turn/completed` 时补 `text_done` / `thinking_done` 和 `is_error` 的 `tool_result`。工具调用的 `name` 是 item 类型（`commandExecution`、`fileChange`、`mcp__<server>__<tool>`…），客户端按 `category` / `detail` 渲染，不按名字。Codex 特有的 `error.code`：`agent_reconnecting`（`willRetry`，可恢复，每 turn 最多 5 条，turn 不收尾）、`agent_context_reset`（可恢复的提示：续接时原 thread 已经不在——`no rollout found` / `thread not found`——adapter 开了新 thread，之前的上下文 agent 看不到了）、`unsupported_request`、`context_window_exceeded`、`sandbox_error`、`policy_violation`，其余 `agent_error`。`initialize` 用 `optOutNotificationMethods` 关掉 adapter 用不到的通知（`turn/diff/updated` 等，它们在大补丁时单行可以超过 10 MB）。单行超过 8 MiB：通知直接丢弃（只记方法名和长度），对应的 item 补一个「内容太大，没有显示」的 `tool_call` / `tool_result`；只有超长的是服务端请求、某个请求的响应或无法识别时才算协议错误
- **ACP**：`agent_message_chunk` → `text_delta`；`agent_thought_chunk` → `thinking_delta`；`tool_call` / `tool_call_update` → `tool_call` / `tool_result`，`content[].type = diff` → `file_changed`，`type = terminal` 的输出 → `tool_output_delta`；`plan` → `plan`；`session_info_update.title` → `title_changed`（agent 自己起的标题优先于 § 7.7 的默认）；`usage_update` 累积到 `turn_done.usage`；`session/prompt` 的 `stopReason` → `turn_done.reason`（`end_turn` → completed、`cancelled`、`max_tokens / max_turn_requests` → max_turns、`refusal` → error）

## 9. 权限请求

### 9.1 请求

```
permission { request_id, kind, title, detail, options }
  kind:    "tool" | "plan" | "question" | "mode_change" | "other"
  title:   "运行命令" / "修改 src/a.ts" / "计划已就绪" / "agent 有个问题"
  detail:  kind = tool     → { tool, category, input, cwd, suggestions? }   // 和 tool_call.detail 同结构
           kind = plan     → { text }                                       // 计划全文（Markdown）
           kind = question → { questions: [ { id, text, choices?: [ { id, label } ], multi?: bool, free_text?: bool } ] }
  options: [ { id, label, kind: allow_once | allow_always | reject_once | reject_always, needs_message?: bool } ]
```

四种 kind 的来源和答复语义：

| kind | 来源 | 答复 |
|---|---|---|
| `tool` | Claude Code `can_use_tool`、Codex `item/*/requestApproval`、ACP `session/request_permission` | 选一个 `option_id`，`reject_*` 可带 `message` |
| `plan` | Claude Code 的 `ExitPlanMode` 工具、Codex plan 模式结束 | options 由 adapter 给，v0.1 是两项：「开始实施」（`allow`）、「继续规划」（`deny`，reject，可带修改意见）；「开始实施，回到之前的模式」这个额外选项 v0.1 没有。允许后 daemon 负责切 mode：优先 adapter 在 `detail.mode` 里指明的，其次进入 plan 之前的模式（不是 dangerous 才回去），再其次 `default_mode`，最后 AgentInfo 的默认；切换后发 `settings_changed`、调 `AgentHandle::set_settings`、记审计（`session.set_settings`，`reason: permission_approved`） |
| `question` | Claude Code 的 `AskUserQuestion`、Codex `item/tool/requestUserInput`、ACP `elicitation/create` | `permission.respond { request_id, answers: { [question_id]: value } }`，没有 `option_id` |
| `mode_change` | agent 自己请求切换模式（ACP `switch_mode` 类工具） | allow / reject |

`question` 和 `plan` 走权限通道而不是另开一套，是为了复用待办列表、多客户端解决、通知这整条链；通知类别仍是 `permission_requested`（语义都是「agent 在等你」）。

`options` 由 adapter 给，每项带 `kind: allow_once | allow_always | reject_once | reject_always`（借用 ACP 的枚举，客户端据此选图标和颜色）：

| | 请求来源 | `allow` | `allow_session` | `deny` |
|---|---|---|---|---|
| Claude Code | `control_request { subtype: can_use_tool, tool_name, input, permission_suggestions }` | `{ behavior: allow, updatedInput }` | 同左 + `updatedPermissions: [{ type: addRules, rules: [{ toolName }], behavior: allow, destination: session }]` | `{ behavior: deny, message }` |
| Codex | `item/commandExecution/requestApproval`、`item/fileChange/requestApproval`、`item/permissions/requestApproval`（`reason` 放进 `detail.tool.detail.description`，`input.reason` 也保留；`fileChange` 的补丁内容取自先前的 `item/started.changes`。显示规则：cwd 之外的路径保持绝对路径；多文件补丁标题写 `Edit N files (a, b, …)`，`description` 依次是 reason、全部路径、`grantRoot` 说明（「并允许本会话写入 <root>」，也写进标题）；拿不到 `item/started` 时标题是 `Edit files (patch contents unknown)` 且 `input.contentUnknown = true`，客户端用警示样式。请求里带非空 `command` 的（zsh 子命令审批）以它为准，标题 `Run sub-command`） | `{ decision: accept }` | `{ decision: acceptForSession }` | `{ decision: decline }`；会话取消时 `cancel`（拒绝并中断 turn）。v0.1 不支持的服务端请求（MCP elicitation、动态工具、ChatGPT token 刷新、v1 旧审批）按 schema 回拒绝，并发 `error { code: unsupported_request }` |
| ACP | `session/request_permission { toolCall, options[] }` | 透传 agent 给的 options，`optionId` 原样回 | 同左 | 同左；会话取消时 `{ outcome: cancelled }` |

ACP 的 options 是 agent 定义的，可能只有两项或有四项，客户端按 `options` 渲染而不是写死三个按钮。

### 9.2 答复

`permission.respond { request_id, option_id?, message?, answers? }`。规则：

- 任一已配对客户端答复即生效；daemon 向所有在线客户端发 `permission_resolved { by_device }`（不只 watcher，列表页的角标也要清）
- 重复答复返回 `already_resolved`；两个客户端同一毫秒答复，先到的赢
- `permission.list { session_id? }` 返回所有未决请求，客户端重连后先拉一次再渲染，不依赖事件重放
- `permissions.request_timeout` 到期自动 deny，message = "timeout"
- 会话被取消时，未决请求自动 deny
- 本机 CLI `anycon attach` 也是一个答复来源

### 9.3 Claude Code 的接法

v0.1 用 stream-json 的 `control_request { subtype: can_use_tool }` / `control_response`——这是官方 Agent SDK 和 CLI 之间的通道，不是公开文档化的契约。接受这个风险的理由：SDK 本身依赖它，不会无迁移路径地 break；adapter 锁定支持的 CLI 版本区间，用 `system/init` 的 `capabilities` 做特性检测，升级时跑回放测试。

备用方案（文档化的）：`--permission-prompts none` + `PreToolUse` http hook 指向 daemon 的本机端口。缺点是要写 hook 配置到用户的 settings 文件。如果 v0.1 实现时 `control_request` 不稳，切到这条，记一条 ADR。

不用 Agent SDK 做 sidecar：那要用户装 Node，违反「用户只装一个东西」（[ADR-17](../decisions.md#adr-17-daemon-与-cli-合一)）。

## 10. Workspace

细化 [architecture.md § 6](../architecture.md#6-workspace-模型)。

- **项目名校验**：不含 `/` 和 NUL、不以 `.` 开头、≤ 255 字节、不是 `.trash` / `.scratch`；重名返回 `exists`。根目录下的项目和关联项目**共用一个命名空间**：新建、克隆、改名、恢复、scratch 转项目、关联遇到任何一边已占用的名字都返回 `exists`
- **路径安全**：所有 `fs.*` 和项目操作先 `canonicalize`，结果必须在所属项目的目录下（根目录下的项目：根目录；关联项目：关联目录本身）。边界按**路径**判断：项目里指向项目外文件的**硬链接**照常可读（它就是项目里的一个文件名），硬链接不是边界；根目录里指向根外的符号链接仍然拒绝
- **能关联哪些目录**（ADR-33 修订）：范围、隐藏目录、内置拒绝表、绝对拒绝、使用时复核这五条规则以 [ADR-33](../decisions.md#adr-33-关联本机已有目录为项目) 为准。拒绝时返回 `forbidden`（在 workspace 根里是 `invalid`），消息说明原因；`workspace.browse` 的条目用 `linkable = false` + `blocked`（`hidden` / `sensitive` / `workspace` / `data_dir` / `filesystem_root`）标出来，已经是项目的看 `project`。同时命中几条时只报一个原因，优先级 `filesystem_root` → `workspace` → `data_dir` → `sensitive` → `hidden`（所以 `~/.ssh` 报 `sensitive`）。拒绝表里的目录和数据目录（以及它们里面）不能 browse 进去（`forbidden`）；包含它们的上级目录（如 `~`）和普通隐藏目录可以进。**不泄露存在性**：`browse` / `link` 先把请求的路径做词法规范化（展开 `~`、去掉 `.` / `..`、套系统别名），**不在任何 root 之内就一律回「不在 `link_roots` 之内」**，不管它解析到哪里、存不存在（经范围外的链接或 `/Volumes/<启动盘>` 拼进来的路径本来也不接受）；再按词法路径判断是否落在拒绝表 / 数据目录里（不区分大小写的卷上按 NFC + 大小写折叠比较），命中就 `forbidden`，不管那个路径存不存在、是不是目录；在拒绝表之外，`browse` 和 `link` 对「不存在」和「不是目录」也给同一个错误（`not_found`）和消息。**已知缺口**：① 范围之内的符号链接（`~/work/lnk -> ~/.ssh`，或指向范围外的链接）后面的路径仍能区分存在与否；② 大小写折叠用的是 Unicode 小写，不等于 APFS 的折叠规则（`ß` ↔ `ss`、`ſ` ↔ `s` 这类写法不在其中），这类变体也能区分拒绝表目录里子目录的存在与否。两种都只泄露存在性，读不到内容。`sensitive` 用于「包含拒绝项」时，消息点名一个确实存在的拒绝项。**root 不能经过符号链接**（ADR-33 第 1 条），每次请求都重判。判定顺序：规范化（固定的 canonical HOME、系统别名、firmlink）→ 逐段 `lstat`（任何一段是链接就忽略这一项）→ canonical 路径和写法只差大小写 / NFC 时以 canonical 路径生效。也就是说判断「经过符号链接」是逐段 `lstat`，大小写或 NFC / NFD 写法和盘上不一致（`~/Work` vs `~/work`）**不算**，这样的项以 canonical 路径生效；没有可用的 root 时，错误消息列出每一项的简短原因（不说链接指向哪里），完整原因在 daemon 日志和 `anycon config set workspace.link_roots …` 的输出里（`--json` 是 `link_roots_unusable`）。想关联的目录路径里有符号链接（`~/code -> ~/real-code`）时，`link_roots` 要写真实路径；启动、`config set` / 热加载时对被忽略的项 warn 一次（只对确实被忽略的项 warn，`/var/folders/…` 这类经系统别名的正常写法不 warn）。拒绝表本身同样按 `/var` / `/tmp` / `/etc` 这组别名匹配。firmlink（`/System/Volumes/Data/X` ↔ `/X`）的归一化**只作用于按原样大小写写出的词法路径**：canonical 结果、启动时固定的 HOME、拒绝表都不按它归一化，所以运维在 `link_roots` 里用 firmlink 写法的大小写变体、或 HOME 本身是数据卷写法时，拒绝表里 HOME 下的项可能匹配不上（已知限制；workspace 根和数据目录按 `(dev, ino)` 判，不受影响）
- **关联已有目录**（ADR-33）：`workspace.link { path, name? }`。`path` 可以是绝对路径或 `~` / `~/…`（按 daemon **启动时固定的 canonical HOME** 展开），NUL / 控制字符、超过 4096 字节返回 `invalid`。判断顺序：① 词法规范化之后不在任何可用的 root 之内 → `forbidden`（不管存不存在；没有可用的 root、列表为空也是 `forbidden`）；② 按词法路径落在拒绝表 / 数据目录里 → `forbidden`；③ canonicalize 后不存在或不是目录 → `not_found`（两种同一条消息）；④ 按 canonical 路径重跑 ADR-33 第 2–4 条。以下也拒绝：在 workspace 根目录里面（`invalid`，它已经是项目或在 `.trash` / `.scratch` 里）；是文件系统根、包含 workspace 根目录或 anycon 数据目录（`ANYCON_HOME`）、在 anycon 数据目录里面（`forbidden`）；同一个 canonical 路径已经关联过（`exists`，消息里带已有的名字）。名字缺省取目录名（不合法时让客户端给）。存进 SQLite `linked_projects (name PRIMARY KEY, path UNIQUE, created_ms)`；发 `workspace.changed { added }`；审计 `workspace.link`（记路径）
- **解除关联**：`workspace.unlink { name }` 删掉那一行，发 `workspace.changed { removed }`，审计；目录和里面的文件不动。不是关联项目 → `invalid`；有运行中（queued / starting / running / waiting_permission）的会话 → `busy`。检查之后才开始的 turn 会被取消、这个项目所有会话的 agent 进程都被收掉（循环直到收干净）。**锁顺序**：`ns_lock`（workspace 命名空间）在前，会话锁在后；任何代码都不能持有会话锁去取 `ns_lock`（`session.promote` 先在会话锁里做检查、释放后再改名、再加锁写回并复核）。已有会话保留：每个会话记着自己的位置（`linked:<canonical 目录>` 或 `folder:<名字>#<dev>:<ino>`，根目录项目不记绝对路径，记目录的设备号和 inode；同一个设备上 inode 不同 = 换了一个目录（比如进了回收站再新建同名文件夹、从备份恢复、删掉后重新 clone），旧会话不能继续，错误消息先说明「如果是有意替换的，新建一个会话；旧会话的历史仍然可以查看」；设备号不同 = workspace 根整体搬到了别的盘，接受并重新记下；旧版本只记了名字的同样接受并补记），之后这个名字指向别的目录（同名重新关联到别处、被盖住的同名根目录文件夹露出来）时，拉起 agent 前发现 cwd 变了就拒绝（`not_found`，提示原来的目录），不会在新目录里续接；同一个目录用任何名字重新关联回来，会话就能继续（旧会话的 `project` 改成新名字，审计 `workspace.link` 的 `detail.adopted_sessions` 记下它们）
- **关联项目的其余操作**：`workspace.rename` 只改表里的名字（同时更新会话的 `project`）；`workspace.trash` 返回 `invalid`（提示 unlink）——anycon 永远不移动、不删除关联目录里的文件；`workspace.list` 现算元数据，目录不在了（不是目录 / 无法访问）时 `missing: true`、`git` 为空、`modified_at_ms = 0`；`missing`（不在了、被换成指向别处的符号链接、或按当前规则已经不允许）的项目上 `session.create` 返回 `not_found`
- **同名冲突**：根目录里事后被手工建出和关联项目同名的文件夹时，关联项目优先，那个文件夹不出现在列表里，`workspace.list` 时 `warn` 一次（每个名字每次进程只记一次）。项目名统一按 NFC 规范化后比较；在不区分大小写的文件系统上（APFS 默认）按不区分大小写比较，`proj` 和 `PROJ` 算重名。对外（`workspace.list`、会话的 `project`、`session.list { project }`）一律用**盘上那一项的原样名字**，所以 Finder 建的 NFD 名字和会话里存的是同一个字符串；`session.list` / `session.search` 的 `project` 过滤按同样的规则比较
- **浏览**：`workspace.browse { path?, show_hidden? }` 只列子目录（跟随符号链接；解析后落在 `link_roots` 之外的、无法读的不列），按名字排序，最多 1000 项（`truncated`）；每项给 `git`（有 `.git`）、`project`（已经是项目时的名字）、`linkable`（按上面的规则现在能不能关联）。`path` 缺省 = 第一个 root；越界或在拒绝表里 `forbidden`；不是目录 `invalid`；macOS 拒绝访问（隐私授权）时 `forbidden`，消息说明去「隐私与安全性」授权。`parent` 在 `path` 是某个 root 时为空。不读文件内容，不进审计。`git` 标记只在不会触发隐私授权的地方探测：列 `~` 本身时不探测 `~/Desktop`、`~/Documents`、`~/Downloads`、`~/Pictures`、`~/Movies`、`~/Music`、`~/Public`（以及 `/Volumes` 下的卷）里有没有 `.git`——用户自己进到这些目录时才会碰到授权
- **克隆**：`git clone --progress`，解析 stderr 的百分比发 `job.progress`；凭证走机器上的 ssh agent / credential helper，daemon 不碰；`job.cancel` 杀 git 进程并删半成品目录；URL 只允许 `https://`、`ssh://`、`git@host:` 三种形式
- **回收站**：`.trash/<name>-<timestamp>/`，`workspace.trash_list` / `restore` / `purge`；超过 `trash_retention_days` 自动删
- **scratch**：`.scratch/<session-id>/`；会话删除即删目录；`ended` 超过 `scratch_retention_days` 的自动清理（每小时扫一次）。「清理前 7 天在会话里发提示」v0.1 没做（`status` 事件没有承载文字的字段），留到 v0.1.x
- **元数据**：git 分支、是否 dirty、remote、`worktree_ready`、最近修改时间，每次 `workspace.list` 现算（`git` 子进程），5 秒缓存。有缓存时前台最多等 50 ms，超了先返回旧值、后台刷新；**没有缓存（冷启动）时等算完，每个项目最多 3 秒**（各项目并行），超时的先不带 `git`、带 `git_pending: true` 返回（在后台还没算完之前，之后的 `workspace.list` 对它同样给 `git_pending`），后台算完、写进缓存之后发 `workspace.changed { updated: [名字] }`（200 ms 防抖合并；`updated` 只是「重新 `workspace.list`」的信号，计算期间项目被改名 / 进回收站 / 解除关联时名字可能已经不存在；同一目录的并发请求共用一次计算；有缓存时的过期刷新不通知），之后的 `workspace.list` 一定带 `git`。`git` 一律带 `-c core.fsmonitor=false -c core.hooksPath=/dev/null`，不执行仓库自己配置的 fsmonitor / hook（clean filter 无法通配关闭，是已知限制）
- **fs watch**：只 watch 根目录一层，防抖 500 ms，`workspace.changed { added, removed, renamed }`

### 10.1 每会话 worktree

[ADR-34](../decisions.md#adr-34-web-ui-是开发者客户端体验对齐-claude-桌面版每个会话可以有自己的-worktree)。daemon 自己用 `git worktree` 建，agent 只看到一个 cwd。

**能不能开**：`GitMeta.worktree_ready` = 项目目录（canonical）等于 `git rev-parse --show-toplevel`（canonical），并且 `git rev-parse --verify HEAD` 成功（至少一个提交）。随 `workspace.list` 现算、同一份 5 秒缓存。不满足时 `session.create { workspace: worktree }` 返回 `invalid`（`data.reason = "not_worktree_ready"`）。scratch 会话（没有 `project`）带 `worktree` 也是 `invalid`。

**创建**（`session.create`）：

1. 同步校验（在返回之前，都很快）：
   - `base` 缺省 = `workspace.branches` 的 `default_base`（当前分支的上游 `@{upstream}`，没有就当前分支，detached 就 HEAD 的提交）。`git rev-parse --verify --end-of-options <base>^{commit}` 解析成提交，失败 → `invalid`（`bad_base`）；`base` 不能以 `-` 开头、不含控制字符、≤ 255 字节。**规范化**：依次试 `refs/heads/<base>`、`refs/remotes/<base>`、`refs/tags/<base>`，存在的记完整引用名（注意 git 自己解析同名写法时标签优先于分支；分支和标签同名并存时 `base_sha` 来自 git 的解析、记下的却是分支，罕见，已知限制），标签写法记 `refs/tags/…`；其他写法（`HEAD`、`HEAD~1`、`@{u}`、SHA）只记解析出的完整提交 SHA。展示名（`SessionWorktree.base` / `WorktreeInfo.base`）另存：引用写法是短名字（`main`、`origin/main`、`v1.0`），其他写法是提交的前 12 位。旧版本建的记录（记的是短名字）中**有会话的**启动时规范成新记法（`HEAD` 类写法改成 SHA），没有会话的（删会话时保留下来的 worktree）保持原样，`workspace.worktrees` 照原样展示；读的时候 `HEAD` 类写法改用创建时的提交。之后算 behind / merge-base / 同步都用记下的引用名或 SHA，不在 worktree 里按原来的写法重新解析
   - `branch` 给了：`git check-ref-format --branch <branch>` 通过，且 `refs/heads/<branch>` 不存在（存在 → `exists`）；不能等于 `main` / `master` / `HEAD`。没给：`<branch_prefix>/<slug>`（前缀为空时就是 `<slug>`）。slug：取第一条消息里的前 4 个 `[A-Za-z0-9]+` 词（单字母的也算），转小写，用 `-` 连，截到 32 个字符；这 4 个词里没有长度 ≥ 2 的（比如中文消息、没有 prompt）就用内置词表随机 `<形容词>-<名词>`。和已有分支、`.anycon/worktrees/` 下已有目录、或 `worktrees` 表里任何记录（**包括 `failed` 的**）的分支名 / 路径冲突时追加 `-2`、`-3`…（失败的记录继续占着名字）。显式给的分支名被 `failed` 的记录占着时返回 `exists`，消息说明「这个名字被创建失败的会话 <id> 占着：给它再发一次消息会用这个名字重试，或者删掉它」
   - 目录名 = 分支名最后一段，只保留 `[a-z0-9._-]`（其余换成 `-`），同样去重
   - 会话行和 `worktrees` 行一起写库，`state = creating`，`SessionSummary.worktree` 随结果返回；会话 `starting`。没有 `prompt` 的 `session.create` 要等第一次 `send` 拉起 agent 时才建 worktree，在那之前 `state` 一直是 `creating`，`git_status` / `diff` 返回 `busy`（`worktree_creating`）
   - 之后每次 worktree 状态变化都立即发带完整 `SessionWorktree` 的 `session.changed`
2. 异步（同一个会话的拉起流程里，在 `spawn_driver` 之前；agent 的 60 秒启动超时从这之后才开始算）：
   - `<项目>/.anycon` 和 `<项目>/.anycon/worktrees` 不存在就建；存在时逐段 `lstat` 必须是真目录（不是符号链接、不是文件），否则失败
   - `<git common dir>/info/exclude`（`git rev-parse --git-common-dir`）里没有 `/.anycon/` 就追加一行（文件不存在就建）
   - `git worktree add --no-track -b <branch> <path> <base_sha>`，超时 300 秒
   - 复制 `.worktreeinclude`：项目目录根下有这个文件时，`git ls-files -z --others --ignored --exclude-from=<项目>/.worktreeinclude`（在项目目录跑）列出匹配的未跟踪文件，逐个复制到 worktree 的同一相对路径：只复制普通文件（`lstat`，符号链接和其他类型跳过），目标路径的每一段都不能是符号链接，保留可执行位；累计超过 `include_max_files` / `include_max_bytes` 就停下，`git` 事件的 `note` 说明没复制完
   - 记下 worktree 目录的 `(dev, ino)`，`state = active`，发 `git { action: worktree_created, branch, base, count: 复制的文件数 }`，之后照常拉起 agent（cwd = worktree 目录）
   - 任何一步失败：`git worktree remove --force <path>`（目录已经建了的话）、`git branch -D <branch>`（分支已经建了的话）、`git worktree prune`，`state = failed`，发 `git { action: worktree_failed, note: 原因 }` 和 `error { code: "worktree_failed", recoverable: false }`，会话进入 `error`。带 prompt 的那条消息留在历史里
3. 审计 `worktree.create`（`detail = { branch, base, path }`）

**cwd 与复核**：会话的 cwd = 项目目录 + `SessionWorktree.path`（相对路径，所以项目改名、关联到新名字都不影响）。ADR-33 第 5 条列出的每个时机（`workspace.list` 除外：拉起 / 续接、每个 turn 开始前、放行权限请求前、插话前、周期检查——周期检查对所有有 turn 或 agent 进程的 worktree 会话做，不只关联项目，也不管是否归档），在原有的项目复核之后再复核 worktree（`active` 和 `kept` 都复核；拉起时 `kept` 复核通过就置回 `active`；周期检查覆盖所有没归档的 worktree 会话——没有进程也查——以及已归档但还有 turn / 进程的）：从项目目录起逐段 `lstat`（`.anycon`、`worktrees`、目录名都不能是符号链接），`(dev, ino)` 等于记下的；daemon 在 worktree 里跑 git 或动手的每个地方（拉起、每个 turn 开始前、状态 / 改动 / 同步、处置）再核对 worktree 根下的 `.git` 文件内容仍是创建时的 `gitdir: …`（agent 能改它，改了按「被换掉」处理）。比较按语义而不是字符串：解析 `gitdir:` 的值，相对路径按 worktree 目录解析后做词法规范化，要求等于 `<git common dir>/worktrees/<创建时记下的管理目录名>`——绝对 / 相对两种写法（git 的 `worktree.useRelativePaths`）都接受；改到别的目录的常见写法拦得住，但比较是词法的（见「限制」）。没有记下指针的旧记录不核对。只是指针不一致（目录身份没变）时的 `missing` 提示「在项目目录里运行 `git worktree repair`」，不是「把路径移走」。不满足：

- 目录**不存在**：`state = missing`，发 `git { action: worktree_missing }`，turn 按 ADR-33 第 5 条的方式中止（`not_found`，`data.reason = "worktree_missing"`）。**下一次 `send`** 时如果路径仍然不存在，就 `git worktree prune` 后按分支重建（`git worktree add <path> <branch>`；分支也不在了就 `-b <branch> <path> <base_sha>`），并像创建时一样复制 `.worktreeinclude`，成功发 `worktree_restored` 再继续这个 turn。创建中途崩溃留下的残局（git 登记过、目录不完整）也在下一次 `send` 时清掉再重建
- 路径存在但不是原来那个目录（被换成别的目录、符号链接）：`state = missing`，同样中止；**不自动重建**，`send` 一直返回 `not_found`（`worktree_missing`），消息说明「把那个路径移走后再发消息，或者归档 / 删除这个会话」

**状态**（`session.git_status`，在 cwd 里跑，超时 15 秒）：`git status --porcelain=v2 --branch -z` 得到分支、HEAD、上游、ahead / behind（相对上游，用于 `unpushed`）、未提交 / 冲突文件数；worktree 会话再算 `git rev-list --left-right --count <base>...HEAD` 得到相对 base 的 `behind` / `ahead`（`base` 已经解析不了就用 `base_sha`）；`MERGE_HEAD` 存在 = `merging`。`include_pr`：`gh pr view --json number,url,title,state,isDraft,baseRefName,statusCheckRollup -- <branch>`（worktree 会话用记录的分支名，不用当前 HEAD；分支名以 `-` 开头直接 `pr_error = "error"`；cwd 里跑，超时 20 秒，用 daemon 环境）；`gh` 不在 PATH → `pr_error = "gh_not_installed"`，没登录 → `gh_not_authenticated`，没有远端 → `no_remote`，没有 PR → `no_pr`，其他失败 → `error`。非 git 的会话 → `invalid`（`not_git`）。

**改动**（`session.diff`，超时 15 秒）：

- 起点：`against = base` → `git merge-base <base> HEAD`（`base` 解析不了用 `base_sha`），只有 worktree 会话（否则 `invalid`）；`uncommitted` → `HEAD`
- 列表：`git diff --no-ext-diff --no-textconv --no-color --find-renames --numstat -z <from>`（工作区对起点，含已暂存和未暂存）+ `--name-status` 得到状态；未跟踪的文件来自 `git ls-files -z --others --exclude-standard`，记为 `added` + `untracked`。先合并、按路径排序、截到 1000 个，**再**给未跟踪的文本文件数行数（> 1 MiB 或二进制不数，`binary`）；数行数有预算（整个请求 2 秒或累计读 64 MiB），超了的不再读、`additions = 0`；预算只作用在截到 1000 个之后留下的未跟踪文件上
- 单个文件（带 `path`）：`path` 必须出现在 numstat / `ls-files` 的名字里（否则 `not_found`；不截到 1000，不为此读其他文件）；`git diff --no-ext-diff --no-textconv --no-color --find-renames <from> -- :(literal)<path>`（改名时带上旧路径）；未跟踪的文件由 daemon 读文件（按 ADR-33 的 `openat` 方式，限在 cwd 里）拼成 `new file` 的 unified diff。超过 256 KiB 截断，`patch_truncated = true`
- 所有 daemon 跑的 git 都带 `GIT_SAFE_CONFIG`（`core.fsmonitor=false`、`core.hooksPath=/dev/null`），`GIT_TERMINAL_PROMPT=0`、`GIT_EDITOR=:`、`GIT_PAGER=cat`

**同步 base**（`session.sync_base`）：

- 前提：worktree 会话、`state = active`、没有归档（`invalid`，`archived`）、没有 turn 在跑（queued / starting / running / waiting_permission → `busy`）。期间会话标记为忙：`send` 返回 `busy`，结束后清除
- `abort`：`MERGE_HEAD` 不存在 → `invalid`；否则 `git merge --abort`，发 `git { action: sync_aborted }`，返回 `aborted`
- 否则：`MERGE_HEAD` 存在 → `invalid`（`merge_in_progress`）；有未提交改动（含未跟踪）→ `invalid`（`uncommitted_changes`）
- `base` 是远端跟踪分支（`git rev-parse --symbolic-full-name <base>` 是 `refs/remotes/<remote>/<branch>`）时先 `git -c protocol.ext.allow=never fetch <remote> <branch>`（超时 120 秒，失败 → `internal`，`fetch_failed`）；本地分支不 fetch（合并本地 base 现在的样子）
- `behind = 0` → `up_to_date`；否则 `git merge --no-gpg-sign -m "Merge <展示名> into <分支>" <规范化的 base>`（超时 60 秒；daemon 建的合并提交不替用户签名，也就不会卡在口令输入上；提交信息用展示名，不出现 `refs/heads/…`）：成功 → `merged`（`commits = behind`）；有冲突 → `conflicts`，冲突文件来自 `git diff --name-only --diff-filter=U -z`，合并停在中途；其他失败（比如没配 `user.email`）→ `internal`（`git_failed`，消息带 stderr 尾部 ≤ 2 KiB），并 `git merge --abort` 收拾
- 每种结果发对应的 `git` 事件；审计 `session.sync_base`（`detail = { outcome, commits }`）

**归档、删除、取消归档**：

- 处置时发现 worktree「被换掉」（目录身份或指针不一致）：**不写 `removed`、不发 `worktree_removed`**，记录保持 `missing`，磁盘不动；归档照常完成，事件 `worktree_missing` 说明「目录没有动过」；取消归档时保持 `missing`（会话照常取消归档，之后 `send` 按「cwd 与复核」的规则提示）
- `failed` 的记录：失败当时已经清理过，之后的归档 / 删除 / `remove_worktree` **只改库，不碰磁盘和分支**：归档只归档（不发 `worktree_removed`），删除会话时记录一起删（名字随之释放），`remove_worktree` 只删记录。`creating`（崩溃残局）清理前核对没有别的记录（同项目、不同 id、非 `failed`）用同一路径或分支，判断「git 登记过这个路径」时不跟随符号链接（路径本身是符号链接就不认）
- 重建（下一次 `send`）进行中删除会话：按删除请求的处置方式处理，不带 `delete_branch` 绝不 `-D` 已有分支；第一次创建进行中被删，分支是刚开的，照样清掉
- `archive` / `delete` 先检查状态（queued / starting / running / waiting_permission 时 `archive` 返回 `busy`；`delete` 取消等审批的 turn，请求按拒绝处理），然后收掉 agent 进程（`retire_tracked`），再处理 worktree（会话标记为忙，git 不在会话锁里跑）：
  - `auto`：没有未提交改动（含未跟踪）→ `git worktree remove <path>`；再看分支：相对 `base_sha` 没有自己的提交（`git rev-list --count <base_sha>..<branch>` = 0）且没有上游 → `git branch -D <branch>`。`state = removed`，发 `worktree_removed`（`note` = `branch_deleted` 或 `branch_kept`，稳定取值，客户端据此显示分支去向）。有未提交改动 → 不动，`state = kept`，发 `worktree_kept`
  - `keep`：不动；归档时 `state = kept`
  - `remove`：`git worktree remove --force <path>`，`state = removed`；`delete` 时再按 `delete_branch` 决定 `git branch -D`（不带就保留分支，哪怕没有自己的提交）
  - 之后 `git worktree prune`
- `delete` 之后 `worktrees` 行保留（`session_id` 置空），除非 worktree 和分支都已删掉（那时连行一起删）
- `unarchive`：`state = removed` → 按「目录不存在」的重建规则重建（路径被占用就换一个目录名并更新记录），`worktree_restored`，`state = active`；`kept` → `active`；重建失败 → 返回错误、保持归档
- 审计：`worktree.remove`（`detail = { branch, force, branch_deleted }`）

**worktree 管理**（`workspace.worktrees` / `remove_worktree`）：

- 列表来自 `worktrees` 表，每项现算 `uncommitted`、`ahead`（相对 `base_sha`）、`bytes`（目录遍历，整个调用 3 秒预算，超了的项 `bytes` 为空）；目录不在了的 `active` / `kept` 项按 `missing` 报
- `remove_worktree`：所属会话存在且没归档，或者有 turn / agent 进程（哪怕已归档）→ `busy`；`failed` 的记录只删库；有未提交改动且没 `force` → `invalid`（`uncommitted_changes`）。删目录（`--force` 视参数）、按 `delete_branch` 删分支、`prune`；会话已删的连行一起删，会话还在（已归档）的置 `state = removed`
- 只删 `worktrees` 表里记着的目录：路径从记录拼出，删之前逐段 `lstat` 并核对 `(dev, ino)`；不一致就只删记录、不碰磁盘（`git worktree prune` 由 git 自己判断）

**项目层面的联动**：

- `workspace.rename`（根目录项目）和 `restore`：对这个项目所有 `active` / `kept` 的 worktree 跑一次 `git worktree repair <paths…>`（worktree 的 `.git` 文件和主仓库里的记录都是绝对路径）。改名期间这些会话标记为 git 忙，repair 完成之后才让会话看到新名字，不留「已改名、还没 repair」的窗口；repair 失败只记 warn，会话的指针核对会给出「运行 `git worktree repair`」的提示
- `workspace.trash` 整个项目时 worktree 跟着进回收站（它们在项目里）；有没归档的 worktree 会话时和现在一样返回 `busy`
- `workspace.rename` / `trash` 的「项目忙」判断也看会话的 git 忙标记（同步 base、归档处置、删 worktree 进行中）
- `workspace.branches`：`git for-each-ref --sort=-committerdate --format=%(refname:short) refs/heads refs/remotes`，去掉 `*/HEAD` 和 `<branch_prefix>/` 开头的本地分支，各取前 200 个
- 关联项目（ADR-33 补充）：上面所有写操作只发生在 `.anycon/worktrees/` 里 anycon 自己建的目录、`info/exclude` 的那一行，以及 git 的 worktree / 分支元数据

**PR 自动归档**（`worktree.auto_archive_on_pr_close = true`）：周期任务每 10 分钟一次，**不在 sweeper 的同步路径上跑**（单独 `tokio::spawn`，原子标记保证同一时刻只有一轮；每小时的 `cleanup` 同样移出），挑没归档、没有 turn 在跑、有上游的 worktree 会话（每次最多 20 个），`gh pr view` 拿到 `MERGED` / `CLOSED` 就按 `auto` 归档；归档前在会话锁里重新检查，这时有 turn、在等审批或有排队消息就跳过、留到下一轮。`gh` 不可用就整轮跳过。

**错误原因**（`data.reason`，客户端据此显示说明；未知的按错误码显示通用说明）：

| reason | 错误码 | 什么时候 |
|---|---|---|
| `not_worktree_ready` | `invalid` | 项目不能开 worktree（不是仓库根目录 / 没有提交） |
| `no_commits` | `invalid` | 仓库还没有提交 |
| `bad_base` | `invalid` | `base` 解析不成提交 |
| `not_git` | `invalid` | 会话不在 git 仓库里（`git_status` / `diff`） |
| `not_worktree` | `invalid` | 对没有 worktree 的会话做 `against = base` / `sync_base` |
| `worktree_creating` | `busy` | worktree 还在创建 |
| `worktree_failed` | `invalid` | worktree 创建失败过 |
| `archived` | `invalid` | 对已归档的会话 `send` / `sync_base`（先取消归档） |
| `worktree_not_active` | `invalid` | worktree 不是 `active`（已保留 / 已删除 / 不见了）时同步；对已 `removed` 的会话发消息（先取消归档） |
| `worktree_missing` | `not_found` | worktree 目录不见了或被换掉（§ 10.1「cwd 与复核」） |
| `uncommitted_changes` | `invalid` | 同步前有未提交改动；删 worktree 时没 `force` |
| `merge_in_progress` | `invalid` | 上次合并还停在冲突里 |
| `no_merge_in_progress` | `invalid` | `abort` 时没有进行中的合并 |
| `fetch_failed` / `git_failed` | `internal` | fetch / merge 本身失败，消息带 git 输出尾部；`git_failed` 也用于取消归档时重建失败 |
| `worktree_remove_failed` | `internal` | `git worktree remove` 失败 |

**限制**：

- 不把主仓库的 `.git` 加进 agent 的可写目录（ADR-34 第 9 条）。Codex `workspace` 模式下 agent 在 worktree 里 `git commit` 会触发它自己的提权审批
- daemon 跑 git / gh 时（包括 Web 每个 turn 结束后自动刷新的 `git_status` / `diff`）会读仓库自己的配置，里面定义的外部命令会被执行：clean / smudge filter（检出、status、diff）、merge driver（同步）、`core.sshCommand` / `core.askPass` / credential helper / `remote.*.uploadpack`（fetch、`gh`）；fsmonitor、hooks、外部 diff / textconv、合并提交签名已关
- agent 能改 cwd 里 worktree 的 `.git` 指针文件（「当前目录」模式下是整个 `.git/`）。daemon 在 worktree 里跑 git 或动手前都核对指针（按词法规范化比较：`..` 跟在符号链接后、值两端有空白这类写法，daemon 和 git 的解析可能不同，所以这层核对比逐字比较宽松，是纵深防御），但用户之后在这个目录里自己跑 git 时读到的配置仍可能是 agent 写的——worktree 模式没有扩大这条面，也没有消除它
- 同一个 worktree 只属于一个会话；「在已有 worktree 里新开会话」不做

## 11. 存储

SQLite（WAL）。全部表：

| 表 | 内容 |
|---|---|
| `sessions` | id、project、agent、title、status、archived、settings(JSON)、agent_session_id、launch_args(JSON)、created、updated、last_seq、usage 累计 |
| `linked_projects` | name（主键）、path（canonical，唯一）、created（ADR-33） |
| `worktrees` | id（主键）、project_loc（同会话的位置记录）、project（名字，展示用）、rel_path、branch、base（规范化的引用名或完整 SHA）、base_display（展示名）、base_sha、session_id（可空）、state、dev、ino、git_pointer（`.git` 指针文件内容，项目路径换成 `{project}`；早期版本建的记录为空）、created（ADR-34）。`sessions.worktree_id` 指向它 |
| `events` | `(session_id, seq)` 主键、ts、turn_id、event(JSON) |
| `turns` | session_id、turn_id、message_id、started、ended、reason、usage(JSON)。汇总用，避免扫 events |
| `messages_fts` | FTS5 虚表：用户消息和 assistant 文本，`session.search` 用 |
| `blobs` | id、session_id（空 = 还没被消息引用）、name、mime、size、sha256、width、height、path（`blobs/` 下的文件名）、created（§ 7.10）|
| `peers` | 已配对客户端：device_id、pubkey、name、kind、paired_at、last_seen |
| `enroll_password` | 节点口令（§ 15.1）：salt（16 字节）、Argon2id 输出（32 字节）、set_at、set_by；连续失败次数、下次允许尝试的时间、是否锁定。最多一行。接入会话和接入码只在内存 |
| `agent_cache` | agent id、AgentInfo(JSON)、diagnose 文本、probed_at |
| `audit` | § 13 |
| `jobs` | clone 等长任务的状态，重启后标 failed |
| `kv` | schema 版本等 |

事件表按会话删除；`history_retention` 到期的会话整条删。`VACUUM` 不自动跑，`anycon stats` 显示可回收空间并给命令。

## 12. 多客户端

- 每个客户端一条 Noise 会话，daemon 对每条连接**单独加密**。所以事件不是广播，是按 watcher 逐个发
- `session.watch { session_id }` / `unwatch`：只有 watcher 收 `session.event`；一个客户端最多 watch 5 个会话
- 列表级变化（`session.changed { id, title, status, pending_permissions, updated_at }`）发给所有在线客户端，频率限制 1 次 / 秒 / 会话
- `permission` 事件**同时**作为 `permission.request` 通知发给所有在线客户端（不只 watcher），因为要能在列表页审批
- 慢客户端：hub 回 `backpressure` 时，daemon 对该客户端**合并** `text_delta` / `thinking_delta`（累积到下一次可发送时一次发出），其他事件不丢；持续 60 秒发不出去（被退回、被限速，或在途窗口一直是满的）就丢掉该客户端发送队列里排着的 `session.event` 通知（其余消息不丢，也不 `unwatch`），客户端回来后按 seq 补
- 发送方规则（[protocol.md § 3.1](../protocol.md#31-消息大小)）：每个对端把最近发出的 `data` / `notify` 信封原文留在一个 16 MiB 的**在途窗口**里，只有同时满足「发出（含重发）已超过 10 s、seq 小于最小的待重发 seq、不属于正在写的那条消息」才逐出；窗口满了写入端**暂停**（≤ 256 KiB 的小消息可以多用 4 MiB 余量，权限请求不会被大文件卡住），不逐出。收到 `backpressure { to, seq }` 暂停该对端，等 `retry_after_ms`（没有就 100–300 ms 加抖动、连续几轮翻倍），把被退回的 `seq` 按升序原样重发（2 MiB 一批，重发也重新计 10 s），再继续；一条消息写到一半被退回时剩下的分片留在手里等，不先发出去。`rate_limited` 让整条 hub 连接的 `data` / `notify` 出站都暂停（封顶 60 秒，控制消息和握手不停）。`handshake` 信封（IK 回复、配对 XXpsk3 的第二条、账号内接入的握手帧）也留原文：发出时不排队、不受暂停影响，但被退回时和 `data` 一样排进重发，等过暂停后原样再发一次，不重置会话——响应方没有别的补救，丢了只能等对端超时。被退回的 `seq` 已不在窗口里：用同一个 `seq` 发一帧 `RESET` 占位让 hub 清掉粘住状态，然后重置该对端的传输；但如果这个 `seq` 早于当前会话（上次握手回复那一封）就只占位、不重置。写入端在对端被退回期间不再加密新消息（增量合并），连接上排队超过 2 MiB 就等，每发 2 MiB 让出 5 ms。**已知代价**：窗口填满后持续大流量的上限约 16 MiB / 10 s ≈ 1.6 MiB/s（单次 ≤ 16 MiB 的读不受影响）；要去掉得让 hub 给「已转发到某 seq」的正向回执，是协议改动。另外旧会话的占位 RESET 是一封真的 `data` 帧，会被 hub 转发给客户端，客户端在 ready 状态收到会多做一次重新握手（自愈、不丢数据）
- `node.enroll_requested`、`node.enroll_resolved`、`node.security_event` 和 `permission.request` 一样发给所有在线客户端（§ 15.1）；`node.enroll_list` 给刚连上的客户端补齐进行中的接入
- 本机 CLI 通过 socket 是另一种 watcher，规则相同，没有加密
- **同一个客户端的请求按到达顺序串行处理**（本机 socket 每个连接一个 worker，经 hub 的客户端每个会话一个）；`agent.list { refresh }`、`agent.diagnose`、`node.doctor`、`session.git_status`、`session.diff`、`session.sync_base`、`workspace.list`（冷缓存最多等 3 秒）、`workspace.branches`、`workspace.worktrees` 例外——又慢又不依赖顺序，各自并发（`sync_base` 和 `send` 靠会话的 git 忙标记互斥）。符合 [protocol.md § 7.3](../protocol.md#73-daemon-侧突发消息合并) 的「按原顺序逐条处理」

## 13. 审计

本地 `audit` 表，`anycon audit` 和 `node.audit` RPC 读。每条：ts、device_id、device_name、action、target、detail(JSON)。

记录的 action：`session.create` `session.send`（只记长度不记内容）`session.cancel` `session.delete` `session.set_settings` `permission.respond`（含 option 和 message）`workspace.create` `workspace.clone` `workspace.rename` `workspace.trash` `workspace.restore` `workspace.link`（记 canonical 路径）`workspace.unlink` `worktree.create` `worktree.remove` `session.sync_base` `fs.read` `node.revoke_device` `node.pair_begin` `pair.completed` `enroll.requested` `enroll.approved` `enroll.denied` `enroll.code` `enroll.completed` `enroll.failed` `enroll.password_set` `enroll.password_cleared` `enroll.password_locked` `enroll.settings` `node.set_agent_defaults` `config.changed`。

`session.create` / `session.set_settings` 的 detail 里带本次设置的 `env_keys`（**键名**，不带值）、`cleared`（被 `clear` 清掉的字段）和 `dangerous`（所选模式按当前配置算出的实际风险，不是写死的 false）；模式切换经审批触发时 `reason: permission_approved`；`node.revoke_device` 带 `hub_notified`（hub 侧的撤销是否已发出；hub 没连上时先记下，重连后补发）；`node.pair_begin` 只记 `{ long }`，**不记短码和 `match_id`**。`enroll.*` 记方法和结果（`enroll.failed` 带 `reason`，口令失败带当时的连续失败次数），**不记接入码、扫码密钥、口令和它的派生值**；接入完成前发起方还没有正式 id，`device_id` 记临时 id，`device_name` 记自报的 `hint` 加 ` (unverified)`；`enroll.completed` 记正式 id 和认证过的名字。`enroll.*` 的 `detail` 里 `method` / `result` / `reason` 是字符串（`result` 取 `requested` / `approved` / `denied` / `completed` / `failed` / `expired`），`failures` 是数字，另有 `attempts_left`、`approval`、`changed`、`failures_reset`、`password`、`source`。低价值的行（`enroll.requested`、预检拒绝）每分钟最多写 30 条。

保留和会话历史同样的 `history_retention`。

## 14. 通知触发

| category | 触发 |
|---|---|
| `permission_requested` | 每次 `permission` 事件，立即 |
| `turn_finished` | `turn_done` 且按 `notify.turn_finished`：`when_unwatched` 时只在该会话没有 watcher 时发。turn 结束时后台任务还在跑（§ 7.3）的，推迟到它们都结束之后、agent 自己跑的那一轮结束时再发；那一轮没来（任务都结束 15 秒后还没有 turn；`idle_timeout` 更短时按它，赶在回收之前）就兜底补发。agent 自己开的 turn 只在有推迟的通知时才发，不然它是已经通知过的 turn 的后续。用户又发了消息的，推迟的通知作废 |
| `error` | `error` 事件且 `recoverable = false`，或会话进入 `error` |
| `enroll_requested` | 一个请求批准的接入（§ 15.1，`code` 且 `approval = true`）到达，立即；每个请求一次。`scan` 不发（用户就在扫码）；批准之后同一会话里的重试也不再发 |
| `security` | 新设备经接入加入；节点口令连续失败满 3 次；口令接入被锁定；口令被设置 / 修改 / 清除；`[enroll]` 被修改 |
| `session_idle` | 会话 `idle` 超过 `session_idle_after` 且期间没有任何客户端 watch 过（提醒「结果还没看」）；后台任务在跑时不算 idle，任务都结束时重新计时。daemon 重启不触发（重启后不给已经 idle 的会话重新计时）；已归档的会话不提醒；一直在 watch 的客户端也算「看过」（idle 开始时和每次扫描都检查当前有没有 watcher）|

同一会话同一 category 60 秒内只发一次；`security` 没有会话，按节点算，60 秒内的多条合并成一次（详情经 `node.security_event` 逐条发给在线客户端）；`enroll_requested` 不合并。

## 15. 连接与可靠性

- hub 重连：指数退避 1s → 60s 加抖动；收到 `4002 restarting` 用 1–3 秒
- 睡眠唤醒：每 5 秒检查单调时钟跳变 > 30 秒即视为唤醒，立即重连并重跑 `agent.list` 探测
- 版本协商：`control.auth` 带协议版本；hub 不兼容时 `auth_fail { reason: version }`，daemon 每 10 分钟重试并在日志里喊；本机 `anycon doctor` 能看到
- Noise 会话按客户端独立，一个客户端握手失败不影响其他
- 所有 RPC 幂等键：`session.send`（和带 prompt 的 `session.create`）带客户端生成的 `message_id`，重发不重复；同一个 `message_id` 配不同内容（prompt / agent / project）返回 `invalid`，不会静默返回第一次的结果
- **配对与 `pair.ack`**：
  1. `node.pair_begin { long? }`：本地生成短码（`long` 时 16 位，否则 8 位；CLI 和 Web 默认都传 `long: true`），一次 Argon2id 算出 `match_id` 和 PSK，向 hub 发 `pair_begin { match_id }`，等 `pair_registered { expires_at_ms }`，返回 `{ code, expires_at_ms, link }`，`link = ${hub.url}/pair#code=<code>`（fragment）。短码和 PSK 从不发给 hub，hub 也不记录 `match_id`。同一 daemon 同时只有一个有效短码
  2. `pair_matched` 之后 daemon 是 Noise XXpsk3 的 responder；握手成功就记下对方静态公钥和 `ClientInfo`，发 `pair_confirm`
  3. 收到 `pair_done`（`peer` = 新设备的正式 id）后，daemon 把新设备加进 peers，并**经刚建立的 Noise 传输发 `pair.ack`**：`{ node_id, peer_id, name, version, fingerprint }`——自己的正式 id、hub 告知的新设备正式 id、节点名、版本、自己静态公钥的指纹。新设备据此校验（`peer_id` 是自己、`node_id` = `pair_done.peer`、`fingerprint` = 握手得到的公钥指纹）后才落盘；hub 伪造 `pair_done` 做不出这条消息
  4. 之后 hub 发 `peer_online`，客户端用 IK 重连
  5. 握手成功到 `pair_done` 之间，新设备以临时 id 暂存在内存里：**不出现在 `node.devices` 和通知里**；看门狗超时、hub 断线、新的 `pair_begin`、`pair_failed` 都会删掉暂存记录（内存、库、`peers.json`），不会留下一个「已配对」的幽灵设备。daemon 崩溃留在库里的暂存记录目前不在启动时清理
- **传输失步与重置**：解密失败、分片乱序（`index` 不连续、`count` 不一致）、重组溢出，都意味着这个客户端的 Noise 传输已经和对端失步。daemon 丢弃该客户端的传输并清空重组器，发一帧 `RESET`（`data` body 的 `0x02`），等客户端重新发起 IK 握手；其他客户端不受影响。收到 `backpressure` 的那一片原样重发，不重新加密。收到 `dropped { too_large }` 说明 daemon 自己没分片，是 bug，记 error 日志
- 发送大消息：一条 RPC 的密文超过 200 KiB 就分片，每条 RPC 一个本地递增的 `msg_id`，同一条的各片连续发送、不与其他消息交错

### 15.1 账号内接入

[ADR-36](../decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)，消息序列见 [protocol.md § 4.4](../protocol.md#44-账号内接入)。**daemon 是唯一的验证点**：hub 只确认发起方和本节点属于同一账号，它的限速只防滥用——hub 本身可能就是攻击者，所以在线猜测的次数由这里定。

- **会话**：收到 `enroll_request` 建一条内存里的接入会话（`enroll_id`、发起方临时 id、方法、5 分钟过期），等 `EnrollHello`。进行中的已满 3 个 → `rejected { busy }`；`enroll.approval = false` 时 `scan` 和请求批准的 `code` → `method_disabled`；`enroll.password = false` → `method_disabled`；没设口令 → `password_not_set`。之后照配对的规则：XXpsk3 里 daemon 是 responder，握手成功就 `pair_confirm`，`pair_done` 之后发 `pair.ack`，握手成功到 `pair_done` 之间的暂存设备规则同 § 15 第 5 条
- **`scan`**：等 `node.enroll_approve { enroll_id, secret }`，`secret` 必须是 32 字节（对非 `scan` 请求带了 `secret` → `invalid`，不算已处理）；用它派生 PSK 后回 `ready`。r 不落盘、不进审计
- **`code`，请求批准**：发 `node.enroll_requested`（所有在线的已配对设备）和 `notify { enroll_requested }`。第一台 `node.enroll_approve` 的设备拿到码（6 位数字，CSPRNG 均匀生成），其余设备收到 `node.enroll_resolved { approved, by_device }`，之后再批准 → `already_resolved`；`node.enroll_deny` 同理，发起方收到 `rejected { denied }`。这个码只对这个 `enroll_id` 有效
- **`code`，主动生成**：`node.enroll_code` 生成一个不绑定请求的码，5 分钟内本账号任一 `approval = false` 的请求都可以用；主动生成的码同时只有一个，新的顶掉旧的
- **码的限额**：每个码合计 3 次尝试，**收到新设备的 CPace 份额时扣**（每个份额最多换来一次第 3 条的校验）。已发 `ready`、等份额的阶段不计时、不占全局尝试名额，批准之后用户可以慢慢输码；收到份额后到第 3 条限 60 秒，超时算这一次失败，回一条不结束会话的 `rejected { expired, attempts_left }`（还有次数时新设备可以重发 `hello`）。只发 `hello` 不扣次数。用掉、到期或用完次数就作废。最后一次失败回 `wrong_secret { attempts_left: 0 }`，之后再来的 `hello` 回 `expired`（请求批准得到的码）或 `no_code`（主动码）。第 3 条通过时再核对一次码还在、没被顶掉、没过期，两个设备抢同一个码只有一个进得来，另一个得到 `expired`。码只在内存里
- **会话的终局**：预检拒绝（`method_disabled`、`password_not_set`、`backoff`、`locked`、`no_code`、`busy`）不结束会话，发起方可以换一种方式重发 `hello`；结束会话的是 `wrong_secret`（口令、`scan`）、`denied`、`expired`、`invalid`
- **`password`**：
  - 存储：`enroll_password` 表只有 salt 和 `Argon2id(口令, salt, m = 64 MiB, t = 3, p = 1, out = 32)`，不存口令本身；派生在设置时做一次（放阻塞线程），接入时直接用存下的值当 CPace 的口令输入。口令两端都按 Unicode NFC 规范化后取 UTF-8
  - 每次尝试前检查：锁定 → `rejected { locked }`；没到下次允许的时间 → `rejected { backoff, retry_after_ms }`；已有一个口令尝试在进行 → `busy`（同时只允许一个，否则并发能绕过退避）
  - **悲观计数**：发出自己的 CPace 份额**之前**先把连续失败次数 +1 写库，第 3 条校验通过后清零。所以错口令、超时、中途断开、`enroll_cancel`、daemon 崩溃都算一次失败
  - 退避：连续失败 n 次（n ≥ 3）后，下次允许尝试的时间是 2^(n−3) 分钟之后；n = 3 时发 `node.security_event { password_failures }`，n = 10 锁定，发 `node.security_event { password_locked }`。重新启用：本机 `anycon passwd --enable`，或已配对设备 `node.set_enroll_enabled { password: true }`，计数清零
  - 设置：本机 `anycon passwd`（Unix socket，交互输入两次），或已配对设备 `node.set_enroll_password`。至少 8 个字符，不做复杂度规则。设置、修改、清除后计数清零、解除锁定，发 `node.security_event { password_changed }`；已配对的设备不受影响。允许远程设置是因为已配对设备本来就有 agent 的全部权限，限制它不增加安全
- **完成**：`pair.ack` 发出后给其余已配对设备发 `node.security_event { device_enrolled }`（带方法和设备名）和 `notify { security }`
- **确认槽**：`pair_done` / `pair_failed` 里没有会话标识，所以同一时刻只有一个会话（配对或某个接入）在等 hub 的回执。两个接入同时握手成功时，后一个等槽（最多 15 秒）；配对和接入撞上时配对放弃，新设备重新配一次
- **暂存设备的清理**：握手成功后暂存的设备（临时 id）在这些时候删掉——hub 的 `enroll_failed`（含收尾失败）、hub 断线、会话过期、停在「等 `pair_done`」超过 70 秒。`pair_failed` 一律归配对，不影响接入（hub 对接入的失败只发带 id 的 `enroll_failed`）。发起方是已配对的正式设备（重新接入）时，失败后恢复原来的记录，而不是删掉
- **配置**：`[enroll]` 经 RPC 改了发一次 `node.security_event { settings_changed }`；直接改 `config.toml`（`anycon config set`、手动编辑）由配置监视发现后补报一次（`by_device = "local"`）并记审计 `enroll.settings`（`source: "config.toml"`）
- **提示信息**：`EnrollHello.hint` 未经认证，只用于 `node.enroll_requested` 和审计，清掉控制字符、按字符截到 64；正式的设备名取 XXpsk3 第 3 条里的 `ClientInfo`，和配对一样

## 16. 安全边界

- agent 以 daemon 用户身份运行，没有沙箱；anycon 提供的是**审批**和**workspace 路径边界**，不是隔离（README 已声明）。关联目录（§ 10）也在边界之内：已配对设备可以不经审批读里面的文件，agent 按所选模式改文件；能关联哪些目录见 ADR-33 的五条规则
- `dangerous` 模式：config 可禁；允许时每次 `session.create` / `set_settings` 选它都进审计
- 会话 `env`：默认整体拒绝（`permissions.allow_session_env = false`）；开启后键必须在运维配置的允许列表 `permissions.session_env_allow` 里，再过黑名单兜底（`LD_*`、`DYLD_*`、`BASH_ENV`、`SHELLOPTS`、`PS4`、`LESSOPEN`、`NODE_*`、`ANTHROPIC_*`、`*_PROXY` 等，完整列表见 § 5.5），且不能覆盖 `agents.env` 里运维配置的键，键名进审计（ADR-31）
- 附件（§ 7.10）：单个 ≤ `max_attachment_bytes`，上传只收四种图片且文件头必须和 mime 一致，发送时还要在该 agent 的 `capabilities.attachments` 里；`blob_id` 先校验格式再进路径
- `fs.read` 只读、限 workspace 根目录和关联目录内；默认读 1 MiB、最大 10 MiB，超出的部分截断（`truncated = true`）。blob 通道（§ 7.10）只收图片
- 响应大小：`session.history` 一页 ≤ 192 KiB（§ 7.9），`tool_result.output` ≤ 64 KiB（§ 8.2），`session.send` ≤ 1 MiB；其余靠传输层分片（≤ 200 KiB / 片，重组上限 16 MiB）。大消息经 hub 的 4 MiB 发送缓冲时会被退回一部分并重发（hub 保证顺序，[protocol.md § 3.1](../protocol.md#31-消息大小)）；daemon 收到 `backpressure` / `rate_limited` 要暂停再按序重发，不能继续灌
- 配对：短码本地生成、不发给 hub；joiner 要等 `pair.ack` 才落盘（§ 15）
- 账号内接入（§ 15.1）：daemon 是唯一的验证点，hub 的限速不算数。口令只存 Argon2id 派生值；口令失败计数在发出 CPace 份额**之前**先写库；同时最多一个口令尝试、3 个接入会话；接入码和扫码密钥只在内存里，不落盘、不进日志和审计
- 本机 socket 0600；CLI 不需要再认证
- `--add-dir` 之类扩大 agent 文件范围的参数不暴露给客户端
- worktree（§ 10.1）：目录在项目里面，复核规则同关联项目；不把主仓库 `.git` 加进 agent 的可写目录；`.worktreeinclude` 只复制、不执行，不跟随符号链接；daemon 的 git 关掉 fsmonitor / hooks / 外部 diff / textconv / 合并签名，仓库配置里的 filter / merge driver / sshCommand / askPass / credential helper / uploadpack 仍会被执行（已知限制，§ 10.1「限制」）

## 17. 诊断

`node.doctor` 返回 [cli.md § 5](cli.md#5-anycon-doctor) 的检查项；`node.stats` 返回运行时长、连接状态、各状态会话数、进程数、数据库大小、磁盘剩余；`node.logs { lines, level }`（v0.2）返回最近日志，不含内容类字段。

## 18. 版本范围

**v0.1**：每会话 worktree、`session.git_status` / `diff` / `sync_base`、worktree 管理、PR 状态与自动归档（§ 10.1，ADR-34 提前进 v0.1）；Claude Code adapter 全部；Codex adapter（app-server，每会话一个进程；§ 5.4 / § 6.3 / § 8 / § 9 里 Codex 一列，`session.fork` / `import` 除外）；探测 § 5（Claude Code、Codex 两项）；选项 § 6；生命周期 § 7（不含 fork / import）；事件 § 8；权限 § 9；workspace § 10；存储；多客户端；审计；通知。

v0.1 另含：图片附件（§ 7.10，ADR-35）；账号内接入（§ 15.1，[ADR-36](../decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)）。

**v0.2**：ACP adapter；非图片附件；`node.set_config` 远程改配置；`node.logs`；`anycon export / import`。

**v0.3**：`session.fork`、`session.import`、自动更新、Prometheus。
