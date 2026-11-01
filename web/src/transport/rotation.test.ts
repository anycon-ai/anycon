// device token 轮换（spec/web.md § 6.4）：到期判断、`rotate_token` 的发送节奏、落盘、多标签页。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { CLIENT_ID, FakeHub, FakeSocket, setupClient as setup } from '@/test/harness'
import { ROTATE_QUICK_SENDS, ROTATE_RECHECK_MS, ROTATE_RETRY_MS, TOKEN_MAX_AGE_MS } from '@/lib/constants'
import { decodeControl } from './wire'

const NOW = 1_800_000_000_000
const DAY = 24 * 3600_000

beforeEach(() => {
  FakeSocket.instances = []
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] })
  vi.setSystemTime(NOW)
})
afterEach(() => {
  vi.useRealTimers()
})

/** 起客户端并等到 hub 认证完成（和它随后立刻做的轮换检查）。 */
async function boot(t: ReturnType<typeof setup>): Promise<void> {
  await t.client.start('acct1')
  await vi.advanceTimersByTimeAsync(0)
}

/** 客户端发出的控制消息类型，按顺序（所有连接）。 */
const sentTypes = () =>
  FakeSocket.instances
    .flatMap((s) => s.sent)
    .filter((e) => e.kind === 'control')
    .map((e) => decodeControl(e.body).type)

describe('rotate_token trigger', () => {
  it('is sent right after auth_ok once the token is 30 days old', async () => {
    const t = setup({ tokenIssuedAtMs: NOW - TOKEN_MAX_AGE_MS })
    await boot(t)
    expect(t.hub.rotateRequests).toBe(1)
    expect(sentTypes()).toEqual(['auth', 'rotate_token'])
    t.client.stop()
  })

  it('is not sent for a token that is younger than 30 days', async () => {
    const t = setup({ tokenIssuedAtMs: NOW - TOKEN_MAX_AGE_MS + 1000 })
    await boot(t)
    expect(t.hub.rotateRequests).toBe(0)
    expect(sentTypes()).toEqual(['auth'])
    t.client.stop()
  })

  it('is sent when the issue time is unknown (a record from before rotation existed)', async () => {
    const t = setup({ tokenIssuedAtMs: null })
    await boot(t)
    expect(t.hub.rotateRequests).toBe(1)
    t.client.stop()
  })

  it('is sent when the issue time is in the future (the clock went backwards)', async () => {
    const t = setup({ tokenIssuedAtMs: NOW + 5 * DAY })
    await boot(t)
    expect(t.hub.rotateRequests).toBe(1)
    t.client.stop()
  })

  it('is never sent on a temporary connection (no device token), not even after a long time', async () => {
    const t = setup({ paired: false, knowsNode: false })
    await boot(t)
    await vi.advanceTimersByTimeAsync(3 * ROTATE_RECHECK_MS)
    expect(t.hub.rotateRequests).toBe(0)
    expect(sentTypes()).toEqual(['auth'])
    t.client.stop()
  })
})

