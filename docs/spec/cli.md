# CLI 功能规格

> `anycon` 一个二进制，daemon 和 CLI 合一（[ADR-17](../decisions.md#adr-17-daemon-与-cli-合一)）。本文列出全部子命令、它们走哪条通道、输出约定。

## 1. 两类命令，两条通道

| 类别 | 例子 | 通道 |
|---|---|---|
| **本机管理** | `init` `daemon` `service` `pair` `passwd` `doctor` `devices` `audit` | 本机 Unix socket `~/.anycon/daemon.sock`，JSON-RPC，和内层协议同一套方法，另加几个只在本机可用的方法（`local.status`、`local.rename_device`、`local.config_reload`、`local.pair_outcome`，经 hub 调用一律 `forbidden`）。socket 权限 0600，不需要 Noise，不经过 hub |
| **客户端** | `nodes` `agents` `projects` `ls` `new` `send` `attach` `approve` | 作为一台设备经 hub 连到目标 daemon，Noise E2E，和 Web / SDK 完全一样 |

客户端命令的目标节点恰好是本机 daemon 时（`--node` 省略且本机有 daemon，或 `--local`），自动走 socket，不绕 hub。这样在 Mac mini 上 `anycon send "跑测试"` 在断网时也能用。

CLI 作为客户端设备的密钥和 device token 存在 `~/.anycon/client/<hub-host>/`，每个 hub 一套；首次用 `anycon connect` 配对（短码）或接入（登录账号，§ 3.3）。

## 2. 目录与配置

```
~/.anycon/                      ANYCON_HOME 可改
├── config.toml                 daemon 配置，见 daemon.md § 3
├── daemon.db                   会话、消息、附件、FTS、审计
├── keys/
│   ├── node.key                daemon 的 Noise 静态私钥，0600
│   └── device-token            hub 签发的设备 token，0600
├── peers.json                  已配对客户端的公钥和名称
├── daemon.sock                 本机 CLI 通道
├── logs/daemon.log             滚动，默认保留 7 天
├── blobs/                      附件
└── client/<hub-host>/          CLI 作为客户端设备时的密钥和 token
```

## 3. 子命令

### 3.1 安装与运行

| 命令 | 说明 |
|---|---|
| `anycon init --hub <url> --workspace <dir> [--name <node-name>]` | 生成 `config.toml` 和 Noise 密钥，创建 workspace 根目录（含 `.trash/`、`.scratch/`）。交互式询问缺省项；`--yes` 全部用默认。已初始化则拒绝，除非 `--force`（会提示重新初始化将使所有已配对设备失效）。结束时提示一句：daemon 跑起来后可以用 `anycon passwd` 设节点口令，出门在外、手边没有已配对设备时用它解锁这台节点（ADR-36） |
| `anycon daemon [--foreground]` | 前台运行。默认即前台，日志到 stderr；由 launchd / systemd 托管时也是这个命令。没有 daemonize 选项，后台化交给服务管理器 |
| `anycon service install \| uninstall \| start \| stop \| restart \| status` | 注册为用户级 launchd（macOS）/ systemd user unit（Linux）。`install` 后自动 `start`，开机自启。`status` 显示进程状态和最近日志 |
| `anycon doctor` | 诊断，见 § 5 |
| `anycon version` | 版本、协议版本、平台 |
| `anycon update [--check]` | v0.3。检查并替换二进制，之后 `service restart` |

### 3.2 配对与设备（本机管理）

| 命令 | 说明 |
|---|---|
| `anycon pair [--short] [--qr] [--name <hint>] [--no-wait]` | 让 daemon 发起配对，打印短码和配对链接。**短码由 daemon 本地生成，不发给 hub**。默认 16 位 `XXXX-XXXX-XXXX-XXXX`（约 79 bit）；`--short` 生成 8 位 `XXXX-XXXX`（约 40 bit），只适合在**自建 hub** 上手输——hub 地址不是本机 / 私网时 CLI 会在 stderr 警告（恶意 hub 可以拿 `match_id` 离线穷举 8 位短码，[ADR-29](../decisions.md#adr-29-短码不经过-hub)）。`--long` 保留为兼容的无操作。链接是 `<hub.url>/pair#code=<短码>`，短码在 URL fragment 里；`--qr` 在终端把链接渲染成二维码。两端各做一次 Argon2id（约 0.1–0.3 s）派生撮合 id 和 PSK。阻塞直到配对完成、hub 拒绝这次配对或 5 分钟超时，完成后打印新设备名；`--no-wait` 打印完短码就返回。**hub 拒绝时马上停下**：daemon 记下 hub 的 `pair_failed`（或配对中途的看门狗超时），CLI 每秒用 `local.pair_outcome { expires_at_ms }`（`expires_at_ms` 就是这次 `node.pair_begin` 返回的值，所以别的配对留下的失败不会被认成这一次的）问一次，问到就退出并在 stderr 写原因，不再干等到短码过期。老版本的 daemon 没有这个方法（回「找不到方法」）：CLI 第一次得到这个回答就不再问，只等设备出现；别的错误只记 debug 日志（`ANYCON_LOG=debug`），下一秒再问。退出码和原因：`quota`（配对收尾时账号的设备数或节点数到了上限，`multi`）、`timeout`（新设备没在限时内完成）、`peer_gone`（新设备中途掉线）、`rate_limited`、`not_found`（hub 已不认这个短码）→ 1；`replaced`（同一个 daemon 上又开始了新的配对：网页的「添加设备」或另一个 `anycon pair`，这个短码作废了）→ 1；`forbidden`（两台设备属于不同账号）→ 5。stderr 是 `error: <原因>: <说明>`。daemon 没连上 hub 时立即退出（退出码 3）；hub 在登记短码时就拒绝（`node.pair_begin` 回 `busy` + `data.reason`：账号今天的配对名额用完了是 `quota`，被限速是 `rate_limited`；`forbidden`）时，和下面 hub 拒绝配对一样处理：同样的原因和退出码（`quota` 的说明不同：登记时它只可能是每日配对次数，说名额随 24 小时滚动窗口回来、数字看「设置 → 配额」，不叫用户去移除设备；配对收尾时的 `quota` 是设备数 / 节点数，才叫用户去移除不用的设备），`--json` 时 stdout 只有 `{"failed":{…}}` 一行（没有短码），不会被当成「节点离线」。`--json`：**第一行 stdout** 是 `{"code":"XXXX-XXXX-XXXX-XXXX","link":"<hub>/pair#code=XXXX-XXXX-XXXX-XXXX","expires_at_ms":…}`（`--short` 时 `code` 是 8 位；打印后 flush，脚本读到这一行就可以去输入短码），配对完成后最后一行 `{"paired":{"device_id","name","kind"}}`；被 hub 拒绝时最后一行是 `{"failed":{"reason":"quota","message":"…"}}`（`reason` 是上面的机器可读原因），再按上面的退出码退出；超时非零退出（1）。同一个 daemon 可以再次 `pair` 加第二台、第三台设备。`--name <hint>`：配对完成后给新设备改成这个名字（新设备自己上报的名字是默认）；已知设备重新配对也能检测到完成（比较 `paired_at_ms`），等待时长取短码的有效期。在 daemon 机器上随时可以用 `anycon rename-device` 再改 |
| `anycon pair --enroll [--no-wait] [--json]` | 生成一个 6 位**接入码**（`node.enroll_code`，[ADR-36](../decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)），5 分钟有效、合计可试 3 次，新的顶掉旧的。给已经登录同一账号的新设备用：Web 的解锁页选「我有接入码」，CLI 是 `anycon connect <hub> --via code`。码走 CPace，不怕恶意 hub 离线穷举，所以 6 位就够，比 16 位短码好输。阻塞到有设备用它完成接入（打印新设备名；以 `node.security_event { device_enrolled, method: code }` 为准，别的方式的接入不算）或过期；`--no-wait` 打印完就返回。接入码本身不经过 hub，daemon 没连上 hub 时只在 stderr 警告、不退出 3；`enroll.approval = false` 时退出码 5。`--json` 第一行是 `{"code","expires_at_ms"}`，完成后最后一行 `{"enrolled":{"device_id","name","kind"}}`。不能和 `--short` / `--qr` 同用（退出码 2） |
| `anycon passwd [--stdin]` · `--clear [--yes]` · `--enable` · `--disable` · `--status [--json]` | 管理**节点口令**（ADR-36），走本机 socket。细节见表格下面 |
| `anycon devices` | 已配对客户端列表：名称、kind、公钥指纹、配对时间、最近连接 |
| `anycon revoke <device>` | 吊销（按名称或 id 前缀）。两步都做：daemon 删公钥 + 通过 hub 删路由 |
| `anycon rename-device <device> <name>` | |

**`anycon passwd`**（节点口令，[daemon.md § 15.1](daemon.md#151-账号内接入)）：

- 不带参数：设置或修改口令，交互输入两次、不回显，至少 8 个字符（`node.set_enroll_password`）。不是 TTY 时必须带 `--stdin`（从 stdin 读一行，去掉行尾换行），否则退出码 2。口令不接受命令行参数，免得进 shell 历史
- `--clear`：清除口令，之后口令解锁不可用。交互确认，`--yes` 跳过
- `--enable` / `--disable`：启用 / 停用口令解锁（`node.set_enroll_enabled`，写 `enroll.password`）。被锁定时 `--enable` 先打印最近的失败记录（`node.audit` 里的 `enroll.failed`：时间、自报的设备名），确认后解除锁定、失败计数清零
- `--status`：是否设置、状态（`enabled` / `disabled` / `locked`）、连续失败次数、下次允许尝试的时间、设置时间和设置者（`node.enroll_status`）

每次改动 daemon 都会通知所有已配对设备并记审计；口令和它的派生值都不进日志。

### 3.3 客户端连接

| 命令 | 说明 |
|---|---|
| `anycon connect <hub-url> --code <short-code> [--name <device-name>]` | **短码配对**：把这台机器的 CLI 作为客户端设备和某个 daemon 配对，不需要登录账号。短码（16 位或 8 位，连字符可省）来自那台机器的 `anycon pair [--short]` 或 Web 的「添加设备」。握手之后 CLI 要等 daemon 经加密传输发来的 `pair.ack`，校验 `peer_id`、`node_id`、公钥指纹都对得上才把节点写进 `nodes.json`；30 秒内没收到就失败、不落盘；本地已有同 id 但公钥不同的节点时报 `key_mismatch`（退出码 4）、不覆盖，提示先 `anycon disconnect <node>` 再配对（节点名取自 `pair.ack`，清掉控制字符、按字符截断到 64，不取握手第 2 条里未经认证的名字）。`--code` 也接受整条配对链接 `…/pair#code=…`（`?code=` 兼容）。`nodes.json` / `client.key` 存在但损坏时报错退出，不会当作「没有」去生成新密钥覆盖旧的。如果已存的设备 token 被 hub 判为 `invalid` / `revoked`（被吊销、hub 丢了数据），`connect` 会丢掉旧 token 和设备 id，以全新的临时连接重新配对，不需要手动删文件 |
| `anycon connect <hub-url> [--node <name\|id>] [--via scan\|approve\|code\|password] [--email <addr>] [--name <device-name>]` | **账号内接入**（[ADR-36](../decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)）：不带 `--code` 时走这条。不需要短码，登录账号后解锁账号名下的一台节点。细节见表格下面 |
| `anycon nodes` | 这个 CLI 已配对的节点列表、在线状态 |
| `anycon disconnect <node>` | 删除本地对该节点的配对（本地删除，不吊销对端） |

**账号内接入**（`anycon connect` 不带 `--code`，消息序列见 [protocol.md § 4.4](../protocol.md#44-账号内接入)）：

1. **登录账号**。`--node` 是本地已配对过的节点（名称 / id）或完整的 32 位 hex id 时，不用列设备也不用登录。这台 CLI 已经是这个 hub 上的设备（配对过别的节点）时，先用设备 token 列设备（`GET /api/devices` 带 `Authorization: Bearer <device token>`），失败才登录。否则先 `GET /api/config` 看 `login_methods`（只认 `email` 和 `token`，`github` / `google` 要浏览器回调，CLI 忽略）：邮箱验证码（`--email` 或提示输入 → `POST /auth/email/send` → 提示输入 6 位码 → `/auth/email/verify`。hub 对绑定和没绑定的邮箱回答一样，所以提示写「如果这个邮箱绑定了账号，验证码已经发出」，不说「已发送」；格式不对的码本地挡掉、不白白用掉 hub 那边每个码 5 次的机会；`401 invalid_code` 重问，同一次登录最多错 3 次，之后退出码 5 并说明要重新运行；`429` 退出码 1，写明 `Retry-After` 的秒数）；`single` 模式也可以选 token（不回显输入 → `/auth/token`）。`multi` 的 hub 对任何邮箱都发码，验证通过时没有账号就建一个，所以用一个没注册过的邮箱登录会得到一个空账号，随后因为「账号里没有节点」退出。拿到的登录 cookie 只在内存里、不落盘，用于这一次 WebSocket 升级。hub 只在升级那一刻读 cookie，所以连上 hub（收到 `auth_ok`）就 `POST /auth/logout`，结束时不管成功失败再保证一次。两种都没开（hub 只配了 OAuth）时退出码 2，提示改用 `--code` 短码配对
2. **选节点**。`GET /api/devices` 里本账号未吊销的 daemon。`--node`（全局选项，`anycon --node X connect <hub>` 和 `anycon connect <hub> --node X` 都行）按名称或 id 前缀匹配账号里的节点；省略时只有一台就用它，多台就交互选择。列表里的名字来自 hub、未经认证，只用来选
3. **选方式**（`--via`，省略就交互选择）：
   - `scan`：在终端画二维码（和 `pair --qr` 同一套渲染），提示用已配对的手机在 Web UI 里「扫码批准新设备」
   - `approve`：向已配对设备发批准请求，提示输入对方显示的 6 位码
   - `code`：输入在已配对设备的「添加设备」或 `anycon pair --enroll` 拿到的码
   - `password`：不回显输入节点口令（Argon2id 约 0.1–0.3 s）

   码在同一次接入里最多试 3 次（输错重新提示），口令只试一次。等待批准时打印剩余时间；`Ctrl+C` 发 `enroll_cancel` 后退出（130）
4. **校验与落盘**。和短码配对一样等 `pair.ack`，校验 `peer_id`、`node_id`、公钥指纹后才写 `nodes.json`；公钥冲突同样报 `key_mismatch`（退出码 4），不覆盖；节点名取 `pair.ack` 的

只支持交互终端（邮箱验证码和批准码都要人看），不是 TTY 时退出码 2，提示脚本里用 `--code` 短码配对。退出码：码 / 口令不对、被拒绝、方式没开启、退避中、已锁定 → 5（stderr 写明原因和退避剩余时间，锁定时附上 `anycon passwd --enable`）；节点离线 → 3；`--node` 在账号里找不到或有歧义 → 2；超时、限速、配额 → 1。

**v0.1 的限制**：hub 对单个设备只允许一条连接（[hub.md § 3.1](hub.md#31-连接生命周期)，新连接以 `4001` 顶掉旧的），而经 hub 的 CLI 命令每个进程各占一条。所以同一个 `ANYCON_HOME`（同一台 CLI 设备）下，一个流式的 `new` / `attach` / `send` 还开着时，再跑 `approve` / `pending` / `cancel`，后者会把前者顶掉，前者以退出码 3 结束（目前 stderr 没有提示）。从 daemon 所在机器的终端用本机 socket（`--local`）不受影响；远程场景请顺序执行（`new --no-wait` → `pending` → `approve`）。允许同一设备多连接或让命令借道已有进程留待后续决定。被顶掉时 stderr 会说明原因（「被同一设备的另一个进程顶掉了」），并仍以 3 退出。

全局选项：`--hub <url>`（多个 hub 时指定）、`--node <name|id>`（多个节点时指定，否则用唯一的那个）、`--local`。

### 3.4 项目与 agent

| 命令 | 说明 |
|---|---|
| `anycon agents [--refresh]` | `agent.list`：每个 agent 的状态、版本、模型和选项 |
| `anycon projects` | 项目列表及推导元数据；关联项目标 `linked` 并显示路径，`missing` 的标出来 |
| `anycon projects create <name> [--git]` | |
| `anycon projects clone <url> [--name]` | 流式打印克隆进度 |
| `anycon projects rename <from> <to>` · `trash <name>` | |
| `anycon projects link <path> [--name <name>]` | `workspace.link`：把本机已有目录关联为项目；`path` 相对路径按当前目录解析成绝对路径再发（远程 `--node` 时不解析，原样发，`~` 由 daemon 展开） |
| `anycon projects unlink <name>` | `workspace.unlink`：只解除关联，不删文件 |
| `anycon projects browse [<path>] [--all]` | `workspace.browse`：列可关联的目录（`--all` = `show_hidden`） |

### 3.5 会话

| 命令 | 说明 |
|---|---|
| `anycon ls [--project <p>] [--status running\|waiting\|idle\|ended] [--all]` | 会话列表，默认最近 20 条；有 worktree 的会话多一列 BRANCH |
| `anycon new [--project <p>] [--agent <a>] [--model <m>] [--thinking <id>] [--mode <id>] [-o key=value]... [--worktree \| --here] [--base <ref>] [--branch <name>] [--no-wait] "<prompt>"` | 创建会话并发第一条消息。默认跟随输出直到本轮结束（等同 `attach`），`--no-wait` 只打印会话 id。`--project` 省略则 scratch。**工作位置**（ADR-34）：`--worktree` 在新的 worktree 和分支里工作，`--here` 在项目目录本身；都不给时按节点的 `worktree.default` 先试 worktree，项目开不了（`not_worktree_ready`）就退回项目目录并在 stderr 提示一行。`--base` / `--branch` 隐含 `--worktree`；对 scratch 或不能开 worktree 的项目给 `--worktree` 直接报错（退出码 2）。跟随输出时，worktree 建好会打印一行 `worktree: anycon/x (base main)`。`--model` / `--thinking` / `--mode` 的值是 `anycon agents` 列出的 id，不做翻译；`-o` 传 agent 特定选项 |
| `anycon send <session> ["<text>"] [--image <path>]... [--no-wait]` | 续发。`--image` 附图（可重复，每条消息至多 20 张；带图时文字可省）：本地先检查文件头，原样上传，不缩放、不去 EXIF（含 GPS 位置）；长边超过 2000 px 或大于约 1.5 MiB 时在 stderr 提醒。`<session>` 支持 id 前缀和 `last`（最近一个会话）。会话已归档时报错并提示 `anycon unarchive <session>`（退出码 2） |
| `anycon attach <session>` | 跟随事件流，直到 `Ctrl+C`（只断开，不取消）。权限请求在终端交互式回答，按键按 `options[]` 生成（常见 `[a]llow / [s]ession / [d]eny`），`question` 类逐题提问；`--auto-approve` 自动答复（危险，**启动时**就在 stderr 警告，不是等到第一次自动答复）：只自动批准 `tool` / `other` 类请求的 **allow_once** 选项，没有 allow_once 就拒绝，绝不升级成 allow_always；`mode_change` / `plan` / `question` 不自动答复（交互终端里照常提示，非交互时留着等 `anycon approve` 或别的设备）。`new` / `send` 同理。**来自 agent 的文本（命令、路径、URL、标题、计划、问题、选项标签、转写行）一律先「可见化」**：控制字符、ANSI 转义、双向 / 零宽字符显示成转义序列，不能改写终端上已显示的内容；默认展示全文，超过 2000 字符才截断并注明 `…(+N chars; press v to view all)`，按 `v` 打印全文而不作答——被提示注入的 agent 不能把危险部分藏在截断之后或用 `\r` 盖掉 |
| `anycon cancel <session>` | 中断当前 turn |
| `anycon approve <request-id> [--session]` · `anycon deny <request-id> [-m "<message>"]` | 回答一条待审批请求，`anycon pending` 列出所有待审批。`approve`（不带 `--session`）选 `allow_once`；请求里没有 allow_once 选项时报错（退出码 2），不会悄悄换成 allow_always |
| `anycon history <session> [--since <seq>] [--json]` | 打印历史 |
| `anycon rename <session> <title>` · `archive [--keep-worktree \| --remove-worktree]` · `delete [--keep-worktree \| --remove-worktree] [--delete-branch]` · `unarchive` | 处置方式缺省 `auto`（ADR-34）。`--remove-worktree`（丢弃未提交的改动）和 `--delete-branch`（丢弃分支上没合并的提交）都要确认：交互终端里先问（`--yes` 跳过），非交互时必须带 `--yes`。这几个命令和 `sync`、`worktrees` 的超时是 150 秒 |
| `anycon diff <session> [--uncommitted] [--stat] [<path>...]` | `session.diff`：默认打印相对 base 的完整 unified diff（逐个文件取 patch，超过 50 个文件时只打印 `--stat` 并提示用路径过滤）；`--stat` 先打印 `session.git_status` 的一行（分支 ← base、ahead / behind、未提交、冲突）再列文件和 `+N −M`；在项目目录本身工作的会话自动用 `--uncommitted`。输出里来自仓库的文本（路径、diff 内容）照样「可见化」控制字符 |
| `anycon sync <session> [--abort]` | `session.sync_base`：打印结果（已是最新 / 合并了 N 个提交 / 冲突文件列表）；冲突时退出码 1 |
| `anycon worktrees [--project <p>]` · `anycon worktrees remove <id> [--force] [--delete-branch] [--yes]` | `workspace.worktrees` / `workspace.remove_worktree`：表格列 ID、PROJECT、BRANCH、SESSION、STATE、UNCOMMITTED、AHEAD、SIZE。`--force`（丢弃未提交的改动）和 `--delete-branch` 都要确认，规则同 `delete` |
| `anycon search "<query>" [--project]` | daemon 本地 FTS |
| `anycon promote <session> <project-name>` | scratch 转项目 |

### 3.6 数据与审计（本机管理）

| 命令 | 说明 |
|---|---|
| `anycon export [--out <file>] [--project] [--since]` | 导出会话历史为一个 zip（JSONL + 附件），用于备份和搬迁（v0.2） |
| `anycon import <file>` | 导入（v0.2） |
| `anycon audit [--since] [--device] [--json]` | 本地审计日志 |
| `anycon config get <key>` · `set <key> <value>` · `edit` · `path` | 读写 `config.toml`；`set` 后对支持热加载的键立即生效，否则提示重启 |
| `anycon logs [-f] [-n 200]` | daemon 日志 |
| `anycon stats` | 运行时长、会话数、磁盘占用、hub 连接状态 |

## 4. 输出约定

- 默认人类可读；`--json` 输出机器可读。流式命令（`attach`、`new`、`clone`）的 `--json` 是 NDJSON，每行一个事件，和 `session.event` 同结构
- 颜色自动检测 TTY：stdout 和 stderr 各自按自己是不是 TTY 决定，`NO_COLOR` 和 `--no-color` 关闭
- `--json` 覆盖所有命令，包括 `config path` / `config edit` / `logs`（NDJSON，`{"line": …}`）
- 全局 `--timeout <秒>`：每个 RPC 的超时（默认 30 秒；`agents --refresh`、`doctor` 放宽到 150 秒）
- 错误到 stderr，格式 `error: <code>: <message>`，`code` 和协议错误码一致
- 退出码：0 成功；1 一般错误（含协议版本不兼容 `protocol_mismatch`、配额 `quota`、`config set` 的 I/O 错误或锁超时、远程传输失步「lost sync; run the command again」）；2 参数错误（含 `invalid` / `invalid_settings`、`config set` 的非法输入、`--short` 与 `--long` 同时给）；3 节点离线；4 未配对 / 未初始化（含 IK 握手无应答但 hub 说节点在线——疑似被吊销，以及公钥不符）；5 权限被拒（`forbidden`，`auto` / 危险模式被节点禁止；账号内接入的码 / 口令不对、请求被拒绝、方式没开启、退避中、口令解锁已锁定等）；130 被 `Ctrl+C` 中断
- 远程传输的背压：CLI 和 daemon / web 一样保留最近发出的信封、收到 `backpressure` 后暂停并按升序原样重发、`rate_limited` 整条暂停（[protocol.md § 3.1](../protocol.md#31-消息大小)）；被退回的信封已不在缓冲里时不原地重握手，直接以退出码 1 结束并提示重跑（CLI 的上行很小，几乎不会发生；也没有「窗口满了暂停」）。`logs -f` 不重复输出、不输出半行、轮转时先把旧文件读完再切换
- 终端输出的安全：**所有来自 agent 和会话的文本**（流式回复、思考、工具输出、用户消息、搜索片段、标题）在写 stdout 前都经过 `safe_text`：ESC、BEL、BS、DEL、其它 C0 / C1（含单字节 CSI、OSC）、单独的 CR、双向覆盖 / 隔离控制符都显示成 `\x1b` 这样的字面转义，只保留 `\n` `\t`（`\r\n` 当 `\n`）；管道（非 TTY）也一样过滤，要原始内容用 `--json`。代价是带颜色的工具输出会显示成 `\x1b[31m…`。审批提示除 2000 字符上限外还有**整条共用的 40 行预算**（标题先占，每个字段至少 3 行），`question` 最多列 30 个选项，超过都注明 `+N lines / choices; press v to view all`
- 连上远程节点后先用 `node.info` 检查协议版本，与本机相差超过一个 minor 报 `protocol_mismatch`；`history` 和 `attach` 补历史的翻页循环有进度保护（一页没带来新事件就报错退出）
- `attach` 的渲染：文本直接输出；工具调用一行灰字 `▸ bash: cargo test`；思考过程默认不显示，`--show-thinking` 开；权限请求高亮并等待按键；turn 结束打印一行用量

## 5. `anycon doctor`

逐项检查并给出修复建议，任一失败退出码非 0（没有注册服务、在前台跑 `anycon daemon` 的机器上，服务那一项也算失败）：

| 检查 | 失败时的提示 |
|---|---|
| `config.toml` 存在且合法 | 运行 `anycon init` |
| 密钥文件存在、权限 0600 | 修复权限的命令 |
| hub 可达（`GET /healthz`）、版本兼容 | 网络 / 反代 / 版本 |
| 设备 token 有效（`control.auth` 成功） | 设备可能已被吊销，重新 `anycon init --force` 后配对 |
| daemon 进程在跑、socket 可连 | `anycon service start` |
| workspace 根目录可写、`.trash` `.scratch` 存在 | |
| 每个 adapter 的探测结果（见 [daemon.md § 5](daemon.md#5-agent-探测与能力声明)） | 未安装 / 未登录 / 版本过低各自的命令 |
| 登录 shell 的 `PATH` 能找到这些 agent | 说明 launchd 环境和终端不同，建议在 `config.toml` 写绝对路径 |
| launchd / systemd 服务已注册且开机自启 | `anycon service install` |
| 系统睡眠策略（macOS：`pmset` 的 `sleep` 是否为 0 或开了 Wake for network access） | 提示机器睡着了就收不到消息 |
| 节点口令（ADR-36）：是否设置、是否被锁定、连续失败次数 | 被锁定：先看 `anycon audit` 里的 `enroll.failed`，确认是自己输错的再 `anycon passwd --enable`；没设口令只是提示（可选），不算失败 |
| 时钟偏差 < 60 秒 | Noise 不依赖时钟，但短码 TTL 和日志依赖 |
| 附件（`blobs`，ADR-35）：文件数、字节数、进行中的上传；数据目录所在卷剩余空间 ≥ 1 GiB | 磁盘快满了：新的图片上传会被拒绝（`disk_full`），清理磁盘或删掉不用的会话 |

`--json` 输出每项的 `{ id, ok, detail, fix }`，Web 的节点设置页「关于」标签也调用同一个 RPC（`node.doctor`）。

## 6. Shell 补全与手册

`anycon completions <bash|zsh|fish>`；Homebrew 安装时自动装。`anycon help <cmd>` 带示例。

## 7. 不做的

- 不做交互式 TUI（全屏界面）。`attach` 是流式打印，不是终端模拟器（[ADR-1](../decisions.md#adr-1-定位连接层不是开发环境)）
- 不做 `anycon shell` 之类远程执行任意命令的功能。想执行命令，让 agent 做，经过权限审批
- 不在 CLI 里常驻账号登录。CLI 是设备，不是账号：只在账号内接入时临时登录一次，用完就退出（§ 3.3）。账号管理（登录方式、设备列表、邀请）和批准别的设备的接入请求都在 Web UI 里做
