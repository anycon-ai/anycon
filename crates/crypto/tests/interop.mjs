// wasm ↔ 原生 Rust 互通测试。运行：`bun crates/crypto/tests/interop.mjs`（仓库根目录或 crates/crypto 下都行）。
//
// 前提：`packages/protocol/wasm/` 里已有 wasm 产物（`just gen-wasm` 或 `crates/crypto/scripts/build-wasm.sh`）。
// 脚本会自己 `cargo build` 两个 example（`vectors`、`interop_peer`），不依赖网络。
//
// 做三件事：
//   1. 用 `vectors` 生成的 JSON 核对 wasm 的已知答案（PSK、指纹、规范化、非法短码、X25519），
//      并让 wasm 以 host 身份读入 Rust 录制的第一条配对握手消息。
//   2. 起 `interop_peer`（原生 Rust，stdin/stdout 文本协议），让 wasm 与它实时跑完整的
//      配对（XXpsk3，两个方向的角色各一次）、错误短码、重连（IK，两个方向）和分片传输。
//   3. wasm API 本身的错误处理（非法 role / 非法长度 / 握手对象被消费 ……）。
//
// 环境变量：
//   ANYCON_VECTORS_OUT        指定目录则把 vectors.json 留在那里（默认放系统临时目录）
//   ANYCON_INTEROP_SKIP_BUILD 设为 1 跳过 cargo build（已经编好时用）

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const crateDir = path.resolve(here, "..");
const repoRoot = path.resolve(crateDir, "../..");
const wasmDir = path.join(repoRoot, "packages/protocol/wasm");

// ---------- 小工具 ----------

const toHex = (u8) => Buffer.from(u8).toString("hex");
const fromHex = (h) => new Uint8Array(Buffer.from(h === "-" ? "" : h, "hex"));
const text = (s) => new TextEncoder().encode(s);
const untext = (u8) => new TextDecoder().decode(u8);
const pattern = (n) => Uint8Array.from({ length: n }, (_, i) => ((i * 31) ^ (i >> 8)) & 0xff);

function sh(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: "utf8", timeout: 900_000, cwd: repoRoot, ...opts });
  if (r.error || r.status !== 0) {
    throw new Error(`${cmd} ${args.join(" ")} failed (${r.error?.message ?? `exit ${r.status}`})\n${r.stdout ?? ""}${r.stderr ?? ""}`);
  }
  return r.stdout;
}

/** 断言 fn 抛出的错误 message 满足 re。 */
function throwsMsg(fn, re, what = "") {
  try {
    fn();
  } catch (e) {
    assert.match(String(e?.message ?? e), re, what);
    return;
  }
  assert.fail(`expected an error matching ${re} ${what}`);
}

// ---------- 载入 wasm ----------

const wasmJs = path.join(wasmDir, "anycon_crypto.js");
const wasmBin = path.join(wasmDir, "anycon_crypto_bg.wasm");
if (!existsSync(wasmJs) || !existsSync(wasmBin)) {
  console.error(`missing wasm artifacts in ${wasmDir}\nrun \`just gen-wasm\` (or crates/crypto/scripts/build-wasm.sh) first`);
  process.exit(2);
}
const wasm = await import(wasmJs);
wasm.initSync({ module: readFileSync(wasmBin) });
const { WasmKeypair, WasmHandshake, WasmTransport, derivePsk, normalizeCode, generatePairCode, fingerprint } = wasm;

// ---------- 构建并运行 Rust 侧 ----------

if (process.env.ANYCON_INTEROP_SKIP_BUILD !== "1") {
  sh("cargo", ["build", "-q", "-p", "anycon-crypto", "--example", "vectors", "--example", "interop_peer"]);
}
const targetDir = JSON.parse(sh("cargo", ["metadata", "--no-deps", "--format-version", "1"], { maxBuffer: 64 << 20 })).target_directory;
const exe = (name) => path.join(targetDir, "debug/examples", name + (process.platform === "win32" ? ".exe" : ""));

const outDir = process.env.ANYCON_VECTORS_OUT
  ? (mkdirSync(process.env.ANYCON_VECTORS_OUT, { recursive: true }), process.env.ANYCON_VECTORS_OUT)
  : mkdtempSync(path.join(tmpdir(), "anycon-vectors-"));
