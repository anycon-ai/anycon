// 账号内接入（ADR-36）的发起方：整个 AnyconClient.enroll 对假 hub + 假 daemon（responder，真的两方密码学）。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { parseEnrollQr } from '@/lib/enrollQr'
import { CLIENT_ID, DAEMON_ID, FakeHub, FakeSocket, setupClient, TEMP_ID } from '@/test/harness'
import type { FakeEnrollDaemon } from '@/test/enrollDaemon'
import { EnrollError, type EnrollHooks, type EnrollStage } from './enroll'
import type { EnrollRequest } from './client'
import { decodeControl, makeEnvelope } from './wire'

beforeEach(() => {
  FakeSocket.instances = []
})
afterEach(() => {
  vi.useRealTimers()
})

const NAME = 'Chrome · macOS · localhost'

interface Rig {
  t: ReturnType<typeof setupClient>
  hub: FakeHub
  daemon: FakeEnrollDaemon
  stages: EnrollStage[]
  qr: string[]
  asks: Array<{ attemptsLeft: number | null; wrong: boolean }>
  hooks: (opts?: { codes?: Array<string | null> }) => EnrollHooks
}

/** 一个没配对过的浏览器（临时连接）+ 一台开着接入的假 daemon。 */
async function rig(opts: { paired?: boolean } = {}): Promise<Rig> {
  const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
  const daemon = hub.enableEnroll()
  const t = setupClient({ paired: opts.paired ?? false, knowsNode: false, hub })
  await t.client.start('acct1')
  const stages: EnrollStage[] = []
  const qr: string[] = []
  const asks: Rig['asks'] = []
  const hooks: Rig['hooks'] = ({ codes = [] } = {}) => ({
    stage: (s) => stages.push(s),
    qr: (p) => qr.push(p),
    askCode: async (ctx) => {
      asks.push(ctx)
      return codes.length > 0 ? codes.shift()! : null
    },
  })
  return { t, hub, daemon, stages, qr, asks, hooks }
}

const req = (r: Partial<EnrollRequest> & Pick<EnrollRequest, 'method'>): EnrollRequest => ({ node: DAEMON_ID, deviceName: NAME, ...r })
const sentControls = () => FakeSocket.instances.flatMap((s) => s.sent).filter((e) => e.kind === 'control').map((e) => decodeControl(e.body))
const errorOf = (p: Promise<unknown>): Promise<EnrollError> => p.then(() => { throw new Error('expected the enrollment to fail') }, (e: unknown) => e as EnrollError)

/** 所有发给 hub 的信封，拼成一串字节：断言口令 / 码 / 密钥没有以明文出现在 hub 看得见的地方。 */
function wireBytes(): string {
  return FakeSocket.instances
    .flatMap((s) => s.sent)
    .map((e) => Array.from(e.body, (b) => String.fromCharCode(b)).join(''))
    .join('\n')
}

