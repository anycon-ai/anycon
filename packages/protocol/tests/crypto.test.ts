// wasm 产物（`just gen-wasm`）存在时跑配对 + 重连握手；不存在时跳过并打印原因。
import assert from 'node:assert/strict'
import { existsSync } from 'node:fs'
import { describe, it } from 'node:test'

import {
  CryptoUnavailableError,
  bytesOfHex,
  extractPairCode,
  formatPairCode,
  hexOf,
  isValidPairCode,
  loadCrypto,
  type Crypto,
  type Handshake,
} from '../src/index.js'
import { packageRoot } from './helpers.js'

const wasmPresent = existsSync(`${packageRoot}wasm/anycon_crypto.js`)
const enc = (s: string) => new TextEncoder().encode(s)
const dec = (b: Uint8Array) => new TextDecoder().decode(b)

if (!wasmPresent) {
  console.warn(
    '[crypto.test] packages/protocol/wasm 不存在：跳过 wasm 握手测试（先跑 `just gen-wasm`）。',
  )
}

/** 交替跑完一次握手，返回两边的 transport。 */
function drive(init: Handshake, resp: Handshake, payloads: string[] = []) {
  let turn = 0
  let a = init
  let b = resp
  while (!a.isFinished() || !b.isFinished()) {
    assert.ok(a.isMyTurn(), 'turn order')
    const msg = a.write(enc(payloads[turn] ?? ''))
    const got = b.read(msg)
    if (payloads[turn]) assert.equal(dec(got), payloads[turn])
    ;[a, b] = [b, a]
    turn++
    assert.ok(turn < 8, 'handshake did not converge')
  }
  return { turns: turn, initiator: init.intoTransport(), responder: resp.intoTransport() }
}

describe('纯 JS 辅助', () => {
  it('短码格式', () => {
    assert.equal(formatPairCode('7k3pq2mx'), '7K3P-Q2MX')
    assert.equal(formatPairCode(' 7k3p-q2mx '), '7K3P-Q2MX')
    assert.ok(isValidPairCode('7K3P-Q2MX'))
    assert.ok(!isValidPairCode('7K3P-Q2M0')) // 0 不在字母表
    assert.ok(!isValidPairCode('7K3P-Q2M'))
  })

  it('16 位长短码也合法，按 4 位分组', () => {
    assert.equal(formatPairCode('7k3pq2mx9acdefhj'), '7K3P-Q2MX-9ACD-EFHJ')
    assert.ok(isValidPairCode('7K3P-Q2MX-9ACD-EFHJ'))
    assert.ok(!isValidPairCode('7K3P-Q2MX-9ACD-EFH')) // 15 位
    assert.ok(!isValidPairCode('7K3P-Q2MX-9ACD-EFH0')) // 0 不在字母表
  })

  it('extractPairCode：裸短码、#code= 链接、fragment；query 里的 code 不认', () => {
    const want = '7K3P-Q2MX'
    assert.equal(extractPairCode('7k3p q2mx'), want)
    assert.equal(extractPairCode('https://hub.example.com/pair#code=7K3P-Q2MX'), want)
    assert.equal(extractPairCode('https://hub.example.com:8443/pair#code=7k3pq2mx&x=1'), want)
    assert.equal(extractPairCode('#code=7K3P-Q2MX'), want)
    assert.equal(extractPairCode('code=7K3P-Q2MX'), want)
    assert.equal(extractPairCode('https://h/pair#code=7K3P-Q2MX-9ACD-EFHJ'), '7K3P-Q2MX-9ACD-EFHJ')
    assert.equal(extractPairCode('https://hub.example.com/pair?code=7K3P-Q2MX'), undefined)
    assert.equal(extractPairCode('https://hub.example.com/pair'), undefined)
    assert.equal(extractPairCode('https://hub.example.com/pair#code=NOPE'), undefined)
    assert.equal(extractPairCode(''), undefined)
  })

  it('hex 往返', () => {
    assert.equal(hexOf(bytesOfHex('00ff10')), '00ff10')
    assert.throws(() => bytesOfHex('abc'))
  })
})