const vectorsPath = path.join(outDir, "vectors.json");
sh(exe("vectors"), [vectorsPath]);
const vectors = JSON.parse(readFileSync(vectorsPath, "utf8"));

/** 遥控 interop_peer 的一次会话。 */
class Peer {
  constructor() {
    this.proc = spawn(exe("interop_peer"), [], { stdio: ["pipe", "pipe", "inherit"] });
    this.lines = [];
    this.waiters = [];
    this.closed = false;
    createInterface({ input: this.proc.stdout }).on("line", (l) => {
      const w = this.waiters.shift();
      if (w) w.resolve(l);
      else this.lines.push(l);
    });
    this.proc.on("exit", () => {
      this.closed = true;
      for (const w of this.waiters.splice(0)) w.reject(new Error("interop_peer exited"));
    });
  }
  async send(line) {
    const reply = new Promise((resolve, reject) => {
      const queued = this.lines.shift();
      if (queued !== undefined) resolve(queued);
      else this.waiters.push({ resolve, reject });
    });
    this.proc.stdin.write(line + "\n");
    const timer = setTimeout(() => this.proc.kill(), 60_000);
    try {
      return await reply;
    } finally {
      clearTimeout(timer);
    }
  }
  /** 期望 `ok ...`，返回 `...` 部分。 */
  async ok(line) {
    const r = await this.send(line);
    assert.ok(r === "ok" || r.startsWith("ok "), `peer: "${line.slice(0, 60)}" -> ${r.slice(0, 200)}`);
    return r.slice(3);
  }
  /** 期望 `err ...`，返回 `...` 部分。 */
  async err(line) {
    const r = await this.send(line);
    assert.ok(r.startsWith("err "), `peer: "${line.slice(0, 60)}" -> expected err, got ${r.slice(0, 200)}`);
    return r.slice(4);
  }
  async close() {
    if (!this.closed) {
      this.proc.stdin.end("quit\n");
      await new Promise((r) => this.proc.once("exit", r));
    }
  }
}

async function withPeer(fn) {
  const peer = new Peer();
  try {
    await fn(peer);
  } finally {
    await peer.close();
  }
}

// ---------- 用例 ----------

const results = [];
async function test(name, fn) {
  try {
    await fn();
    results.push({ name, ok: true });
    console.log(`  ok   ${name}`);
  } catch (e) {
    results.push({ name, ok: false });
    console.log(`  FAIL ${name}\n${String(e?.stack ?? e).replace(/^/gm, "       ")}`);
  }
}

/** 通过 Transport 双向各来一轮：wasm ↔ peer。 */
async function exchange(wt, peer, label) {
  const a = text(`wasm says hi (${label})`);
  assert.equal(untext(fromHex(await peer.ok(`decrypt ${toHex(wt.encrypt(a))}`))), untext(a));
  const b = text(`rust says hi (${label})`);
  assert.equal(untext(wt.decrypt(fromHex(await peer.ok(`encrypt ${toHex(b)}`)))), untext(b));
}

console.log("known answers (vectors.json)");

await test("derivePsk matches independent known answers", () => {
  for (const { code, psk } of vectors.kat.derive_psk) assert.equal(toHex(derivePsk(code)), psk, code);
});

await test("derivePsk accepts lowercase / dash / spaces, same PSK", () => {
  const want = toHex(derivePsk("ABCD2345"));
  for (const c of ["ABCD-2345", "abcd-2345", " abcd 2345 "]) assert.equal(toHex(derivePsk(c)), want, c);
});

await test("derivePsk rejects invalid codes with 'invalid pair code'", () => {
  for (const c of vectors.kat.invalid_codes) throwsMsg(() => derivePsk(c), /^invalid pair code$/, JSON.stringify(c));
});

await test("normalizeCode", () => {
  for (const { input, output } of vectors.kat.normalize) assert.equal(normalizeCode(input), output);
});

await test("fingerprint matches known answers", () => {
  for (const { public: pub, fingerprint: fp } of vectors.kat.fingerprint) assert.equal(fingerprint(fromHex(pub)), fp);
  throwsMsg(() => fingerprint(new Uint8Array(31)), /invalid key/);
});

