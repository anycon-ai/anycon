import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { CLIENT_ID, DAEMON_ID, FakeHub, FakeRpcError, FakeSocket, setupClient as setup, TEMP_ID, type AckMode } from '@/test/harness'
import { PairError, type PairStage } from './client'
import { encode } from '@msgpack/msgpack'
import { decodeControl, makeEnvelope } from './wire'
import { RpcCallError } from './rpcClient'

beforeEach(() => {
  FakeSocket.instances = []
})
afterEach(() => {
  vi.useRealTimers()
})

describe('AnyconClient — hub connection', () => {
  it('authenticates with the stored device token and client info', async () => {
    const t = setup()
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.hubStates.at(-1)?.phase).toBe('hub_authed'))
    const auth = t.hub.authFrames[0]!
    expect(auth.device_token).toBe('tok')
    expect(auth.protocol_version).toBe(1)
    expect(auth.client).toMatchObject({ kind: 'web', platform: 'browser', origin: 'http://localhost', name: 'Chrome · macOS · localhost' })
    expect(t.hub.socket.url).toBe('ws://localhost/v1/ws')
    t.client.stop()
  })

  it('sends device_token null when the browser was never paired', async () => {
    const t = setup({ paired: false, knowsNode: false })
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.hub.authFrames).toHaveLength(1))
    expect(t.hub.authFrames[0]!.device_token ?? null).toBeNull() // F 的编码器按 Rust 的 skip_serializing_if 省略 null 键
    expect(t.client.snapshot.paired).toBe(false)
    t.client.stop()
  })

  it('wipes local data when another account logs in on this browser', async () => {
    const t = setup()
    await t.client.start('someone-else')
    expect(t.storage.wiped).toBe(1)
    expect(t.storage.identity?.account_id).toBe('someone-else')
    expect(t.storage.identity?.device_token).toBeUndefined()
    t.client.stop()
  })

  it('does not reconnect after close code 4001 (superseded)', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const t = setup()
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    t.hub.socket.serverClose(4001)
    expect(t.hubStates.at(-1)).toMatchObject({ phase: 'hub_disconnected', halt: 'superseded' })
    await vi.advanceTimersByTimeAsync(60_000)
    expect(FakeSocket.instances).toHaveLength(1)
    t.client.takeOver()
    await vi.advanceTimersByTimeAsync(0)
    expect(FakeSocket.instances).toHaveLength(2)
    t.client.stop()
  })

  it('reconnects within 1-3 s after 4002 (hub restarting)', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const t = setup()
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    t.hub.socket.serverClose(4002)
    expect(t.hubStates.at(-1)?.retryInMs).toBeGreaterThanOrEqual(1000)
    expect(t.hubStates.at(-1)?.retryInMs).toBeLessThanOrEqual(3000)
    await vi.advanceTimersByTimeAsync(3000)
    expect(FakeSocket.instances).toHaveLength(2)
    t.client.stop()
  })

  it('backs off exponentially on abnormal closes', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const t = setup()
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    t.hub.socket.serverClose(1006)
    const first = t.hubStates.at(-1)!.retryInMs!
    await vi.advanceTimersByTimeAsync(first)
    await vi.advanceTimersByTimeAsync(0)
    t.hub.socket.serverClose(1006)
    const second = t.hubStates.at(-1)!.retryInMs!
    expect(second).toBeGreaterThan(first)
    t.client.stop()
  })

  it('halts and forgets the token when the device was revoked', async () => {
    const t = setup()
    t.hub.authFail = 'revoked'
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.hubStates.at(-1)?.halt).toBe('revoked'))
    expect(t.storage.identity?.device_token).toBeUndefined()
    t.client.stop()
  })

  it('shows a version halt on auth_fail version', async () => {
    const t = setup()
    t.hub.authFail = 'version'
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.hubStates.at(-1)?.halt).toBe('version'))
    t.client.stop()
  })
})