describe('enroll — node password', () => {
  it('unlocks with the right password: temp connection, CPace + XXpsk3, pair.ack, then persisted', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    const res = await r.t.client.enroll(req({ method: 'password', password: 'correct horse', deviceName: 'My Browser' }), r.hooks())

    expect(res.nodeId).toBe(DAEMON_ID)
    expect(r.stages).toEqual(['connecting', 'starting', 'deriving', 'verifying', 'handshaking', 'confirming', 'finishing', 'done'])
    expect(r.hub.enrollBegins).toEqual([DAEMON_ID])
    expect(r.hub.joinMatchIds).toEqual([]) // 不是短码配对：没有 match_id
    // hello：明文，不带 approval；hint 只是自报的名字
    expect(r.daemon.hellos).toHaveLength(1)
    expect(r.daemon.hellos[0]).toMatchObject({ method: 'password', approval: undefined, hint: { name: 'My Browser', kind: 'web', platform: 'browser' } })
    expect(r.daemon.hellos[0]!.nonce).toMatch(/^[0-9a-f]{32}$/)
    // daemon 从 XXpsk3 第 3 条（已认证）里学到 ClientInfo；失败计数清零
    expect(r.daemon.lastClient).toMatchObject({ name: 'My Browser', kind: 'web', platform: 'browser' })
    expect(r.daemon.failures).toBe(0)
    // 临时连接原地升级成设备；节点名取自经过认证的 pair.ack
    expect(r.t.storage.identity).toMatchObject({ device_id: CLIENT_ID, device_token: 'tok-new', device_name: 'My Browser' })
    expect(r.t.storage.nodes.get(DAEMON_ID)).toMatchObject({ name: 'test-node' })
    expect(Array.from(r.t.storage.nodes.get(DAEMON_ID)!.public_key)).toEqual(Array.from(r.hub.daemonKp.public))
    await vi.waitFor(() => expect(r.t.client.isReady(DAEMON_ID)).toBe(true))
    // hub 只看到密文 / CPace 份额：口令不在任何信封里
    expect(wireBytes()).not.toContain('correct horse')
    // 收尾是配对的收尾：N 发了一次 pair_confirm，没有 enroll_cancel
    expect(sentControls().filter((c) => c.type === 'pair_confirm')).toHaveLength(1)
    expect(r.hub.enrollCancels).toEqual([])
    r.t.client.stop()
  })

  it('a formal device (already paired to something else) enrolls on the connection it has, keeping its token', async () => {
    const r = await rig({ paired: true })
    r.daemon.password = 'correct horse'
    await r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks())
    expect(r.t.storage.identity).toMatchObject({ device_id: CLIENT_ID, device_token: 'tok' })
    expect(r.t.storage.nodes.has(DAEMON_ID)).toBe(true)
    expect(r.daemon.session).toBeNull()
    // 正式设备发 enroll_begin 时 from 是自己的正式 id
    expect(r.hub.authFrames.at(-1)!.device_token).toBe('tok')
    r.t.client.stop()
  })

  it('a wrong password is "wrong_secret": nothing is stored, the hub session is cancelled, the counter stays up', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: 'wrong horse' }), r.hooks()))
    expect(err).toBeInstanceOf(EnrollError)
    expect(err.code).toBe('wrong_secret')
    expect(r.t.storage.nodes.size).toBe(0)
    expect(r.t.storage.identity?.device_token).toBeUndefined()
    expect(r.daemon.failures).toBe(1) // 悲观计数：份额发出时 +1，没被第 3 条清零
    expect(r.hub.enrollCancels).toHaveLength(1) // 会话还在 hub 上：主动取消，不占名额
    expect(r.t.client.snapshot.paired).toBe(false)
    r.t.client.stop()
  })

  it('one session tries the password once: a later enroll is a brand new session', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    await errorOf(r.t.client.enroll(req({ method: 'password', password: 'nope nope' }), r.hooks()))
    await r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks())
    expect(r.hub.enrollBegins).toEqual([DAEMON_ID, DAEMON_ID])
    expect(r.daemon.failures).toBe(0)
    r.t.client.stop()
  })

  it.each([
    ['password_not_set', () => ({ password: null })],
    ['locked', () => ({ passwordState: 'locked' as const })],
    ['method_disabled', () => ({ passwordState: 'disabled' as const })],
    ['busy', () => ({ busy: true })],
  ] as const)('maps the daemon rejection %s to the error code', async (code, patch) => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    Object.assign(r.daemon, patch())
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks()))
    expect(err.code).toBe(code)
    expect(r.t.storage.nodes.size).toBe(0)
    r.t.client.stop()
  })

  it('backoff carries retry_after_ms so the UI can count down', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    r.daemon.passwordState = 'backoff'
    r.daemon.backoffMs = 240_000
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks()))
    expect(err.code).toBe('backoff')
    expect(err.retryAfterMs).toBe(240_000)
    r.t.client.stop()
  })

  it('an empty password never leaves the browser', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: '' }), r.hooks()))
    expect(err.code).toBe('invalid')
    r.t.client.stop()
  })
})