await test("generatePairCode format and alphabet", () => {
  const alphabet = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";
  const seen = new Set();
  for (let i = 0; i < 300; i++) {
    const code = generatePairCode();
    assert.match(code, /^[2-9A-HJKMNP-Z]{4}-[2-9A-HJKMNP-Z]{4}$/, code);
    for (const ch of code.replace("-", "")) assert.ok(alphabet.includes(ch));
    derivePsk(code);
    seen.add(code);
  }
  assert.ok(seen.size > 290, "codes should not repeat");
});

await test("WasmKeypair.fromSecret reproduces RFC 7748 public key", () => {
  const { secret, public: pub } = vectors.kat.x25519;
  const kp = WasmKeypair.fromSecret(fromHex(secret));
  assert.equal(toHex(kp.public), pub);
  assert.equal(toHex(kp.secret), secret);
  kp.free();
});

await test("WasmKeypair.generate is random and self-consistent", () => {
  const a = WasmKeypair.generate();
  const b = WasmKeypair.generate();
  assert.notEqual(toHex(a.secret), toHex(b.secret));
  assert.equal(a.public.length, 32);
  assert.equal(toHex(WasmKeypair.fromSecret(a.secret).public), toHex(a.public));
  throwsMsg(() => WasmKeypair.fromSecret(new Uint8Array(31)), /invalid key/);
});

await test("recorded Rust pairing transcript: wasm host reads m1 and gets the payload", () => {
  const p = vectors.pairing;
  const host = WasmKeypair.fromSecret(fromHex(p.host.secret));
  assert.equal(toHex(host.public), p.host.public);
  const psk = derivePsk(p.code);
  assert.equal(toHex(psk), p.psk);
  const hs = WasmHandshake.pairing("responder", host, psk);
  assert.equal(hs.isMyTurn(), false);
  assert.equal(toHex(hs.read(fromHex(p.m1))), p.m1_payload);
  assert.equal(hs.isMyTurn(), true);
  assert.equal(hs.remotePublic(), undefined, "XX responder doesn't know the initiator's static key yet");
});

console.log("live pairing (XXpsk3) wasm <-> native Rust");

await test("wasm joiner (initiator) <-> Rust host (responder)", () =>
  withPeer(async (peer) => {
    const code = generatePairCode();
    const psk = derivePsk(code);
    const joiner = WasmKeypair.generate();
    const [, hostPub] = (await peer.ok("keypair")).split(" ");
    await peer.ok(`pairing responder ${toHex(psk)}`);
    assert.equal(await peer.ok(`psk ${code.toLowerCase()}`), toHex(psk), "Rust and wasm derive the same PSK");

    const hs = WasmHandshake.pairing("initiator", joiner, psk);
    assert.equal(hs.isMyTurn(), true);
    assert.equal(hs.remotePublic(), undefined);

    // -> e
    const m1 = hs.write(text("joiner-m1"));
    assert.equal(untext(fromHex(await peer.ok(`read ${toHex(m1)}`))), "joiner-m1");
    assert.equal(await peer.ok("remote"), "none");
    // <- e, ee, s, es
    const m2 = fromHex(await peer.ok("write " + toHex(text("host-m2"))));
    assert.equal(untext(hs.read(m2)), "host-m2");
    assert.equal(toHex(hs.remotePublic()), hostPub, "wasm learned the Rust host's static key after m2");
    // -> s, se, psk
    assert.equal(hs.isFinished(), false);
    const m3 = hs.write(text("joiner-m3"));
    assert.equal(hs.isFinished(), true);
    assert.equal(untext(fromHex(await peer.ok(`read ${toHex(m3)}`))), "joiner-m3");
    assert.equal(await peer.ok("finished"), "true");
    assert.equal(await peer.ok("remote"), toHex(joiner.public), "Rust learned the wasm joiner's static key after m3");

    const wt = hs.intoTransport();
    await peer.ok("transport");
    assert.equal(toHex(wt.remotePublic()), hostPub);
    assert.equal(await peer.ok("tremote"), toHex(joiner.public));
    await exchange(wt, peer, "pairing joiner=wasm");
  }));