describe('AnyconClient — node link', () => {
  it('runs the IK handshake when the daemon is online and then serves RPC', async () => {
    const t = setup()
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    expect(t.ready).toEqual([DAEMON_ID])
    expect(t.nodeStates.at(-1)).toMatchObject({ nodeId: DAEMON_ID, conn: 'ready' })
    await expect(t.client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    t.client.stop()
  })

  it('reports peer_offline and refuses requests when the daemon is offline', async () => {
    const t = setup()
    t.hub.daemonOnline = false
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.nodeStates.at(-1)?.conn).toBe('peer_offline'))
    await expect(t.client.request(DAEMON_ID, 'node.info')).rejects.toMatchObject({ kind: 'offline' })
    t.client.stop()
  })

  it('handshakes when peer_online arrives later and drops the transport on peer_offline', async () => {
    const t = setup()
    t.hub.daemonOnline = false
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.nodeStates.at(-1)?.conn).toBe('peer_offline'))
    t.hub.daemonOnline = true
    t.hub.socket.control({ type: 'peer_online', device: DAEMON_ID })
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    t.hub.socket.control({ type: 'peer_offline', device: DAEMON_ID })
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(false))
    expect(t.nodeStates.at(-1)?.conn).toBe('peer_offline')
    t.client.stop()
  })

  it('fails the request with undeliverable when hub drops the envelope', async () => {
    const t = setup()
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    t.hub.daemonOnline = false // hub 还没来得及发 peer_offline 就收到数据
    const p = t.client.request(DAEMON_ID, 'node.info')
    await expect(p).rejects.toSatisfy((e: unknown) => e instanceof RpcCallError && e.kind === 'undeliverable')
    t.client.stop()
  })

  it('times out requests that never get an answer', async () => {
    const t = setup()
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    await expect(t.client.request(DAEMON_ID, 'nobody.home', {}, 50)).rejects.toMatchObject({ kind: 'timeout' })
    t.client.stop()
  })

  it('dispatches daemon notifications', async () => {
    const t = setup()
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    t.hub.notify('session.changed', { session_id: 's1' })
    await vi.waitFor(() => expect(t.notes).toEqual([[DAEMON_ID, 'session.changed', { session_id: 's1' }]]))
    t.client.stop()
  })

  describe('plaintext notify envelopes (ADR-37)', () => {
    const notify = (from: string, body: unknown, seq = 1) => makeEnvelope(from, CLIENT_ID, 'notify', seq, encode(body))

    it('hands a paired node’s notify to the sink as { category, session_id }', async () => {
      const t = setup()
      await t.client.start('acct1')
      await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
      t.hub.socket.deliver(notify(DAEMON_ID, { category: 'permission_requested', session_id: 'sess_1', detail: null }))
      t.hub.socket.deliver(notify(DAEMON_ID, { category: 'enroll_requested', session_id: null, detail: null }, 2))
      await vi.waitFor(() =>
        expect(t.notifies).toEqual([
          [DAEMON_ID, { category: 'permission_requested', session_id: 'sess_1' }],
          [DAEMON_ID, { category: 'enroll_requested', session_id: null }],
        ]),
      )
      t.client.stop()
    })

    it('ignores a notify from a device that is not a paired node, and bodies it cannot read', async () => {
      const t = setup()
      await t.client.start('acct1')
      await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
      t.hub.socket.deliver(notify('x'.repeat(32), { category: 'security', session_id: null }))
      t.hub.socket.deliver(notify(DAEMON_ID, { category: 'launch_missiles' }, 2))
      t.hub.socket.deliver(notify(DAEMON_ID, 'nonsense', 3))
      t.hub.socket.deliver(notify(DAEMON_ID, { category: 'error', session_id: 'sess_ok' }, 4)) // 之后的正常信封照常处理
      await vi.waitFor(() => expect(t.notifies).toEqual([[DAEMON_ID, { category: 'error', session_id: 'sess_ok' }]]))
      t.client.stop()
    })
  })

  it('maps an RPC error response to the stable error code', async () => {
    const hub = new FakeHub({
      rpc: {
        'session.set_settings': () => {
          throw new FakeRpcError('invalid_settings', 'thinking')
        },
      },
    })
    const t = setup({ hub })
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    const err = await t.client.request(DAEMON_ID, 'session.set_settings', {}).catch((e: unknown) => e)
    expect(err).toBeInstanceOf(RpcCallError)
    expect(err).toMatchObject({ kind: 'rpc', code: 'invalid_settings', field: 'thinking' })
    t.client.stop()
  })

  it('marks the node noise_failed after the daemon ignores two handshakes', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const hub = new FakeHub({ ignoreHandshake: true })
    const t = setup({ hub })
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    expect(t.nodeStates.at(-1)?.conn).toBe('noise_handshaking')
    await vi.advanceTimersByTimeAsync(8_000)
    expect(t.nodeStates.at(-1)?.conn).toBe('noise_handshaking') // 第二次尝试
    await vi.advanceTimersByTimeAsync(8_000)
    expect(t.nodeStates.at(-1)?.conn).toBe('noise_failed')
    t.client.stop()
  })

  it('retries a data envelope after backpressure', async () => {
    const t = setup()
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    t.hub.backpressureOnce = 1
    const p = t.client.request(DAEMON_ID, 'node.info')
    await expect(p).resolves.toEqual({ name: 'n' })
    expect(t.hub.dataFrames).toHaveLength(1)
    t.client.stop()
  })
})