describe('enroll — access code', () => {
  it('uses a code made in "add device" (no approval) and asks for it once the daemon is ready', async () => {
    const r = await rig()
    r.daemon.activeCode = '123456'
    const res = await r.t.client.enroll(req({ method: 'code', approval: false }), r.hooks({ codes: ['123 456'] }))
    expect(res.nodeId).toBe(DAEMON_ID)
    expect(r.daemon.hellos[0]).toMatchObject({ method: 'code', approval: false })
    expect(r.stages).toEqual(['connecting', 'starting', 'code', 'verifying', 'handshaking', 'confirming', 'finishing', 'done'])
    expect(r.asks).toEqual([{ attemptsLeft: null, wrong: false }])
    expect(wireBytes()).not.toContain('123456')
    r.t.client.stop()
  })

  it('after a wrong code it asks again in the same session with the attempts left, and sends pair_confirm only once', async () => {
    const r = await rig()
    r.daemon.activeCode = '123456'
    await r.t.client.enroll(req({ method: 'code', approval: false }), r.hooks({ codes: ['000000', '654321', '123456'] }))
    expect(r.asks).toEqual([
      { attemptsLeft: null, wrong: false },
      { attemptsLeft: 2, wrong: true },
      { attemptsLeft: 1, wrong: true },
    ])
    expect(r.hub.enrollBegins).toHaveLength(1) // 一个会话
    const nonces = r.daemon.hellos.map((h) => h.nonce)
    expect(new Set(nonces).size).toBe(3) // 每一轮新 nonce
    expect(sentControls().filter((c) => c.type === 'pair_confirm')).toHaveLength(1)
    r.t.client.stop()
  })

  it('three wrong codes use the code up: wrong_secret with no attempts left, and the session is cancelled', async () => {
    const r = await rig()
    r.daemon.activeCode = '123456'
    const err = await errorOf(r.t.client.enroll(req({ method: 'code', approval: false }), r.hooks({ codes: ['000001', '000002', '000003', '123456'] })))
    expect(err.code).toBe('wrong_secret')
    expect(err.attemptsLeft).toBe(0)
    expect(r.asks).toHaveLength(3) // 第 4 个（正确的）码从来没被问到
    expect(r.hub.enrollCancels).toHaveLength(1)
    expect(r.t.storage.nodes.size).toBe(0)
    r.t.client.stop()
  })

  it('"no_code" when the daemon has no active code', async () => {
    const r = await rig()
    const err = await errorOf(r.t.client.enroll(req({ method: 'code', approval: false }), r.hooks({ codes: ['123456'] })))
    expect(err.code).toBe('no_code')
    expect(r.asks).toHaveLength(0)
    r.t.client.stop()
  })

  it('gives up cleanly when the user closes the code prompt', async () => {
    const r = await rig()
    r.daemon.activeCode = '123456'
    const err = await errorOf(r.t.client.enroll(req({ method: 'code', approval: false }), r.hooks({ codes: [null] })))
    expect(err.code).toBe('cancelled')
    expect(r.hub.enrollCancels).toHaveLength(1)
    r.t.client.stop()
  })

  it('"request approval": waits for an already-paired device to approve, then takes the code shown there', async () => {
    const r = await rig()
    const hooks = r.hooks({ codes: ['482913'] })
    const stage = hooks.stage
    // 看到「等待」就让 daemon 这边（扮演已配对设备）批准
    hooks.stage = (s) => {
      stage(s)
      if (s === 'waiting') queueMicrotask(() => r.daemon.approveCode('482913'))
    }
    const res = await r.t.client.enroll(req({ method: 'code', approval: true }), hooks)
    expect(res.nodeId).toBe(DAEMON_ID)
    expect(r.daemon.hellos[0]).toMatchObject({ method: 'code', approval: true })
    expect(r.stages).toEqual(['connecting', 'starting', 'waiting', 'code', 'verifying', 'handshaking', 'confirming', 'finishing', 'done'])
    r.t.client.stop()
  })

  it('"request approval" after a wrong code asks again in the same session with the code that was already approved (same code, new nonce)', async () => {
    const r = await rig()
    const hooks = r.hooks({ codes: ['000000', '482913'] })
    const stage = hooks.stage
    let approved = false
    hooks.stage = (s) => {
      stage(s)
      if (s === 'waiting' && !approved) {
        approved = true
        queueMicrotask(() => r.daemon.approveCode('482913'))
      }
    }
    const res = await r.t.client.enroll(req({ method: 'code', approval: true }), hooks)
    expect(res.nodeId).toBe(DAEMON_ID)
    expect(r.asks).toEqual([
      { attemptsLeft: null, wrong: false },
      { attemptsLeft: 2, wrong: true }, // 份额到了才扣一次，输错一次还剩 2 次
    ])
    expect(r.hub.enrollBegins).toHaveLength(1) // 一个会话
    expect(r.daemon.hellos).toHaveLength(2)
    expect(new Set(r.daemon.hellos.map((h) => h.nonce)).size).toBe(2) // 重试用新的 nonce
    expect(r.daemon.session).toBeNull()
    r.t.client.stop()
  })

  it('"request approval": the user may take minutes to type the code — the daemon sent `ready` when it was approved, and its 60 s window only starts at our CPace share', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] })
    const r = await rig()
    const hooks = r.hooks()
    const stage = hooks.stage
    hooks.stage = (s) => {
      stage(s)
      if (s === 'waiting') queueMicrotask(() => r.daemon.approveCode('482913'))
    }
    // 批准之后，用户隔了 4 分钟才输完码
    hooks.askCode = (ctx) => {
      r.asks.push(ctx)
      return new Promise<string | null>((resolve) => setTimeout(() => resolve('482913'), 4 * 60_000))
    }
    const p = r.t.client.enroll(req({ method: 'code', approval: true }), hooks).then(
      (v) => v,
      (e: unknown) => e as EnrollError,
    )
    await vi.advanceTimersByTimeAsync(4 * 60_000 + 1_000)
    await vi.advanceTimersByTimeAsync(30_000) // 万一 daemon 不理我们，这里会走到 web 自己的 20 秒超时，下面的断言给出清楚的失败
    const res = await p
    expect(res).not.toBeInstanceOf(Error)
    expect(res).toMatchObject({ nodeId: DAEMON_ID })
    expect(r.daemon.completed).toBe(1)
    r.t.client.stop()
  })

  it.each([['expired'], ['no_code']] as const)('the daemon refusing at the CPace share (%s: the code ran out or was replaced while `ready` was waiting) ends the enrollment with that reason, not a hang', async (reason) => {
    const r = await rig()
    r.daemon.activeCode = '123456'
    r.daemon.refuseShare = reason
    const err = await errorOf(r.t.client.enroll(req({ method: 'code', approval: false }), r.hooks({ codes: ['123456', '123456'] })))
    expect(err.code).toBe(reason)
    expect(r.asks).toHaveLength(1) // 码已经用不了：不再问第二次
    expect(r.t.storage.nodes.size).toBe(0)
    r.t.client.stop()
  })

  it('a denied request is "denied"', async () => {
    const r = await rig()
    const hooks = r.hooks()
    const stage = hooks.stage
    hooks.stage = (s) => {
      stage(s)
      if (s === 'waiting') queueMicrotask(() => r.daemon.deny())
    }
    const err = await errorOf(r.t.client.enroll(req({ method: 'code', approval: true }), hooks))
    expect(err.code).toBe('denied')
    r.t.client.stop()
  })

  it('the daemon turning approvals off is "method_disabled"', async () => {
    const r = await rig()
    r.daemon.approvalEnabled = false
    const err = await errorOf(r.t.client.enroll(req({ method: 'code', approval: true }), r.hooks()))
    expect(err.code).toBe('method_disabled')
    r.t.client.stop()
  })
})