describe('token_rotated', () => {
  it('persists the new token and its issue time in one write and uses it for the next connection', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.rotateReplies = ['tok2']
    const t = setup({ hub, tokenIssuedAtMs: NOW - 31 * DAY })
    await boot(t)
    expect(t.storage.saved).toHaveLength(1)
    expect(t.storage.saved[0]).toMatchObject({ device_token: 'tok2', device_token_issued_at_ms: NOW, device_id: CLIENT_ID })
    expect(t.storage.identity).toBe(t.storage.saved[0])
    // 轮换成功之后不再请求，哪怕过了重试间隔
    await vi.advanceTimersByTimeAsync(2 * ROTATE_RETRY_MS)
    expect(hub.rotateRequests).toBe(1)
    // 重连带的是新 token
    hub.socket.serverClose(1006)
    await vi.advanceTimersByTimeAsync(1000)
    expect(hub.authFrames.map((a) => a.device_token)).toEqual(['tok', 'tok2'])
    expect(hub.rotateRequests).toBe(1)
    t.client.stop()
  })

  it('a device revoked after this connection rotated its token forgets the rotated token (close 4004)', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.rotateReplies = ['tok2']
    const t = setup({ hub, tokenIssuedAtMs: null })
    await boot(t)
    expect(t.storage.identity?.device_token).toBe('tok2')
    hub.socket.serverClose(4004)
    await vi.advanceTimersByTimeAsync(0)
    expect(t.hubStates.at(-1)?.halt).toBe('revoked')
    expect(t.storage.identity?.device_token).toBeUndefined()
    expect(t.storage.identity?.device_token_issued_at_ms).toBeUndefined()
    expect(t.client.snapshot.paired).toBe(false)
    t.client.stop()
  })

  it('is ignored when this browser holds no device token', async () => {
    const t = setup({ paired: false, knowsNode: false })
    await boot(t)
    t.hub.socket.control({ type: 'token_rotated', device_token: 'sneaky' })
    await vi.advanceTimersByTimeAsync(0)
    expect(t.storage.saved).toEqual([])
    expect(t.storage.identity?.device_token).toBeUndefined()
    expect(t.client.snapshot.paired).toBe(false)
    t.client.stop()
  })

  it('keeps the new token in memory even when the write fails (the old one is only valid for a minute)', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.rotateReplies = ['tok2']
    const t = setup({ hub, tokenIssuedAtMs: null })
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    t.storage.saveIdentity = async () => {
      throw new Error('quota exceeded')
    }
    await boot(t)
    expect(warn).toHaveBeenCalled()
    hub.socket.serverClose(1006)
    await vi.advanceTimersByTimeAsync(1000)
    expect(hub.authFrames.at(-1)!.device_token).toBe('tok2')
    t.client.stop()
  })
})

describe('rotate_token retries and rechecks', () => {
  it('re-sends every 75 s while the hub stays silent, then once an hour for as long as the token is due', async () => {
    const t = setup({ tokenIssuedAtMs: null }) // hub 不回（像被频率限制忽略了）
    await boot(t)
    expect(t.hub.rotateRequests).toBe(1)
    await vi.advanceTimersByTimeAsync(ROTATE_RETRY_MS - 1)
    expect(t.hub.rotateRequests).toBe(1)
    await vi.advanceTimersByTimeAsync(1)
    expect(t.hub.rotateRequests).toBe(2)
    await vi.advanceTimersByTimeAsync(ROTATE_RETRY_MS)
    expect(t.hub.rotateRequests).toBe(ROTATE_QUICK_SENDS)
    // 头几次发完了：不再每 75 秒发，改成每小时一次
    await vi.advanceTimersByTimeAsync(ROTATE_RECHECK_MS - 1)
    expect(t.hub.rotateRequests).toBe(ROTATE_QUICK_SENDS)
    await vi.advanceTimersByTimeAsync(1)
    expect(t.hub.rotateRequests).toBe(ROTATE_QUICK_SENDS + 1)
    await vi.advanceTimersByTimeAsync(2 * ROTATE_RECHECK_MS)
    expect(t.hub.rotateRequests).toBe(ROTATE_QUICK_SENDS + 3)
    t.client.stop()
  })

  it('a reply to the retry stops the retries (token_rotated lost the first time)', async () => {
    const t = setup({ tokenIssuedAtMs: null })
    await boot(t)
    expect(t.hub.rotateRequests).toBe(1)
    t.hub.rotateReplies = ['tok2']
    await vi.advanceTimersByTimeAsync(ROTATE_RETRY_MS)
    expect(t.hub.rotateRequests).toBe(2)
    expect(t.storage.identity?.device_token).toBe('tok2')
    await vi.advanceTimersByTimeAsync(5 * ROTATE_RECHECK_MS)
    expect(t.hub.rotateRequests).toBe(2)
    t.client.stop()
  })

  it('a new connection starts counting again', async () => {
    const t = setup({ tokenIssuedAtMs: null })
    await boot(t)
    await vi.advanceTimersByTimeAsync(2 * ROTATE_RETRY_MS)
    expect(t.hub.rotateRequests).toBe(ROTATE_QUICK_SENDS)
    t.hub.socket.serverClose(1006)
    await vi.advanceTimersByTimeAsync(1000)
    expect(t.hub.rotateRequests).toBe(ROTATE_QUICK_SENDS + 1)
    // 新连接重新数：又是 75 秒一次
    await vi.advanceTimersByTimeAsync(ROTATE_RETRY_MS)
    expect(t.hub.rotateRequests).toBe(ROTATE_QUICK_SENDS + 2)
    t.client.stop()
  })

  it('a tab that stays connected re-checks hourly and rotates once the token turns 30 days old', async () => {
    const t = setup({ tokenIssuedAtMs: NOW - TOKEN_MAX_AGE_MS + 90 * 60_000 }) // 90 分钟后到期
    await boot(t)
    expect(t.hub.rotateRequests).toBe(0)
    await vi.advanceTimersByTimeAsync(ROTATE_RECHECK_MS)
    expect(t.hub.rotateRequests).toBe(0) // 1 小时：还差半小时
    await vi.advanceTimersByTimeAsync(ROTATE_RECHECK_MS)
    expect(t.hub.rotateRequests).toBe(1) // 2 小时：到期了
    t.client.stop()
  })

  it('stops all of it when the connection drops, and when the client stops', async () => {
    const t = setup({ tokenIssuedAtMs: null })
    await boot(t)
    t.hub.socket.serverClose(4001) // 被别的标签页顶掉：不重连
    await vi.advanceTimersByTimeAsync(3 * ROTATE_RECHECK_MS)
    expect(t.hub.rotateRequests).toBe(1)

    const u = setup({ tokenIssuedAtMs: null })
    await boot(u)
    expect(u.hub.rotateRequests).toBe(1)
    u.client.stop()
    await vi.advanceTimersByTimeAsync(3 * ROTATE_RECHECK_MS)
    expect(u.hub.rotateRequests).toBe(1)
  })
})