describe('AnyconClient — pairing (joiner, XXpsk3 initiator)', () => {
  const sentControls = () =>
    FakeSocket.instances
      .flatMap((s) => s.sent)
      .filter((e) => e.kind === 'control')
      .map((e) => decodeControl(e.body))

  /** 开着 fake 定时器跑一次配对，快进到超时，返回结果（成功 null / 错误）。 */
  async function pairWithTimers(t: ReturnType<typeof setup>, code = 'ABCD-2345'): Promise<unknown> {
    const p = t.client.pair(code, t.storage.identity!.device_name).then(
      () => null,
      (e: unknown) => e,
    )
    await vi.advanceTimersByTimeAsync(60_000)
    return p
  }

  it('pairs with a code, persists the identity and node key only after pair.ack, and reconnects as the new device', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.pairCodeOk = 'ABCD2345'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    const stages: PairStage[] = []
    const res = await t.client.pair('abcd-2345', 'My Browser', (s) => stages.push(s))
    expect(res.nodeId).toBe(DAEMON_ID)
    expect(Array.from(res.publicKey)).toEqual(Array.from(hub.daemonKp.public))
    expect(stages).toEqual(['connecting', 'deriving', 'matching', 'handshaking', 'confirming', 'verifying', 'done'])
    expect(t.storage.identity).toMatchObject({ device_id: CLIENT_ID, device_token: 'tok-new', device_name: 'My Browser' })
    // 节点名来自经过认证的 pair.ack；daemon 从消息 3 的 payload 里学到我们的 ClientInfo
    expect(t.storage.nodes.get(DAEMON_ID)?.name).toBe('test-node')
    expect(hub.joinerInfo).toEqual({ name: 'My Browser', kind: 'web', platform: 'browser', version: expect.any(String), origin: 'http://localhost' })
    // PairDone 之后同一条连接就是正式设备：不重连，收到 PeerOnline 后自动 IK 握手
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    expect(t.client.snapshot).toMatchObject({ paired: true, deviceId: CLIENT_ID })
    expect(hub.authFrames.at(-1)!.device_token ?? null).toBeNull()
    expect(hub.authFrames.filter((a) => a.device_token).length).toBe(0)
    // 握手信封用的是正式 id
    const last = hub.socket.sent.filter((e) => e.kind === 'handshake').at(-1)!
    expect(last.from).toBe(CLIENT_ID)
    t.client.stop()
  })

  it('only ever sends the hub a match_id: the code (and the PSK) never leaves the browser', async () => {
    const hub = new FakeHub()
    hub.pairCodeOk = 'ABCD2345'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    await t.client.pair('abcd-2345', 'x')
    expect(hub.joinMatchIds).toEqual([hub.crypto.deriveMatchId('ABCD2345')])
    const join = sentControls().find((c) => c.type === 'pair_join')
    expect(join).toEqual({ type: 'pair_join', match_id: hub.crypto.deriveMatchId('ABCD2345') })
    const wire = JSON.stringify(sentControls())
    expect(wire).not.toContain('ABCD')
    // 握手信封（hub 能看到的字节）里也不能直接出现短码
    for (const e of FakeSocket.instances.flatMap((s) => s.sent)) expect(new TextDecoder('latin1').decode(e.body)).not.toContain('ABCD2345')
    t.client.stop()
  })

  it('derives the pairing secrets once (match_id and PSK from one KDF) and wipes the PSK afterwards', async () => {
    const hub = new FakeHub()
    hub.pairCodeOk = 'ABCD2345'
    const crypto = hub.crypto
    const derive = vi.spyOn(crypto, 'derivePairSecrets')
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    const stages: PairStage[] = []
    await t.client.pair('abcd-2345', 'x', (s) => stages.push(s))
    // 一次是 web（joiner），一次是假 hub 扮演的 daemon；各一次 KDF，不是各两次
    expect(derive).toHaveBeenCalledTimes(2)
    expect(stages.slice(0, 3)).toEqual(['connecting', 'deriving', 'matching'])
    // web 派生出来的那份 PSK 用完已清零（假 hub 那份是另一个数组）
    const webPsk = derive.mock.results[0]!.value.psk as Uint8Array
    expect(webPsk).toHaveLength(32)
    expect(webPsk.every((b) => b === 0)).toBe(true)
    const hubPsk = derive.mock.results[1]!.value.psk as Uint8Array
    expect(hubPsk.some((b) => b !== 0)).toBe(true)
    t.client.stop()
  })

  it('wipes the PSK when pairing fails before the handshake object exists (code unknown to the hub)', async () => {
    const hub = new FakeHub()
    hub.pairCodeOk = 'WXYZ6789' // 与输入的短码不同：hub 回 pair_failed{not_found}
    const derive = vi.spyOn(hub.crypto, 'derivePairSecrets')
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    await expect(t.client.pair('abcd-2345', 'x')).rejects.toMatchObject({ code: 'not_found' })
    const webPsk = derive.mock.results[0]!.value.psk as Uint8Array
    expect(webPsk.every((b) => b === 0)).toBe(true)
    t.client.stop()
  })

  it('lets the UI paint the "deriving" step before the blocking KDF runs', async () => {
    const hub = new FakeHub()
    hub.pairCodeOk = 'ABCD2345'
    const order: string[] = []
    const t = setup({ paired: false, knowsNode: false, hub })
    ;(t.client as unknown as { deps: { yieldToPaint: () => Promise<void> } }).deps.yieldToPaint = async () => {
      order.push('paint')
    }
    vi.spyOn(hub.crypto, 'derivePairSecrets').mockImplementation(() => {
      order.push('kdf')
      return { matchId: hub.crypto.deriveMatchId('ABCD2345'), psk: hub.crypto.derivePsk('ABCD2345') }
    })
    await t.client.start('acct1')
    await t.client.pair('abcd-2345', 'x', (s) => order.push(s))
    expect(order.slice(0, 4)).toEqual(['connecting', 'deriving', 'paint', 'kdf'])
    t.client.stop()
  })

  it('reports a failing key derivation as a pairing failure (not "invalid code") and does not contact the hub', async () => {
    const hub = new FakeHub()
    hub.pairCodeOk = 'ABCD2345'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    vi.spyOn(hub.crypto, 'derivePairSecrets').mockImplementation(() => {
      throw new Error('out of memory')
    })
    await expect(t.client.pair('abcd-2345', 'x')).rejects.toMatchObject({ code: 'handshake_failed' })
    expect(hub.joinMatchIds).toEqual([])
    t.client.stop()
  })

  it('pairs with a 16-character code', async () => {
    const hub = new FakeHub()
    hub.pairCodeOk = 'ABCD2345WXYZ6789'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    const res = await t.client.pair('abcd-2345-wxyz-6789', 'x')
    expect(res.nodeId).toBe(DAEMON_ID)
    expect(hub.joinMatchIds).toEqual([hub.crypto.deriveMatchId('ABCD2345WXYZ6789')])
    t.client.stop()
  })

  it('rejects a malformed code before touching the hub', async () => {
    const hub = new FakeHub()
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    for (const bad of ['ABCD', 'ABCD-2345-WXYZ', 'ABCD-234O', '']) await expect(t.client.pair(bad, 'x')).rejects.toMatchObject({ code: 'invalid_code' })
    expect(hub.joinMatchIds).toEqual([])
    t.client.stop()
  })

  it('starts the IK handshake itself when the hub sends no PeerOnline after PairDone', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.pairCodeOk = 'ABCD2345'
    hub.peerOnlineAfterPair = false
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    const p = t.client.pair('ABCD-2345', t.storage.identity!.device_name)
    await vi.advanceTimersByTimeAsync(10)
    await p
    expect(t.client.isReady(DAEMON_ID)).toBe(false)
    await vi.advanceTimersByTimeAsync(1600)
    expect(t.client.isReady(DAEMON_ID)).toBe(true)
    t.client.stop()
  })

  it('can pair again after the device was revoked (token cleared, halted)', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.pairCodeOk = 'ABCD2345'
    const t = setup({ hub })
    hub.authFail = 'revoked'
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.snapshot.halt).toBe('revoked'))
    hub.authFail = null
    await t.client.pair('ABCD-2345', t.storage.identity!.device_name)
    expect(t.storage.identity?.device_token).toBe('tok-new')
    t.client.stop()
  })

  it('re-pairing a node that is already known ends up ready, with one IK exchange answered for the right handshake', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.pairCodeOk = 'ABCD2345'
    hub.ikDelayMs = 25 // daemon 的 IK 回复比 pair.ack 慢到：两次握手请求会交错
    const t = setup({ hub })
    hub.authFail = 'revoked'
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.snapshot.halt).toBe('revoked'))
    hub.authFail = null
    await t.client.pair('ABCD-2345', t.storage.identity!.device_name)
    // PairDone 之后 hub 的 PeerOnline 和 attachNode 都会要求握手：只能有一条在途，回复不能喂错握手
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true), { timeout: 3000 })
    expect(t.nodeStates.some((s) => s.conn === 'noise_failed')).toBe(false)
    expect(hub.ikHandshakes).toBe(1) // PairDone 之后 attachNode 和 hub 的 PeerOnline 各要求一次，但只发了一条 IK
    await expect(t.client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    t.client.stop()
  })

  it('re-pairing right after a revoke: the old link sends no IK into the pairing handshake (temporary connection)', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.pairCodeOk = 'ABCD2345'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    // 第一次配对：完成后留下「1.5 秒内没有 PeerOnline 就自己发 IK」的兜底定时器
    const first = t.client.pair('ABCD-2345', t.storage.identity!.device_name)
    await vi.advanceTimersByTimeAsync(10)
    await first
    await vi.advanceTimersByTimeAsync(10)
    expect(t.client.isReady(DAEMON_ID)).toBe(true)

    // 吊销：hub 以 4004 断开，token 被清掉
    FakeSocket.instances.at(-1)!.serverClose(4004)
    await vi.advanceTimersByTimeAsync(0)
    expect(t.client.snapshot.halt).toBe('revoked')

    // 马上以临时连接重新配对；daemon 的消息 2 晚到，兜底定时器正好落在配对握手中间
    hub.pairReplyDelayMs = 3000
    const ikBefore = hub.ikHandshakes
    const second = t.client.pair('ABCD-2345', t.storage.identity!.device_name).then(
      () => null,
      (e: unknown) => e,
    )
    await vi.advanceTimersByTimeAsync(60_000)
    expect(hub.pairHandshakeFailures).toBe(0)
    expect(await second).toBeNull()
    expect(t.storage.identity?.device_token).toBe('tok-new')
    // 只有 pair_done 之后那一条 IK
    await vi.advanceTimersByTimeAsync(10)
    expect(hub.ikHandshakes - ikBefore).toBe(1)
    expect(t.client.isReady(DAEMON_ID)).toBe(true)
    t.client.stop()
  })

  it('falls back to a short id label when the daemon sends no hello payload', async () => {
    const { parseNodeHello } = await import('./client')
    expect(parseNodeHello(new Uint8Array(0))).toEqual({})
    expect(parseNodeHello(new TextEncoder().encode('not json'))).toEqual({})
    expect(parseNodeHello(new TextEncoder().encode('{"name":"mac","version":"1.2.3","x":1}'))).toEqual({ name: 'mac', version: '1.2.3' })
    expect(parseNodeHello(new TextEncoder().encode('{"name":"a\\u001b[31mb"}'))).toEqual({ name: 'a[31mb' })
  })

  it('sends the joiner handshake messages through the hub as handshake envelopes', async () => {
    const hub = new FakeHub()
    hub.pairCodeOk = 'ABCD2345'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    await t.client.pair('ABCD-2345', 'x')
    const sent = FakeSocket.instances.flatMap((s) => s.sent)
    // 配对握手的消息 1 和 3 用临时 id 发出；之后的 IK 握手用正式 id
    expect(sent.filter((e) => e.kind === 'handshake' && e.to === DAEMON_ID && e.from === TEMP_ID)).toHaveLength(2)
    expect(sent.filter((e) => e.kind === 'handshake' && e.to === DAEMON_ID && e.from === CLIENT_ID).length).toBeGreaterThanOrEqual(1)
    const control = sentControls().map((c) => c.type)
    expect(control).toContain('pair_join')
    expect(control).toContain('pair_confirm')
    t.client.stop()
  })

  it('uses PairDone.peer (the permanent id) as the node id when the daemon paired under a temporary id', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.pairCodeOk = 'ABCD2345'
    hub.matchedPeer = 'f'.repeat(32)
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    const res = await t.client.pair('ABCD-2345', 'x')
    expect(res.nodeId).toBe(DAEMON_ID)
    expect([...t.storage.nodes.keys()]).toEqual([DAEMON_ID])
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    t.client.stop()
  })

  it('surfaces not_found from the hub', async () => {
    const hub = new FakeHub()
    hub.pairCodeOk = 'ZZZZ2222'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    await expect(t.client.pair('ABCD-2345', 'x')).rejects.toMatchObject({ code: 'not_found' })
    expect(t.storage.nodes.size).toBe(0)
    t.client.stop()
  })

  it('fails with timeout when the hub never answers', async () => {
    const hub = new FakeHub()
    hub.pairFailReason = 'timeout'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    const err = await t.client.pair('ABCD-2345', 'x').catch((e: unknown) => e)
    expect(err).toBeInstanceOf(PairError)
    expect((err as PairError).code).toBe('timeout')
    t.client.stop()
  })

  describe('pair.ack verification (a hub that forges messages 2 and PairDone must not get a node persisted)', () => {
    beforeEach(() => {
      vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    })

    async function run(ackMode: AckMode, opts: { impersonate?: boolean; existing?: Uint8Array } = {}) {
      const hub = new FakeHub()
      hub.pairCodeOk = 'ABCD2345'
      hub.ackMode = ackMode
      hub.impersonate = opts.impersonate ?? false
      const t = setup({ paired: false, knowsNode: false, hub })
      if (opts.existing) t.storage.nodes.set(DAEMON_ID, { node_id: DAEMON_ID, name: 'old', public_key: opts.existing, paired_at: 5, last_seen: 5 })
      await t.client.start('acct1')
      await vi.advanceTimersByTimeAsync(0)
      const result = await pairWithTimers(t)
      return { t, hub, result }
    }

    it('fails after 30 s without a pair.ack and persists no node', async () => {
      const { t, result } = await run('none')
      expect(result).toBeInstanceOf(PairError)
      expect((result as PairError).code).toBe('timeout')
      expect(t.storage.nodes.size).toBe(0)
      t.client.stop()
    })

    it('ignores a forged (undecryptable) ack and times out without persisting', async () => {
      const { t, result } = await run('forged')
      expect((result as PairError).code).toBe('timeout')
      expect(t.storage.nodes.size).toBe(0)
      t.client.stop()
    })

    it('a hub that does not know the code (wrong PSK) and fakes PairDone gets nothing persisted', async () => {
      const { t, result } = await run('good', { impersonate: true })
      expect(result).toBeInstanceOf(PairError)
      expect(t.storage.nodes.size).toBe(0)
      t.client.stop()
    })

    it.each([
      ['wrong_peer', 'peer_id'],
      ['wrong_node', 'node_id'],
      ['wrong_fingerprint', 'fingerprint'],
      ['not_ack', 'pair.ack'],
    ] as const)('rejects an ack whose %s does not match', async (mode, what) => {
      const { t, result } = await run(mode)
      expect(result).toBeInstanceOf(PairError)
      expect((result as PairError).code).toBe('ack_invalid')
      expect((result as PairError).message).toContain(what)
      expect(t.storage.nodes.size).toBe(0)
      t.client.stop()
    })

    it('refuses to overwrite a node that has the same id but a different public key', async () => {
      const other = createOtherKey()
      const { t, result } = await run('good', { existing: other })
      expect((result as PairError).code).toBe('key_changed')
      expect(Array.from(t.storage.nodes.get(DAEMON_ID)!.public_key)).toEqual(Array.from(other))
      expect(t.storage.nodes.get(DAEMON_ID)!.name).toBe('old')
      t.client.stop()
    })

    it('keeps an existing node (and its paired_at) when the key is the same', async () => {
      const hub = new FakeHub()
      hub.pairCodeOk = 'ABCD2345'
      const t = setup({ paired: false, knowsNode: false, hub })
      t.storage.nodes.set(DAEMON_ID, { node_id: DAEMON_ID, name: 'old', public_key: hub.daemonKp.public, paired_at: 5, last_seen: 5 })
      await t.client.start('acct1')
      await vi.advanceTimersByTimeAsync(0)
      expect(await pairWithTimers(t)).toBeNull()
      expect(t.storage.nodes.get(DAEMON_ID)).toMatchObject({ name: 'test-node', paired_at: 5 })
      t.client.stop()
    })
  })
})