describe('enroll — scan', () => {
  it('shows a QR with anycon-enroll:1:<node>:<enroll_id>:<base32(r)> and finishes once a paired device approves with r', async () => {
    const r = await rig()
    const hooks = r.hooks()
    const qr = hooks.qr!
    hooks.qr = (payload) => {
      qr(payload)
      const parsed = parseEnrollQr(payload)!
      // 已配对设备扫到 → node.enroll_approve { enroll_id, secret }
      queueMicrotask(() => r.daemon.approveScan(parsed.secret))
    }
    const res = await r.t.client.enroll(req({ method: 'scan' }), hooks)
    expect(res.nodeId).toBe(DAEMON_ID)
    expect(r.qr).toHaveLength(1)
    const parsed = parseEnrollQr(r.qr[0]!)!
    expect(parsed.nodeId).toBe(DAEMON_ID)
    expect(parsed.enrollId).toBe('0'.repeat(31) + '1') // hub 分配的 enroll_id
    expect(parsed.secret).toHaveLength(32)
    expect(r.stages).toEqual(['connecting', 'starting', 'waiting', 'handshaking', 'confirming', 'finishing', 'done'])
    expect(r.daemon.hellos[0]).toMatchObject({ method: 'scan' })
    // r 只在二维码里：hub 看见的信封里没有它（hex 或原始字节都没有）
    const secretHex = Array.from(parsed.secret, (b) => b.toString(16).padStart(2, '0')).join('')
    expect(wireBytes()).not.toContain(secretHex)
    expect(wireBytes()).not.toContain(Array.from(parsed.secret, (b) => String.fromCharCode(b)).join(''))
    r.t.client.stop()
  })

  it('ignores handshake envelopes from the daemon that are not enrollment frames (leftovers of an old IK handshake)', async () => {
    const r = await rig()
    const hooks = r.hooks()
    hooks.qr = (payload) => {
      r.hub.socket.deliver(makeEnvelope(DAEMON_ID, TEMP_ID, 'handshake', 999, new Uint8Array([0xde, 0xad, 0xbe, 0xef])))
      queueMicrotask(() => r.daemon.approveScan(parseEnrollQr(payload)!.secret))
    }
    await expect(r.t.client.enroll(req({ method: 'scan' }), hooks)).resolves.toMatchObject({ nodeId: DAEMON_ID })
    r.t.client.stop()
  })

  it('does not show the QR before the daemon is waiting for approval', async () => {
    const r = await rig()
    r.daemon.approvalEnabled = false
    const err = await errorOf(r.t.client.enroll(req({ method: 'scan' }), r.hooks()))
    expect(err.code).toBe('method_disabled')
    expect(r.qr).toEqual([])
    r.t.client.stop()
  })

  it('a different secret from the one in the QR cannot finish the handshake', async () => {
    const r = await rig()
    const hooks = r.hooks()
    hooks.qr = () => queueMicrotask(() => r.daemon.approveScan(new Uint8Array(32).fill(7)))
    const err = await errorOf(r.t.client.enroll(req({ method: 'scan' }), hooks))
    expect(err.code).toBe('wrong_secret')
    expect(r.t.storage.nodes.size).toBe(0)
    r.t.client.stop()
  })

  it('leaving the page (cancelEnroll) while the QR is up cancels the hub session and ends with "cancelled"', async () => {
    const r = await rig()
    const hooks = r.hooks()
    const record = hooks.qr!
    hooks.qr = (p) => {
      record(p)
      queueMicrotask(() => r.t.client.cancelEnroll())
    }
    const err = await errorOf(r.t.client.enroll(req({ method: 'scan' }), hooks))
    expect(err.code).toBe('cancelled')
    expect(r.hub.enrollCancels).toEqual(['0'.repeat(31) + '1'])
    expect(r.hub.enrollId).toBeNull()
    // 之后可以再来一次（新的二维码、新的 enroll_id、新的密钥）
    const hooks2 = r.hooks()
    const record2 = hooks2.qr!
    hooks2.qr = (p) => {
      record2(p)
      queueMicrotask(() => r.daemon.approveScan(parseEnrollQr(p)!.secret))
    }
    await expect(r.t.client.enroll(req({ method: 'scan' }), hooks2)).resolves.toMatchObject({ nodeId: DAEMON_ID })
    expect(r.qr).toHaveLength(2)
    const [a, b] = [parseEnrollQr(r.qr[0]!)!, parseEnrollQr(r.qr[1]!)!]
    expect(b.enrollId).not.toBe(a.enrollId)
    expect(Array.from(b.secret)).not.toEqual(Array.from(a.secret))
    r.t.client.stop()
  })
})