describe('several tabs sharing one stored identity', () => {
  /** 另一个标签页轮换了 token 并落盘：库里是新的，这个标签页内存里还是旧的。 */
  function otherTabRotated(t: ReturnType<typeof setup>, token: string, issuedAt: number): void {
    t.storage.identity = { ...t.storage.identity!, device_token: token, device_token_issued_at_ms: issuedAt }
  }

  it('a stale tab whose token expired adopts the newer stored token instead of forgetting it', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.validTokens = new Set(['tok', 'tok2'])
    const t = setup({ hub })
    await boot(t)
    expect(t.hubStates.at(-1)?.phase).toBe('hub_authed')

    otherTabRotated(t, 'tok2', NOW + 1000)
    hub.validTokens = new Set(['tok2']) // 60 秒宽限过了：旧 token 失效
    hub.socket.serverClose(1006)
    await vi.advanceTimersByTimeAsync(1000) // 重连用旧 token → invalid → 换上库里的 → 马上重连
    await vi.advanceTimersByTimeAsync(0)

    expect(hub.authFrames.map((a) => a.device_token)).toEqual(['tok', 'tok', 'tok2'])
    expect(t.hubStates.at(-1)).toMatchObject({ phase: 'hub_authed', halt: null })
    expect(t.storage.identity).toMatchObject({ device_token: 'tok2', device_token_issued_at_ms: NOW + 1000 })
    expect(t.client.snapshot.paired).toBe(true)
    t.client.stop()
  })

  it('a token that is invalid and has no newer copy in storage is forgotten, and the UI halts on it', async () => {
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.validTokens = new Set()
    const t = setup({ hub })
    await boot(t)
    expect(t.hubStates.at(-1)).toMatchObject({ phase: 'hub_disconnected', halt: 'invalid_token' })
    expect(t.storage.identity?.device_token).toBeUndefined()
    expect(t.storage.identity?.device_id).toBeUndefined()
    expect(t.client.snapshot.paired).toBe(false)
    await vi.advanceTimersByTimeAsync(10 * 60_000)
    expect(hub.authFrames).toHaveLength(1) // 不再重连
    t.client.stop()
  })

  it('a revoked device is still forgotten right away', async () => {
    const t = setup()
    t.hub.authFail = 'revoked'
    await boot(t)
    expect(t.hubStates.at(-1)?.halt).toBe('revoked')
    expect(t.storage.identity?.device_token).toBeUndefined()
    expect(t.storage.identity?.device_token_issued_at_ms).toBeUndefined()
    t.client.stop()
  })

  it('forgetting only clears the token that was rejected, not a different one another tab stored', async () => {
    const t = setup()
    t.hub.autoAuth = false
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    otherTabRotated(t, 'tok2', NOW + 1000)
    t.hub.socket.control({ type: 'auth_fail', reason: 'revoked' })
    await vi.advanceTimersByTimeAsync(0)
    expect(t.hubStates.at(-1)?.halt).toBe('revoked')
    expect(t.client.snapshot.paired).toBe(false) // 内存里丢掉了
    expect(t.storage.identity?.device_token).toBe('tok2') // 库里别人存的那份没动
    t.client.stop()
  })

  it('before asking for a rotation a stale tab looks at the store: a token another tab just rotated is adopted, not rotated again', async () => {
    const t = setup({ tokenIssuedAtMs: NOW - 40 * DAY })
    t.hub.autoAuth = false
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    expect(t.hub.authFrames).toHaveLength(1)
    otherTabRotated(t, 'tok2', NOW - 1000) // 在这个标签页等 auth_ok 的时候，另一个标签页轮换完了
    t.hub.socket.control({ type: 'auth_ok', device_id: CLIENT_ID, server_time_ms: NOW, peers_online: [] })
    await vi.advanceTimersByTimeAsync(0)
    expect(t.hub.rotateRequests).toBe(0)
    // 下次重连用的是库里的新 token
    t.hub.socket.serverClose(1006)
    await vi.advanceTimersByTimeAsync(1000)
    expect(t.hub.authFrames.at(-1)!.device_token).toBe('tok2')
    t.client.stop()
  })

  it('does not adopt a stored token that belongs to another identity (different Noise key)', async () => {
    const t = setup({ tokenIssuedAtMs: NOW - 40 * DAY })
    t.hub.autoAuth = false
    await t.client.start('acct1')
    await vi.advanceTimersByTimeAsync(0)
    otherTabRotated(t, 'tok-of-someone-else', NOW)
    t.storage.identity = { ...t.storage.identity!, static_keypair: { public: new Uint8Array(32).fill(9), secret: new Uint8Array(32).fill(9) } }
    t.hub.socket.control({ type: 'auth_ok', device_id: CLIENT_ID, server_time_ms: NOW, peers_online: [] })
    await vi.advanceTimersByTimeAsync(0)
    expect(t.hub.rotateRequests).toBe(1) // 照常请求轮换自己的
    t.client.stop()
  })
})

describe('issue time at pairing', () => {
  it('records the issue time of the token pair_done hands out', async () => {
    vi.useRealTimers() // 配对里有真实的 Argon2 派生和握手，不用假定时器
    const hub = new FakeHub({ rpc: { 'node.info': () => ({ name: 'n' }) } })
    hub.pairCodeOk = 'ABCD2345'
    const t = setup({ paired: false, knowsNode: false, hub })
    await t.client.start('acct1')
    const before = Date.now()
    await t.client.pair('abcd-2345', 'My Browser')
    const at = t.storage.identity?.device_token_issued_at_ms
    expect(t.storage.identity?.device_token).toBe('tok-new')
    expect(at).toBeGreaterThanOrEqual(before)
    expect(at).toBeLessThanOrEqual(Date.now())
    // 刚签发的 token 不会马上被请求轮换
    expect(hub.rotateRequests).toBe(0)
    t.client.stop()
  })
})