describe('AnyconClient — page in the background (spec/web.md § 7)', () => {
  it('suspendForBackground closes the hub connection (a normal close), does not reconnect, and onVisible reconnects and re-syncs', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const t = setup()
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    expect(t.ready).toEqual([DAEMON_ID])
    const first = t.hub.socket

    expect(t.client.suspendForBackground()).toBe(true)
    expect(first.closedWith).toBe(1000)
    expect(t.hubStates.at(-1)).toMatchObject({ phase: 'hub_disconnected', halt: null, attempt: 0 })
    expect(t.nodeStates.at(-1)?.conn).toBe('hub_disconnected')
    expect(t.client.isReady(DAEMON_ID)).toBe(false)
    // 不是失败：没有退避，也没有任何重连
    await vi.advanceTimersByTimeAsync(10 * 60_000)
    expect(FakeSocket.instances).toHaveLength(1)

    await t.client.onVisible()
    await vi.advanceTimersByTimeAsync(0)
    expect(FakeSocket.instances).toHaveLength(2)
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    expect(t.hubStates.at(-1)).toMatchObject({ phase: 'hub_authed' })
    expect(t.ready).toEqual([DAEMON_ID, DAEMON_ID]) // 节点又就绪了一次：页面据此重新同步（nodeReady → syncNode）
    await expect(t.client.request(DAEMON_ID, 'node.info')).resolves.toEqual({ name: 'n' })
    t.client.stop()
  })

  it('cancels a reconnect that is already queued (the hub was down when the page went to the background)', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const t = setup()
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    t.hub.socket.serverClose(1006)
    expect(t.hubStates.at(-1)?.retryInMs).toBeGreaterThan(0)

    expect(t.client.suspendForBackground()).toBe(true)
    await vi.advanceTimersByTimeAsync(60_000)
    expect(FakeSocket.instances).toHaveLength(1) // 后台里不重连：重连成功就又成了「在线」

    await t.client.onVisible()
    await vi.advanceTimersByTimeAsync(0)
    expect(FakeSocket.instances).toHaveLength(2)
    await vi.waitFor(() => expect(t.hubStates.at(-1)?.phase).toBe('hub_authed'))
    t.client.stop()
  })

  it('does not disconnect while a pairing or enrollment is in progress (and says so)', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] })
    const hub = new FakeHub()
    hub.autoAuth = false // 配对卡在「等认证」：pairing 一直是 true
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    const pairing = t.client.pair('ABCD-2345', t.storage.identity!.device_name).catch((e: unknown) => e)
    await vi.advanceTimersByTimeAsync(300)
    expect(t.client.suspendForBackground()).toBe(false)
    expect(hub.socket.closedWith).toBeNull()
    expect(FakeSocket.instances).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(20_000) // 配对超时收场之后可以断
    expect(await pairing).toBeInstanceOf(PairError)
    expect(t.client.suspendForBackground()).toBe(true)
    expect(hub.socket.closedWith).toBe(1000)
    t.client.stop()
  })

  it('does not disconnect while a request is in flight; once it is answered the next try goes through', async () => {
    const t = setup()
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    const slow = t.client.request(DAEMON_ID, 'nobody.home', {}, 80).catch((e: unknown) => e) // 假 daemon 不回这个方法
    expect(t.client.suspendForBackground()).toBe(false)
    expect(t.hub.socket.closedWith).toBeNull()
    expect(await slow).toBeInstanceOf(RpcCallError)
    expect(t.client.suspendForBackground()).toBe(true)
    expect(t.hub.socket.closedWith).toBe(1000)
    t.client.stop()
  })

  it('a hub halt (revoked) is not lifted by going to the background and coming back', async () => {
    const t = setup()
    t.hub.authFail = 'revoked'
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.hubStates.at(-1)?.halt).toBe('revoked'))
    const sockets = FakeSocket.instances.length
    expect(t.client.suspendForBackground()).toBe(true)
    expect(t.hubStates.at(-1)).toMatchObject({ halt: 'revoked', retryInMs: null })
    await t.client.onVisible()
    expect(FakeSocket.instances).toHaveLength(sockets)
    t.client.stop()
  })

  it('manual actions on a visible page also end the suspension (retry / take over / pairing need a connection)', async () => {
    const t = setup()
    await t.client.start('acct1')
    await vi.waitFor(() => expect(t.client.isReady(DAEMON_ID)).toBe(true))
    expect(t.client.suspendForBackground()).toBe(true)
    t.client.forceReconnect()
    await vi.waitFor(() => expect(t.hubStates.at(-1)?.phase).toBe('hub_authed'))
    expect(FakeSocket.instances).toHaveLength(2)

    expect(t.client.suspendForBackground()).toBe(true)
    t.client.takeOver()
    await vi.waitFor(() => expect(t.hubStates.at(-1)?.phase).toBe('hub_authed'))
    expect(FakeSocket.instances).toHaveLength(3)
    t.client.stop()
  })

  it('a stopped client has nothing to suspend', () => {
    const t = setup()
    expect(t.client.suspendForBackground()).toBe(true)
    expect(FakeSocket.instances).toHaveLength(0)
  })
})

function createOtherKey(): Uint8Array {
  return new Uint8Array(32).fill(7)
}
