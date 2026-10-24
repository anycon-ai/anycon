# 协议

> 对应 v0.1。0.x 阶段每个 minor 版本都可能有不兼容的改动（§ 9）。`crates/protocol` 是唯一真源，本文是解释性说明。

## 1. 分层

协议分两层，分界线就是 hub 的可见性边界：

```
┌──────────────────────────────────────────────┐
│ 外层信封 (Envelope)              hub 可见     │
│   from · to · kind · seq · len               │
│ ┌──────────────────────────────────────────┐ │
│ │ Noise 加密 payload              hub 不可见 │ │
│ │   JSON-RPC 2.0 消息                       │ │
│ └──────────────────────────────────────────┘ │
└──────────────────────────────────────────────┘
```

- **外层**：hub 路由、排队、计数所需的最少信息
- **内层**：客户端和 daemon 之间的实际协议，JSON-RPC 2.0。dev server 预览走另一条 Noise 传输，内层是二进制的隧道帧（§ 11）

两层解耦的好处：内层协议随便改，hub 不需要发版；hub 代码里没有任何业务概念。

## 2. 传输

- 客户端和 daemon 都通过 WebSocket 连接 hub：`wss://<hub>/v1/ws`
- 连接建立后第一帧是设备认证（`control.auth`，带设备 token，由配对时 hub 签发）；还没有身份的设备（首次配对的新客户端、第一次启动的 daemon）用空 token 的临时连接，只能做配对或账号内接入（§ 4.4）
- 之后每帧是一个外层信封，二进制帧，编码为 MessagePack（外层）；内层 payload 是 Noise 密文，解密后是 JSON
- hub 有空闲超时，长时间无帧主动断开；daemon 自动重连，对用户透明
- 心跳由 WebSocket ping/pong 承担，不占用信封

## 3. 外层信封

```
Envelope {
  from:  DeviceId        // 发送设备
  to:    DeviceId        // 目标设备
  kind:  Kind            // handshake | data | notify | control
  seq:   u64             // 发送方单调递增，用于去重和断点续传
  body:  bytes           // kind 决定含义，见下
}
```

| kind | body | hub 行为 |
|---|---|---|
| `handshake` | Noise 握手消息（接入时还有接入的前置消息和 CPace 份额，§ 4.4；预览隧道带版本和票据 id，§ 11.2） | 转发；配对期间按 `match_id` 撮合（hub 不知道短码），接入期间按 `enroll_id` |
| `data` | 带 1 字节框架头的 Noise 密文：`0x00` 整条、`0x01` 分片（`msg_id / index / count`）、`0x02` 重置。单条 RPC 超过约 200 KiB 时分片，hub 只看到若干 `data` 信封；任何一片丢失即传输失步，接收方按重置处理、重新握手 | 转发；目标离线则回 `undeliverable`（或入队，若开启了队列）；超过 `ANYCON_MAX_ENVELOPE_BYTES` 回 `dropped { too_large }` |
| `notify` | `{ category: enum, session_id? }` 明文 + 可选密文详情 | 转发 + 按 category 触发 Web Push（收件设备离线时，[ADR-37](decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)；daemon 只发 `category` + `session_id`，`detail` 为空——Transport 是有状态计数器，带外的密文详情会打乱数据流，要等有单独的封装密钥再做） |
| `control` | hub ↔ 设备的控制消息（见 §8） | hub 自己处理，不转发 |

`notify.category` 是外层**唯一一个有语义的明文字段**（另有一个不透明的 `session_id`），枚举值：`permission_requested` / `turn_finished` / `error` / `session_idle`，以及账号内接入的 `enroll_requested` / `security`（[ADR-36](decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)，这两类没有 `session_id`）。（离线、溢出等由 hub 自己发出的是 `control` 消息，见 §8。）

### 3.1 消息大小

三层限制，从外到内：