describe('enroll — hub-side failures', () => {
  it.each(['forbidden', 'quota', 'rate_limited', 'offline', 'busy'] as const)('enroll_begin refused with %s', async (reason) => {
    const r = await rig()
    r.hub.enrollBeginFail = reason
    r.daemon.password = 'correct horse'
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks()))
    expect(err.code).toBe(reason)
    expect(r.hub.enrollCancels).toEqual([]) // 没有会话，没有什么可取消的
    r.t.client.stop()
  })

  it('no answer at all to enroll_begin (the hub drops a rate-limit receipt) is "hub_no_reply" after 20 s — not the generic "timeout" that a scan would retry by itself', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const r = await rig()
    r.hub.enrollBeginSilent = true
    r.daemon.password = 'correct horse'
    let result: unknown = 'pending'
    void r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks()).then(
      (v) => (result = v),
      (e: unknown) => (result = e),
    )
    await vi.advanceTimersByTimeAsync(19_000)
    expect(result).toBe('pending') // 等待时间不变：20 秒
    await vi.advanceTimersByTimeAsync(2_000)
    expect(result).toBeInstanceOf(EnrollError)
    expect((result as EnrollError).code).toBe('hub_no_reply')
    expect(r.hub.enrollBegins).toEqual([DAEMON_ID])
    expect(r.hub.enrollCancels).toEqual([]) // 没有会话，没有什么可取消的
    r.t.client.stop()
  })

  it('a node that is not this account\'s is "not_found"', async () => {
    const r = await rig()
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: 'x'.repeat(8), node: 'e'.repeat(32) }), r.hooks()))
    expect(err.code).toBe('not_found')
    r.t.client.stop()
  })

  it('the hub ending the session (timeout) ends the wait with that reason and does not echo a cancel', async () => {
    const r = await rig()
    const hooks = r.hooks()
    hooks.qr = () => queueMicrotask(() => r.hub.failEnroll('timeout'))
    const err = await errorOf(r.t.client.enroll(req({ method: 'scan' }), hooks))
    expect(err.code).toBe('timeout')
    expect(r.hub.enrollCancels).toEqual([])
    r.t.client.stop()
  })

  it('the hub ending the session while the code prompt is open interrupts the prompt', async () => {
    const r = await rig()
    r.daemon.activeCode = '123456'
    const hooks = r.hooks()
    // 用户一直不输入：askCode 永远不结算；hub 在这时结束了会话
    hooks.askCode = () => {
      queueMicrotask(() => r.hub.failEnroll('peer_gone'))
      return new Promise<string | null>(() => {})
    }
    const err = await errorOf(r.t.client.enroll(req({ method: 'code', approval: false }), hooks))
    expect(err.code).toBe('peer_gone')
    r.t.client.stop()
  })

  it('ignores the late "not_found" receipt for a session it cancelled earlier when the next enrollment starts', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    const hooks = r.hooks()
    hooks.qr = () => queueMicrotask(() => r.t.client.cancelEnroll())
    const first = await errorOf(r.t.client.enroll(req({ method: 'scan' }), hooks))
    expect(first.code).toBe('cancelled')
    const staleId = r.hub.enrollCancels[0]!
    const second = r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks())
    // hub 对已结束会话的 enroll_cancel 回执（有些 hub 会回）这时才到
    r.hub.socket.control({ type: 'enroll_failed', enroll_id: staleId, reason: 'not_found' })
    await expect(second).resolves.toMatchObject({ nodeId: DAEMON_ID })
    r.t.client.stop()
  })

  it('only one pairing / enrollment at a time', async () => {
    const r = await rig()
    r.daemon.activeCode = '123456'
    const hooks = r.hooks()
    hooks.askCode = () => new Promise<string | null>(() => {})
    const first = r.t.client.enroll(req({ method: 'code', approval: false }), hooks)
    await vi.waitFor(() => expect(r.hub.enrollId).not.toBeNull())
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: 'x'.repeat(8) }), r.hooks()))
    expect(err.code).toBe('cancelled')
    r.t.client.cancelEnroll()
    expect((await errorOf(first)).code).toBe('cancelled')
    r.t.client.stop()
  })
})

