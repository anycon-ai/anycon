# @anycon/protocol

[English](https://github.com/anycon-ai/anycon/blob/main/packages/protocol/README.md)

anycon 协议的 TypeScript 侧：无网络、无状态的那一层。Web UI 和 `@anycon/sdk` 共用。

| 模块 | 内容 |
|---|---|
| `src/generated.ts` | **生成**，不要手改。`just gen-ts` 从 `schema/protocol.json`（`crates/protocol` 导出）生成：`Control`、`Event`、`SessionEvent`、各方法的 params / result…… |
| `src/envelope.ts` | 外层信封与控制消息的 msgpack 编解码：`encodeEnvelope` / `decodeEnvelope`、`encodeControl` / `decodeControl`、`controlEnvelope`、`CONTROL_TYPES`；以及 `data` 信封 body 的分片框架 `encodeFrames` / `decodeFrame` / `Reassembler` / `resetBody`（见下） |
| `src/rpc.ts` | JSON-RPC 2.0：`encodeRpc` / `decodeRpc`、`isRequest` / `isNotification` / `isResponse`、`AnyconRpcError`、与传输无关的 `RpcPeer` |
| `src/method-types.ts` | **生成**，不要手改。`just gen-ts` 用 `tools/gen-method-types.ts` 从 `schema/methods.json`（`crates/protocol` 的方法注册表）生成：`RequestMethods`（C→D 请求：方法名 → `{ params, result }`）、`NotificationMethods`（D→C 通知：方法名 → `{ params }`）。`tests/method-types.test.ts` 断言它与 schema 同步 |
| `src/crypto.ts` | `loadCrypto()`：封装 `crates/crypto` 的 wasm 产物（Noise XXpsk3 / IK、分片传输、`derivePairSecrets`、`generatePairCodeLong`、`fingerprint`），`Uint8Array` 进出；纯 JS 的短码辅助 `normalizePairCode` / `isValidPairCode` / `formatPairCode` / `extractPairCode` |
| `src/constants.ts` | `PROTOCOL_VERSION`、`ENVELOPE_VERSION`、`METHODS`（方法名常量，含 `PAIR_ACK`）、`CLOSE_CODE`、`PAIR_CODE_ALPHABET`、`PAIR_CODE_LEN` / `PAIR_CODE_LEN_LONG`、`ERROR_CODE_NUMBERS` |

## 用法

```ts
import {
  METHODS, RpcPeer, Reassembler, encodeEnvelope, decodeEnvelope, controlEnvelope,
  encodeFrames, decodeFrame, loadCrypto,
  type Control, type SessionEvent,
} from '@anycon/protocol'

// 信封：body 是 Uint8Array（msgpack bin），kind 是 snake_case 字符串
const frame = encodeEnvelope({ v: 1, from: myId, to: nodeId, kind: 'data', seq: 7, body })
const env = decodeEnvelope(bytes)

// 控制消息：发给 hub 的 from / to 都填空串。配对只发 match_id，短码本身不出本机。
// derivePairSecrets 是一次 Argon2id（64 MiB × 3 轮，Node 约 0.1–0.3 s、浏览器约 0.1–1 s，同步阻塞）：
// match_id 和 PSK 一次算出，别分别调 deriveMatchId / derivePsk（各是一次完整 KDF）；psk 用完 fill(0)
const crypto = await loadCrypto()
const { matchId, psk } = crypto.derivePairSecrets('7K3P-Q2MX')
ws.send(encodeEnvelope(controlEnvelope('', seq, { type: 'pair_join', match_id: matchId })))
// …用 psk 做 crypto.pairing('initiator', keypair, psk) 的 XXpsk3 握手…
psk.fill(0)

// JSON-RPC：一条消息 = 一次 Transport.encrypt = 一个 msg_id；密文按 frame 切成若干 data 信封（短消息就是一个 WHOLE）
const rpc = new RpcPeer({
  send: (bytes) => {
    for (const body of encodeFrames(transport.encrypt(bytes), msgId++ >>> 0)) sendData(body)
  },
})
// 收：const ct = reassembler.push(decodeFrame(env.body)); if (ct) rpc.handle(transport.decrypt(ct))
rpc.onNotification<SessionEvent>(METHODS.SESSION_EVENT, (e) => { /* … */ })
const info = await rpc.request(METHODS.NODE_INFO, {}, { timeoutMs: 10_000 })   // 错误抛 AnyconRpcError，.code 是稳定 snake_case
```

注意：

- 请求没有参数时传 `{}`（不是 `undefined` / `null`），Rust 侧的 `Empty` 是 `{}`；`RpcPeer.request` 已经默认成 `{}`。
- `decodeControl` 对未知 `type` 抛 `UnknownControlError`（hub / daemon 版本错位时可能出现），调用方应忽略这一帧而不是断连。
- 信封键序、控制消息键序、`skip_serializing_if` 字段的省略都与 Rust 的 `rmp_serde::to_vec_named` 一致，由 `tests/vectors` 守着。

## data 信封的分片（ADR-30）

hub 对单个信封有大小上限（默认 256 KiB），一条 RPC（如一页历史）可能更大，所以 Noise 密文在信封层分片。`data` 信封 body 的第一个字节是 flags，字节格式与 `crates/protocol/src/envelope.rs` 的 `mod frame` 逐字节一致：

| flags | 含义 | 载荷 |
|---|---|---|
| `0x00` WHOLE | 一整条 `Transport.encrypt` 的输出 | 密文 |
| `0x01` FRAGMENT | 分片 | `[u32 be msg_id][u16 be index][u16 be count]` + 密文切片。同一 `msg_id` 的分片按 `index` 顺序、不与其他消息交错地发送 |
| `0x02` RESET | 发送方发现传输失步，请对端丢弃传输并重新握手 | 无 |

- `encodeFrames(ciphertext, msgId)` → `Uint8Array[]`（每个是一个 `data` 信封的 body，每片载荷 ≤ `MAX_FRAGMENT` = 200 KiB；`msgId` 是 u32，每条 RPC 一个）、`decodeFrame(body)` → `DataFrame`（格式错抛 `FrameError`，`.code` 是 `empty | unknown_flags | truncated | out_of_order | count_mismatch | too_large`）、`new Reassembler()`（每个对端一个；`push(frame)` 返回整条密文或 `undefined`；总长上限 `MAX_MESSAGE` = 16 MiB；出错时进行中的重组被丢弃；`clear()`）、`resetBody()`、`pushBody(reassembler, body)`。
- `Reassembler` 严格按序，与 Rust 一致：`index` 必须等于期望的下一片；`index >= count` 报 `out_of_order`；同一 `msg_id` 中途改 `count` 报 `count_mismatch`；`count == 0` 报 `count_mismatch`；`count == 1` 的单片 FRAGMENT 也能收；一条收完后同一 `msg_id` 再来要从 `index 0` 开始；`index` / `count` 是 u16，比较不会溢出（`count = 65535` 走到最后一片也正常）。
- Noise 的 nonce 是顺序的，**任何一片丢失或乱序都意味着传输失步**。hub 对已配对的 `(from, to)` 只会转发或退回 `backpressure { to, seq }`，不会静默丢弃，并且「粘住」（一个 seq 被退回后，后续信封一律退回，直到它被重发成功，docs/protocol.md § 3.1）。发送方：保留最近发出的信封原文（按字节封顶，≥ 8 MiB）；收到 `backpressure` 后暂停该对端，等 `retry_after_ms`（最近一条 `rate_limited` 给的，没有就 100–300 ms + 抖动），把所有被退回的 seq **按升序原样重发**（不能重新加密），再继续；被退回的 seq 已不在缓冲里 → 丢 Transport 重新握手。`rate_limited { retry_after_ms }` 只是暂停多久的提示，不再意味着有信封被丢。`dropped { seq, reason }` 每连接每秒至多一条，是「发错东西」的信号（`too_large` = 没分片 / 分片后仍超限，`not_paired` = 配对关系不对），**不要据此重发**，当作错误处理并重新握手。接收方解密失败、重组出错、收到 RESET 都要丢 Transport 并重新握手。SDK 的实现见 `packages/sdk/src/link.ts`。
- 向量：`tests/vectors/frames.json`（`tools/gen-vectors` 用 Rust 生成）。大消息的密文约定为 `(i % 251) as u8` 的确定性图样，只存各片的头和长度，TS 侧还原后逐字节比对 encode；另有 decode（含各种坏 body）和 Reassembler 的逐步结果（含出错后的状态清理）。

## wasm（Noise）

```ts
const crypto = await loadCrypto()                    // Node / Bun：自动读 ../wasm/anycon_crypto_bg.wasm
const crypto = await loadCrypto({ wasm: wasmUrl })   // 浏览器：把 .wasm 的 URL 交给打包器，例如 Vite 的 `?url` 导入
```

产物由 `just gen-wasm` 生成到 `packages/protocol/wasm/`（已 gitignore）。产物不存在时 `loadCrypto()` 抛 `CryptoUnavailableError`，消息里写明要跑 `just gen-wasm`。
包导出 `@anycon/protocol/wasm/*`，浏览器侧可以直接 `import url from '@anycon/protocol/wasm/anycon_crypto_bg.wasm?url'`。

**配对 KDF（ADR-29 修订）**：`Crypto.derivePairSecrets(code) -> { matchId, psk }` 对规范化短码做一次 `Argon2id(salt = "anycon-pair-v2", m = 64 MiB, t = 3, p = 1, out = 64)`：前 8 字节的 hex 是发给 hub 撮合的 `matchId`，后 32 字节是 PSK。耗时 Node / 本机 wasm 约 0.1–0.3 s，浏览器约 0.1–1 s，**同步占用线程**（UI 要先显示「正在派生密钥…」再调用，让浏览器先画一帧）。配对路径只调这一个，不要再分别调 `deriveMatchId` / `derivePsk`（各是一次完整 KDF，等于算两遍）；`psk` 是拷贝，用完 `fill(0)`。已知向量：`ABCD2345` → `matchId = 94d67d781538d997`、`psk = 23e8b63d…6012`；`23456789` → `f9f3c6c9940049ac` / `17d04f19…bb20`（`tests/crypto.test.ts`）。

`Crypto` 接口里的密钥对是纯数据（`{ public, secret }`），握手时才在 wasm 里临时重建，没有 wasm-bindgen 的所有权问题。
角色：配对（XXpsk3）joiner 是 `initiator`、host（daemon）是 `responder`；重连（IK）客户端是 `initiator`。**initiator 先写**。

## 命令

```bash
bun run --cwd packages/protocol test        # bun test（wasm 存在时多跑真实握手和 Argon2id 向量）
bun run --cwd packages/protocol test:node   # 同一批用例在 Node 上跑（tsx）
bun run --cwd packages/protocol typecheck
bun run --cwd packages/protocol build       # tsc → dist/（ESM + .d.ts）
bun run --cwd packages/protocol lint        # oxlint
```

## 测试向量

`tests/vectors/` 里是 Rust 真实编码器的输出（`<name>.hex`）和逻辑值（`manifest.json`），覆盖全部 `Control` 变体、
信封的各种 `seq` / `body` 长度（空、bin8、bin16、u64）、JSON-RPC 的四种形状；`frames.json` 是 data 信封分片框架的向量。TS 侧解码后必须等于逻辑值，再编码必须逐字节一致。
由 `tools/gen-vectors`（独立的 cargo 工程，不是 workspace 成员）生成：

```bash
just gen-vectors      # 协议编码变了才需要；CI 的 gen-check 会 diff
```

`tests/constants.test.ts` 解析 `crates/protocol/src/{methods,lib,control,error,envelope}.rs` 的源码，
断言方法名（含 `pair.ack`）、版本号、短码长度、frame 常量（flags / 片大小 / 重组上限）、close code、错误码数值、`Control` 变体（含 `pair_registered`、`dropped`）、`Kind` 变体与这里一致（漂移守卫）。

## 方法类型表（生成）

`src/method-types.ts` 不手写：`crates/protocol` 的 `methods::REGISTRY` 是「方法名 → params / result 类型名」的唯一真源，`just gen-schema` 导出到 `schema/methods.json`，`just gen-ts` 再跑 `tools/gen-method-types.ts`（`bun packages/protocol/tools/gen-method-types.ts [--check]`）生成这个文件。类型名必须是 `generated.ts` 导出的名字，找不到就报错。改了注册表之后 `just gen`，提交 `schema/` 和这两个生成文件。`serde_json::Value` 在 schema 里已经是 `unknown`，没有手写的绕过。

## 在打包器里消费

`exports` 带两个开发条件：`source` 和 `bun`，指向 `src/index.ts`，所以 workspace 内不用先 `build`：

- tsconfig：`"customConditions": ["source"]`（根 `tsconfig.base.json` 已经带）
- Vite：`resolve: { conditions: ['source'] }`
- 发布 / 普通 Node 解析走 `dist/`