await test("Rust joiner (initiator) <-> wasm host (responder)", () =>
  withPeer(async (peer) => {
    const code = generatePairCode();
    const psk = derivePsk(code);
    const host = WasmKeypair.generate();
    const [, joinerPub] = (await peer.ok("keypair")).split(" ");
    await peer.ok(`pairing initiator ${toHex(psk)}`);

    const hs = WasmHandshake.pairing("responder", host, psk);
    assert.equal(await peer.ok("turn"), "true");
    const m1 = fromHex(await peer.ok("write " + toHex(text("rust-m1"))));
    assert.equal(untext(hs.read(m1)), "rust-m1");
    assert.equal(hs.remotePublic(), undefined);
    const m2 = hs.write(text("wasm-m2"));
    assert.equal(untext(fromHex(await peer.ok(`read ${toHex(m2)}`))), "wasm-m2");
    assert.equal(await peer.ok("remote"), toHex(host.public));
    const m3 = fromHex(await peer.ok("write " + toHex(text("rust-m3"))));
    assert.equal(await peer.ok("finished"), "true");
    assert.equal(untext(hs.read(m3)), "rust-m3");
    assert.equal(hs.isFinished(), true);
    assert.equal(toHex(hs.remotePublic()), joinerPub);

    const wt = hs.intoTransport();
    await peer.ok("transport");
    assert.equal(toHex(wt.remotePublic()), joinerPub);
    await exchange(wt, peer, "pairing host=wasm");
  }));

await test("wrong pairing code: handshake fails at message 3 with 'decrypt failed' (wasm host)", () =>
  withPeer(async (peer) => {
    const host = WasmKeypair.generate();
    await peer.ok("keypair");
    await peer.ok(`pairing initiator ${toHex(derivePsk("ABCD-2345"))}`);
    const hs = WasmHandshake.pairing("responder", host, derivePsk("ABCD-2346"));
    hs.read(fromHex(await peer.ok("write")));
    await peer.ok(`read ${toHex(hs.write(new Uint8Array(0)))}`);
    const m3 = fromHex(await peer.ok("write"));
    throwsMsg(() => hs.read(m3), /^decrypt failed$/);
    throwsMsg(() => hs.write(new Uint8Array(0)), /handshake aborted/);
    assert.equal(hs.isFinished(), false);
  }));

await test("wrong pairing code: handshake fails at message 3 (Rust host, wasm joiner)", () =>
  withPeer(async (peer) => {
    const joiner = WasmKeypair.generate();
    await peer.ok("keypair");
    await peer.ok(`pairing responder ${toHex(derivePsk("ABCD-2346"))}`);
    const hs = WasmHandshake.pairing("initiator", joiner, derivePsk("ABCD-2345"));
    await peer.ok(`read ${toHex(hs.write(new Uint8Array(0)))}`);
    hs.read(fromHex(await peer.ok("write")));
    const m3 = hs.write(new Uint8Array(0));
    assert.equal(await peer.err(`read ${toHex(m3)}`), "decrypt failed");
    assert.equal(await peer.ok("finished"), "false");
  }));

console.log("live reconnect (IK) wasm <-> native Rust");

await test("wasm client (initiator) <-> Rust daemon (responder)", () =>
  withPeer(async (peer) => {
    const client = WasmKeypair.generate();
    const [, daemonPub] = (await peer.ok("keypair")).split(" ");
    await peer.ok("reconnect responder -");

    const hs = WasmHandshake.reconnect("initiator", client, fromHex(daemonPub));
    assert.equal(toHex(hs.remotePublic()), daemonPub, "IK initiator knows the daemon key from the start");
    assert.equal(await peer.ok("remote"), "none");
    const m1 = hs.write(new Uint8Array(0));
    await peer.ok(`read ${toHex(m1)}`);
    assert.equal(await peer.ok("remote"), toHex(client.public), "Rust IK responder knows the client after m1");
    assert.equal(await peer.ok("finished"), "false");
    const m2 = fromHex(await peer.ok("write"));
    assert.equal(await peer.ok("finished"), "true");
    hs.read(m2);
    assert.equal(hs.isFinished(), true);

    const wt = hs.intoTransport();
    await peer.ok("transport");
    assert.equal(toHex(wt.remotePublic()), daemonPub);
    await exchange(wt, peer, "reconnect client=wasm");
  }));