describe('wasm 加载', { skip: wasmPresent ? false : 'wasm 未构建' }, () => {
  it('配对握手 XXpsk3：joiner = initiator，三条消息，各自拿到对方公钥，传输往返', async () => {
    const crypto: Crypto = await loadCrypto()
    const code = crypto.generatePairCode()
    assert.match(code, /^[2-9A-HJKMNP-Z]{4}-[2-9A-HJKMNP-Z]{4}$/)
    assert.equal(crypto.normalizeCode(code.toLowerCase()), code.replace('-', ''))
    const { matchId, psk } = crypto.derivePairSecrets(code)
    assert.match(matchId, /^[0-9a-f]{16}$/)
    assert.equal(psk.length, 32)
    // 规范化之后同一个码，结果相同（每次调用是一次完整 Argon2id，所以只比一次）
    const again = crypto.derivePairSecrets(code.replace('-', '').toLowerCase())
    assert.equal(again.matchId, matchId)
    assert.deepEqual(again.psk, psk)

    const joiner = crypto.generateKeypair()
    const host = crypto.generateKeypair()
    const { turns, initiator, responder } = drive(
      crypto.pairing('initiator', joiner, psk),
      crypto.pairing('responder', host, psk),
      ['', '', JSON.stringify({ name: 'ci-runner' })],
    )
    assert.equal(turns, 3)
    assert.deepEqual(initiator.remotePublic, host.public)
    assert.deepEqual(responder.remotePublic, joiner.public)
    assert.equal(crypto.fingerprint(host.public).split(':').length, 8)

    const ct = initiator.encrypt(enc('hello'))
    assert.equal(dec(responder.decrypt(ct)), 'hello')
    // 大于单片上限，自动分片
    const big = new Uint8Array(150_000).map((_, i) => i % 251)
    assert.deepEqual(responder.decrypt(initiator.encrypt(big)), big)
    // 空明文
    assert.equal(responder.decrypt(initiator.encrypt(new Uint8Array(0))).length, 0)
    // 反方向
    assert.equal(dec(initiator.decrypt(responder.encrypt(enc('pong')))), 'pong')
    initiator.free()
    responder.free()
  })

  it('derivePairSecrets：Argon2id 向量（与 Rust / Node crypto.argon2Sync 一致）', async () => {
    const crypto = await loadCrypto()
    const vectors = [
      {
        code: 'ABCD2345',
        matchId: '94d67d781538d997',
        psk: '23e8b63dba0b3c479ab554d3fd58d8346b68799bbf88cf96418123133a9f6012',
      },
      {
        code: '23456789',
        matchId: 'f9f3c6c9940049ac',
        psk: '17d04f19b36f2f987faf376e4962f2b51f3b764b1733c17e6297bc9c9751bb20',
      },
    ]
    for (const v of vectors) {
      const got = crypto.derivePairSecrets(v.code)
      assert.equal(got.matchId, v.matchId, v.code)
      assert.equal(hexOf(got.psk), v.psk, v.code)
    }
    // 带连字符 / 小写的写法规范化后是同一个码
    const dashed = crypto.derivePairSecrets('abcd-2345')
    assert.equal(dashed.matchId, vectors[0]!.matchId)
    // psk 是拷贝：调用方清零不会影响下一次派生
    dashed.psk.fill(0)
    assert.equal(hexOf(crypto.derivePairSecrets('ABCD2345').psk), vectors[0]!.psk)
    // 单独的 derivePsk / deriveMatchId 各是一次完整 KDF，结果与 derivePairSecrets 一致
    assert.equal(crypto.deriveMatchId('ABCD2345'), vectors[0]!.matchId)
    assert.equal(hexOf(crypto.derivePsk('ABCD2345')), vectors[0]!.psk)
    // 坏码抛错
    assert.throws(() => crypto.derivePairSecrets('ABCD234O'))
    assert.throws(() => crypto.derivePairSecrets('ABCD'))
  })

  it('derivePairSecrets：一次 KDF 的耗时有下限（Argon2id 64 MiB × 3 轮），不是 HKDF 级别', async () => {
    const crypto = await loadCrypto()
    crypto.derivePairSecrets('WARM2345') // 预热 wasm 内存
    const started = performance.now()
    crypto.derivePairSecrets('7K3P-Q2MX')
    const elapsed = performance.now() - started
    // 本机约 100–300 ms；HKDF 只要几十微秒。下限放宽到 10 ms 防 CI 机器抖动，同时能拦住退回快速 KDF。
    assert.ok(elapsed > 10, `derivePairSecrets took ${elapsed.toFixed(1)} ms`)
  })

  it('match_id 与 PSK 来自同一次 KDF 的不同字节；16 位长码也行；generatePairCodeLong 是 16 位', async () => {
    const crypto = await loadCrypto()
    const { matchId, psk } = crypto.derivePairSecrets('7K3P-Q2MX')
    assert.notEqual(hexOf(psk).slice(0, 16), matchId)
    assert.throws(() => crypto.deriveMatchId('7K3P-Q2M0'))
    assert.throws(() => crypto.deriveMatchId('7K3P'))

    const long = crypto.generatePairCodeLong()
    assert.match(long, /^([2-9A-HJKMNP-Z]{4}-){3}[2-9A-HJKMNP-Z]{4}$/)
    assert.ok(isValidPairCode(long))
    const secrets = crypto.derivePairSecrets(long)
    assert.match(secrets.matchId, /^[0-9a-f]{16}$/)
    assert.equal(secrets.psk.length, 32)
    // 长短码走完整配对握手
    const { turns } = drive(
      crypto.pairing('initiator', crypto.generateKeypair(), secrets.psk),
      crypto.pairing('responder', crypto.generateKeypair(), secrets.psk),
    )
    assert.equal(turns, 3)
  })

  it('PSK 不同则握手失败', async () => {
    const crypto = await loadCrypto()
    const a = crypto.pairing(
      'initiator',
      crypto.generateKeypair(),
      crypto.derivePairSecrets('AAAA-BBBB').psk,
    )
    const b = crypto.pairing(
      'responder',
      crypto.generateKeypair(),
      crypto.derivePairSecrets('CCCC-DDDD').psk,
    )
    assert.throws(() => drive(a, b))
  })

  it('重连握手 IK：两条消息，responder 读完第一条就知道 initiator 公钥', async () => {
    const crypto = await loadCrypto()
    const client = crypto.generateKeypair()
    const daemon = crypto.generateKeypair()
    const init = crypto.reconnect('initiator', client, daemon.public)
    const resp = crypto.reconnect('responder', daemon)
    const m1 = init.write()
    resp.read(m1)
    assert.deepEqual(resp.remotePublic(), client.public)
    const m2 = resp.write()
    init.read(m2)
    assert.ok(init.isFinished() && resp.isFinished())
    const a = init.intoTransport()
    const b = resp.intoTransport()
    assert.deepEqual(a.remotePublic, daemon.public)
    assert.equal(dec(b.decrypt(a.encrypt(enc('{"jsonrpc":"2.0"}')))), '{"jsonrpc":"2.0"}')
  })

  it('密钥对可由私钥还原；乱序 / 篡改的密文解密失败', async () => {
    const crypto = await loadCrypto()
    const kp = crypto.generateKeypair()
    assert.deepEqual(crypto.keypairFromSecret(kp.secret).public, kp.public)

    const client = crypto.generateKeypair()
    const daemon = crypto.generateKeypair()
    const init = crypto.reconnect('initiator', client, daemon.public)
    const resp = crypto.reconnect('responder', daemon)
    resp.read(init.write())
    init.read(resp.write())
    const a = init.intoTransport()
    const b = resp.intoTransport()
    const c1 = a.encrypt(enc('one'))
    const c2 = a.encrypt(enc('two'))
    assert.throws(() => b.decrypt(c2)) // 乱序
    const tampered = c1.slice()
    tampered[tampered.length - 1] = tampered[tampered.length - 1]! ^ 1
    assert.throws(() => b.decrypt(tampered))
  })
})

