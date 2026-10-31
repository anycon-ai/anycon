# @anycon/sdk

[English](https://github.com/anycon-ai/anycon/blob/main/packages/sdk/README.md)

anycon 的 TypeScript SDK，**客户端方向**：配对一次，之后连接节点（daemon）、管理项目和会话、收事件流、处理权限请求。
Node ≥ 20 与浏览器同构（Node 用 `ws`，浏览器用原生 WebSocket）。ESM。功能规格见 [docs/spec/sdk.md](../../docs/spec/sdk.md)。

```ts
import { pair, connect, policies } from '@anycon/sdk'

// 一次性：在节点上 `anycon pair` 拿到短码（默认 16 位；`--short` 是 8 位，只适合自建 hub）；也可以直接给配对链接
await pair('https://hub.example.com', { code: '7K3P-Q2MX-W4NH-8CRD', name: 'ci-runner' })
// await pair(hub, { code: 'https://hub.example.com/pair#code=7K3P-Q2MX-W4NH-8CRD', name: 'ci-runner' })

const node = await connect('https://hub.example.com', { node: 'mac-mini' })   // 只配对了一个节点时可省略
const agents = await node.agents.list()                       // 模型 / 思考 / 权限模式的取值来自这里
const session = await node.sessions.create({
  project: 'app', agent: 'claude-code',
  settings: { model: 'opus', thinking: 'high', mode: 'acceptEdits' },
})

for await (const ev of session.send('跑一下测试，把失败的修了')) {
  if (ev.type === 'text_delta') process.stdout.write(ev.text)
  if (ev.type === 'permission') await ev.allow()               // 或 ev.deny('别删文件') / ev.allowSession()
  if (ev.type === 'turn_done') console.log(ev.usage)
}

// 不想手动处理权限：策略回调（对 SDK 自己在用的会话生效）
const node2 = await connect(hub, { onPermission: policies.allowReadsDenyRest })
const result = await session.run('把 README 翻译成英文')        // { text, usage, reason, events }

await session.setSettings({ model: 'sonnet' })                // 下一 turn 生效；settings 里缺省的字段 = 保持
await session.setSettings({}, { clear: ['thinking', 'max_turns'] })   // 清除（恢复为未设置）要用 clear
await session.cancel()
const { events } = await session.history({ afterSeq: 0 })
await node.close()
```

## 每会话 worktree 与 git（ADR-34）

会话可以在 daemon 新建的 git worktree（和分支）里工作，不碰项目主目录。`sessions.create` 不带 `workspace` 就是在项目目录本身工作——SDK **不替调用方读节点的 `worktree.default`**，要 worktree 就显式带：

```ts
const branches = await node.projects.branches('app')         // { current, default_base, local, remote, truncated? }
const s = await node.sessions.create({
  project: 'app', agent: 'claude-code',
  workspace: { mode: 'worktree', base: branches.default_base },   // 还可以 branch: 'feature/login'；缺省由 daemon 起名
  prompt: 'fix the login redirect',                          // 可选：创建后立即发第一条消息（SDK 生成 uuid v4 的 message_id）
})
s.worktree      // { id, branch, base, base_sha, path, state }；state 起初 'creating'，建好后 'active'，随 session.changed 更新
for await (const ev of s.events({ fromSeq: 0 })) {           // 从头看第一个 turn（含 { type: 'git', action: 'worktree_created', … } 事件）
  if (ev.type === 'git') console.log(ev.action, ev.branch)
  if (ev.type === 'turn_done') break
}

const st = await s.gitStatus({ includePr: true })            // 分支 / HEAD / ahead / behind / uncommitted / conflicts / merging / upstream；includePr 时 pr 或 pr_error
const list = await s.diff()                                  // { from_sha, files: [{ path, status, additions, deletions, untracked? }], truncated? }
const one = await s.diff({ against: 'uncommitted', path: 'src/a.ts' })   // against: 'base'（缺省，worktree 会话）| 'uncommitted'；带 path 才有 patch
const sync = await s.syncBase()                              // { outcome: 'up_to_date' | 'merged' | 'conflicts' | 'aborted', commits, conflicts? }
if (sync.outcome === 'conflicts') await s.syncBase({ abort: true })

await s.archive({ worktree: 'auto' })                        // 'auto'（缺省：干净就删，有未提交改动保留）| 'keep' | 'remove'（丢弃未提交的改动）
await s.delete({ worktree: 'remove', deleteBranch: true })   // deleteBranch：同时 git branch -D（没合并的提交会丢），只在 worktree 也被删时生效

await node.worktrees.list({ project: 'app' })                // WorktreeInfo[]：含已保留的、会话已删除的
await node.worktrees.remove(id, { force: true, deleteBranch: false })   // 会话没归档 → busy；有未提交改动而没 force → invalid（error.reason === 'uncommitted_changes'）
```

- 已归档的会话 `send()` / `syncBase()` 抛 `RpcError`（`invalid`，`reason === 'archived'`），先 `unarchive()`；会话在运行或等审批时 `archive()` 抛 `busy`（先 `cancel()` 或答复权限请求）。
- 不带参数的 `archive()` / `delete()` 和以前完全一样（只发 `session_id`）；没有 worktree 的会话忽略这些选项。
- `prompt` 时 `create()` 不替你收第一个 turn 的事件：创建后马上 `s.events({ fromSeq: 0 })`（补历史期间到的实时事件会扣住、补完再接上，seq 严格递增不重复）。权限策略也要等会话有进行中的 `events()` / `send()` 才会代答。`messageId` 可传自己的幂等键：`create()` 超时后重试传同一个，daemon 返回第一次创建的会话，不会再开第二个。
- 创建被拒的原因看 `RpcError.code` / `.reason`：项目不是 `worktree_ready` → `invalid` / `not_worktree_ready`，分支已存在 → `exists`，`base` 解析不了 → `invalid` / `bad_base`。`syncBase` 的失败：turn 在跑 → `busy`；有未提交改动 → `invalid` / `uncommitted_changes`；上次合并停在冲突里 → `invalid` / `merge_in_progress`；fetch / merge 失败 → `internal` / `fetch_failed` / `git_failed`。`gitStatus` 在会话不在 git 仓库里时 → `invalid` / `not_git`。

## 附件（ADR-35）

图片和其它文件先经 blob 通道传到节点（`blob.put_begin` → 512 KiB 一块、最多 2 块在途的 `blob.put_chunk` → `blob.put_end`），消息只带 daemon 返回的引用（`AttachmentRef`）：

```ts
import { readFile } from 'node:fs/promises'
import { AttachmentError, acceptsAttachment } from '@anycon/sdk'

const ref = await node.uploadImage(await readFile('shot.png'), {
  name: 'shot.png',                                          // mime 缺省按文件头判断（png / jpeg / gif / webp）
  onProgress: (sent, total) => console.log(`${sent}/${total}`),
})                                                           // → { blob_id, name, mime, bytes, width?, height? }
try {
  for await (const ev of session.send('这张图里是什么？', { attachments: [ref] })) {   // 文字可以是空串
    if (ev.type === 'user_message' && ev.attachments) {
      const { bytes } = await node.downloadAttachment(ev.attachments[0]!)   // 历史里的图（blob.get 分页拉回）
    }
  }
} catch (err) {
  if (err instanceof AttachmentError && err.reason === 'blob') { /* 图已被回收：重新 uploadImage 再发 */ }
  else throw err
}
await node.sessions.create({ project: 'app', agent: 'claude-code', attachments: [ref2] })   // 第一条消息只有图也行
await node.discardAttachment(ref3)                       // 传了但不发；不丢的话 daemon 24 小时后回收

// 任意文件：同一个通道、同一个队列
const doc = await node.uploadFile(await readFile('spec.pdf'), {
  name: 'spec.pdf',                                          // mime 缺省：先认图片文件头，再看扩展名，都不行是 application/octet-stream
})                                                           // → { blob_id, name, mime: 'application/pdf', bytes }
if (acceptsAttachment(agent, doc.mime)) {                    // agent：node.agents.list() 里的一项
  for await (const ev of session.send('总结一下这个文件', { attachments: [doc] })) { /* … */ }
}
```

- **图片和其它文件。** `image/png`、`image/jpeg`、`image/gif`、`image/webp` 作为图片交给模型（daemon 核对文件头、读宽高）；其它文件以 daemon 所在机器上的路径交给 agent，由 agent 用自己的工具读（Claude Code 可能先请求读取权限）。收任意文件的 agent 在 `capabilities.attachments` 里列出 `'*/*'`（`ATTACHMENT_ANY_FILE`）；`acceptsAttachment(agent, mime)` 同时认点名的类型和通配项。
- **`uploadFile(bytes, { name, mime? })`。** `mime` 缺省时先按文件头认四种图片，再查一张常见扩展名的小表（`pdf`、`txt`、`md`、`csv`、`json`、`html`、`xml`、`yaml`、`zip`、`gz`、`tar`、`js`、`ts`、`py`、`rs`、`go`……，见 `guessMime`），都不行用 `application/octet-stream`；扩展名是 `.png` 但内容不是 PNG 的不会被猜成图片。显式给的 `mime` 必须是成形的 `type/subtype`（不带参数），发出去前转小写，不成形抛 `AnyconError('invalid_mime')`，什么都不发；真正的校验在 daemon。`uploadImage` 仍然只传图片：给了别的 `mime`，或者没给而文件头认不出，抛 `AnyconError('unsupported_image')`。
- **SDK 不缩放、不重编码图片。** 调用方自己把图控制在**长边 ≤ 2000 px、单张 ≤ 约 1.5 MiB**（比如先用 `sharp` / ImageMagick 缩一下）。daemon 的硬上限是每个文件 10 MiB（`sessions.max_attachment_bytes`）、每条消息至多 20 个附件且合计 ≤ 20 MiB；而图一旦进了 Claude Code 的会话历史，之后每个 turn 都会带着它重发，大图会让会话很快超过 Claude 的请求体上限。同理，`node.uploadImage()` 原样上传字节：**不去掉 EXIF 元数据（包括 GPS 位置），也不按 EXIF 方向转正**，只有 Web UI 会在上传前重编码图片。在意隐私或方向的调用方，请在 `uploadImage` 之前自己处理（比如 `sharp(bytes).rotate().toBuffer()`：按 EXIF 转正，输出默认不带元数据）。
- agent 收不收某种文件看 `acceptsAttachment((await node.agents.list())[i], ref.mime)`；节点支不支持 blob 通道看 `(await node.info()).capabilities.blobs`。
- 同一个 `Node` 上的上传（图片和文件一样）**排队、一次一个**（`Promise.all` 一批也只有一个在传、2 块在途），免得「允许」「停止」之类的请求排在几 MB 的文件后面。超时（每块默认 90 秒，`requestTimeoutMs`）/ 掉线的块各自重试 2 次，daemon 的明确拒绝不重试；失败或 `signal` 中止时 SDK 尽力 `blob.discard`。
- 被拒抛 `AttachmentError`（`RpcError` 的子类，`.code` 照旧，`.reason` 收窄）：

  | `reason` | `code` | 什么时候 / 怎么办 |
  |---|---|---|
  | `too_large` | `invalid` | 单个文件超过节点上限，或一条消息合计超过 20 MiB：缩小 / 分几条发 |
  | `quota` | `busy` | 没被引用的附件全节点超过 1 GiB：先发出去，或 `discardAttachment()` |
  | `disk_full` | `busy` | 节点数据盘快满了 |
  | `blob` | `not_found` | 引用的附件已被回收（24 小时没用）或丢掉了：重新上传 |
  | `blob_in_use` | `invalid` | 这个附件已经用在别的会话里（一个附件只属于一个会话）：重新上传 |
  | `in_use` | `invalid` | `discardAttachment` 一个已被消息引用的附件（它随会话删除） |
  | `finalizing` / `incomplete` / `hash_mismatch` / `mime_mismatch` | `invalid` | 上传的收尾和校验没过（文件头和 `mime` 对不上、内容在路上坏了） |

  把附件发给不收这种类型的 agent → `RpcError`（`unsupported`）；这台设备同时进行的上传超过 8 个 → `RpcError`（`busy`，没有 `reason`）；`uploadImage` 给了非图片 `mime`、或 `mime` 缺省而文件头认不出 → `AnyconError('unsupported_image')`；`uploadFile` 给了不成形的 `mime` → `AnyconError('invalid_mime')`。这两种情况什么都不发。

## API 速览

- `pair(hubUrl, { code, name, storage?, profile?, headers?, ackTimeoutMs? })` — joiner。`code` 可以是 16 位（默认）或 8 位（`anycon pair --short`）短码（大小写、连字符随意），也可以是配对链接 `https://hub/pair#code=…` / 只有 `#code=…`（短码在 fragment 里，不会发给 hub）。流程：先在本地做**一次** `derivePairSecrets(code)`（Argon2id，64 MiB × 3 轮，Node 约 0.1–0.3 s、浏览器 wasm 约 0.1–1 s，同步占用线程；`match_id` 和 PSK 同出一次计算，用完 PSK 清零；所以配对测试里每次 `pair()` 要多算这么久，超时别设在 1 s 以下）→ 连 hub → `PairJoin { match_id }`（**hub 从头到尾看不到短码**）→ Noise XXpsk3（initiator，PSK 来自上面那次派生）→ `PairConfirm` → `PairDone`，然后**等 daemon 经新建立的 Noise 传输发来 `pair.ack`**（最多 `ackTimeoutMs`，默认 30 秒），校验 `peer_id` 是本设备的正式 id、`node_id == PairDone.peer`、`fingerprint` 等于握手得到的节点公钥的指纹，**全部通过才落盘**（设备身份 + 节点公钥）；超时 / 不符 → `PairingFailed`，什么都不留。节点名取自 `pair.ack`（已认证，控制字符被去掉）。握手第 3 条的 payload 带本设备的 `{ name, kind, platform, version }`，daemon 用它显示设备名。节点以 `PairDone.peer`（正式设备 id）为键。已有同 id 节点但公钥不同 → 抛 `NodeKeyChanged`，不覆盖（见下）。本地身份被 hub 作废（`auth_fail invalid | revoked` / close 4004）时 `pair()` 会清掉它并用全新身份重试。`headers` 见下面「账号归属」。
- `forgetNode(hubUrl, nodeNameOrIdPrefix, { storage?, profile? })` — 忘掉本地记录的某个节点；`NodeKeyChanged` 之后确认新公钥可信时用它，再 `pair()` 一次。
- `connect(hubUrl, { node?, storage?, onPermission?, headers?, … })` → `Node`。每次 WS 连上、收到 `peer_online`（或 `auth_ok.peers_online` 含该节点）都重新做 IK 握手；掉线指数退避重连（1s → 30s，hub 重启 `4002` 用 1–3 s 抖动）。
- 同时连多个节点：`const client = await Client.open(hubUrl)` + 多次 `client.node('name')`（hub 对单设备只允许一条连接，所以多节点共用一个 `Client`）。
- `Node`：`info()`、`agents.list({ refresh? })`（带 etag 缓存）、`projects.list/create/clone/link/unlink/browse/branches/rename/trash/trashList/restore/purge`（`link(path, { name? })` 把 daemon 所在机器上的已有目录关联为项目——不移动、不复制，路径要在 daemon 的 `workspace.link_roots` 之内；`unlink(name)` 只解除关联、不动目录；`browse({ path?, showHidden? })` 列 `link_roots` 之内的子目录供选择；关联项目的 `kind` 为 `linked`，目录没了时 `missing` 为 true）、`sessions.list/get/create/search`（`create` 接受 `workspace`、`prompt`、`attachments`、`messageId`）、`worktrees.list/remove`、`uploadImage(bytes, { name, mime?, signal?, onProgress?, requestTimeoutMs? })` / `uploadFile(bytes, { name, mime?, signal?, onProgress?, requestTimeoutMs? })` / `downloadAttachment(refOrBlobId)` / `discardAttachment(refOrBlobId)`（见上面「附件」）、`permissions.list`、`online` / `waitOnline()`、事件 `online` / `offline` / `permission` / `permission_error` / `error`（非致命的通道问题）/ `session_changed` …、`call(method, params)`（类型化）和 `rpc(method, params)`（原始）。
- `Session`：`send(text, { attachments?, messageId?, signal? })` → 可迭代事件流（**立即发送**，迭代到本 turn 的 `turn_done` 或不可恢复的 `error`；中途 `break` 不取消 turn）、`run(text)`、`respond(requestId, optionId | { answers }, message?)`、`setSettings(settings, { clear? })`、`cancel`、`history`、`events()`、`rename/archive/delete/restart`（`archive({ worktree? })`、`delete({ worktree?, deleteBranch? })`）、`worktree`（getter）、`gitStatus({ includePr? })`、`diff({ against?, path? })`、`syncBase({ abort? })`。`watch` / `unwatch` 和掉线后的补事件（`session.history`）是内部的。
- 权限：`permission` 事件上有 `allow()` / `allowSession()` / `deny(message?)` / `respond(optionId)` / `answer(answers)`；`onPermission` 策略返回 `'allow' | 'allow_session' | 'deny' | { option, message } | { answers } | undefined`。**只对 SDK 自己正在用的会话（有进行中的 `send()` / `events()`）代答**，不会替用户在 Web 上开的会话做决定；没人答复的请求 `permissionTimeoutMs`（默认 10 分钟）后由 SDK deny。
  - **不会悄悄升级成 `allow_always`**：`allow()` / `'allow'` 只选 `allow_once`；请求里没有 `allow_once` 时 `allow()` 报错 `no_matching_option`，策略的 `'allow'` 则发 `node.on('permission_error', (error, request) => …)` 并照常走超时 deny。要「本会话都允许」显式用 `allowSession()` / `'allow_session'`（没有 `allow_always` 选项时退回 `allow_once`，只会收窄）。
  - `policies.allowAll` **不放行 `mode_change` 和 `plan`**（切换权限模式 / 批准计划是提权），也不处理 `question`——这些留给调用方或超时 deny。要连它们一起放行必须显式：`policies.allowAllWith({ modeChange: true, plan: true })`。策略回调抛异常也会发 `permission_error`，不再被吞掉。
- 错误：`PairingRequired`、`PairingFailed(reason)`、`NodeKeyChanged`、`NodeOffline`、`NodeRejected`、`AgentUnavailable(reason)`、`SettingsInvalid(field, reason)`、`NodeBusy`、`ProtocolMismatch(ours, theirs)`、`AttachmentError(code, reason)`（附件被拒，`RpcError` 的子类）、`RpcError(code)`（其余协议错误，`.code` 是稳定的 snake_case，如 `already_resolved`）、`ConnectionError`、`AnyconError`（基类）。

## 账号归属：第一次配对要带 cookie

hub 上的设备都属于某个账号。配对时账号这样定（规则见 docs/spec/hub.md § 4 的「跨账号」「daemon 首次身份」）：

- host（daemon）和 joiner（SDK）里**有账号的那一边**说了算；两边账号不同 → `PairingFailed('forbidden')`。
- SDK 是 joiner，而且和 CLI 一样**没有浏览器 cookie**。所以：**daemon 已经配过对**（它在 hub 上是正式设备，有账号）时，`pair()` 直接可用，新设备被归到 daemon 的账号下；
- **全新的 daemon**（`anycon init` 之后第一次配对，hub 上还没有它的账号）+ 没有 cookie 的 SDK = 两边都没有账号 → hub 回 `PairFailed { reason: 'forbidden' }`，什么都不落库。

最常见的做法是第一次配对在浏览器里做（Web 的登录 cookie 让 daemon 入账号）。纯脚本 / CI 环境没有浏览器时，用 `headers` 带上登录 cookie 先把 daemon 配进账号——cookie 用 `/auth/token` 换：

```bash
curl -si -X POST "$HUB/auth/token" -H 'X-Anycon: 1' -H 'Content-Type: application/json' \
  -d "{\"token\":\"$ANYCON_ADMIN_TOKEN\"}" | grep -i '^set-cookie: anycon_session='
```

```ts
// 1) 第一次：带 cookie（节点上先 `anycon pair --json` 取短码）。这台 SDK 设备同时成了第一个已配对设备。
await pair(hub, { code, name: 'bootstrap', storage, headers: { Cookie: 'anycon_session=…' } })
// 2) 之后任何新设备（节点上再跑一次 `anycon pair`）：不需要 cookie
await pair(hub, { code: code2, name: 'ci-runner' })
```

`headers`（`pair()`、`connect()` / `Client.open()` 都有）是 WebSocket 升级请求的 HTTP 头，**只在 Node / Bun 生效**；浏览器的原生 WebSocket 不能设置头（同源 cookie 由浏览器自己带），会被忽略。它也可以用来带 hub 前面的反向代理要求的头（如 Cloudflare Access 的 `CF-Access-Client-Id`）。已配对的设备用 device token 认证，`connect()` 不需要 cookie。

## 存储

设备身份和已配对节点存在本地，后端是最简键值接口 `SdkStorage { get, set, delete }`：

| 后端 | 用在 | 位置 |
|---|---|---|
| `FileStorage(dir)` | Node / Bun 默认 | `$ANYCON_HOME/sdk/<hub-host>/<profile>/`（`ANYCON_HOME` 缺省 `~/.anycon`），目录 0700、文件 0600，原子写 |
| `IndexedDBStorage(scope)` | 浏览器默认 | 数据库 `anycon-sdk:<hub-host>/<profile>` |
| `MemoryStorage` | 测试 / 一次性脚本 | 进程内 |

`profile` 默认 `default`，`ANYCON_PROFILE` 或 `{ profile }` 切换。profile 会变成目录名，所以只允许字母数字和 `. _ -`（以字母数字开头、最多 64 位、不含 `..`），路径分隔符 / `..` / 空串直接抛 `invalid_profile`；`ANYCON_HOME` 是空串时当作没设。CI 里可以不落盘直接注入：`ANYCON_DEVICE_KEY`（32 字节私钥 hex）+ `ANYCON_DEVICE_TOKEN`，以及要连的节点 `ANYCON_NODE_ID` + `ANYCON_NODE_KEY`（daemon 公钥 hex）。

## 大消息与传输失步

hub 对单个信封有大小上限（默认 256 KiB），而一页 `session.history` 或一条 `tool_result` 可能更大。所以 SDK 把每条 JSON-RPC 消息加密之后按 `frame`（`@anycon/protocol`，ADR-30）切成 ≤ 200 KiB 的 `data` 信封，对端重组后再解密；两个方向都是。Noise 的 nonce 是顺序的，**任何一片丢失都意味着传输失步**，所以：

- hub 对已配对的 `(from, to)` 只会转发或退回 `backpressure { to, seq }`，**不会静默丢**，并且「粘住」：一个 seq 被退回后，hub 只放行被退回 seq 里最小的那个（重发成功才往后），其余一律退回（docs/protocol.md § 3.1）。所以 SDK 保留最近发出的信封**原文**（按字节封顶，默认 16 MiB，协议下限 8 MiB；`connect({ resendBufferBytes })` 可调）：发出不满 10 s 的、以及被退回等着重发的**永不逐出**；保留量超过上限时**暂停发新的**（把上限当作在途窗口，等最老的一批过了 10 s 再继续），而不是逐出。收到 `backpressure` 后**暂停**向该节点发新东西，攒齐被退回的 seq，等 `retry_after_ms`（最近一条 `rate_limited` 给的，封顶 30 s；没有就 100–300 ms + 抖动，连续被退回时翻倍，最多约 2.4 s），把所有被退回的 seq **按升序原样重发**（不重新加密），再发排队的新请求；重发期间又被退回就再来一轮。hub 报了 SDK 没留的 seq（没发过，或不属于当前会话）→ 无法保证顺序，丢掉 Transport、重新 IK 握手（进行中的请求以 `NodeOffline` 失败）。被退回的是 `handshake` 信封时，按 `retry_after_ms`（没有就 300–1000 ms + 抖动，连续被退回时翻倍，封顶 30 s）延后再重新握手，不立即重发、也不重置。因此 5 MiB、9 MiB 的 `fs.read` 回复和连续数十 MiB 的流量也能按序到达；
- 解密失败、分片乱序 / 超限 / 格式不对、或收到 daemon 的 `RESET` 帧，SDK 丢掉 Transport（进行中的请求以 `NodeOffline` 失败）并直接重新 IK 握手，之后的请求照常；
- hub 的 `dropped { seq, reason }` 每连接每秒至多一条，是「发错东西」的信号而不是逐条回执，**不据此重发**：`node.on('error', …)` 收到 `AnyconError`（`envelope_too_large` = 分片之后仍超限，说明 hub 的 `ANYCON_MAX_ENVELOPE_BYTES` 比一片还小；`not_paired` 则是 `NodeRejected`），进行中的请求失败，通道重新握手；
- hub 的 `rate_limited { retry_after_ms }` 是**整条 hub 连接**（所有节点、含重发）的「暂停多久」提示（伴随 `backpressure` 出现，封顶 30 s，重连后清零），**不再意味着有信封被丢**：到点之前所有节点新的信封排队，被退回的 seq 也等到那之后才重发；不失败请求、不重新握手、不发 `error` 事件，也不会因为 1 秒内刚发过数据就把传输当作失步。

## 开发

```bash
bun run --cwd packages/sdk test         # bun test
bun run --cwd packages/sdk test:node    # 同一批用例在 Node 上跑：node --import tsx --conditions source --test
bun run --cwd packages/sdk typecheck
bun run --cwd packages/sdk build        # 先构建 @anycon/protocol，再 tsc → dist/
```

`just test-sdk-real`（= `ANYCON_SDK_REAL=1 bun test tests/real.e2e.test.ts`，先 `cargo build -p anycon-hub -p anycon-cli` 和 `just gen-wasm`）是对**真实** `anycon-hub` + `anycon daemon`（mock agent）的端到端：临时目录、随机端口、固定 `ANYCON_ADMIN_TOKEN`；先用带管理员 cookie 的 `pair()` 把 daemon 配进账号，再常规 `pair()` → `connect()` → `info` / `agents.list` / `projects.create` / `sessions.create` → `send('run permission')`（权限、`tool_call`、用量）→ `history()` 对账 → `setSettings` → `cancel()`；最后三个用例是大消息 / 大流量：一条 5 MiB 的 `fs.read` 回复逐字节一致地到达；连续 10 × 3 MiB（共 30 MiB）之后节点仍在线、没有 30 秒超时；SDK 自己当发送方连发 13 × 900 KiB 的 `session.send`、越过客户端 10 MiB/分钟的预算后暂停—等待—重发，全部完成且不掉线——它们依赖 hub 的粘住规则 + 按连接种类的速率预算和 daemon 的升序重发，失败时错误里附着 hub / daemon 日志的末尾。默认跳过；Node 上同样能跑：`ANYCON_SDK_REAL=1 node --import tsx --conditions source --test tests/real.e2e.test.ts`。二进制路径可用 `ANYCON_BIN_DIR` 覆盖（默认 `target/debug`）。

其余快速测试用 `tests/fake-hub.ts`（真实 WebSocket，只转发；实现 Auth / `PairBegin { match_id }` / `PairJoin { match_id }` / 转发 / undeliverable / 按 `(from, to)` 粘住的背压（`interceptor` 返回 `'backpressure'` / `'rate_limited'` 退回，之后的信封一律退回，直到被退回的 seq 重发成功；`arrivals` / `bounced` 记录到达与退回顺序）/ `dropped`，按真实 hub 的线上约定：正式 id 在 `PairDone` 时才分配、临时连接只能 `PairJoin` / `PairConfirm`、超过 `maxEnvelopeBytes` 的信封回 `dropped { too_large }`；还能「作恶」——`forgePairDoneOnConfirm`、`tamperPairDonePeer`，用来测配对不会被中间人骗）
和 `tests/fake-daemon.ts`（TS 写的 Noise responder：本地生成短码、`pair.ack`、`frame` 分片 / 重组 / RESET，加脚本化的 `node.info` / `agent.list` / `workspace.*` / `session.*` / `permission.*` / `test.echo`，以及按真 daemon 规则实现的 `blob.*`：`blobChunkSize`、`maxAttachmentBytes`、`blobRefusal`（`quota` / `disk_full`）、`chunkHook`（给块加延迟 / 让它失败）、`chunksPeak`（同时在处理的块数）供附件测试用）。
每个场景对两种 Noise 实现各跑一遍：`tests/mock-crypto.ts`（玩具实现，形状同真实 Noise，永远在）和真实 wasm（`just gen-wasm` 之后出现在 `packages/protocol/wasm/`；不存在时打印提示并只跑 mock）。
