// 用真实 wasm（crates/crypto 的产物，经 @anycon/protocol 的 loadCrypto）跑一遍握手和传输，
// 并把整个 AnyconClient 的 IK / 配对流程接到真 Noise 上。wasm 没构建（没跑 `just gen-wasm`）时跳过。
import { existsSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { beforeAll, describe, expect, it, vi } from 'vitest'
import { parseEnrollQr } from '@/lib/enrollQr'
import { loadCrypto, type FullCrypto } from '@/protocol'
import { CLIENT_ID, DAEMON_ID, FakeHub, FakeSocket, memoryStorage } from '@/test/harness'
import { AnyconClient } from './client'

const wasmPath = resolve(dirname(fileURLToPath(import.meta.url)), '../../../packages/protocol/wasm/anycon_crypto_bg.wasm')
const hasWasm = existsSync(wasmPath)

describe.skipIf(!hasWasm)('real wasm crypto', () => {
  let crypto: FullCrypto
  beforeAll(async () => {
    // 在 vitest(jsdom) 里 F 的 import.meta.url 不是 file: URL，所以显式给字节
    crypto = await loadCrypto({ wasm: new Uint8Array(readFileSync(wasmPath)) })
  })

  // 独立实现（Node 的 crypto.argon2Sync，不经过 Rust）算出的向量：
  //   Argon2id(password = 规范化短码, salt = "anycon-pair-v2", m = 64 MiB, t = 3, p = 1, out = 64)，前 8 字节 → match_id，后 32 字节 → PSK
  const VECTORS = [
    { code: 'ABCD-2345', matchId: '94d67d781538d997', psk: '23e8b63dba0b3c479ab554d3fd58d8346b68799bbf88cf96418123133a9f6012' },
    { code: '2345-6789', matchId: 'f9f3c6c9940049ac', psk: '17d04f19b36f2f987faf376e4962f2b51f3b764b1733c17e6297bc9c9751bb20' },
    { code: 'abcd-2345-wxyz-6789', matchId: 'e66dfa8007f7cee8', psk: 'a2a528b342989ee2b3a9cb9589244bf429a622cffa2e25a002b4446dc6ea08b0' },
  ]
  const hex = (b: Uint8Array): string => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')

  it.each(VECTORS)('derivePairSecrets($code) matches the independently computed Argon2id vector, in one KDF', ({ code, matchId, psk }) => {
    const s = crypto.derivePairSecrets(code)
    expect(s.matchId).toBe(matchId)
    expect(hex(s.psk)).toBe(psk)
    // 旧的单项导出各自是一次完整 KDF，结果一致
    expect(crypto.deriveMatchId(code)).toBe(matchId)
    expect(hex(crypto.derivePsk(code))).toBe(psk)
    s.psk.fill(0)
    expect(s.psk.every((b) => b === 0)).toBe(true)
  }, 30_000)

  it('derivePairSecrets rejects an invalid code', () => {
    expect(() => crypto.derivePairSecrets('ABCD')).toThrow()
    expect(() => crypto.derivePairSecrets('ABCD-234O')).toThrow()
  })

  it('completes the XXpsk3 pairing handshake with the joiner as initiator', () => {
    const code = crypto.generatePairCode()
    const psk = crypto.derivePairSecrets(code).psk
    const joiner = crypto.generateKeypair()
    const host = crypto.generateKeypair()
    const i = crypto.pairing('initiator', joiner, psk)
    const r = crypto.pairing('responder', host, psk)
    expect(i.isMyTurn()).toBe(true)
    r.read(i.write())
    i.read(r.write())
    r.read(i.write())
    expect(i.isFinished() && r.isFinished()).toBe(true)
    expect(Array.from(i.remotePublic()!)).toEqual(Array.from(host.public))
    expect(Array.from(r.remotePublic()!)).toEqual(Array.from(joiner.public))
    const ti = i.intoTransport()
    const tr = r.intoTransport()
    const big = new Uint8Array(100 * 1024).map((_, k) => k % 251)
    expect(Array.from(tr.decrypt(ti.encrypt(big)))).toEqual(Array.from(big))
  })

  it('fails the third pairing message when the code differs', () => {
    const joiner = crypto.generateKeypair()
    const host = crypto.generateKeypair()
    const i = crypto.pairing('initiator', joiner, crypto.derivePairSecrets('ABCD-2345').psk)
    const r = crypto.pairing('responder', host, crypto.derivePairSecrets('WXYZ-6789').psk)
    r.read(i.write())
    i.read(r.write())
    expect(() => r.read(i.write())).toThrow()
  })

  it('runs the IK reconnect handshake and rejects a mismatched responder key', () => {
    const client = crypto.generateKeypair()
    const daemon = crypto.generateKeypair()
    const other = crypto.generateKeypair()
    const i = crypto.reconnect('initiator', client, daemon.public)
    const r = crypto.reconnect('responder', daemon)
    r.read(i.write())
    i.read(r.write())
    expect(i.isFinished() && r.isFinished()).toBe(true)

    const bad = crypto.reconnect('initiator', client, other.public)
    const r2 = crypto.reconnect('responder', daemon)
    expect(() => {
      r2.read(bad.write())
    }).toThrow()
  })

  function boot(hub: FakeHub, paired: boolean) {
    FakeSocket.instances = []
    const kp = crypto.generateKeypair()
    const storage = memoryStorage({
      identity: {
        id: 'self',
        static_keypair: { public: kp.public, secret: kp.secret },
        device_name: 'Chrome · macOS · localhost',
        account_id: 'acct1',
        ...(paired ? { device_id: CLIENT_ID, device_token: 'tok' } : {}),
      },
      nodes: paired ? [{ node_id: DAEMON_ID, name: 'n', public_key: hub.daemonKp.public, paired_at: 1, last_seen: 1 }] : [],
    })
    const client = new AnyconClient({
      crypto: async () => crypto,
      wsFactory: hub.factory,
      wsUrl: () => 'ws://localhost/v1/ws',
      storage,
      origin: () => 'http://localhost',
      yieldToPaint: async () => {},
    })
    return { client, storage }
  }

  it('AnyconClient: IK handshake + RPC over the real wasm transport (including a >64 KiB reply)', async () => {
    const big = 'x'.repeat(150_000)
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n', big }) } }, crypto)
    const { client } = boot(hub, true)
    await client.start('acct1')
    await vi.waitFor(() => expect(client.isReady(DAEMON_ID)).toBe(true))
    const res = await client.request<{ name: string; big: string }>(DAEMON_ID, 'node.info')
    expect(res.name).toBe('n')
    expect(res.big.length).toBe(150_000)
    client.stop()
  })

  it('AnyconClient: a 1 MB history page is fragmented by the daemon and arrives intact over the real wasm transport', async () => {
    const events = Array.from({ length: 260 }, (_, i) => ({ seq: i + 1, text: `${i}:` + 'h'.repeat(4000) }))
    const hub = new FakeHub({ rpc: { 'session.history': () => ({ events, has_more: true }) } }, crypto)
    const { client } = boot(hub, true)
    await client.start('acct1')
    await vi.waitFor(() => expect(client.isReady(DAEMON_ID)).toBe(true))
    const before = hub.daemonSeq
    const res = await client.request<{ events: unknown[] }>(DAEMON_ID, 'session.history', {})
    expect(res).toEqual({ events, has_more: true })
    expect(hub.daemonSeq - before).toBeGreaterThanOrEqual(5)
    // 反方向：客户端发一条 500 KB 的请求，daemon 重组 + 解密后原样收到
    const big = 'u'.repeat(500_000)
    await client.request(DAEMON_ID, 'echo', { big }, 2000).catch(() => {}) // daemon 没有 echo 处理器：只关心它收到了
    const got = hub.received.find((m) => 'method' in m && m.method === 'echo')
    expect(got && 'params' in got ? (got.params as { big: string }).big.length : 0).toBe(500_000)
    expect(hub.oversize).toBe(0)
    client.stop()
  })

  it('AnyconClient: pairing over the real wasm persists the node only after the pair.ack arrives on the PSK-bound transport', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } }, crypto)
    hub.pairCodeOk = 'ABCD2345'
    const { client, storage } = boot(hub, false)
    await client.start('acct1')
    const res = await client.pair('abcd-2345', 'My Browser')
    expect(res.nodeId).toBe(DAEMON_ID)
    expect(Array.from(storage.nodes.get(DAEMON_ID)!.public_key)).toEqual(Array.from(hub.daemonKp.public))
    expect(storage.nodes.get(DAEMON_ID)!.name).toBe('test-node')
    expect(hub.joinMatchIds).toEqual(['94d67d781538d997']) // Argon2id 向量，见上
    await vi.waitFor(() => expect(client.isReady(DAEMON_ID)).toBe(true))
    client.stop()
  })

  it('AnyconClient: re-pairing a known node after a revoke ends up ready over the real wasm (overlapping IK requests must not cross their replies)', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } }, crypto)
    hub.pairCodeOk = 'ABCD2345'
    hub.ikDelayMs = 25
    const { client, storage } = boot(hub, true)
    hub.authFail = 'revoked'
    await client.start('acct1')
    await vi.waitFor(() => expect(client.snapshot.halt).toBe('revoked'))
    hub.authFail = null
    await client.pair('ABCD-2345', storage.identity!.device_name)
    await vi.waitFor(() => expect(client.isReady(DAEMON_ID)).toBe(true), { timeout: 3000 })
    await expect(client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    expect(hub.ikHandshakes).toBe(1)
    client.stop()
  })

  it('AnyconClient: pairs with a 16-character code over the real wasm', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } }, crypto)
    hub.pairCodeOk = crypto.normalizeCode(crypto.generatePairCodeLong())
    expect(hub.pairCodeOk).toHaveLength(16)
    const { client, storage } = boot(hub, false)
    await client.start('acct1')
    await client.pair(hub.pairCodeOk.replace(/(.{4})(?=.)/g, '$1-').toLowerCase(), 'My Browser')
    expect(storage.nodes.has(DAEMON_ID)).toBe(true)
    client.stop()
  })

  it.each([['forged'], ['none']] as const)('AnyconClient: real wasm pairing with ackMode=%s persists nothing (hub forged PairDone)', async (mode) => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    try {
      const hub = new FakeHub({}, crypto)
      hub.pairCodeOk = 'ABCD2345'
      hub.ackMode = mode
      const { client, storage } = boot(hub, false)
      await client.start('acct1')
      await vi.advanceTimersByTimeAsync(0)
      const p = client.pair('ABCD-2345', 'x').then(
        () => null,
        (e: unknown) => e,
      )
      await vi.advanceTimersByTimeAsync(60_000)
      expect(await p).toMatchObject({ code: 'timeout' })
      expect(storage.nodes.size).toBe(0)
      client.stop()
    } finally {
      vi.useRealTimers()
    }
  })

  it('AnyconClient: real wasm - a hub that does not know the code cannot complete the PSK handshake, so no ack and nothing persisted', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    try {
      const hub = new FakeHub({}, crypto)
      hub.pairCodeOk = 'ABCD2345'
      hub.impersonate = true
      const { client, storage } = boot(hub, false)
      await client.start('acct1')
      await vi.advanceTimersByTimeAsync(0)
      const p = client.pair('ABCD-2345', 'x').then(
        () => null,
        (e: unknown) => e,
      )
      await vi.advanceTimersByTimeAsync(60_000)
      expect(await p).toBeInstanceOf(Error)
      expect(storage.nodes.size).toBe(0)
      client.stop()
    } finally {
      vi.useRealTimers()
    }
  })
  // ---- 账号内接入（ADR-36）：真实 CPace / Argon2id / HKDF，N 一侧是 AnyconClient，D 一侧是同一份 wasm 的 responder ----

  it('AnyconClient: enrolls with the node password over the real wasm (Argon2id + CPace + XXpsk3), and a wrong password does not', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } }, crypto)
    const daemon = hub.enableEnroll()
    daemon.password = 'correct horse battery'
    const { client, storage } = boot(hub, false)
    await client.start('acct1')
    const bad = await client.enroll({ node: DAEMON_ID, method: 'password', deviceName: 'My Browser', password: 'wrong horse battery' }, { stage() {} }).then(
      () => null,
      (e: unknown) => e,
    )
    expect(bad).toMatchObject({ code: 'wrong_secret' })
    expect(storage.nodes.size).toBe(0)
    const res = await client.enroll({ node: DAEMON_ID, method: 'password', deviceName: 'My Browser', password: 'correct horse battery' }, { stage() {} })
    expect(res.nodeId).toBe(DAEMON_ID)
    expect(storage.nodes.get(DAEMON_ID)!.name).toBe('test-node')
    await vi.waitFor(() => expect(client.isReady(DAEMON_ID)).toBe(true))
    client.stop()
  }, 60_000)

  it('AnyconClient: enrolls with a 6-digit code and with a scan secret over the real wasm', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } }, crypto)
    const daemon = hub.enableEnroll()
    daemon.activeCode = '123456'
    const { client, storage } = boot(hub, false)
    await client.start('acct1')
    await client.enroll({ node: DAEMON_ID, method: 'code', approval: false, deviceName: 'My Browser' }, { stage() {}, askCode: async () => '123 456' })
    expect(storage.nodes.has(DAEMON_ID)).toBe(true)
    await client.removeNode(DAEMON_ID)

    // 扫码：二维码里的 r 经「已配对设备」交给 daemon
    await client.enroll(
      { node: DAEMON_ID, method: 'scan', deviceName: 'My Browser' },
      { stage() {}, qr: (payload) => queueMicrotask(() => daemon.approveScan(parseEnrollQr(payload)!.secret)) },
    )
    expect(storage.nodes.has(DAEMON_ID)).toBe(true)
    client.stop()
  }, 60_000)
})