describe('wasm 缺失时的错误', { skip: wasmPresent ? 'wasm 已构建' : false }, () => {
  it('抛 CryptoUnavailableError 并提示 just gen-wasm', async () => {
    await assert.rejects(loadCrypto(), (err: unknown) => {
      assert.ok(err instanceof CryptoUnavailableError)
      assert.match(err.message, /just gen-wasm/)
      return true
    })
  })
})

describe('enroll crypto (ADR-36)', { skip: !wasmPresent }, () => {
  it('cpace agrees on the same code and feeds a pairing handshake', async () => {
    const c = await loadCrypto()
    const ci = c.enrollChannelId('node1', 'code')
    const sid = c.enrollSid(c.generateEnrollNonce(), c.generateEnrollNonce())
    const code = enc(c.normalizeEnrollCode('123-456'))
    const n = c.cpace(code, ci, sid)
    const d = c.cpace(code, ci, sid)
    const pskN = n.finish('initiator', d.share)
    const pskD = d.finish('responder', n.share)
    assert.equal(hexOf(pskN), hexOf(pskD))
    const { initiator, responder } = drive(
      c.pairing('initiator', c.generateKeypair(), pskN),
      c.pairing('responder', c.generateKeypair(), pskD),
    )
    assert.equal(dec(responder.decrypt(initiator.encrypt(enc('hi')))), 'hi')
  })

  it('cpace disagrees on a wrong code and rejects bad shares', async () => {
    const c = await loadCrypto()
    const ci = c.enrollChannelId('node1', 'code')
    const sid = new Uint8Array(32).fill(1)
    const n = c.cpace(enc('123456'), ci, sid)
    const d = c.cpace(enc('654321'), ci, sid)
    assert.notEqual(hexOf(n.finish('initiator', d.share)), hexOf(d.finish('responder', n.share)))
    const bad = c.cpace(enc('123456'), ci, sid)
    assert.throws(() => bad.finish('initiator', new Uint8Array(32)), /invalid key/)
    const unused = c.cpace(enc('123456'), ci, sid)
    unused.free()
    unused.free()
  })

  it('derives password prs, scan psk and validates inputs', async () => {
    const c = await loadCrypto()
    const salt = new Uint8Array(16).fill(7)
    assert.equal(
      hexOf(c.deriveEnrollPasswordPrs('café latte', salt)),
      hexOf(c.deriveEnrollPasswordPrs('café latte', salt)),
    )
    assert.throws(() => c.validateEnrollPassword('1234567'), /too short/)
    c.validateEnrollPassword('12345678')
    assert.throws(() => c.normalizeEnrollCode('12345'))
    const secret = c.generateEnrollSecret()
    assert.equal(secret.length, 32)
    assert.notEqual(
      hexOf(c.deriveEnrollScanPsk(secret, 'a')),
      hexOf(c.deriveEnrollScanPsk(secret, 'b')),
    )
  })
})