await test("Rust client (initiator) <-> wasm daemon (responder)", () =>
  withPeer(async (peer) => {
    const daemon = WasmKeypair.generate();
    const [, clientPub] = (await peer.ok("keypair")).split(" ");
    await peer.ok(`reconnect initiator ${toHex(daemon.public)}`);

    const hs = WasmHandshake.reconnect("responder", daemon, undefined);
    assert.equal(hs.remotePublic(), undefined);
    hs.read(fromHex(await peer.ok("write")));
    assert.equal(toHex(hs.remotePublic()), clientPub, "wasm IK responder knows the client after m1");
    const m2 = hs.write(new Uint8Array(0));
    assert.equal(hs.isFinished(), true);
    await peer.ok(`read ${toHex(m2)}`);
    assert.equal(await peer.ok("finished"), "true");

    const wt = hs.intoTransport();
    await peer.ok("transport");
    assert.equal(toHex(wt.remotePublic()), clientPub);
    await exchange(wt, peer, "reconnect daemon=wasm");
  }));

await test("wasm daemon pins the expected client key; impostor is rejected", () =>
  withPeer(async (peer) => {
    const daemon = WasmKeypair.generate();
    const expected = WasmKeypair.generate();
    await peer.ok("keypair"); // Rust 端扮演另一把钥匙的「冒名客户端」
    await peer.ok(`reconnect initiator ${toHex(daemon.public)}`);
    const hs = WasmHandshake.reconnect("responder", daemon, expected.public);
    const m1 = fromHex(await peer.ok("write"));
    throwsMsg(() => hs.read(m1), /^invalid key$/);
    throwsMsg(() => hs.write(new Uint8Array(0)), /handshake aborted/);
  }));

await test("wasm client rejects a daemon that doesn't hold the expected key (IK, wrong static)", () =>
  withPeer(async (peer) => {
    const client = WasmKeypair.generate();
    const wrongDaemon = WasmKeypair.generate(); // 客户端以为对面是它
    await peer.ok("keypair"); // 真正的对面是 Rust 侧另一把钥匙
    await peer.ok("reconnect responder -");
    const hs = WasmHandshake.reconnect("initiator", client, wrongDaemon.public);
    assert.equal(await peer.err(`read ${toHex(hs.write(new Uint8Array(0)))}`), "decrypt failed");
  }));

console.log("transport (chunking, tampering) wasm <-> native Rust");

/** 建一条 wasm(initiator) ↔ Rust(responder) 的 IK 通道。 */
async function ikChannel(peer) {
  const client = WasmKeypair.generate();
  const [, daemonPub] = (await peer.ok("keypair")).split(" ");
  await peer.ok("reconnect responder -");
  const hs = WasmHandshake.reconnect("initiator", client, fromHex(daemonPub));
  await peer.ok(`read ${toHex(hs.write(new Uint8Array(0)))}`);
  hs.read(fromHex(await peer.ok("write")));
  const wt = hs.intoTransport();
  await peer.ok("transport");
  return wt;
}

await test("100 KiB and 250 KB messages round-trip in both directions (chunked)", () =>
  withPeer(async (peer) => {
    const wt = await ikChannel(peer);
    for (const size of [100 * 1024, 250_000]) {
      const msg = pattern(size);
      const ct = wt.encrypt(msg);
      const frames = Math.ceil(size / 65519);
      assert.equal(ct.length, size + frames * 18);
      assert.equal(new DataView(ct.buffer, ct.byteOffset).getUint16(0), 65519 + 16, "first frame is a full chunk");
      assert.equal(await peer.ok(`decrypt ${toHex(ct)}`), toHex(msg));
      const back = fromHex(await peer.ok(`encrypt ${toHex(msg)}`));
      assert.equal(toHex(wt.decrypt(back)), toHex(msg));
    }
  }));

await test("chunk boundaries (65518 / 65519 / 65520 / 131038 bytes) interoperate", () =>
  withPeer(async (peer) => {
    const wt = await ikChannel(peer);
    for (const size of [1, 65518, 65519, 65520, 131038, 131039]) {
      const msg = pattern(size);
      assert.equal(await peer.ok(`decrypt ${toHex(wt.encrypt(msg))}`), toHex(msg), `wasm->rust ${size}`);
      assert.equal(toHex(wt.decrypt(fromHex(await peer.ok(`encrypt ${toHex(msg)}`)))), toHex(msg), `rust->wasm ${size}`);
    }
  }));