| 层 | 上限 | 说明 |
|---|---|---|
| hub：单个信封 | `ANYCON_MAX_ENVELOPE_BYTES`，默认 256 KiB（不能小于 `MAX_FRAGMENT + 信封头` ≈ 201 KiB，否则 hub 拒绝启动）；超过 `max(2 倍上限, 64 KiB)` 的帧在 WebSocket 层直接断开连接 | 防御性上限。超限的信封被丢弃，hub 回发送方 `dropped { seq, reason: too_large }`；收到它说明发送方没有分片，是 bug，要记日志 |
| hub：目标连接的发送缓冲 | 每连接 4 MiB（控制消息另有 64–256 KiB） | 满了不丢：hub 退回 `backpressure { to, seq }`，并**按 `(from, to)` 粘住**（见下）。持续 30 秒读不动的连接以 `4003` 断开 |
| hub：每连接入站速率 | 客户端连接默认 10 MiB / 分钟；daemon 连接默认 100 MiB / 分钟（它是所有客户端的汇聚点；`ANYCON_RATE_*_BYTES_PER_MIN` 可调） | 超限**也不丢**：和缓冲满一样退回 `backpressure { to, seq }`，另外每秒至多一条 `rate_limited { retry_after_ms }` 告诉发送方停多久。只有持续超限 10 秒以上才以 `1008` 断开 |
| 传输：一条 RPC 消息 | 分片 ≤ 200 KiB / 片，重组后密文 ≤ 16 MiB | 一条 JSON-RPC 消息加密后，超过一片就拆成 `msg_id / index / count` 的多个 `data` 信封，接收方收齐再解密（[ADR-30](decisions.md#adr-30-内层分片与大小上限)）。接收方严格按序：`index` 必须是期望的下一片且 `< count`，同一 `msg_id` 的 `count` 不能变 |
| daemon：响应内容 | `session.history` 一页编码后 ≤ 192 KiB；`tool_result.output` ≤ 64 KiB；`fs.read` ≤ 10 MiB；`session.send` ≤ 1 MiB；`blob.put_chunk` / `blob.get` 一块 ≤ 512 KiB（base64 前） | daemon 主动控制，避免无意义的巨型消息，细节见 [spec/daemon.md § 7.9、§ 8.2](spec/daemon.md#79-历史分页) |

分片是**传输层**行为，对 JSON-RPC 层透明。Noise 传输用隐式递增的 nonce，所以任何一片丢失都会让整条传输失步：解密失败 / 分片乱序时，daemon 丢弃该客户端的传输并发 RESET，客户端丢弃传输并重新 IK 握手。

**hub 的顺序保证（粘住规则）**：hub 对已配对的 `(from, to)` 之间的 `data` / `notify` 信封只有两种处理——转发，或退回 `backpressure { to, seq }`——**从不静默丢弃**（`too_large` / `not_paired` 是发送方的 bug，另算）。hub 为每对 `(发送连接, to)` 维护一个**被退回 `seq` 的有序集合**：集合为空时信封按正常规则转发或退回；集合非空时，只有 `seq` 等于集合最小值的信封才会被尝试转发（成功就从集合里去掉），其它任何 `seq` 一律退回并加入集合。集合有上限（4096 项），超过说明发送方不守规则，hub 以 `1008` 断开它。这样顺序保证不依赖发送方是否已经收齐回执：回执晚到、发送方先重发了后面的 `seq`，hub 也只会再退回一次，不会放行。`handshake` 信封不受粘住影响，而且**任一方向成功转发的 `handshake` 信封、任一端断开或吊销都会清掉这对设备的粘住状态**——传输被重置后发送方不会再重发旧传输的信封，不清就会永久卡住。因此只要发送方按下面的规则做，接收方不会看到乱序：

- 发送方保留最近发出的信封原文：**凡是可能还在路上、还没被 hub 处理的信封都不能丢**（实现上：最近 N 秒内发出的、以及最后一个已知被 hub 转发或退回的 `seq` 之后的一律保留；保留量超过上限（≥ 8 MiB，建议 16 MiB）时**暂停发送**，把上限当作在途窗口用，而不是逐出）。收到 `backpressure { seq }` 后**暂停**向该对端发送新东西，等 `retry_after_ms`（没有 `rate_limited` 时默认 100–300 ms 并随机抖动），然后把所有被退回的 `seq` **按升序原样重发**（不能重新加密），再继续发新的。重发期间又收到退回就再等一轮。被退回的 `seq` 已经不在保留缓冲里（发送方自己丢了）→ 用同一个 `seq` 发一帧 RESET 占位让 hub 解除粘住，然后丢弃该对端的传输、重新握手；属于**上一次**传输的退回（重新握手之前发出的）只占位、不再重置当前传输。
- `handshake` 信封被退回（目标缓冲满或超速）时，按 `retry_after_ms` 延后再原样重发同一条握手消息（发起方也可以重新发起握手），不要立即重发也不要重置。响应方（daemon）的握手回复只能原样重发：对端在等的就是这一条。
- **已知限制（v0.1）**：没有正向回执（「已转发到某 seq」），发送方只能按时间保守地保留在途信封，所以窗口填满后持续大流量的上限约为 窗口 / 保留时间（daemon：16 MiB / 10 s ≈ 1.6 MiB/s）；单次不超过窗口的传输不受影响。正向回执要改协议（[roadmap](roadmap.md)）。另外「旧会话的 RESET 占位帧」会被 hub 当普通信封转发到对端，对端在 ready 状态收到会多做一次重新握手（自愈、不丢数据）；让 RESET 帧能区分会话也留到 v0.2。
- `rate_limited { retry_after_ms }` 是**整条连接**（所有对端、含重发）暂停多久的提示，不单独对应某封信，发送方可以把它封顶在 30 s；`dropped` 不触发重发（见 § 8）。
- 发送大消息时不必等回执，但一次性往一个对端灌超过缓冲上限（4 MiB）一定会被退回一部分；发送方可以按 1–2 MiB 一批自行节流，减少往返。

## 4. 配对与握手

### 4.1 配对流程

```
daemon                      hub                        新客户端
  │ 本地生成短码 7K3P-Q2MX    │                            │
  │─ control.pair_begin ────►│                            │
  │   { match_id }           │  hub 只拿到 match_id        │
  │◄─ control.pair_registered│  TTL 5min                  │
  │                          │                            │
  │       (用户把短码输入新客户端，或打开 /pair#code=…)       │
  │                          │                            │
  │                          │◄─ control.pair_join ───────│
  │                          │   { match_id }             │
  │◄─ control.pair_matched ──│── control.pair_matched ───►│
  │                          │                            │
  │◄════ handshake 帧 (Noise XX + PSK, 3 条消息) ════════►│
  │                          │  hub 只转发                │
  │                          │                            │
  │─ control.pair_confirm ──►│◄─ control.pair_confirm ────│
  │                          │  hub 记录 A↔B 已配对        │
  │◄─ control.pair_done ─────│── control.pair_done ──────►│  签发设备 token / id
  │                          │                            │
  │══ data: pair.ack（Noise 密文，带双方正式 id）═════════►│  新客户端校验后才落盘
```

- 短码：大写字母数字，去掉易混淆字符（0/O、1/I/L）。**默认 16 位**，约 79 bit（`XXXX-XXXX-XXXX-XXXX`）：`anycon pair`、链接、二维码、`--json` 都是它；`anycon pair --short` / `node.pair_begin { long: false }` 生成 8 位，约 40 bit（`XXXX-XXXX`），只适合在自建 hub 上手输，CLI 在 hub 不是本机 / 私网地址时会警告
- **短码由 daemon 本地生成，永远不发给 hub**。两端各自做一次 `Argon2id(password = 规范化短码, salt = "anycon-pair-v2", m = 64 MiB, t = 3, p = 1, out = 64)`：前 8 字节的 hex 是 `match_id`（发给 hub 撮合），后 32 字节是 PSK（`anycon_crypto::derive_pair_secrets`，wasm `derivePairSecrets`；本机约 0.1–0.3 s，浏览器约 0.1–1 s）。hub 不记录 match_id
- 链接形式 `${hub}/pair#code=<短码>`：短码在 URL fragment 里，浏览器不会把它发给 hub。Web 端未登录时的跳转也不能把它放进 query（只能放 fragment 或 `sessionStorage`），否则刷新一次就进了 hub 和反代的访问日志。`${hub}` 是 daemon 配置里的 `hub.url`，不是 hub 自己认为的地址
- 残余风险：`match_id` 是短码的确定函数，拿到它的 hub 可以**离线**穷举短码，猜中就能派生 PSK、对这次配对做完整的中间人（`pair.ack` 挡不住：它用的就是那把 PSK）。Argon2id 让每次猜测都要 64 MiB 内存和约 0.1 s，8 位短码的全空间约 2^40 次，一张 GPU 量级要数年，但对不信任的 hub 仍不该用 8 位；16 位（2^79）不可行。盐是全局常量，所以理论上可以为 8 位码预计算一张表，代价同上。彻底解决要换 PAKE（[ADR-29](decisions.md#adr-29-短码不经过-hub)）：§ 4.4 的接入码用 CPace，之后由它取代 8 位短码
- Noise 模式：`Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`。XX 带内交换静态公钥；psk3 在第三条消息混入 PSK，hub 没有短码就无法完成握手，也就无法做中间人（上一条说的穷举除外）。被动窃听者拿不到临时密钥，不能用握手记录离线验证短码的猜测。注意 PSK 到第 3 条才混入，所以 initiator 写完第 3 条并不代表对端真的持有 PSK，这就是 `pair.ack` 存在的原因
- 握手载荷：第 2 条（daemon → 新设备）带 JSON `{ "name": <节点名>, "version": <daemon 版本> }`，**这一条不受 PSK 保护**，只能当提示用，认证过的节点名以 `pair.ack` 为准；第 3 条（新设备 → daemon，已受 PSK 保护）带新设备的 `ClientInfo` JSON `{ name, kind, platform, version, origin? }`，daemon 据此记录设备名。第 1 条和 IK 重连的两条载荷为空
- 握手成功后 daemon 立刻记下对方静态公钥和 `ClientInfo`；**新客户端要等 `pair_done` 之后、经新传输收到 daemon 的 `pair.ack`（`PairAck { node_id, peer_id, name, version, fingerprint }`：daemon 正式 id、hub 告知 daemon 的自己的正式 id、节点名、版本、daemon 静态公钥指纹），校验 `peer_id` 等于自己的正式 id、`node_id` 等于 `pair_done.peer`、`fingerprint` 等于握手得到的静态公钥的指纹之后才落盘**。hub 伪造 `pair_done` 无法伪造 `pair.ack`，因为后者在 PSK 绑定的 Noise 传输里。30 秒内没收到 → 配对失败、不落盘；本地已有同 id 但公钥不同的节点 → 失败并提示，不覆盖
- 配对也可以从任一已配对客户端发起（它通过加密通道让 daemon 执行 `pair_begin`）
- **daemon 的首次身份**：daemon 第一次连 hub 时还没有设备 token，以 `kind = daemon` 的临时连接发 `pair_begin`；第一次配对完成时 hub 在 `pair_done` 里同时给 daemon 签发 `device_id` 和 `device_token`，账号归入 joiner 所在账号（浏览器的登录 cookie）。两边都没有账号则 `pair_failed { forbidden }`
- `pair_done` 之后同一条连接原地升级为正式设备，`from` 必须换成新 id；紧接着双方各收到一条 `peer_online`，客户端据此发起 IK 握手

### 4.2 重连握手

已配对的双方用 `Noise_IK_25519_ChaChaPoly_BLAKE2s`：发起方已知对方静态公钥，一个往返建立会话，每次重连重新握手。

- 客户端是 initiator，daemon 是 responder。客户端在 `auth_ok.peers_online` 里看到某台 daemon、以及之后每收到一条它的 `peer_online` 时，都丢掉对这台 daemon 的旧 Transport 重新握手
- 同一设备的新连接顶掉旧连接时，hub 只发 `peer_online`，不先发 `peer_offline`——所以收到 `peer_online` 时不能假设之前收到过下线通知
- daemon 用配对时记下的公钥创建 responder：对方的静态公钥对不上，握手直接忽略并记日志
- 收到 `peer_offline` 或和 hub 断线，丢掉对应的 Transport

### 4.3 多 daemon

客户端对每台 daemon 维护独立的 Noise 会话。

### 4.4 账号内接入

[ADR-36](decisions.md#adr-36-账号内接入账号负责路由daemon-负责开门)。新设备已经登录账号，要接入同一账号下的一台 daemon 时用，不需要事先拿到短码。hub 按「账号 + 节点 id」撮合，不按 `match_id`；握手的后半段和 § 4.1 完全相同（XXpsk3、`pair_confirm`、`pair_done`、`pair.ack`），只差 PSK 的来源。

```
新设备 N                     hub                          daemon D
  │─ control.enroll_begin ──►│  N 登录的账号拥有 D？           │
  │   { node }               │  D 在线？限速                  │
  │◄─ control.enroll_started │── control.enroll_request ────►│
  │   { enroll_id, node,      │   { enroll_id, peer,           │
  │     expires_at_ms }       │     expires_at_ms }           │
  │                          │                               │
  │══ handshake: EnrollHello { method, nonce, hint } ════════►│
  │◄═ handshake: EnrollReply { status, nonce, salt?, cpace? } ═│  wait_approval：等已配对设备
  │                          │                               │  批准后再发一条 ready
  │══ handshake: CPace 份额（code / password）═══════════════►│
  │◄════ handshake: Noise XXpsk3（N 是 initiator）═══════════►│  PSK = CPace 输出 / 扫码密钥
  │                          │                               │
  │─ control.pair_confirm ──►│◄────── control.pair_confirm ──│
  │◄─ control.pair_done ─────│── control.pair_done ─────────►│
  │◄══════════ data: pair.ack（同 § 4.1）═════════════════════│  N 校验后才落盘
```

- **谁能发起**：N 是本账号的正式设备，或升级请求带本账号登录态的临时连接（Web 是 cookie；CLI 是 `anycon connect` 里临时登录拿到的会话）。没有账号 → `enroll_failed { forbidden }`；`node` 不是本账号未吊销的 daemon → `not_found`；D 不在线 → `offline`
- **`EnrollHello`**（明文，hub 看得到也改得了）：`{ v: 1, method, nonce, hint }`。`method` 是 `scan`、`code`（带 `approval: bool`：`true` 请求批准，`false` 用已有的接入码）或 `password`；`nonce` 是 16 字节随机数；`hint` 是新设备自报的 `{ name, kind, platform }`，**只用于批准界面的提示**，认证过的 `ClientInfo` 仍以 XXpsk3 第 3 条为准
- **`EnrollReply`**（明文）：`{ status: wait_approval | ready | rejected, nonce?, reason?, retry_after_ms?, attempts_left?, salt?, cpace? }`。`rejected.reason`：`method_disabled`、`password_not_set`、`backoff`（带 `retry_after_ms`）、`locked`、`no_code`、`busy`（进行中的接入满 3 个，或已有一个口令尝试在进行）、`denied`、`wrong_secret`（码还有次数时带 `attempts_left`）、`expired`、`invalid`（帧格式或顺序不对）
- **三种方法**：
  - `password`：D 回 `ready`，带节点盐和自己的 CPace 份额。N 用 `Argon2id(口令, 盐, m = 64 MiB, t = 3, p = 1, out = 32)` 作 CPace 的口令输入，回自己的份额。一个接入会话只试一次
  - `code`：`approval = true` 时 D 回 `wait_approval`，给所有在线的已配对设备发 `node.enroll_requested`，并发 `notify { category: enroll_requested }`；某台已配对设备 `node.enroll_approve` 拿到 6 位码后，D 再回 `ready` 带 CPace 份额。`approval = false` 时用 `node.enroll_code` 主动生成的码，没有就 `no_code`。CPace 的口令输入是 6 位码的 ASCII。同一接入会话里可以重发 `EnrollHello` 再试（每次换新 nonce，daemon 总是用最新那条 `hello` 的 nonce，等批准期间重发也一样），每个码合计最多 3 次，**收到 N 的 CPace 份额时才扣**：只发 `hello`、批准本身都不扣。等份额的阶段不计时，收到份额后到第 3 条限 60 秒（超时 D 回 `rejected { expired, attempts_left }`，还有次数时会话不结束）
  - `scan`：N 先生成 32 字节随机数 r，显示二维码 `anycon-enroll:1:<node_id>:<enroll_id>:<base32(r)>`（不是 URL）。D 回 `wait_approval`，不推送；已配对设备在 Web UI 里扫码后 `node.enroll_approve { enroll_id, secret: r }`，D 回 `ready`。PSK = `HKDF-BLAKE2s(r, info = "anycon-enroll-scan-v1" ‖ node_id)`，不跑 CPace
- **CPace**：按 draft-irtf-cfrg-cpace 的 ristretto255 + SHA-512 构造（对过草案附录 B.3 的测试向量），精确定义以 `anycon_crypto::enroll` 为准（另写客户端请直接用它的 wasm / Python 绑定）。会话标识 sid = N 的 `nonce` ‖ D 的 `nonce`（直接拼接，不用 hub 给的 `enroll_id`）；信道标识 CI = `lv_cat("anycon-enroll-v1", node_id, method)`（`lv_cat` 是每段前加 LEB128 长度）；生成元 = `from_uniform_bytes(SHA-512(lv_cat("CPaceRistretto255", PRS, 补零到 128 字节块, CI, sid)))`；ISK = `SHA-512(lv_cat("CPaceRistretto255_ISK", sid, K) ‖ lv_cat(Y_N, "") ‖ lv_cat(Y_D, ""))`。扫码 PSK 的 HKDF 不带盐（RFC 5869 的默认：32 字节零）。输出的 ISK 截成 32 字节当 XXpsk3 的 PSK（`anycon_crypto::enroll_*`，wasm / Python 同名）。N 填进 CI 的 node_id 来自账号的设备列表，hub 撒谎就握不上手
- **失败与计数**：XXpsk3 第 3 条在 D 那里校验失败，说明 PSK 不一致（码 / 口令错，或有中间人）。D 回 `rejected { wrong_secret }`，计一次失败；`password` 从 D 发出 CPace 份额起，没有以一条通过校验的第 3 条结束的都算失败。限额和锁定见 [spec/daemon.md § 15.1](spec/daemon.md#151-账号内接入)
- **结束**：成功后照 § 4.1 收尾；收尾阶段的失败（只有一边 `pair_confirm`、配额、写库出错）发的是 `enroll_failed`，不是 `pair_failed`，daemon 据此删掉暂存的设备。hub 侧的接入会话 5 分钟内没有走到 `pair_done` 就作废（`enroll_failed { timeout }`）；任一方可以 `enroll_cancel`（会话不存在 → `enroll_failed { not_found }`）；N 是临时连接时，临时连接最长 10 分钟的上限照旧
- **hub 看到什么**：谁在什么时候对哪台节点发起了接入、有没有完成。`handshake` 体不解析、不记录（`hint` 是明文，和配对第 2、3 条的载荷一样对待）；接入码、口令、r 都不经过 hub

## 5. 内层：JSON-RPC 2.0

内层是标准 JSON-RPC 2.0，三种语义：

- **request / response**：有 `id`，期待回复
- **notification**：无 `id`，单向推送。流式输出全部走 notification

方法名用 `域.动作` 命名。以下是 v0.1 的方法集合（草案）。

### 5.1 节点（daemon）

| 方法 | 方向 | 说明 |
|---|---|---|
| `node.info` | C→D | 版本、能力、workspace 根、已安装 adapter 列表；`capabilities.worktree` 为 true 时带 `worktree { default_mode, branch_prefix }`（节点配置 `[worktree]`，ADR-34） |
| `node.devices` | C→D | 已配对设备列表 |
| `node.revoke_device` | C→D | 吊销一台设备（daemon 删公钥，并通过 control 通知 hub 删路由） |
| `node.pair_begin` | C→D | `{ long? }` → `{ code, expires_at_ms, link }`：让 daemon 本地生成短码并向 hub 登记 `match_id`；`long` 缺省为 `true`，生成 16 位；`long = false` 生成 8 位（`anycon pair --short`，只适合自建 hub，ADR-29）。`link = ${hub}/pair#code=<code>`。错误：daemon 没连上 hub → `unsupported`；hub 拒绝登记（回 `pair_failed`）→ `busy` 且 `data.reason` 是 `quota`（账号今天的配对名额用完了，`multi`）或 `rate_limited`，`forbidden` 且 `data.reason = "forbidden"`（约定同 `blob.put_begin` 的 `busy` + `quota`）；hub 没有确认登记 → `internal`。登记之后的失败不在这里返回，见 [spec/daemon.md § 15](spec/daemon.md#15-连接与可靠性) 的 `local.pair_outcome` |
| `pair.ack` | D→C notification | `{ node_id, peer_id, name, version, fingerprint }`：配对握手完成、`pair_done` 之后 daemon 发给新客户端的第一条加密通知，见 §4.1 |
| `node.enroll_requested` | D→C notification | `{ enroll_id, method, hint, expires_at_ms }`：有设备请求批准接入（§ 4.4 的 `code`，`approval = true`；`scan` 不发），发给所有在线的已配对设备。`hint` 未经认证 |
| `node.enroll_list` | C→D | → `{ enrolls: [{ enroll_id, method?, approval, hint?, status: waiting_hello \| wait_approval \| ready, created_at_ms, expires_at_ms }] }`：进行中的接入会话，含还没收到 `hello` 的（`waiting_hello`）和 `scan` 的（节点设置里「待批准的接入请求」和扫码确认页用；打开页面或重连后拉一次，之后靠 `node.enroll_requested` / `node.enroll_resolved` 增量） |
| `node.enroll_approve` | C→D | `{ enroll_id, secret? }`：批准。`code` 请求返回 `{ code, expires_at_ms }`（6 位，只给批准的这一台）；`scan` 请求必须带 `secret`（二维码里的 r），返回 `{}`；非 `scan` 请求带了 `secret` → `invalid`。已被别的设备处理过 → `already_resolved` |
| `node.enroll_deny` | C→D | `{ enroll_id }`：拒绝，新设备收到 `rejected { denied }` |
| `node.enroll_resolved` | D→C notification | `{ enroll_id, result: approved \| denied \| completed \| failed \| expired, by_device? }`：对收到过 `hello` 的会话（含 `scan`）发给所有在线客户端，包括批准的那台自己，让别的设备收起批准卡片。本机 socket 的 `by_device` 是 `"local"` |
| `node.enroll_code` | C→D | `{}` → `{ code, expires_at_ms }`：「添加设备」时主动生成 6 位接入码，5 分钟内本账号任一 `code`（`approval = false`）接入可用；新的顶掉旧的 |
| `node.enroll_status` | C→D | → `{ approval, password: { set, state: enabled \| disabled \| locked, failures, retry_after_ms?, set_at_ms?, set_by? } }`（`set_by` 是设备 id，本机 CLI 设置的是 `"local"`） |
| `node.set_enroll_password` | C→D | `{ password? }`：设置 / 修改节点口令，省略 = 清除。少于 8 个字符 → `invalid` |
| `node.set_enroll_enabled` | C→D | `{ approval?, password? }`：启用 / 停用两类接入（写 `[enroll]` 配置）；口令被锁定后用 `password: true` 重新启用，失败计数清零 |
| `node.security_event` | D→C notification | `{ kind: device_enrolled \| password_failures \| password_locked \| password_changed \| settings_changed, ts_ms, by_device?, device_id?, device_name?, method?, failures?, password_set? }`（平铺字段，按 `kind` 填其中几个）：同时发 `notify { category: security }` |
| `node.stats` | C→D | 运行时长、连接状态、各状态会话数、磁盘占用 |
| `node.doctor` | C→D | 诊断检查项（同 `anycon doctor --json`） |
| `node.audit` | C→D | 本地审计日志 `{ since?, device?, limit }` |
| `node.set_agent_defaults` | C→D | `{ agent, model?, thinking?, mode?, options? }`：设置某 agent 在该节点上的默认模型 / 思考 / 权限模式。和 `session.create` 走同一套校验（含危险模式的 `forbidden`） |

### 5.1.1 Agent

| 方法 | 方向 | 说明 |
|---|---|---|
| `agent.list` | C→D | `{ refresh? }` → `[AgentInfo]`：本机每个 agent 的状态、版本、模型（含每个模型的思考等级）、权限模式、其他选项、能力。结构见 [spec/daemon.md § 5.2](spec/daemon.md#52-agentinfo) |
| `agent.diagnose` | C→D | `{ id }` → 命令解析、PATH、版本、认证等诊断信息 |
| `agent.changed` | D→C notification | 探测结果变化 |

### 5.2 Workspace

| 方法 | 方向 | 说明 |
|---|---|---|
| `workspace.list` | C→D | 项目列表 + 推导的元数据。每项带 `kind`（`folder` 根目录下的文件夹 / `linked` 关联的本机目录）；关联目录不在了时 `missing: true`；`git.worktree_ready` 表示能给会话开 worktree（项目目录是仓库顶层且有提交）；`git_pending: true` 表示 git 元数据还在算（`git` 为空不代表不是 git 项目），算完之后发 `workspace.changed { updated }` |
| `workspace.create` | C→D | 新建空项目 `{ name, git_init }` |
| `workspace.clone` | C→D | 克隆 `{ url, name? }`，返回 job id |
| `workspace.link` | C→D | 把本机已有目录关联为项目 `{ path, name? }` → `Project`。`path` 是绝对路径或 `~/…`，必须在 daemon 的 `workspace.link_roots` 之内，并且过 ADR-33 的规则（隐藏目录、内置拒绝表、数据目录 / 文件系统根）；越界或被规则拒绝 `forbidden`，在 workspace 根里 `invalid`，重名或已关联 `exists`，不存在 / 不是目录 `not_found` |
| `workspace.unlink` | C→D | `{ name }`：解除关联，不动目录；对根目录下的项目返回 `invalid`；有运行中的会话返回 `busy` |
| `workspace.browse` | C→D | `{ path?, show_hidden? }` → `{ path, parent?, roots, linkable, project?, blocked?, entries: [{ name, path, git?, project?, linkable, blocked? }], truncated? }`（`git` / `truncated` 为 false 时省略；`blocked` 是不能关联的原因 `hidden` / `sensitive` / `workspace` / `data_dir` / `filesystem_root`）：给关联选择器列 `link_roots` 之内的子目录（只有目录名）；拒绝表和数据目录里面不能列（`forbidden`） |
| `workspace.branches` | C→D | `{ project }` → `{ current?, default_base, local, remote, truncated? }`：新建会话选 base 用。`default_base` 是当前分支的上游（有的话）或当前分支；不列 anycon 自己的 `<branch_prefix>/…` 分支；非 git 项目 `invalid` |
| `workspace.worktrees` | C→D | `{ project? }` → `{ worktrees: [WorktreeInfo] }`：anycon 建的全部 worktree（含已保留的、会话已删除的），带状态、未提交文件数、自己的提交数、磁盘占用 |
| `workspace.remove_worktree` | C→D | `{ id, force?, delete_branch? }`：删掉一个 worktree。所属会话没归档、或者还有 turn / agent 进程 → `busy`；有未提交改动而没 `force` → `invalid`（`data.reason = "uncommitted_changes"`） |
| `workspace.rename` | C→D | `{ from, to }`。关联项目只改名字，不动目录；项目里有 anycon 的 worktree 时改名后跑 `git worktree repair` |
| `workspace.trash` | C→D | 移到 `.trash/`。关联项目返回 `invalid`（用 `workspace.unlink`） |
| `workspace.trash_list` / `restore` / `purge` | C→D | 回收站 |
| `job.cancel` | C→D | 取消长任务 |
| `workspace.changed` | D→C notification | 项目增删改名事件（含关联 / 解除关联）；`updated` 列出元数据（`git`）有了或变了的项目，比如 daemon 重启后冷缓存里第一次算完——只当作「重新 `workspace.list`」的信号，里面的名字可能已经被改名或删除 |
| `job.progress` | D→C notification | 长任务进度 `{ job_id, stage, percent?, log_line? }` |
| `job.done` | D→C notification | `{ job_id, ok, error? }` |

### 5.3 文件（只读浏览）

| 方法 | 方向 | 说明 |
|---|---|---|
| `fs.list` | C→D | 列目录。`path` 是 `<项目名>/<项目内相对路径>`：第一段是关联项目时相对那个目录解析，否则相对 workspace 根；canonicalize 后都不能走出所在的项目 / 根目录 |
| `fs.read` | C→D | `{ path, max_bytes? }` → `{ text, binary, truncated, bytes }`。默认读 1 MiB，上限 10 MiB，超出部分截断并置 `truncated`；二进制文件不返回内容。响应超过一片时由传输层分片（§3.1）。blob 通道（§ 5.6）目前只收图片 |

### 5.4 会话

| 方法 | 方向 | 说明 |
|---|---|---|
| `session.list` | C→D | `{ project?, include_archived?, limit? }` → `{ sessions: [SessionSummary] }`，会话列表 + 状态。客户端应传 `limit` |
| `session.get` | C→D | `{ session_id }` → `SessionSummary` |
| `session.create` | C→D | `{ project?, agent, prompt?, message_id?, attachments?, settings?, workspace? }`；带 `prompt` 或 `attachments` 时创建后立刻发第一条消息（`message_id` 必填，附件规则同 `session.send`，§ 5.6）；`project` 为空则是 scratch 会话。`settings` 结构见 [spec/daemon.md § 6.1](spec/daemon.md#61-sessionsettings)：`model`、`thinking`、`mode`、`options`、`env` 等。`workspace` 缺省是 `{ mode: "local" }`；`{ mode: "worktree", base?, branch? }` 让 daemon 新建 worktree 和分支（[spec/daemon.md § 10.1](spec/daemon.md#101-每会话-worktree)）：返回时 `worktree.state = creating`，建好之后才拉起 agent，失败时会话进入 `error`。项目不是 `worktree_ready` → `invalid`（`data.reason = "not_worktree_ready"`），分支已存在 → `exists`，`base` 解析不了 → `invalid`（`data.reason = "bad_base"`） |
| `session.send` | C→D | 发送用户输入 `{ session_id, message_id, content, attachments? }`；`message_id` 由客户端生成，用于幂等重发（同一个 `message_id` 配不同的内容或附件 → `invalid`）。`attachments` 是 `blob.put_end` 的结果（§ 5.6），带附件时 `content` 可以为空。**没有单独的 resume 方法**：会话的 agent 进程已退出（空闲超时、daemon 重启、`interrupted`）时，`send` 会自动 resume（adapter 支持才行；不支持的会话转为 `ended`），`session.restart` 是显式重启。已归档的会话返回 `invalid`（`data.reason = "archived"`），先 `unarchive` |
| `session.cancel` | C→D | 中断当前 turn；没有 turn 时清空等待发送的消息（关联项目目录变了期间留在队列里的） |
| `session.set_settings` | C→D | `{ session_id, settings, clear? }`：`settings` 是增量（缺省 / `null` 的字段保持，`options` 里值为 `null` 表示删键），`clear` 列出要恢复为未设置的字段（`model` `thinking` `mode` `env` `system_prompt_append` `max_turns` `max_budget_usd`），在合并之后生效；turn 进行中返回 `busy`；成功后发 `settings_changed` 事件，结果带生效后的完整设置 |
| `session.watch` / `session.unwatch` | C→D | 订阅 / 退订该会话的 `session.event`。未 watch 的会话只收列表级 `session.changed` |
| `session.history` | C→D | `{ session_id, after_seq?, limit?, before_seq? }` → `{ events, has_more }`。`after_seq` 拉增量；`before_seq` 向上翻页（返回 seq 小于它的最近 `limit` 条）；`limit` 默认 200、上限 1000。**一页还受字节上限约束**：编码后超过 192 KiB 就在这里截断并置 `has_more = true`（单个事件本身超限也单独成页），客户端按返回的最后一个 seq 继续拉 |
| `session.event` | D→C notification | 会话事件流，见 §6。只发给 watcher |
| `session.changed` | D→C notification | `{ session_id, title, status, archived, pending_permissions, background_tasks, updated_at_ms, last_seq, deleted, worktree? }`，发给所有在线客户端；`deleted = true` 表示会话已被删除；`worktree` 带 worktree 的当前状态；`background_tasks` 是 agent 进程里还在跑的后台任务数（缺省 = 0，和 `SessionSummary` 的同名字段一样只反映 daemon 内存里的当前状态，见 [daemon.md § 7.3](spec/daemon.md#73-进程保活与-idle_timeout)） |
| `session.rename` / `archive` / `unarchive` / `delete` / `restart` | C→D | 见 [spec/daemon.md § 7.8](spec/daemon.md#78-归档删除重启)。`archive` 的参数是 `{ session_id, worktree? }`（会话 queued / starting / running / waiting_permission 时返回 `busy`），`delete` 是 `{ session_id, worktree?, delete_branch? }`；`worktree` 取 `auto`（缺省）/ `keep` / `remove`，只带 `session_id` 的旧请求照旧合法 |
| `session.git_status` | C→D | `{ session_id, include_pr? }` → `SessionGitStatus`：分支、HEAD、（worktree 会话）base / ahead / behind、未提交和冲突的文件数、是否在合并、上游和未推送数；`include_pr` 时用 `gh pr view` 查 PR（拿不到时 `pr_error`）。会话不在 git 仓库里 → `invalid`（`data.reason = "not_git"`） |
| `session.diff` | C→D | `{ session_id, against?, path? }` → `{ from_sha, files, truncated }`：只读。`against = base`（缺省，只有 worktree 会话）从 merge-base 到工作区，`uncommitted` 从 HEAD 到工作区；都含未跟踪的文件。不带 `path` 只给文件列表和行数（≤ 1000 个文件），带 `path` 只给那一个文件和它的 unified diff（≤ 256 KiB） |
| `session.sync_base` | C→D | `{ session_id, abort? }` → `{ outcome, commits, conflicts }`：把 base 合并进 worktree 分支（远端跟踪分支先 fetch）。turn 在跑 → `busy`；有未提交改动 → `invalid`（`data.reason = "uncommitted_changes"`）；上次合并还停在冲突里 → `invalid`（`merge_in_progress`，用 `abort` 或让 agent 解决）；fetch / merge 本身失败 → `internal`（`data.reason = "fetch_failed"` / `"git_failed"`，消息里带 git 的输出尾部） |
| `session.search` | C→D | 在 daemon 本地 FTS 搜索 |
| `session.promote` | C→D | scratch 会话转成项目 |

### 5.5 权限

| 方法 | 方向 | 说明 |
|---|---|---|
| `permission.request` | D→C notification | `{ session_id, request_id, kind, title, detail, options, expires_at_ms? }`。`kind` 取 `tool` / `plan` / `question` / `mode_change` / `other`；`options` 是 adapter 给出的可选答复，结构见 [spec/daemon.md § 9](spec/daemon.md#9-权限请求)。发给**所有**在线客户端，不只 watcher |
| `permission.respond` | C→D | `{ request_id, option_id?, message?, answers? }`；`kind = question` 时用 `answers` |
| `permission.list` | C→D | `{ session_id? }` → 未决请求列表，重连后先拉 |
| `permission.resolved` | D→C notification | `{ request_id, option_id, by_device }` |

多个客户端同时在线时，任一客户端答复即生效，其余客户端收到 `permission.resolved`；重复答复返回 `already_resolved`。daemon 自己也会答复：关联项目的目录在 turn 进行中变了（[ADR-33](decisions.md#adr-33-关联本机已有目录为项目) 第 5 条）时，放行的 `permission.respond` 被 daemon **消耗**并按拒绝答复 agent，发起答复的客户端得到 `not_found` 且 `data.reason = "project_dir_changed"`（不要重试；别的原因的 `not_found`——未知的 `request_id`、找不到会话——没有这个 `reason`），所有客户端收到 `permission.resolved { by_device: "daemon", 拒绝选项 }`，随后是 `error { code: not_found }` 和 `turn_done { reason: error }`；超时（`permissions.request_timeout`）和取消时 daemon 自动答复也用 `by_device: "daemon"`。

### 5.6 附件 / blob（[ADR-35](decisions.md#adr-35-图片附件经-blob-通道传到-daemon消息只带引用)）

目前只有图片：`image/png` `image/jpeg` `image/gif` `image/webp`。`node.info.capabilities.blobs = true` 表示 daemon 支持这一组方法；某个 agent 收不收附件看它的 `capabilities.attachments`。

| 方法 | 方向 | 说明 |
|---|---|---|
| `blob.put_begin` | C→D | `{ name, mime, size, sha256 }` → `{ blob_id, chunk_size, expires_at_ms }`。`mime` 不在上面四种里 → `unsupported`；`size` 为 0 或超过 `sessions.max_attachment_bytes` → `invalid`（`data.reason = "too_large"`）；`name` 1–255 字节、不含 `/` `\` 和控制字符；`sha256` 是 64 位小写 hex。同一设备同时进行中的上传至多 8 个（再多 `busy`）；完成但没被引用的 blob 全节点合计超过 1 GiB → `busy`（`data.reason = "quota"`），数据目录所在卷剩余空间不够 `size + 1 GiB` → `busy`（`disk_full`）。名字不能含控制字符（Unicode Cc）、双向控制符（U+200E U+200F U+202A–U+202E U+2066–U+2069）和 U+2028 / U+2029。`chunk_size` 目前是 512 KiB |
| `blob.put_chunk` | C→D | `{ blob_id, index, data }` → `{}`。`data` 是标准 base64（带 padding），解码后长度必须正好是这一块应有的长度（最后一块是余数）；偏移 = `index × chunk_size`。可以乱序、并发、重发同一块（幂等）。上传不存在 / 已过期 / 已完成 → `not_found`；`index` 越界、长度不对、base64 不合法 → `invalid`；`put_end` 已经开始收尾 → `invalid`（`data.reason = "finalizing"`） |
| `blob.put_end` | C→D | `{ blob_id }` → `AttachmentRef { blob_id, name, mime, bytes, width?, height? }`。缺块 → `invalid`（`data.reason = "incomplete"`，上传保留，补发后再 `put_end`）；SHA-256 不符（`hash_mismatch`）或文件头和 mime 不符（`mime_mismatch`）→ `invalid`，上传作废。对已经完成的 blob 重复 `put_end`、以及并发的 `put_end` 都返回同一个结果。`put_end` 开始收尾后不再接受块，已经在写的块先写完再校验。文件头只是前缀比对（不解码图片）。记录的 `name` 扩展名和 mime 对不上时 daemon 在后面补上（`x.html` → `x.html.jpg`） |
| `blob.get` | C→D | `{ blob_id, offset?, max_bytes? }` → `{ blob: AttachmentRef, offset, data, eof }`。`max_bytes` 缺省和上限都是 512 KiB；`max_bytes = 0` 按 1 处理；`offset` 超过大小 → `invalid`；blob 不存在、还没 `put_end`、或者记录在而文件不在了 → `not_found`（文件不在时 `data.reason = "blob"`）。任何已配对设备都能读任何 blob |
| `blob.discard` | C→D | `{ blob_id }` → `{}`。丢掉一个进行中的上传，或已完成但还没被消息引用的 blob；已被消息引用 → `invalid`（`data.reason = "in_use"`）；不存在也返回成功（幂等） |

消息只带引用：`session.send` / `session.create` 的 `attachments` 放 `blob.put_end` 的结果。daemon 逐个核对：blob 不存在 → `not_found`（`data.reason = "blob"`）；`name` / `mime` / `bytes` 和记录不一致、或已经被**别的**会话的消息引用（`blob_in_use`）→ `invalid`；mime 不在该会话 agent 的 `capabilities.attachments` 里 → `unsupported`；一条消息至多 20 个附件、合计 ≤ 20 MiB（超了 `invalid` / `too_large`）。积压的多条消息合并成一个 turn 时，附件累计超过 20 个或 20 MiB 就停，剩下的作为下一个 turn。`session.create` 带 `message_id` 的重试先按幂等返回第一次建好的会话，再核对附件（不会因为附件已经绑定到第一次的会话而得到 `blob_in_use`）。带附件时 `content` 可以为空串。第一次被引用时 blob 绑定到这个会话，随会话删除；没被引用的 blob 24 小时后回收，进行中的上传 10 分钟没有新块就作废。

分块在内层，hub 看到的只是若干个 `data` 信封。

### 5.7 预览（[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)）

`node.info.capabilities.preview = true` 表示 daemon 支持这一组方法且节点开着预览（`preview.enabled`）。隧道本身不走 JSON-RPC，见 § 11；端口策略见 [spec/daemon.md § 19](spec/daemon.md#19-预览)。

| 方法 | 方向 | 说明 |
|---|---|---|
| `preview.open` | C→D | `{ port, session_id? }` → `{ label, port, ticket_id, psk, expires_at_ms, session_id? }`。为本机端口签发一张票据：`label` 是这个端口固定的预览 label（26 位小写 base32），`ticket_id` 32 位 hex，`psk` 64 位 hex（票据密钥，只交给预览页）。同一设备对同一端口已有一张剩余有效期超过一半的票据时返回它。端口不允许 → `forbidden`（`data.reason = "port_not_allowed"`）；预览关着 → `forbidden`（`disabled`）；`port` 为 0 → `invalid`；`session_id` 不存在 → `not_found` |
| `preview.close` | C→D | `{ label }` → `{}`。作废这个 label 的全部票据，对用它们的隧道发 `GOAWAY { closed }`。不存在也返回成功 |
| `preview.list` | C→D | → `{ previews: [{ label, port, ticket_id, session_id?, created_by, created_by_name, created_at_ms, expires_at_ms, tunnels }] }`：全部未过期的票据 |
| `preview.ports` | C→D | `{ session_id? }` → `{ ports: [{ port, source, session_id?, pid?, process? }], detection }`。`source`：`session`（监听进程在会话的 agent 进程树 / 进程组里）、`workspace`（监听进程的工作目录在会话的工作目录里）、`config`（`preview.allow_ports`）。带 `session_id` 只列这个会话的（外加 `config` 的）。`detection = unavailable`：这台机器上没有可用的探测手段，只列 `config` 的 |

票据密钥、令牌和预览 URL 都不写日志；`preview.open` / `preview.close` 记本地审计（不含密钥）。

## 6. 会话事件

`session.event` 的 payload：

```
SessionEvent {
  session_id: string
  seq:        u64          // 会话内单调递增，持久化，用于增量拉取
  ts_ms:      timestamp    // 毫秒
  turn_id?:   u64          // 事件所属的 turn（daemon 填）
  event:      Event
}

Event =
  | { type: "user_message",     message_id, content, by_device, attachments? }   // 用户消息也进事件流，历史回放才完整；attachments 是 [AttachmentRef]（§ 5.6），以 daemon 的记录为准
  | { type: "turn_start",       turn_id, message_id }         // message_id 为空串 = agent 自己开始的 turn（后台任务结束后），没有用户消息（daemon.md § 7.5）
  | { type: "text_delta",       text }                        // 流式文本
  | { type: "text_done" }
  | { type: "thinking_delta",   text }                        // text 可为空串
  | { type: "thinking_done",    duration_ms }
  | { type: "tool_call",        id, name, category, title, input, detail?, parent_id? }   // detail 按 category 归一化；parent_id 见下
  | { type: "tool_output_delta", id, text }                  // 命令输出流式，agent 支持才有
  | { type: "tool_result",      id, output, is_error, detail? }   // output 超过 64 KiB 时被截断，见 spec/daemon.md § 8.2
  | { type: "plan",             entries: [ { content, status } ] }
  | { type: "file_changed",     path, kind: created | modified | deleted }
  | { type: "permission",       request_id, kind, title, detail, options: [ { id, label, kind } ], expires_at_ms? }   // 同时也发 permission.request
  | { type: "permission_resolved", request_id, option_id?, by_device, answers?, message? }   // 问答类答复可以没有 option_id；answers / message 是设备答复时带的，daemon 自己答复的没有
  | { type: "turn_done",        turn_id, reason, usage? }
  | { type: "status",           state: queued | starting | running | waiting_permission | idle | ended | error | interrupted,
                                position?, queued_messages? }
  | { type: "settings_changed", settings }
  | { type: "title_changed",    title }
  | { type: "git",              action, branch, base?, count?, files?, note? }   // daemon 自己做的 git 操作（ADR-34）：worktree_created / worktree_failed / worktree_removed / worktree_kept / worktree_restored / worktree_missing / sync_up_to_date / sync_merged / sync_conflicts / sync_aborted；worktree_removed 的 note 是 branch_deleted | branch_kept，其他动作的 note 是英文说明
  | { type: "error",            code, message, recoverable }
```

`tool_call.category` 取 `shell | file_edit | file_read | search | web | subagent | mcp | other`，各 agent 工具名到 category 的映射见 [spec/daemon.md § 8](spec/daemon.md#8-事件翻译)。adapter 的职责就是把各 agent 的原生事件翻译成这个集合。

子 agent 内部的调用带 `tool_call.parent_id`：启动这个子 agent 的那次调用（`category = subagent`）的 `id`，子 agent 再启动子 agent 时一层层指上去；主 agent 自己的调用不带。后台启动的子 agent 和转到后台的命令（Claude Code Bash 的 `run_in_background`），调用会马上返回，这条 `tool_result` 带 `detail.background = true`；任务结束时同一个 `id` 再来一条 `tool_result`（不带这个标记），取代前一条。后台命令结束的那条，`output` 是 agent 给的一句总结，能取到退出码时带 `detail.exit_code`，退出码非 0 算 `is_error`。子 agent 结束的那条 `tool_result` 可以带 `detail.total_tokens`：agent 报的这个子 agent 一共用掉的 token，`turn_done.usage` 不含这部分。

错误响应统一为 JSON-RPC error，`data` 里带稳定的 `code`（snake_case）供客户端 i18n：`invalid`、`invalid_settings`（`data.field` 指出字段，如 `mode`、`env.BASH_ENV`）、`agent_unavailable`（`data.reason`）、`node_busy`、`busy`、`already_resolved`、`exists`、`not_found`、`forbidden`、`unsupported`、`protocol_mismatch`、`internal`。完整枚举见 `crates/protocol/src/error.rs`。

## 7. 对端离线

### 7.1 默认行为：hub 不排队

hub 默认**不暂存任何信封**。目标设备无活跃连接时，`data` 信封被丢弃，hub 向发送方回 `control.undeliverable { to, seq }`。

客户端侧：

- 收到 `peer_offline` 后，UI 明确显示节点离线，输入框提示但不禁用
- 用户此时发出的消息**留在客户端本地**，标记为「未发送」，可编辑可删除
- 收到 `peer_online` 后，客户端按顺序重发本地未发送的消息
- 节点离线时不发起 `blob.put_begin`——它需要 daemon 答复（Web 在节点不在线时不让添加附件）

这样 hub 不暂存任何消息字节；代价是手机发完锁屏后，要等下次打开客户端且节点在线时消息才会送出。

客户端离线时 daemon 有事件：daemon 本地持久化，客户端回来用 `session.history` 拉，任何情况下都不需要 hub 为客户端排队。

### 7.2 可选：hub 离线队列

hub 管理员可开启（`ANYCON_QUEUE_ENABLED=true`）。开启后：

- hub 对每个目标设备维护一个 `data` 信封队列，目标无连接时入队而不是回 `undeliverable`
- 限制：条数、总字节、TTL，任一超限则丢弃最旧的并向发送方发 `control.queue_overflow`
- 客户端可用 `control.queue_drop { seq }` 撤回自己排队中的信封
- 目标上线后，hub 按入队顺序推送，然后才转发实时帧
- 客户端 UI 把未送达的消息标记为「排队中」而非「未发送」

队列只为扛**短暂中断**（Mac 睡眠唤醒、几分钟断网），限制故意很小。

### 7.3 daemon 侧：突发消息合并

无论消息来自 hub 队列的集中推送还是客户端的本地重发，daemon 都可能在短时间内收到同一会话的多条 `session.send`。在 agent 开始处理第一条之前到达的后续消息**合并为一个 turn**：各条内容按顺序拼接（带分隔和时间戳）成一条用户消息，agent 读完全部上下文后响应一次。

原因：逐条喂给 agent 会产生 N 个 turn 的 token 开销，而且「先做 X」「算了不做 X」这种序列会被真的执行一遍。合并后 agent 看到的是完整语境，自然处理撤回和重复发送。

`permission.respond`、`session.cancel`、`workspace.*` 等不触发 agent turn 的消息按原顺序逐条处理，不合并。例外：又慢又不依赖顺序的只读 / 自带互斥的方法各自并发，不占同一客户端的串行队列——`agent.list { refresh }`、`agent.diagnose`、`node.doctor`、`session.git_status`、`session.diff`、`session.sync_base`（和 `send` 靠会话的「忙」标记互斥）、`workspace.list`（冷缓存最多等 3 秒）、`workspace.branches`、`workspace.worktrees`（`gh` 最长 20 秒、fetch 最长 120 秒，不能堵住同一客户端的 `permission.respond`）。

## 8. 控制消息（hub ↔ 设备）

`kind = control` 的信封由 hub 处理，不转发。设备发给 hub 的控制帧 `from` 为空串；hub 发给设备的控制帧 `to` 是该设备的 id。`auth.protocol_version` 和 hub 的协议版本最多差 1，否则 `auth_fail { version }`。

| 消息 | 方向 | 说明 |
|---|---|---|
| `auth` | 设备→hub | 连接后第一帧：`{ device_token?, protocol_version, client, preview_token? }`，`device_token` 为空 = 还没有身份的临时连接；带 `preview_token` 的是预览连接（§ 11.1，只在预览域名上接受） |
| `auth_ok` | hub→设备 | `{ device_id, server_time_ms, peers_online, quota? }`。`quota` 只在 `multi` 里对永久连接带：`{ devices, devices_max, daemons, daemons_max, connections, connections_max }`，是账号此刻的设备数、daemon 数、在线连接数（含这一条）和各自的上限；`single`、临时连接和预览连接没有 |
| `auth_fail` | hub→设备 | `{ reason: invalid \| revoked \| version \| quota \| protocol }`，随后 hub 关闭连接。`quota`：账号的在线连接数已到上限（`multi`，[spec/hub.md § 7.1](spec/hub.md#71-配额仅-multi)），关闭码 `1008`；预览连接则是账号的预览连接数到上限（§ 11.1） |
| `pair_begin { match_id }` / `pair_registered { expires_at_ms }` / `pair_join { match_id }` / `pair_matched { peer, role }` / `pair_confirm { peer }` / `pair_done { peer, device_id?, device_token? }` / `pair_failed { reason }` | 见 §4.1 | `pair_failed.reason`：`not_found \| timeout \| peer_gone \| rate_limited \| quota \| forbidden`。`quota`（`multi`）：账号今天的配对次数用完了（`pair_begin` / `pair_join` 时），或这次配对要新建的设备会超出设备数 / daemon 数上限（两边 `pair_confirm` 之后，一台设备都不建） |
| `enroll_begin { node }` / `enroll_started { enroll_id, node, expires_at_ms }` / `enroll_request { enroll_id, peer, expires_at_ms }` / `enroll_cancel { enroll_id }` / `enroll_failed { enroll_id?, reason }` | 见 §4.4 | 账号内接入。`enroll_begin` 只能由本账号的正式设备或带账号登录态的临时连接发；`enroll_request` 是 hub 发给 daemon 的；收尾复用 `pair_confirm` / `pair_done`，收尾失败发 `enroll_failed`（不是 `pair_failed`）；`enroll_cancel` 一个不存在的会话得到 `not_found`（这条回执走每秒一条的共享预算，可能被丢）。客户端发 `enroll_begin` 后要有自己的超时（回执限频时同一秒的第二条会被丢）。`enroll_failed.reason`：`forbidden \| not_found \| offline \| rate_limited \| quota \| busy \| timeout \| peer_gone \| cancelled`。`quota`（`multi`）同上：`enroll_begin` 时每日次数用完，或收尾时设备数 / daemon 数超限 |
| `revoke { device }` | 设备→hub | 删除某设备路由（需已配对关系） |
| `peer_online { device }` / `peer_offline { device }` | hub→设备 | 对端上下线（预览连接断开时 daemon 也收到 `peer_offline`） |
| `preview_peer { peer, label }` | hub→daemon | 本节点有一条预览连接接上来了（§ 11.1）。`peer` 是它的临时 id，`label` 是令牌绑定的 label；daemon 重连后对每条还在的预览连接再发一次 |
| `undeliverable { to, seq }` | hub→设备 | 对端离线且队列未开启，该信封已丢弃 |
| `dropped { seq, reason }` | hub→设备 | `reason: too_large \| not_paired \| rate_limited`。信封被 hub 丢弃；`too_large` 说明发送方没分片（bug），`not_paired` 说明 `(from, to)` 没有配对关系。`rate_limited` 在协议里有定义但 hub 不发（速率超限走 `backpressure` + `rate_limited`，不丢）。只发给已认证的发送方（含临时连接），限频，每连接每秒最多一条；收到它不要重发，重置该对端的传输 |
| `backpressure { to, seq }` | hub→设备 | `seq` 那封没有转发（目标发送缓冲满，或发送方超过速率预算）。`data` / `notify`：hub 把 `seq` 加进这对设备的被退回集合，之后只放行集合里最小的 `seq`，其余一律退回（§ 3.1 粘住规则）；发送方暂停、然后按升序原样重发所有被退回的 `seq`。`handshake` 被退回时不进集合，发送方按 `retry_after_ms` 延后再重新发起握手。不限频，每封被退回的信都有一条；这条回执本身发不出去（发送方不读 socket）时 hub 立即以 `4003` 断开它 |
| `rate_limited { retry_after_ms, to? }` | hub→设备 | 发送方超过速率预算；伴随 `backpressure` 出现。`to` 为空：每连接预算，**整条连接**暂停多久（取值 100 ms–30 s，默认预算下一般 1–2 秒）；`to` 有值：预览流量的账号预算（§ 11.5），只暂停发往这个对端的信封。回执本身限频（每秒至多一条）。「一直被限速退回、期间没有任何信封被转发」持续 10 秒会被以 `1008` 断开，老实按提示暂停再重发的发送方只会被匀速限流（[spec/hub.md § 3.4](spec/hub.md#34-速率)） |
| `queue_overflow { to, dropped_seqs }` | hub→设备 | 队列开启时，超限丢弃 |
| `queue_drop { seq }` | 设备→hub | 队列开启时，撤回自己排队中的信封 |
| `quota_update { quota }` | hub→设备 | 当前配额用量（`multi`），载荷同 `auth_ok.quota`。账号的设备被创建 / 吊销，或在线连接上线 / 下线时，发给该账号此刻在线的**其他**永久连接（刚认证的那条已在 `auth_ok` 里拿到）。每日配对次数不在里面，那一项用 HTTP 的 `GET /api/quota` 取（[spec/hub.md § 2.3](spec/hub.md#23-账号与设备-api)） |
| `rotate_token` / `token_rotated { device_token }` | 设备→hub / hub→设备 | 轮换设备 token（v0.2） |

控制消息本身也计入每连接的发送缓冲上限；一个不读 socket 的连接即使只收控制消息，也会被以 `4003 slow_consumer` 断开。

## 9. 版本与兼容

- 外层信封带 `v` 字段，hub 和设备协商；外层极少改动
- 内层 JSON-RPC 的 `node.info` 返回 daemon 的协议版本和能力位图；客户端据此决定能用哪些方法
- 兼容策略：hub 与 daemon、daemon 与客户端各自**兼容相邻一个 minor 版本**。CI 跑 `hub(N) × daemon(N-1)` 和 `daemon(N) × client(N-1)` 矩阵
- 0.x 阶段不承诺兼容，每个 minor 可以 break

## 10. 类型生成

```
crates/protocol (Rust, schemars)
        │
        ▼
   schema/*.json  ──► packages/protocol  (TS: json-schema-to-typescript)
                 ──► sdk/python/anycon/_types.py  (pydantic: datamodel-code-generator；v0.1.x，目前未实现)
```

生成产物提交进仓库，CI 有一步 `just gen && git diff --exit-code`。

## 11. 预览隧道

[ADR-39](decisions.md#adr-39-预览-dev-servernoise-隧道里的端口转发)。浏览器预览 origin（`<label>.<预览域名>`）里的 service worker（下称 P）和 daemon（D）之间的一条独立 Noise 传输，把 P 拦下的 HTTP 请求和 WebSocket 转给 D 本机的 `127.0.0.1:<端口>`。格式的真源是 `anycon_protocol::preview::tunnel`（TS：`packages/protocol/src/preview.ts`，字节级一致由 `tests/vectors/preview.json` 保证）。

```
预览页 P（service worker）          hub                              daemon D
  │─ wss://<label>.<预览域名>/v1/ws ─►│                                  │
  │─ control.auth { preview_token } ─►│ 令牌 → (账号, 节点 D, label)       │
  │◄─ control.auth_ok { device_id: P, peers_online: [D] }               │
  │                                  │── control.preview_peer { P, label } ─►│
  │══ handshake: 01 ‖ ticket_id ‖ NKpsk0 第一条 ═══════════════════════════►│ 查票据、核对 label 和端口
  │◄═ handshake: 00 ‖ NKpsk0 第二条（payload = PreviewHello）══════════════│
  │══ data: 隧道消息（ack ‖ 帧 …）══════════════════════════════════════►│ → 127.0.0.1:<端口>
  │◄═ data: 隧道消息 ══════════════════════════════════════════════════│
```

### 11.1 连接与身份

- 预览连接只在预览域名上接受：`wss://<label>.<预览域名>/v1/ws`，`Origin` 必须正好是这个预览 origin。第一帧 `auth { preview_token, protocol_version, client }`（不带 `device_token`，`client.kind = web`）。令牌不存在、过期、或绑定的不是这个主机名里的 label → `auth_fail { invalid }`；账号的预览连接数满 → `auth_fail { quota }`
- hub 给它一个临时 id P（和设备 id 同样格式），`auth_ok.peers_online` 里有 D（D 在线时）。之后它只能和 D 互发 `handshake` / `data`，别的一律不转发（控制消息忽略，发给别人的信封回 `dropped { not_paired }`）。D 上下线时它收到 `peer_online` / `peer_offline`
- hub 给 D 发 `preview_peer { peer: P, label }`；D 重连后，hub 对每条还在的预览连接再发一次。P 断开时 D 收到 `peer_offline { device: P }`
- 同一个预览 origin 的所有标签页共用一个 service worker，也就共用一条预览连接

### 11.2 握手

- 模式 `Noise_NKpsk0_25519_ChaChaPoly_BLAKE2s`。P 是 initiator：没有静态密钥，钉住 D 的静态公钥（`PreviewLaunch.node_public`）。D 是 responder。PSK = 票据密钥（32 字节），在第一条消息就混入。prologue = `"anycon-preview-v1"` ‖ `0x00` ‖ label ‖ `0x00` ‖ ticket_id（16 字节）：P 的 label 取自己的主机名，D 的取 `preview_peer.label`，hub 说谎只会让握手失败
- P → D 的 `handshake` 信封 body：`0x01`（版本）‖ ticket_id（16 字节）‖ Noise 第一条（payload 为空）
- D → P 的回复 body：`0x00` ‖ Noise 第二条，payload 是 JSON `PreviewHello { port, expires_at_ms, session_id? }`；或者 `0x01` ‖ 原因（UTF-8）：`unknown_ticket` `expired` `label_mismatch` `port_not_allowed` `disabled` `busy` `malformed`。拒绝回复不受 Noise 保护，只当提示用
- D 只处理已经收到 `preview_peer` 的 P 发来的握手；没收到就忽略（不回复）
- P 任何时候都可以重新发第一条消息。D 收到后丢掉 P 的旧传输和上面的全部 stream，重新开始
- 传输失步（解密失败、收到 `FRAGMENT` 帧体）：收方丢掉传输，发 `RESET` 帧体（`0x02`，§ 3）；P 收到或自己发现失步就重新握手

### 11.3 消息与帧

- `data` 信封 body = `0x00` ‖ 密文（§ 3 的 `WHOLE`）。一条隧道消息的明文上限 128 KiB，加密后仍小于一片，所以预览隧道**从不分片**
- 明文 = `ack`（u64 大端）‖ 帧\*。`ack` 是发送方此刻已经从对端收到并解密的传输消息条数（这条传输建立以来累计，不含握手）。只有 `ack`、没有帧的是**纯回执消息**
- 帧 = 类型（u8）‖ stream（u32 大端）‖ 载荷长度（u32 大端）‖ 载荷。一条消息里可以有多个帧，帧不跨消息。不认识的类型跳过

| 类型 | 名字 | 方向 | 载荷 |
|---|---|---|---|
| `0x01` | `REQUEST` | P→D | msgpack `PreviewRequestHead { method, path, headers: [[名, 值]…], websocket }`，打开一个 stream。`path` 以 `/` 开头、可带查询串；头名小写 |
| `0x02` | `RESPONSE` | D→P | msgpack `PreviewResponseHead { status, headers, encoding }`。`encoding = gzip` 时后面的 `DATA` 拼起来是 gzip 流，`headers` 里已经去掉 `content-encoding` / `content-length` |
| `0x03` | `DATA` | 双向 | 消息体的一段，≤ 64 KiB。占 stream 窗口 |
| `0x04` | `END` | 双向 | 空：这个方向的消息体结束 |
| `0x05` | `RESET` | 双向 | msgpack `PreviewReset { code, message? }`。`code`：`refused` `timeout` `too_large` `cancelled` `flow_control` `protocol` `limit` `internal`。立即终止 stream 的两个方向 |
| `0x06` | `WINDOW` | 双向 | u32 大端：给对端这个 stream 增加的窗口字节 |
| `0x07` | `WS_MESSAGE` | 双向 | 标志（u8：`0x01` 二进制、`0x02` 最后一段）‖ 数据。一条 WebSocket 消息可以拆成多段，最后一段带 `0x02`；拼起来 ≤ 16 MiB。数据字节占 stream 窗口 |
| `0x08` | `WS_CLOSE` | 双向 | 关闭码（u16 大端）‖ 原因（UTF-8，≤ 123 字节） |
| `0x09` | `GOAWAY` | D→P | stream 0，msgpack `PreviewGoaway { code, message? }`。`code`：`expired` `closed` `revoked` `disabled` `port_not_allowed` `shutdown`。D 随后丢掉这条传输 |

### 11.4 Stream

- stream id 由 P 分配：非零，严格递增。D 遇到不递增的 id → `RESET { protocol }`；同时打开的 stream 至多 256 个，超出 `RESET { limit }`
- **HTTP**：P 发 `REQUEST`、`DATA`\*（请求体）、`END`（没有请求体也要发，可以和 `REQUEST` 放同一条消息）。D 回 `RESPONSE`、`DATA`\*、`END`。两个方向都 `END` 了 stream 就结束
- **WebSocket**：P 发 `REQUEST { websocket: true }`（不发 `END`）。上游接受升级时 D 回 `RESPONSE { status: 101 }`，之后双方互发 `WS_MESSAGE`；一方发 `WS_CLOSE`，另一方回 `WS_CLOSE` 后结束。上游拒绝升级时 D 回普通的 `RESPONSE` + `DATA`\* + `END`
- `RESET` 之后到达的同一 stream 的帧一律忽略（包括 `WINDOW`）
- 头部 msgpack 编码后 ≤ 64 KiB，超了 `RESET { too_large }`

### 11.5 流控与回执

- **stream 窗口**：每个 stream 每个方向初始 256 KiB。`DATA` 和 `WS_MESSAGE` 的数据字节（不含标志字节）占窗口，收方消费后用 `WINDOW` 归还。超出窗口 → `RESET { flow_control }`
- **在途窗口（端到端回执）**：发送方给自己的每条传输消息从 0 编号，保留它的信封原文（含 `seq`），直到对端发来的 `ack` 大于这个编号才释放。未被回执的密文上限：D → P 8 MiB，P → D 2 MiB；满了就等回执，纯回执消息不受这个限制。这样在途窗口按往返时间周转，不受 § 3.1「保留 10 秒」的限制
- **何时回执**：每条发出去的消息都带当前的 `ack`。手里没有别的要发时：收到 ≥ 256 KiB 或 ≥ 32 条还没回执过的消息，立即发一条纯回执；否则最迟 50 ms 后发。收到纯回执本身不触发回执（避免来回）
- **和 hub 的背压**：照 § 3.1 的发送方规则——被退回的 `seq` 按升序原样重发，`rate_limited` 时暂停。`rate_limited.to` 有值（预览流量的账号预算）时只暂停发往这个对端的信封，别的对端照常。已经被回执的消息一定被 hub 转发过；被退回的 `seq` 原文已经不在了 → 重置传输（D 发 `RESET` 帧体，P 重新握手）
- `undeliverable { to: P }`：P 已经断开，D 丢掉它的传输

### 11.6 hub 看到什么

预览连接什么时候建立、断开，属于哪个账号、哪个节点、哪个 label，信封的大小和时间。看不到端口、路径、请求和响应的任何内容；令牌只存哈希。
