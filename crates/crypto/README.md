# anycon-crypto

Noise 握手与传输加密。一份 Rust 实现，三种用法：Rust crate（daemon / cli）、wasm（web、`@anycon/protocol`、sdk）、Python（`python` feature，只有骨架）。

公开 API 的文档在 `src/lib.rs` 顶部；这里只记「怎么跑」和「消费方要知道的约定」。

## 协议约定

| | 配对 | 重连 |
|---|---|---|
| 模式 | `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` | `Noise_IK_25519_ChaChaPoly_BLAKE2s` |
| initiator | joiner（新设备） | 客户端（已知 daemon 公钥） |
| responder | host（daemon） | daemon |
| 消息数 | 3（I→R、R→I、I→R） | 2（I→R、R→I） |
| 谁先写 | **initiator** | **initiator** |
| 写完即完成 | initiator 写完第 3 条；responder 读完第 3 条 | responder 写完第 2 条；initiator 读完第 2 条 |

- **配对 KDF（match_id + PSK）**：`derive_pair_secrets(code)` 做一次 `Argon2id(password = normalize_code(code), salt = "anycon-pair-v2", m = 64 MiB, t = 3, p = 1, out = 64)`：前 8 字节的 hex 是给 hub 撮合的 `match_id`，后 32 字节是 PSK。`derive_psk` / `derive_match_id` 各自是一次完整 KDF，配对路径只调 `derive_pair_secrets`。本机 release 约 0.1–0.3 s、wasm 约 0.1–1 s；之所以这么贵，是因为短码是两端唯一的共享秘密、熵低，而 hub 拿到 `match_id` 后可以离线穷举（ADR-29）。短码必须规范化后恰好 8 或 16 个字母表内字符（`23456789ABCDEFGHJKMNPQRSTUVWXYZ`），否则 `InvalidCode`；debug 构建下根 `Cargo.toml` 给 argon2 开了 opt-level 3，否则一次要十几秒。
- **PSK 到第 3 条才混入**：第 1、2 条的 payload 对主动中间人（hub）不保密也不可信。需要在配对期间交换的信息放第 3 条的 payload 里，或握手完成后走 Transport。第 3 条验证失败（短码不对）时 responder 的 `read` 返回 `Decrypt`（wasm 里是 `decrypt failed`）。
- **对方静态公钥**：XX initiator 读完第 2 条后有；XX responder 读完第 3 条后有；IK initiator 一开始就有；IK responder 读完第 1 条后有。daemon 必须在 IK 读完第 1 条后核对 `remote_public()` 是否在 peers 里（或者创建 responder 时把期望公钥传给 `reconnect(Responder, kp, Some(&pk))`，不符时 `read` 返回 `InvalidKey`）。
- **握手出错后该 `Handshake` 作废**：`read` 失败（包括公钥不符）后，所有后续调用都报错，重新建一个。
- **传输分片**：`Transport::encrypt` 把明文按 65519 字节切片，每片输出 `[u16 大端 密文长度][密文]`（密文长度 = 片长 + 16），各片直接拼接。空明文是一片（长度字段 16，只有 tag）。`decrypt` 先整体校验边界：没有任何分片、长度头不全、长度 < 16、长度超出剩余字节、尾部多余字节，都是 `Malformed`；之后逐片解密，失败为 `Decrypt`。**一条 `encrypt` 的输出必须整体交给一次 `decrypt`**（一个 `data` 信封 = 一次）。
- **nonce 是隐式计数器**：丢包、重放、乱序都会 `Decrypt`。失败的 `decrypt` 不推进接收计数器，所以通道仍可继续收合法消息。一个 `Transport` 只能顺序使用（`&mut self`）。
- 密钥：`Keypair.secret` 在 `Drop` 时清零（`Clone` 出来的副本同样）。`Keypair::generate` 用 `OsRng`（wasm 下是 `crypto.getRandomValues`），系统随机数不可用时 panic。
- 指纹：BLAKE2s-256(公钥) 前 8 字节，小写 hex，`:` 分隔。

## 测试

```bash
cargo test -p anycon-crypto                                       # 单元测试（握手、分片、短码、KAT）
cargo clippy -p anycon-crypto --all-targets -- -D warnings
cargo check -p anycon-crypto --features wasm --target wasm32-unknown-unknown
cargo check -p anycon-crypto --features python                    # 骨架能编译即可，不构建 wheel
```