await test("empty plaintext is a single 16-byte-tag frame, both directions", () =>
  withPeer(async (peer) => {
    const wt = await ikChannel(peer);
    const ct = wt.encrypt(new Uint8Array(0));
    assert.equal(ct.length, 2 + 16);
    assert.equal(await peer.ok(`decrypt ${toHex(ct)}`), "-");
    const back = fromHex(await peer.ok("encrypt -"));
    assert.equal(back.length, 18);
    assert.equal(wt.decrypt(back).length, 0);
    await exchange(wt, peer, "after empty"); // 计数器仍然对齐
  }));

await test("tampered / truncated / replayed ciphertext", () =>
  withPeer(async (peer) => {
    const wt = await ikChannel(peer);
    // Rust -> wasm：篡改、截断、重放。
    const ct = fromHex(await peer.ok("encrypt " + toHex(text("do not touch"))));
    const bad = Uint8Array.from(ct);
    bad[bad.length - 1] ^= 1;
    throwsMsg(() => wt.decrypt(bad), /^decrypt failed$/);
    throwsMsg(() => wt.decrypt(ct.slice(0, ct.length - 3)), /^malformed frame$/);
    throwsMsg(() => wt.decrypt(new Uint8Array(0)), /^malformed frame$/);
    assert.equal(untext(wt.decrypt(ct)), "do not touch", "failed attempts don't advance the receive counter");
    throwsMsg(() => wt.decrypt(ct), /^decrypt failed$/);
    // wasm -> Rust：同样。
    const out = wt.encrypt(text("hello rust"));
    const bad2 = Uint8Array.from(out);
    bad2[5] ^= 0x80;
    assert.equal(await peer.err(`decrypt ${toHex(bad2)}`), "decrypt failed");
    assert.equal(await peer.err(`decrypt ${toHex(out.slice(0, 10))}`), "malformed frame");
    assert.equal(untext(fromHex(await peer.ok(`decrypt ${toHex(out)}`))), "hello rust");
  }));

console.log("wasm API behaviour");

await test("invalid role / key lengths are JS errors with Rust messages", () => {
  const kp = WasmKeypair.generate();
  throwsMsg(() => WasmHandshake.pairing("host", kp, new Uint8Array(32)), /invalid role/);
  throwsMsg(() => WasmHandshake.pairing("initiator", kp, new Uint8Array(31)), /^invalid key$/);
  throwsMsg(() => WasmHandshake.reconnect("initiator", kp, undefined), /^invalid key$/);
  throwsMsg(() => WasmHandshake.reconnect("initiator", kp, new Uint8Array(5)), /^invalid key$/);
});

await test("handshake state machine errors", () => {
  const psk = derivePsk(generatePairCode());
  const a = WasmHandshake.pairing("initiator", WasmKeypair.generate(), psk);
  const b = WasmHandshake.pairing("responder", WasmKeypair.generate(), psk);
  assert.equal(a.isMyTurn(), true);
  assert.equal(b.isMyTurn(), false);
  throwsMsg(() => b.write(new Uint8Array(0)), /^not your turn to write$/);
  throwsMsg(() => a.intoTransport(), /^handshake not finished$/);
  // a 的 intoTransport 失败时对象已被消费 —— 这是 wasm-bindgen `self` 语义，新建一个继续。
  const c = WasmHandshake.pairing("initiator", WasmKeypair.generate(), psk);
  const m1 = c.write(new Uint8Array(0));
  b.read(m1);
  c.free();
});

await test("intoTransport consumes the handshake", () => {
  const client = WasmKeypair.generate();
  const daemon = WasmKeypair.generate();
  const ci = WasmHandshake.reconnect("initiator", client, daemon.public);
  const dr = WasmHandshake.reconnect("responder", daemon, undefined);
  dr.read(ci.write(new Uint8Array(0)));
  ci.read(dr.write(new Uint8Array(0)));
  const ct = ci.intoTransport();
  const dt = dr.intoTransport();
  assert.throws(() => ci.isFinished()); // 已被消费
  assert.equal(untext(dt.decrypt(ct.encrypt(text("wasm<->wasm")))), "wasm<->wasm");
  assert.equal(toHex(ct.remotePublic()), toHex(daemon.public));
  assert.equal(toHex(dt.remotePublic()), toHex(client.public));
});

// ---------- 汇总 ----------

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed${process.env.ANYCON_VECTORS_OUT ? ` (vectors: ${vectorsPath})` : ""}`);
process.exit(failed.length === 0 ? 0 : 1);
