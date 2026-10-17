# @anycon/protocol

[简体中文](https://github.com/anycon-ai/anycon/blob/main/packages/protocol/README.zh-CN.md)

The TypeScript side of the [Anycon](https://github.com/anycon-ai/anycon) protocol: the layer with no networking and no state. The Web UI and [`@anycon/sdk`](https://www.npmjs.com/package/@anycon/sdk) both build on it. To pair with a node and drive sessions, use `@anycon/sdk`. This package is for code that works with the wire format directly.

```bash
npm install @anycon/protocol
```

The design docs linked below are in Chinese for now.

| Module | Contents |
|---|---|
| `src/generated.ts` | **Generated, do not edit.** `just gen-ts` generates it from `schema/protocol.json` (exported by `crates/protocol`): `Control`, `Event`, `SessionEvent`, the params / result types of each method, … |
| `src/envelope.ts` | msgpack codec for the outer envelope and control messages: `encodeEnvelope` / `decodeEnvelope`, `encodeControl` / `decodeControl`, `controlEnvelope`, `CONTROL_TYPES`. Also the fragmentation framing for `data` envelope bodies: `encodeFrames` / `decodeFrame` / `Reassembler` / `resetBody` (see below) |
| `src/rpc.ts` | JSON-RPC 2.0: `encodeRpc` / `decodeRpc`, `isRequest` / `isNotification` / `isResponse`, `AnyconRpcError`, and the transport-agnostic `RpcPeer` |
| `src/method-types.ts` | **Generated, do not edit.** `just gen-ts` runs `tools/gen-method-types.ts` on `schema/methods.json` (the method registry in `crates/protocol`) to produce `RequestMethods` (client → daemon requests: method name → `{ params, result }`) and `NotificationMethods` (daemon → client notifications: method name → `{ params }`). `tests/method-types.test.ts` asserts that it is in sync with the schema |
| `src/crypto.ts` | `loadCrypto()`: wraps the wasm build of `crates/crypto` (Noise XXpsk3 / IK, a transport that fragments long messages, `derivePairSecrets`, `generatePairCodeLong`, `fingerprint`), with `Uint8Array` in and out. Also pure-JS pairing-code helpers: `normalizePairCode` / `isValidPairCode` / `formatPairCode` / `extractPairCode` |
| `src/constants.ts` | `PROTOCOL_VERSION`, `ENVELOPE_VERSION`, `METHODS` (method name constants, including `PAIR_ACK`), `CLOSE_CODE`, `PAIR_CODE_ALPHABET`, `PAIR_CODE_LEN` / `PAIR_CODE_LEN_LONG`, `ERROR_CODE_NUMBERS` |

## Usage

```ts
import {
  METHODS,
  Reassembler,
  RpcPeer,
  controlEnvelope,
  decodeEnvelope,
  decodeFrame,
  encodeEnvelope,
  encodeFrames,
  loadCrypto,
  type Control,
  type SessionEvent,
} from '@anycon/protocol'

// Envelopes: body is a Uint8Array (msgpack bin), kind is a snake_case string
const frame = encodeEnvelope({ v: 1, from: myId, to: nodeId, kind: 'data', seq: 7, body })
const env = decodeEnvelope(bytes)

// Control messages: for messages to the hub, from and to are both empty strings.
// Pairing sends only match_id. The pairing code itself never leaves this device.
// derivePairSecrets is one Argon2id run (64 MiB × 3 passes; about 0.1–0.3 s in Node and
// 0.1–1 s in browsers, blocking the thread). It produces match_id and the PSK in one go, so
// don't call deriveMatchId / derivePsk separately (each is a full KDF run). Zero psk with fill(0) after use.
const crypto = await loadCrypto()
const { matchId, psk } = crypto.derivePairSecrets('7K3P-Q2MX')
ws.send(encodeEnvelope(controlEnvelope('', seq, { type: 'pair_join', match_id: matchId })))
// …run the XXpsk3 handshake with crypto.pairing('initiator', keypair, psk)…
psk.fill(0)

// JSON-RPC: one message = one Transport.encrypt = one msg_id. encodeFrames splits the ciphertext
// into one or more data envelopes (a short message is a single WHOLE frame).
const rpc = new RpcPeer({
  send: (bytes) => {
    for (const body of encodeFrames(transport.encrypt(bytes), msgId++ >>> 0)) sendData(body)
  },
})
// Receiving: const ct = reassembler.push(decodeFrame(env.body)); if (ct) rpc.handle(transport.decrypt(ct))
rpc.onNotification<SessionEvent>(METHODS.SESSION_EVENT, (e) => {
  /* … */
})
// Errors throw AnyconRpcError; .code is a stable snake_case string
const info = await rpc.request(METHODS.NODE_INFO, {}, { timeoutMs: 10_000 })
```

Notes:

- Pass `{}` for requests without parameters (not `undefined` / `null`). The Rust side's `Empty` is `{}`. `RpcPeer.request` already defaults to `{}`.
- `decodeControl` throws `UnknownControlError` for an unknown `type` (this can happen when the hub and daemon versions don't match). Callers should ignore that frame instead of disconnecting.
- Envelope key order, control message key order, and the omission of `skip_serializing_if` fields all match Rust's `rmp_serde::to_vec_named`. `tests/vectors` enforces this.

## Fragmenting data envelopes (ADR-30)

The hub limits the size of a single envelope (256 KiB by default), and one RPC (such as a page of history) can be larger, so Noise ciphertext is fragmented at the envelope layer. The first byte of a `data` envelope body is a flags byte. The byte format matches `mod frame` in `crates/protocol/src/envelope.rs` exactly:

| flags | Meaning | Payload |
|---|---|---|
| `0x00` WHOLE | The complete output of one `Transport.encrypt` | Ciphertext |
| `0x01` FRAGMENT | One fragment | `[u32 be msg_id][u16 be index][u16 be count]` + a slice of the ciphertext. Fragments of one `msg_id` are sent in `index` order and are not interleaved with other messages |
| `0x02` RESET | The sender found its transport out of sync and asks the peer to drop the transport and handshake again | None |

- `encodeFrames(ciphertext, msgId)` → `Uint8Array[]` (each is the body of one `data` envelope, with at most `MAX_FRAGMENT` = 200 KiB of payload per fragment; `msgId` is a u32, one per RPC). `decodeFrame(body)` → `DataFrame` (a malformed body throws `FrameError`, whose `.code` is `empty | unknown_flags | truncated | out_of_order | count_mismatch | too_large`). `new Reassembler()` (one per peer): `push(frame)` returns the whole ciphertext or `undefined`; the total length is capped at `MAX_MESSAGE` = 16 MiB; on error the reassembly in progress is discarded; `clear()` resets it. Also `resetBody()` and `pushBody(reassembler, body)`.
- `Reassembler` is strictly ordered, the same as in Rust: `index` must equal the next expected fragment; `index >= count` is `out_of_order`; changing `count` midway through a `msg_id` is `count_mismatch`; `count == 0` is `count_mismatch`; a single FRAGMENT with `count == 1` is accepted; after one message completes, another with the same `msg_id` must start again at `index 0`. `index` / `count` are u16, and comparisons don't overflow (`count = 65535` works through the last fragment).
- Noise nonces are sequential, so **losing or reordering any fragment desynchronizes the transport**. For a paired `(from, to)`, the hub only forwards an envelope or bounces it back with `backpressure { to, seq }`. It never drops one silently, and bounces are "sticky": once a seq is bounced, every later envelope is bounced too, until that seq is resent successfully ([docs/protocol.md](https://github.com/anycon-ai/anycon/blob/main/docs/protocol.md) § 3.1).
  - Senders keep the raw bytes of recently sent envelopes (capped by size, at least 8 MiB). On `backpressure`, pause that peer, wait `retry_after_ms` (from the latest `rate_limited`, or 100–300 ms plus jitter if there is none), resend every bounced seq **in ascending order, byte for byte** (never re-encrypt), then continue. If a bounced seq is no longer in the buffer, drop the Transport and handshake again.
  - `rate_limited { retry_after_ms }` is only a hint for how long to pause. It doesn't mean an envelope was dropped.
  - `dropped { seq, reason }` arrives at most once per connection per second and means something wrong was sent (`too_large` = not fragmented, or still over the limit after fragmenting; `not_paired` = the devices aren't paired as expected). **Don't resend on it.** Treat it as an error and handshake again.
  - Receivers drop the Transport and handshake again on a decryption failure, a reassembly error, or a RESET.

  The SDK's implementation is in `packages/sdk/src/link.ts`.
- Vectors: `tests/vectors/frames.json` (generated from Rust by `tools/gen-vectors`). The ciphertext of large messages is the deterministic pattern `(i % 251) as u8`. Only each fragment's header and length are stored; the TS side rebuilds the data and compares its encoding byte for byte. The file also has decode cases (including various bad bodies) and step-by-step `Reassembler` results (including state cleanup after errors).

## wasm (Noise)

```ts
const crypto = await loadCrypto() // Node / Bun: reads ../wasm/anycon_crypto_bg.wasm automatically
const crypto = await loadCrypto({ wasm: wasmUrl }) // Browser: let your bundler provide the .wasm URL, e.g. with Vite's `?url` import
```

The published package ships the wasm build in `wasm/`. In a repository checkout, `just gen-wasm` builds it into `packages/protocol/wasm/` (gitignored). If the build is missing, `loadCrypto()` throws `CryptoUnavailableError`, and the message says to run `just gen-wasm`. The package exports `@anycon/protocol/wasm/*`, so in the browser you can `import url from '@anycon/protocol/wasm/anycon_crypto_bg.wasm?url'` directly.

**Pairing KDF (ADR-29, revised).** `Crypto.derivePairSecrets(code) -> { matchId, psk }` runs `Argon2id(salt = "anycon-pair-v2", m = 64 MiB, t = 3, p = 1, out = 64)` once over the normalized pairing code. The hex of the first 8 bytes is the `matchId` sent to the hub for matching, and the last 32 bytes are the PSK. It takes about 0.1–0.3 s in Node and about 0.1–1 s in browsers, and it **blocks the thread** (a UI should show "Deriving key…" before calling it, so the browser can paint a frame first). The pairing path calls only this function. Don't also call `deriveMatchId` / `derivePsk` separately (each is a full KDF run, so that computes it twice). `psk` is a copy: zero it with `fill(0)` after use. Known vectors: `ABCD2345` → `matchId = 94d67d781538d997`, `psk = 23e8b63d…6012`; `23456789` → `f9f3c6c9940049ac` / `17d04f19…bb20` (`tests/crypto.test.ts`).

Keypairs in the `Crypto` interface are plain data (`{ public, secret }`) and are rebuilt inside wasm only for the duration of a handshake, so there are no wasm-bindgen ownership issues.
Roles: in pairing (XXpsk3) the joiner is `initiator` and the host (daemon) is `responder`. In reconnection (IK) the client is `initiator`. **The initiator writes first.**

## Consuming from a bundler

`exports` has two development conditions, `source` and `bun`, which point to `src/index.ts`, so inside the workspace you don't need to `build` first:

- tsconfig: `"customConditions": ["source"]` (the root `tsconfig.base.json` already sets it)
- Vite: `resolve: { conditions: ['source'] }`
- The published package and plain Node resolution use `dist/`

## Development

```bash
bun run --cwd packages/protocol test        # bun test (with the wasm build present, also runs real handshakes and the Argon2id vectors)
bun run --cwd packages/protocol test:node   # the same tests on Node (tsx)
bun run --cwd packages/protocol typecheck
bun run --cwd packages/protocol build       # tsc → dist/ (ESM + .d.ts)
bun run --cwd packages/protocol lint        # oxlint
```

### Test vectors

`tests/vectors/` holds the output of the real Rust encoder (`<name>.hex`) and the logical values (`manifest.json`). They cover every `Control` variant, envelopes with various `seq` / `body` lengths (empty, bin8, bin16, u64), and the four JSON-RPC shapes. `frames.json` holds the vectors for the data-envelope framing. On the TS side, decoding must yield the logical value, and re-encoding must match byte for byte.
They are generated by `tools/gen-vectors` (a standalone cargo project, not a workspace member):

```bash
just gen-vectors      # only needed when the protocol encoding changes; CI's gen-check diffs the result
```

`tests/constants.test.ts` parses the source of `crates/protocol/src/{methods,lib,control,error,envelope}.rs` (and `crates/crypto/src/lib.rs` for the pairing-code length)
and asserts that method names (including `pair.ack`), version numbers, pairing-code lengths, frame constants (flags / fragment size / reassembly limit), close codes, error code numbers, `Control` variants (including `pair_registered` and `dropped`), and `Kind` variants match this package (a drift guard).

### Generated method type table

`src/method-types.ts` isn't written by hand. `methods::REGISTRY` in `crates/protocol` is the single source of truth for "method name → params / result type names". `just gen-schema` exports it to `schema/methods.json`, then `just gen-ts` runs `tools/gen-method-types.ts` (`bun packages/protocol/tools/gen-method-types.ts [--check]`) to generate this file. Type names must be names exported by `generated.ts`; an unknown name is an error. After changing the registry, run `just gen` and commit `schema/` together with the two generated files. `serde_json::Value` is already `unknown` in the schema, so there are no hand-written workarounds.

## License

[Apache-2.0](https://github.com/anycon-ai/anycon/blob/main/LICENSE)