describe('enroll — finishing is the same as pairing', () => {
  it('refuses to overwrite a node whose key changed (key_changed) and stores nothing new', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    const old = new Uint8Array(32).fill(9)
    r.t.storage.nodes.set(DAEMON_ID, { node_id: DAEMON_ID, name: 'old', public_key: old, paired_at: 1, last_seen: 1 })
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks()))
    expect(err.code).toBe('key_changed')
    expect(Array.from(r.t.storage.nodes.get(DAEMON_ID)!.public_key)).toEqual(Array.from(old))
    r.t.client.stop()
  })

  it('without a pair.ack nothing is saved: times out after 30 s', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const r = await rig()
    r.daemon.password = 'correct horse'
    r.hub.ackMode = 'none'
    const p = r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks()).then(
      () => null,
      (e: unknown) => e as EnrollError,
    )
    await vi.advanceTimersByTimeAsync(40_000)
    const err = await p
    expect(err).toBeInstanceOf(EnrollError)
    expect(err!.code).toBe('timeout')
    expect(r.t.storage.nodes.size).toBe(0)
    r.t.client.stop()
  })

  it('a pair.ack with a wrong fingerprint is "ack_invalid" and nothing is saved', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    r.hub.ackMode = 'wrong_fingerprint'
    const err = await errorOf(r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks()))
    expect(err.code).toBe('ack_invalid')
    expect(r.t.storage.nodes.size).toBe(0)
    r.t.client.stop()
  })

  it('never confuses the temporary id with the formal one: the handshake frames come from the temporary id', async () => {
    const r = await rig()
    r.daemon.password = 'correct horse'
    await r.t.client.enroll(req({ method: 'password', password: 'correct horse' }), r.hooks())
    const hs = FakeSocket.instances.flatMap((s) => s.sent).filter((e) => e.kind === 'handshake' && e.to === DAEMON_ID)
    expect(hs.length).toBeGreaterThanOrEqual(4) // hello, cpace, noise 1, noise 3（+ 之后的 IK 握手）
    expect(hs[0]!.from).toBe(TEMP_ID)
    r.t.client.stop()
  })
})