match_id / PSK 的已知答案是用 Node 24 自带的 `crypto.argon2Sync` 独立算的（`ABCD2345` → `94d67d781538d997` / `23e8b63d…6012`），指纹的已知答案用 Python `hashlib.blake2s`（见 `src/tests.rs`、`examples/vectors.rs`），都不是 Rust 自己对自己。

## wasm 构建

产物到 `packages/protocol/wasm/`（已 gitignore）：`anycon_crypto.js`、`anycon_crypto_bg.wasm`、`anycon_crypto.d.ts`（`--target web`，用前先 `await init()` 或 `initSync({ module })`）。

```bash
just gen-wasm                      # wasm-pack（bunx wasm-pack@0.13）
crates/crypto/scripts/build-wasm.sh   # 不依赖 wasm-pack / npm / GitHub 的等价做法
```

`build-wasm.sh` 需要 `rustup target add wasm32-unknown-unknown` 和与 `Cargo.lock` 里 `wasm-bindgen` 版本一致的 CLI：
`cargo install wasm-bindgen-cli --version <版本> --locked`。脚本在版本不符时会提示确切命令。

无需任何 `RUSTFLAGS`：snow 自带的 `getrandom 0.3` 后端被关掉了，RNG 走 `rand 0.8` 的 `OsRng`（`getrandom 0.2` + `js` feature），所以 `cargo check --target wasm32-unknown-unknown` 不需要 `--cfg getrandom_backend`。

## 互通测试（wasm ↔ 原生 Rust）

```bash
just gen-wasm                      # 或 crates/crypto/scripts/build-wasm.sh
bun crates/crypto/tests/interop.mjs   # 在 crates/crypto 目录下是 `bun tests/interop.mjs`
```

脚本会自己 `cargo build` 两个 example，然后：

1. `examples/vectors.rs` 生成 `vectors.json`（已知答案 + 一份 Rust↔Rust 的配对录像），wasm 逐项核对，并以 host 身份读入录像里的第一条握手消息。
2. `examples/interop_peer.rs` 是一个用 stdin/stdout 遥控的原生 Rust 对端，wasm 与它**实时**跑完整的配对（两个方向各一次）、错误短码、重连（两个方向）、100 KiB / 250 KB 分片、空明文、篡改 / 截断 / 重放。握手消息一条条在两边的实现之间传递，所以任何字节层面的不一致都会暴露。
3. wasm 自身的错误处理（非法 role、非法长度、`intoTransport` 消费握手对象……）。

环境变量：`ANYCON_VECTORS_OUT=<dir>` 把 `vectors.json` 留在 `<dir>`；`ANYCON_INTEROP_SKIP_BUILD=1` 跳过 `cargo build`。

单独生成向量：`cargo run -q -p anycon-crypto --example vectors -- out.json`（或 `ANYCON_VECTORS_OUT=dir`）。

> 为什么握手没有做成纯静态 fixture：Noise 握手每条消息都依赖双方各自的临时密钥，对端的回复没法预先录好，只能让两边活着对话。静态向量里能回放的只有「读第一条消息」。

## wasm 导出（JS 侧）

```ts
WasmKeypair.generate() / WasmKeypair.fromSecret(secret)      // .public / .secret (Uint8Array)
WasmHandshake.pairing("initiator" | "responder", kp, psk)
WasmHandshake.reconnect("initiator" | "responder", kp, remotePublic?)
  .isMyTurn() .write(payload) .read(message) .isFinished() .remotePublic() .intoTransport()
WasmTransport.encrypt(pt) .decrypt(ct) .remotePublic()
derivePairSecrets(code) → { matchId, psk }（用完 .free()）  derivePsk(code)  deriveMatchId(code)
normalizeCode(code) generatePairCode() generatePairCodeLong() fingerprint(public)
```

错误都是 JS `Error`，`message` 是 Rust 错误文本：`decrypt failed`、`malformed frame`、`invalid pair code`、`invalid key`、`not your turn to write`、`handshake not finished`、`handshake already finished`、`handshake aborted`、`noise: …`。`intoTransport()` 消费握手对象；用完的对象调 `.free()`（`Symbol.dispose` 也支持）。`secret` getter 返回拷贝，用完请自行清零。
