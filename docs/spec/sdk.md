# SDK 功能规格

> Python（`anycon`）和 TypeScript（`@anycon/sdk`）两个 SDK，各有两个方向：**客户端方向**（调用节点）和**节点方向**（把自己的 agent 注册为节点）。两个 SDK 的 API 形状保持一致，只按语言习惯调整命名。

## 1. 分层

```
packages/protocol · sdk/python/anycon/_protocol   类型 + JSON-RPC 编解码 + Noise wasm/wheel。无网络、无状态
        ▲
@anycon/sdk · anycon                              连接管理、重连、Promise/async API、事件流
```

Web UI 只用上面那层（[ADR-13](../decisions.md#adr-13-web-ui-不依赖-ts-sdk)）。

## 2. 身份与配对

SDK 是设备。首次使用要配对一次，之后密钥和 device token 存在本地：

```python
# 一次性：在 Mac mini 上 `anycon pair` 拿到短码
await anycon.pair("https://hub.example.com", code="7K3P-Q2MX", name="ci-runner")
# 之后
async with anycon.connect("https://hub.example.com", node="mac-mini") as node: ...
```

- 存储位置：`~/.anycon/sdk/<hub-host>/<profile>/`，`profile` 默认 `default`，环境变量 `ANYCON_PROFILE` 可切（`profile` 只允许 `[A-Za-z0-9][A-Za-z0-9._-]{0,63}` 且不含 `..`，不合法就抛 `invalid_profile`，不改写；`ANYCON_PROFILE` / `ANYCON_HOME` 为空串都当作未设置）。CI 里可以用 `ANYCON_DEVICE_KEY` / `ANYCON_DEVICE_TOKEN` 环境变量直接注入，不落盘
- `code` 接受 16 位或 8 位短码（`anycon pair [--short]`），也接受整条配对链接（取 `#code=` 后面的）。短码只在本地用来算 `match_id` 和 PSK（一次 Argon2id，`derivePairSecrets`，Node / 浏览器 wasm 里约 0.1–1 s），不发给 hub
- 配对完成的判据是 daemon 经加密传输发来的 `pair.ack`，不是 hub 的 `pair_done`：`pair()` 校验 `peer_id` 是自己的正式 id、`node_id` 等于 `pair_done.peer`、`fingerprint` 等于握手得到的公钥指纹，通过才把节点写进存储；30 秒没收到抛 `PairingFailed`，本地已有同 id 但公钥不同的节点时抛 `NodeKeyChanged`（带已存的和新出示的公钥指纹）、旧记录原样保留，用户确认后用 `forgetNode(hubUrl, nameOrIdPrefix)` 忘掉旧记录再配对；同 id 同公钥（吊销后重新配对）允许。节点名取 `pair.ack` 里的（去掉控制字符、限长 64）
- 本地的 device token 被 hub 判为 `invalid` / `revoked`（被吊销、hub 丢了数据）时，`pair()` 丢掉旧 token 和设备 id，用临时连接重新配对，而不是抛 `NodeRejected` 卡住
- `node=` 接受节点名或 id 前缀；只配对了一个节点时可省略
- 一个 SDK 进程可同时连多个节点，每个节点一条独立的 Noise 会话

## 3. 客户端方向

### 3.1 Python

```python
import anycon

async with anycon.connect(hub, node="mac-mini") as node:
    info     = await node.info()                      # node.info
    agents   = await node.agents.list()               # agent.list → [AgentInfo]
    projects = await node.projects.list()
    job      = await node.projects.clone("git@github.com:me/app.git")
    linked   = await node.projects.link("~/work/app")    # workspace.link；browse() / unlink() 同理
    async for p in job.progress(): ...
    await job

    session = await node.sessions.create(
        project="app", agent="claude-code",
        settings={"model": "opus", "thinking": "high", "mode": "acceptEdits"},   # 值来自 agents.list()
    )

    # 发送并消费事件流；turn 结束时迭代结束
    async for ev in session.send("跑一下测试，把失败的修了"):
        match ev:
            case anycon.TextDelta(text):           print(text, end="")
            case anycon.ToolCall(name, input):     ...
            case anycon.PermissionRequest() as req:
                await req.allow()                  # 或 req.deny("别删文件") / req.allow_session()
            case anycon.TurnDone(usage):           print(usage)

    # 不想手动处理权限：
    session = await node.sessions.create(..., on_permission=anycon.policies.allow_reads_deny_rest)
    result = await session.run("把 README 翻译成英文")   # 发送 + 收完 + 返回最终文本

    ref = await node.upload_image(png_bytes, name="shot.png")    # blob.* 分块上传 → AttachmentRef（见 § 3.2「附件」；任意文件用 upload_file）
    async for ev in session.send("这张图里是什么？", attachments=[ref]): ...

    await session.set_settings(model="sonnet")          # 下一 turn 生效
    await session.set_settings(clear=["thinking", "max_turns"])   # 清除（恢复为未设置）要在 clear 里点名，缺省 / None 是「保持」
    await session.cancel()
    history = await session.history(after_seq=0)
    await node.sessions.get("sess_…")                   # 重新拿到已有会话
```

- `send()` 返回异步迭代器，迭代到 `TurnDone` / `Error` 结束；中途 `break` 不取消 turn，只是不再消费（事件仍被 daemon 持久化）。已归档的会话 `send()` 抛 `invalid`（`reason = "archived"`），先 `unarchive()`；会话在运行或等审批时 `archive()` 抛 `busy`
- `run()` 是便捷封装：发送、按 `on_permission` 策略自动答复、返回拼接的文本和 usage
- `on_permission`：协程 `(req) -> Decision`，没给时权限请求作为事件抛给调用方；两者都没处理的请求在 `permission_timeout`（默认 10 分钟）后由 SDK 代答 `deny`，避免 agent 永远挂着
- 节点离线：`connect()` 成功但 `node.online` 为 False；调用会抛 `NodeOffline`。`wait_online(timeout)` 等上线
- 多个客户端同时在线时，别的设备答复的权限请求以 `PermissionResolved` 事件出现
- 同步包装器 `anycon.sync.connect(...)` 后补，内部跑一个事件循环线程

### 3.2 TypeScript

```ts
import { connect } from "@anycon/sdk";

const node = await connect(hub, { node: "mac-mini" });
const session = await node.sessions.create({ project: "app", agent: "claude-code",
                                             settings: { model: "opus" } });
for await (const ev of session.send("跑测试")) {
  if (ev.type === "text_delta") process.stdout.write(ev.text);
  if (ev.type === "permission") await ev.allow();
}
await node.close();
```

Node 用 `ws`，浏览器用原生 WebSocket，同构；事件类型从 `packages/protocol` 导出，和 Web UI 共用。

**权限的便捷路径不会悄悄扩大授权**：事件上的 `ev.allow()` 只选 `allow_once`，没有这个选项时抛 `no_matching_option`；`ev.allowSession()` 没有 `allow_always` 时退回 `allow_once`（只会收窄）。内置策略 `policies.allowAll` 只放行 `tool` / `other` 类请求，**不放行 `mode_change` 和 `plan`，也不代答 `question`**；需要时显式用 `policies.allowAllWith({ modeChange?, plan? })`。策略回调抛异常、或策略要的决定无法执行时，`Node` 发 `permission_error(error, request)` 事件，请求仍按超时 deny，不会被吞掉。

**事件**：`Node` 是 `EventEmitter` 风格，除 `online` / `offline` / `permission` / `session_changed` 外有两个报问题的事件：`permission_error`（上面）和 `error`（非致命的通道问题，payload 是 `AnyconError`）。hub 的 `dropped { seq, reason }` 每连接每秒至多一条，是「发错东西」的信号，**不据此逐条重发**：`error` 事件收到 `envelope_too_large`（分片之后仍超限，说明 hub 的 `ANYCON_MAX_ENVELOPE_BYTES` 比一片还小）时进行中的请求失败、通道重新握手；`not_paired` 变成 `NodeRejected`。

**背压与速率**（[protocol.md § 3.1](../protocol.md#31-消息大小)）：SDK 把最近发出的 `data` 信封原文留在一个**在途窗口**里（默认 16 MiB，`resendBufferBytes` 可调）：发出不满 10 s（`resendRetainMs`）的和等待重发的永不逐出，窗口满了就暂停发新的、等最老一批过了 10 s 再继续，不逐出。收到 `backpressure { to, seq }` 只暂停那个节点（新请求排队），等 `retry_after_ms`（最近一条 `rate_limited` 给的，封顶 30 s；没有就 100–300 ms 加抖动，连续几轮翻倍到 ×8），然后把所有被退回的 `seq` 按升序**原样**重发，再放行排队的请求。`rate_limited` 是「整条 hub 连接暂停多久」的提示，所有节点共享（之后才连上的节点也要等；取较大值，重连清零）：不让请求失败、不重新握手、不发 `error`。被退回的 `seq` 已不在窗口里 → 丢传输、重新 IK（SDK 不发 RESET 占位帧，hub 转发新的 `handshake` 时会清掉粘住集合）；旧传输的 `backpressure` 被忽略。握手本身被退回时不重发也不重置，按同样的退避（300–1000 ms 起、翻倍、封顶 30 s，不早于 `rate_limited` 过期）延后再发起，这一次不占握手重试次数。

**返回类型以方法注册表为准**：`projects.rename()` 返回 `RenamedProject`（`{ from, to }`）；`session.rename / archive / unarchive / restart / promote` 返回空对象；`archive` / `delete` 可带 worktree 处置（见下面「worktree 与 git」）。

**worktree 与 git**（ADR-34，[daemon.md § 10.1](daemon.md#101-每会话-worktree)）：

```ts
const s = await node.sessions.create({ project: "app", agent: "claude-code", prompt: "fix the login redirect",
                                       workspace: { mode: "worktree", base: "origin/main" } });
s.worktree;                                   // { id, branch, base, base_sha, path, state }，state 起初是 "creating"
await s.gitStatus({ includePr: true });       // session.git_status
await s.diff();                               // { from_sha, files }；s.diff({ against: "uncommitted", path: "src/a.ts" }) 带 patch
await s.syncBase();                           // { outcome, commits, conflicts }；s.syncBase({ abort: true })
await s.archive({ worktree: "auto" });        // auto | keep | remove；s.delete({ worktree: "remove", deleteBranch: true })
await node.projects.branches("app");          // workspace.branches
await node.worktrees.list({ project: "app" }); // workspace.worktrees；node.worktrees.remove(id, { force, deleteBranch })
```

`sessions.create` 不带 `workspace` 就是在项目目录本身工作（线路缺省 `local`），SDK 不替调用方读节点的 `worktree.default`。`create` 现在也接受 `prompt`（创建后立即发第一条消息）：`message_id` 由 SDK 生成 uuid v4，也可以用 `messageId` 自己给——它是 `create` 的幂等键，超时重试时传同一个，daemon 返回第一次创建的会话；空串 `prompt` 当作没有。`create({ prompt })` 不替调用方收第一个 turn：用 `s.events({ fromSeq: 0 })` 从头看（补历史期间到达的实时事件先扣住，补完按 seq 合并，消费者看到的 seq 严格递增、不重复）。`node.worktrees.list()` 直接返回 `WorktreeInfo[]`（和 `projects.list()` 一样解包）。`Session.worktree` 随 `session.changed` 更新，通知里不带 `worktree` 时保持原值。Python 的 API 形状相同（`workspace={"mode": "worktree"}`、`git_status(include_pr=True)`…），随 Python SDK 一起实现。

**附件**（[ADR-35](../decisions.md#adr-35-附件经-blob-通道传到-daemon消息只带引用)，[protocol.md § 5.6](../protocol.md#56-附件--blobadr-35)）：图片或任意文件先经 blob 通道传到 daemon，消息只带引用。图片作为图片交给模型；其它文件以 daemon 上的路径交给 agent，由 agent 用自己的工具读（Claude Code 读它可能要审批）。

```ts
const ref = await node.uploadImage(bytes, { name: 'shot.png' })          // → AttachmentRef { blob_id, name, mime, bytes, width?, height? }
const doc = await node.uploadFile(pdfBytes, { name: 'spec.pdf' })         // 任意文件；不是图片的没有 width / height
if (acceptsAttachment(agentInfo, doc.mime)) { … }                         // agent 点名了这个 mime 或者有 `*/*`
for await (const ev of session.send('这张图里是什么？', { attachments: [ref] })) { … }   // 有附件时文字可以是空串
await node.sessions.create({ project: 'app', agent: 'claude-code', attachments: [ref] })   // 第一条消息只有图也行
const { ref: meta, bytes: data } = await node.downloadAttachment(userMessage.attachments[0])   // 历史里的图（blob.get 分页）
await node.discardAttachment(ref)                                         // 传了但不发（不丢的话 daemon 24 小时后回收）
```

- `uploadFile(bytes, { name, mime?, signal?, onProgress?, requestTimeoutMs? })`：任意文件。`mime` 缺省时先按文件头认四种图片，再按扩展名查一张常见类型的小表（`pdf` `txt` `md` `csv` `json` `html` `yaml` `zip` `gz` 和常见源码等；四种图片的扩展名不在表里——图片只认文件头），都不中就是 `application/octet-stream`。给了 `mime` 时先检查格式（`type/subtype`、RFC 6838 的字符、不带参数、≤ 127 字节，转小写），不对抛 `AnyconError('invalid_mime')`、什么都不发。从不抛 `unsupported_image`。上传流程和 `uploadImage` 相同，两者共用一条队列。
- `uploadImage(bytes, { name, mime?, signal?, onProgress?, requestTimeoutMs? })`：`blob.put_begin` → 按 daemon 给的 `chunk_size`（512 KiB）切块、**最多 2 块在途**的 `blob.put_chunk` → `blob.put_end`，返回 daemon 的记录（宽高由 daemon 读文件头得出），原样放进 `send` / `create`。同一个 `Node` 上的上传**排队、一次一张**（ADR-35「2026-10 修订」：每节点 1 张在上传、2 块在途，「允许」「停止」最多排在约 1.4 MB 后面）。超时（每块默认 90 秒）/ 掉线的块各自重试 2 次（掉线时先等节点重新上线），daemon 的明确答复不重试（块答 `finalizing` 说明 `put_end` 已经接手，SDK 不再发块、直接 `put_end` 拿结果）；失败、或 `signal` 中止时尽力 `blob.discard`。`onProgress(sent, total)` 从 `(0, total)` 开始、以 `(total, total)` 结束。
- **SDK 不缩放、不重编码图片**（Node 没有通用的图片编解码，浏览器的规整规则在 Web 里）：调用方自己把图控制在**长边 ≤ 2000 px、单张 ≤ 约 1.5 MiB**。硬上限是 daemon 的每张 10 MiB（`sessions.max_attachment_bytes`）、每条消息至多 20 张且合计 ≤ 20 MiB；Claude 单张 base64 后约 5 MB、一次请求超过 20 张时每张长边 ≤ 2000 px，而且图一旦进了 Claude Code 的会话历史，之后每个 turn 都会带着它重发——大图很快会让会话超过请求体上限。
- daemon 是唯一的校验点：`uploadImage` 在 `mime` 缺省时按文件头（magic bytes）判断类型，认不出、或者给的 `mime` 不是四种图片之一，抛 `AnyconError('unsupported_image')`、什么都不发（daemon 现在什么类型都收，不在本地拦的话一张 BMP 会悄悄变成普通文件）；名字、大小、SHA-256、图片的文件头都由 daemon 判。agent 收不收看 `AgentInfo.capabilities.attachments`（点名了这个 mime，或者有 `*/*`；`acceptsAttachment()` 就是这条规则），不收 → `RpcError`（`unsupported`）。
- 附件被拒抛 **`AttachmentError`**（`RpcError` 的子类，`code` 不变，`reason` 收窄成 `AttachmentErrorReason`）：`too_large`（`invalid`：单张或每条消息合计超限）、`quota`（`busy`：没被引用的 blob 全节点超过 1 GiB——先发出去或 `discardAttachment()`）、`disk_full`（`busy`：daemon 的数据盘快满了）、`blob`（`not_found`：引用的 blob 已被回收 / 丢掉——重新上传）、`blob_in_use`（`invalid`：已经用在**别的**会话里，一张图只属于一个会话）、`in_use`（`invalid`：`discardAttachment` 一个已被引用的）、`finalizing` / `incomplete` / `hash_mismatch` / `mime_mismatch`（`invalid`：`put_end` 的收尾和校验）。没有 `reason` 的 `busy`（这台设备同时进行的上传超过 8 个）、`unsupported` 仍是普通 `RpcError`。
- `send` 的 `messageId` 幂等：重发同一条消息时附件也要一样（否则 `invalid`）；`create` 带 `messageId` 的重试先按幂等返回第一次建好的会话，不会因为附件已经绑定而得到 `blob_in_use`。

**清除设置**：`session.setSettings(settings, { clear? })`。`settings` 是增量（缺省的字段保持），要把某个值清掉（恢复为未设置 / agent 默认）在 `clear` 里点名：`['model', 'thinking', 'mode', 'env', 'system_prompt_append', 'max_turns', 'max_budget_usd']`，见 [daemon.md § 6](daemon.md#6-会话选项)。

## 4. 节点方向（自定义 agent）

SDK 进程本身就是一个 daemon 节点：对 hub 来说是一台 `kind = sdk` 的设备，对客户端来说实现了内层协议的 daemon 侧子集。

```python
import anycon

@anycon.agent(name="my-bot", options=[
    anycon.Option.select("model", choices=["small", "large"], default="small", live=True),
])
async def my_bot(session: anycon.AgentSession, prompt: str):
    yield anycon.Thinking("想一下…")                   # → thinking_delta
    yield "你说的是：" + prompt                         # → text_delta
    ok = await session.ask_permission(kind="shell", detail={"command": "ls"})
    if ok:
        yield anycon.ToolCall("ls", {"cwd": session.cwd}, category="shell")
        yield anycon.ToolResult(output="…")
    yield anycon.Done(usage={"input_tokens": 10, "output_tokens": 20})

anycon.serve(hub="https://hub.example.com", name="bot-host", workspace="~/bots")
```

- `serve()` 首次运行打印短码（和 `anycon pair` 一样），之后用存下来的密钥
- 一个进程可注册多个 `@agent`，`agent.list` 返回全部
- `workspace` 可选。给了就和 daemon 一样有项目模型（文件夹即项目）；不给则 `node.info.capabilities.workspace = false`，客户端只能开 scratch 会话
- 会话历史由 SDK 存在 `~/.anycon/sdk/.../sessions.db`（SQLite），实现 `session.list` / `history`，客户端体验和 daemon 一致
- `session.ask_permission()` 阻塞直到任一客户端答复
- `session.messages()`：多 turn 时拿到后续用户输入（async iterator），不实现则每条消息新起一次函数调用
- 并发：每个会话一个 task，上限 `max_concurrent`，默认 4
- TS 版节点方向 v0.3

## 5. 错误

| 异常 | 含义 |
|---|---|
| `PairingRequired` | 本地没有该 hub 的设备身份 |
| `PairingFailed(reason)` | 配对失败：短码不存在 / 过期、握手失败、`pair.ack` 超时或与 hub 告知的不一致（不保存任何东西）、与本地已有节点的公钥冲突 |
| `NodeOffline` | 节点不在线 |
| `NodeRejected` | Noise 握手失败，设备被吊销或 daemon 重装 |
| `NodeKeyChanged` | 配对时发现本地已有同 id 节点但公钥不同（daemon 重装、或有人冒充）：不覆盖，`forgetNode` 之后再配对 |
| `AgentUnavailable(reason)` | agent 未安装 / 未登录 / 被禁用 |
| `SettingsInvalid(field, reason)` | 模型 / 思考 / 模式 / 选项不被该 agent 接受 |
| `NodeBusy` | 并发上限 |
| `ProtocolMismatch(ours, theirs)` | 版本不兼容 |
| `AttachmentError(code, reason)` | 附件被 daemon 拒绝（`blob.*`，或 `send` / `create` 核对附件），`RpcError` 的子类：`reason` 是 `too_large` / `quota` / `disk_full` / `blob` / `blob_in_use` / `in_use` / `finalizing` / `incomplete` / `hash_mismatch` / `mime_mismatch`，见 § 3.2「图片附件」 |
| `RpcError(code, message)` | 其他协议错误，`code` 与 protocol.md 错误码一致 |

## 6. 发布

- Python：`anycon` 纯 Python 包依赖 `anycon-crypto`（maturin abi3 wheel），`requires-python >= 3.12`。**v0.1 没有 Python 代码**：`sdk/python/` 里只有一个 `README.md` 和一个 `pyproject.toml` 占位（声明包名、版本和「推迟」状态），实现和类型生成在 v0.1.x，见 [roadmap](../roadmap.md)。本文的 Python 代码块是 API 设计，不是现有功能
- TS：`@anycon/protocol`、`@anycon/sdk`，Node ≥ 20，bun 和 Node 两个运行时都跑测试。**v0.1 只发 ESM**，CJS 后补
- 版本号跟随协议 minor；兼容矩阵见 [protocol.md § 9](../protocol.md#9-版本与兼容)
