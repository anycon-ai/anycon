// 设备 token 轮换（docs/spec/sdk.md § 2）：策略函数、落盘的原子性、自动轮换的触发 / 重试 / 停止、环境变量凭据不自动轮换、
// 显式 `rotateToken()`、写存储失败、迟到的回复。计时都调到几十毫秒，不会真的等 75 秒或 1 小时。
import assert from 'node:assert/strict'
import { mkdtemp, readdir, rm, stat } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { describe, it } from 'node:test'

import type { Crypto } from '@anycon/protocol'

import {
  Client,
  FileStorage,
  MemoryStorage,
  pair,
  type ClientOptions,
  type StoredIdentity,
  type TokenRotation,
} from '../src/index.js'
import { HubConnection } from '../src/hub.js'
import {
  ROTATE_QUICK_SENDS,
  ROTATE_RECHECK_MS,
  ROTATE_RETRY_MS,
  TOKEN_MAX_AGE_MS,
  isTokenDue,
  nextRotationDelay,
} from '../src/rotation.js'
import { StateStore } from '../src/storage.js'
import { FAST, cryptoImpls, sleep, startEnv, until, type Env } from './harness.js'
import { mockCrypto } from './mock-crypto.js'

const DAY = 24 * 3600 * 1000

async function readIdentity(storage: MemoryStorage | FileStorage): Promise<StoredIdentity> {
  return JSON.parse((await storage.get('identity'))!) as StoredIdentity
}

/** 把已配对的身份改成指定的签发时间（`undefined` = 删掉字段，模拟升级前的老记录）。 */
async function setIssuedAt(
  storage: MemoryStorage | FileStorage,
  issuedAtMs: number | undefined,
): Promise<StoredIdentity> {
  const identity = await readIdentity(storage)
  if (issuedAtMs === undefined) delete identity.tokenIssuedAtMs
  else identity.tokenIssuedAtMs = issuedAtMs
  await storage.set('identity', JSON.stringify(identity))
  return identity
}

/** 配对并把 token 的签发时间改成「31 天前」，返回身份。 */
async function pairWithOldToken(env: Env): Promise<StoredIdentity> {
  await env.pair()
  return setIssuedAt(env.storage, Date.now() - 31 * DAY)
}

function openClient(env: Env, crypto: Crypto, options: ClientOptions = {}): Promise<Client> {
  return Client.open(env.url, { storage: env.storage, crypto, ...FAST, ...options })
}

/** 记录 `set` 调用的存储：断言身份记录是「一次写完」。 */
class RecordingStorage extends MemoryStorage {
  readonly sets: Array<{ key: string; value: string }> = []
  failIdentityWrites = false
  override set(key: string, value: string): Promise<void> {
    if (this.failIdentityWrites && key === 'identity') {
      return Promise.reject(new Error('disk full'))
    }
    this.sets.push({ key, value })
    return super.set(key, value)
  }
}

/** 临时设置环境变量凭据（CI 注入的那种），跑完还原。 */
async function withEnvCredentials<T>(
  identity: StoredIdentity,
  node: { id: string; publicKey: string },
  fn: () => Promise<T>,
): Promise<T> {
  const keys = ['ANYCON_DEVICE_KEY', 'ANYCON_DEVICE_TOKEN', 'ANYCON_NODE_ID', 'ANYCON_NODE_KEY']
  const saved = { ...process.env }
  Object.assign(process.env, {
    ANYCON_DEVICE_KEY: identity.secretKey,
    ANYCON_DEVICE_TOKEN: identity.deviceToken,
    ANYCON_NODE_ID: node.id,
    ANYCON_NODE_KEY: node.publicKey,
  })
  try {
    return await fn()
  } finally {
    for (const key of keys) {
      if (key in saved) process.env[key] = saved[key]
      else delete process.env[key]
    }
  }
}

describe('轮换策略', () => {
  it('isTokenDue：签发时间未知 / 在未来 / 满 30 天才算到期', () => {
    const now = 1_800_000_000_000
    assert.equal(TOKEN_MAX_AGE_MS, 30 * DAY)
    assert.equal(isTokenDue(undefined, now), true, '没有记录当作很旧')
    assert.equal(isTokenDue('yesterday', now), true, '不是数字')
    assert.equal(isTokenDue(Number.NaN, now), true)
    assert.equal(isTokenDue(now + 1, now), true, '签发时间在未来：时钟被拨回过，当作未知')
    assert.equal(isTokenDue(now, now), false)
    assert.equal(isTokenDue(now - 30 * DAY + 1, now), false, '差 1 毫秒满 30 天')
    assert.equal(isTokenDue(now - 30 * DAY, now), true, '正好 30 天')
    assert.equal(isTokenDue(now - 100 * DAY, now), true)
    assert.equal(isTokenDue(now - 5_000, now, 4_000), true, '可以改阈值')
  })

  it('nextRotationDelay：头 3 次发送之间隔 75 秒，之后每小时复查', () => {
    assert.equal(ROTATE_RETRY_MS, 75_000)
    assert.equal(ROTATE_RECHECK_MS, 3600_000)
    assert.equal(ROTATE_QUICK_SENDS, 3)
    assert.deepEqual(
      [1, 2].map((n) => nextRotationDelay(n)),
      [75_000, 75_000],
    )
    assert.deepEqual(
      [3, 4, 50].map((n) => nextRotationDelay(n)),
      [3600_000, 3600_000, 3600_000],
    )
    assert.equal(nextRotationDelay(1, 10, 20), 10)
    assert.equal(nextRotationDelay(3, 10, 20), 20)
  })
})

describe('身份落盘', () => {
  it('rotateIdentityToken：token 和签发时间在一次 set 里写完，其余字段不动', async () => {
    const storage = new RecordingStorage()
    const store = new StateStore(storage)
    const identity: StoredIdentity = {
      deviceId: 'dev',
      deviceToken: 'old',
      secretKey: '11'.repeat(32),
      name: 'ci',
      createdAtMs: 5,
    }
    await store.setIdentity(identity)
    storage.sets.length = 0
    assert.equal(await store.rotateIdentityToken('new', 1234), true)
    assert.equal(storage.sets.length, 1, 'one write for the whole record')
    assert.equal(storage.sets[0]!.key, 'identity')
    assert.deepEqual(JSON.parse(storage.sets[0]!.value), {
      ...identity,
      deviceToken: 'new',
      tokenIssuedAtMs: 1234,
    })
  })

  it('rotateIdentityToken：本地没有身份（被吊销清掉了）时什么都不写', async () => {
    const storage = new RecordingStorage()
    const store = new StateStore(storage)
    assert.equal(await store.rotateIdentityToken('new', 1), false)
    assert.equal(storage.sets.length, 0)
    assert.equal(await storage.get('identity'), undefined)
  })

  it('和 clearIdentity 串行：先清掉再轮换，不会把作废的身份写回去', async () => {
    const storage = new MemoryStorage()
    const store = new StateStore(storage)
    await store.setIdentity({
      deviceId: 'dev',
      deviceToken: 'old',
      secretKey: '11'.repeat(32),
      name: 'ci',
      createdAtMs: 5,
    })
    const [, rotated] = await Promise.all([
      store.clearIdentity(),
      store.rotateIdentityToken('new', 1),
    ])
    assert.equal(rotated, false)
    assert.equal(await storage.get('identity'), undefined)
  })
})

for (const impl of cryptoImpls) {
  describe(`设备 token 轮换（${impl.label}）`, () => {
    it('pair() 记下 token 的签发时间', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const before = Date.now()
        await env.pair()
        const identity = await readIdentity(env.storage)
        assert.ok(identity.tokenIssuedAtMs !== undefined)
        assert.ok(identity.tokenIssuedAtMs >= before && identity.tokenIssuedAtMs <= Date.now())
        // 用已有身份再配一次：hub 没有重新签发 token，签发时间原样保留
        // （不解析节点名：解析会再连一次 hub，旧 token 会顺手被轮换掉）
        await setIssuedAt(env.storage, 4242)
        const code = await env.daemon.beginPairing()
        await pair(env.url, {
          code,
          name: 'ci-runner',
          storage: env.storage,
          crypto,
          resolveNodeName: false,
        })
        assert.equal((await readIdentity(env.storage)).tokenIssuedAtMs, 4242)
      } finally {
        await env.stop()
      }
    })

    it('token 满 30 天：连上后自动轮换，新 token 和签发时间一次写进存储，重连用新 token，事件和回调各来一次', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const old = await pairWithOldToken(env)
        const storage = new RecordingStorage()
        await storage.set('identity', JSON.stringify(old))
        storage.sets.length = 0
        const seen: TokenRotation[] = []
        const viaEvent: TokenRotation[] = []
        const client = await Client.open(env.url, {
          storage,
          crypto,
          ...FAST,
          onTokenRotated: (r) => void seen.push(r),
        })
        client.on('tokenRotated', (r) => viaEvent.push(r))
        try {
          await until(() => viaEvent.length === 1, 3000, 'the token to be rotated')
          assert.deepEqual(env.hub.rotateRequests, [client.deviceId])
          const fresh = await readIdentity(storage)
          assert.equal(fresh.deviceToken, env.hub.currentToken(client.deviceId))
          assert.notEqual(fresh.deviceToken, old.deviceToken)
          assert.ok(Date.now() - fresh.tokenIssuedAtMs! < 5000, 'issued at is now')
          assert.deepEqual(
            storage.sets.filter((s) => s.key === 'identity').length,
            1,
            'the identity record is written exactly once',
          )
          assert.equal(fresh.secretKey, old.secretKey)
          assert.equal(fresh.deviceId, old.deviceId)
          assert.equal(viaEvent[0]!.persisted, true)
          assert.equal(viaEvent[0]!.deviceToken, fresh.deviceToken)
          assert.equal(viaEvent[0]!.issuedAtMs, fresh.tokenIssuedAtMs)
          assert.deepEqual(seen, viaEvent)

          // 旧 token 过了宽限：重连必须用新 token，否则认证失败、client 变成 fatal
          env.hub.expireOldTokens(client.deviceId)
          const authsBefore = env.hub.authTokens.length
          env.hub.dropConnection(client.deviceId)
          await until(() => env.hub.authTokens.length > authsBefore, 3000, 'the reconnect')
          assert.equal(env.hub.authTokens.at(-1), fresh.deviceToken)
          await until(() => client.connected, 3000, 'the client to be connected again')
          assert.equal(client.fatalError, undefined)
          // 新 token 还很新：重连之后不再发请求
          await sleep(100)
          assert.equal(env.hub.rotateRequests.length, 1)
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('token 还新（配对不久）：不发 rotate_token', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        await setIssuedAt(env.storage, Date.now() - 29 * DAY)
        const client = await openClient(env, crypto, { tokenRotation: { retryAfterMs: 20 } })
        try {
          await sleep(200)
          assert.deepEqual(env.hub.rotateRequests, [])
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('老记录没有签发时间：当作很旧，连上就轮换一次，之后记录里有了签发时间', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const old = await setIssuedAt(env.storage, undefined)
        assert.equal(old.tokenIssuedAtMs, undefined)
        const client = await openClient(env, crypto)
        try {
          await until(() => env.hub.rotations.length === 1, 3000, 'the rotation')
          await until(
            async () => (await readIdentity(env.storage)).tokenIssuedAtMs !== undefined,
            3000,
            'the issue time to be stored',
          )
          assert.equal(
            (await readIdentity(env.storage)).deviceToken,
            env.hub.currentToken(client.deviceId),
          )
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('tokenRotation.auto = false：到期也不自动轮换，显式 rotateToken() 仍可用', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await pairWithOldToken(env)
        const client = await openClient(env, crypto, { tokenRotation: { auto: false } })
        try {
          await sleep(150)
          assert.deepEqual(env.hub.rotateRequests, [])
          const r = await client.rotateToken()
          assert.equal(r.persisted, true)
          assert.equal((await readIdentity(env.storage)).deviceToken, r.deviceToken)
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('环境变量凭据：不自动轮换；rotateToken() 返回新 token、触发事件和回调、不写存储，重连用新 token', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const old = await pairWithOldToken(env)
        const nodes = JSON.parse((await env.storage.get('nodes'))!) as Array<{
          id: string
          publicKey: string
        }>
        await withEnvCredentials(old, nodes[0]!, async () => {
          const empty = new RecordingStorage()
          const events: TokenRotation[] = []
          const callbacks: TokenRotation[] = []
          const client = await Client.open(env.url, {
            storage: empty,
            crypto,
            ...FAST,
            tokenRotation: { retryAfterMs: 20 },
            onTokenRotated: (r) => void callbacks.push(r),
          })
          client.on('tokenRotated', (r) => events.push(r))
          try {
            await sleep(200)
            assert.deepEqual(env.hub.rotateRequests, [], 'never rotates by itself')

            const r = await client.rotateToken()
            assert.equal(r.persisted, false, 'the caller has to save it')
            assert.equal(r.persistError, undefined)
            assert.equal(r.deviceToken, env.hub.currentToken(client.deviceId))
            assert.notEqual(r.deviceToken, old.deviceToken)
            assert.deepEqual(events, [r])
            assert.deepEqual(callbacks, [r])
            assert.equal(empty.sets.length, 0, 'nothing is written to the storage')
            assert.equal(await empty.get('identity'), undefined)

            env.hub.expireOldTokens(client.deviceId)
            const authsBefore = env.hub.authTokens.length
            env.hub.dropConnection(client.deviceId)
            await until(() => env.hub.authTokens.length > authsBefore, 3000, 'the reconnect')
            assert.equal(env.hub.authTokens.at(-1), r.deviceToken)
            await until(() => client.connected, 3000, 'the client to be connected again')
            assert.equal(client.fatalError, undefined)
          } finally {
            await client.close()
          }
        })
      } finally {
        await env.stop()
      }
    })

    it('hub 不回复：rotateToken() 以 rotate_timeout 失败；超时之后才到的回复仍然被存下来', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const client = await openClient(env, crypto)
        try {
          env.hub.rotation = 'ignore'
          await assert.rejects(client.rotateToken({ timeoutMs: 80 }), { code: 'rotate_timeout' })

          // 回复比客户端等得久：hub 已经换了，新 token 不能丢
          env.hub.rotation = 'rotate'
          env.hub.rotateReplyDelayMs = 200
          const late: TokenRotation[] = []
          client.on('tokenRotated', (r) => late.push(r))
          await assert.rejects(client.rotateToken({ timeoutMs: 50 }), { code: 'rotate_timeout' })
          await until(() => late.length === 1, 3000, 'the late reply')
          assert.equal(late[0]!.persisted, true)
          assert.equal((await readIdentity(env.storage)).deviceToken, late[0]!.deviceToken)
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('rotateToken()：已经在等回复时不重复发；没连上抛 not_connected；关闭后抛 client_closed', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        env.hub.rotateReplyDelayMs = 100
        const client = await openClient(env, crypto, {
          reconnect: { initialMs: 5000, maxMs: 5000 },
        })
        try {
          const [a, b] = await Promise.all([client.rotateToken(), client.rotateToken()])
          assert.deepEqual(a, b)
          assert.equal(env.hub.rotateRequests.length, 1)

          env.hub.dropConnection(client.deviceId)
          await until(() => !client.connected, 3000, 'the disconnect')
          await assert.rejects(client.rotateToken(), { code: 'not_connected' })
        } finally {
          await client.close()
        }
        await assert.rejects(client.rotateToken(), { code: 'client_closed' })
      } finally {
        await env.stop()
      }
    })

    it('断线或关闭时，还在等回复的 rotateToken() 立刻失败', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        env.hub.rotation = 'ignore'
        const client = await openClient(env, crypto, { tokenRotation: { closeWaitMs: 0 } })
        try {
          const dropped = client.rotateToken({ timeoutMs: 10_000 })
          const caught = assert.rejects(dropped, { code: 'connection_failed' })
          await until(() => env.hub.rotateRequests.length === 1, 3000, 'the request')
          env.hub.dropConnection(client.deviceId)
          await caught
          await until(() => client.connected, 3000, 'the reconnect')

          const closing = client.rotateToken({ timeoutMs: 10_000 })
          const closed = assert.rejects(closing, { code: 'client_closed' })
          await until(() => env.hub.rotateRequests.length === 2, 3000, 'the second request')
          await client.close()
          await closed
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('hub 一直不回复：头 3 次发送各隔 retryAfterMs，之后按 recheckMs 复查；close() 之后不再发', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await pairWithOldToken(env)
        env.hub.rotation = 'ignore'
        const client = await openClient(env, crypto, {
          tokenRotation: { retryAfterMs: 30, recheckMs: 400, closeWaitMs: 0 },
        })
        try {
          await until(() => env.hub.rotateRequests.length >= 3, 3000, 'three quick sends')
          assert.equal(env.hub.rotateRequests.length, 3)
          await sleep(150) // 第 4 次要等 recheckMs（400 ms），这里不该出现
          assert.equal(
            env.hub.rotateRequests.length,
            3,
            'then it slows down to the recheck interval',
          )
          await until(() => env.hub.rotateRequests.length >= 4, 3000, 'the recheck')
          await client.close()
          const sent = env.hub.rotateRequests.length
          await sleep(500)
          assert.equal(env.hub.rotateRequests.length, sent, 'timers are cleared on close')
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('第一次请求成功后不再重试', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await pairWithOldToken(env)
        const client = await openClient(env, crypto, {
          tokenRotation: { retryAfterMs: 30, recheckMs: 60 },
        })
        try {
          await until(() => env.hub.rotations.length === 1, 3000, 'the rotation')
          await sleep(250)
          assert.equal(env.hub.rotateRequests.length, 1)
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('每条新连接重新计次：重连后还没换成功就再发', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await pairWithOldToken(env)
        env.hub.rotation = 'ignore'
        const client = await openClient(env, crypto, {
          tokenRotation: { retryAfterMs: 5000, recheckMs: 5000, closeWaitMs: 0 },
        })
        try {
          await until(() => env.hub.rotateRequests.length === 1, 3000, 'the first request')
          env.hub.dropConnection(client.deviceId)
          await until(
            () => env.hub.rotateRequests.length === 2,
            3000,
            'the request after reconnecting',
          )
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('刚发出轮换请求就 close()：等回复落盘再关，新 token 不会丢', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await pairWithOldToken(env)
        env.hub.rotateReplyDelayMs = 150
        const client = await openClient(env, crypto)
        const old = await readIdentity(env.storage)
        await until(() => env.hub.rotateRequests.length === 1, 3000, 'the request')
        await client.close() // 回复还有 150 ms 才到
        const stored = await readIdentity(env.storage)
        assert.notEqual(stored.deviceToken, old.deviceToken)
        assert.equal(stored.deviceToken, env.hub.currentToken(client.deviceId))
      } finally {
        await env.stop()
      }
    })

    it('刚发出轮换请求就 close()，hub 一直不回复：最多多等 closeWaitMs', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await pairWithOldToken(env)
        env.hub.rotation = 'ignore'
        const client = await openClient(env, crypto, { tokenRotation: { closeWaitMs: 300 } })
        await until(() => env.hub.rotateRequests.length === 1, 3000, 'the request')
        const started = Date.now()
        await client.close()
        const waited = Date.now() - started
        assert.ok(waited < 2500, `close() gave up waiting (${waited} ms)`)
      } finally {
        await env.stop()
      }
    })

    it('写存储失败：事件里 persisted = false 并带错误，client 不会 fatal，重连仍用新 token', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const old = await pairWithOldToken(env)
        const storage = new RecordingStorage()
        await storage.set('identity', JSON.stringify(old))
        storage.failIdentityWrites = true
        const events: TokenRotation[] = []
        const client = await Client.open(env.url, {
          storage,
          crypto,
          ...FAST,
          onTokenRotated: (r) => void events.push(r),
        })
        try {
          await until(() => events.length === 1, 3000, 'the rotation')
          assert.equal(events[0]!.persisted, false)
          assert.match(events[0]!.persistError?.message ?? '', /disk full/)
          assert.equal(
            (await readIdentity(storage)).deviceToken,
            old.deviceToken,
            'storage untouched',
          )
          assert.equal(client.fatalError, undefined)
          assert.equal(client.connected, true)

          env.hub.expireOldTokens(client.deviceId)
          const authsBefore = env.hub.authTokens.length
          env.hub.dropConnection(client.deviceId)
          await until(() => env.hub.authTokens.length > authsBefore, 3000, 'the reconnect')
          assert.equal(env.hub.authTokens.at(-1), events[0]!.deviceToken)
          await until(() => client.connected, 3000, 'the client to be connected again')
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('回调抛错或返回被拒的 promise 不影响连接和事件', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        let calls = 0
        const client = await openClient(env, crypto, {
          onTokenRotated: () => {
            calls++
            if (calls === 1) throw new Error('boom')
            return Promise.reject(new Error('boom again'))
          },
        })
        try {
          const first = await client.rotateToken()
          const second = await client.rotateToken()
          assert.notEqual(first.deviceToken, second.deviceToken)
          assert.equal(calls, 2)
          assert.equal(client.fatalError, undefined)
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('FileStorage：轮换后身份文件仍是 0600，没有残留的临时文件', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      const dir = await mkdtemp(join(tmpdir(), 'anycon-sdk-rotate-'))
      try {
        const storage = new FileStorage(dir)
        const code = await env.daemon.beginPairing()
        await pair(env.url, { code, name: 'persisted', storage, crypto })
        await setIssuedAt(storage, Date.now() - 31 * DAY)
        const old = await readIdentity(storage)
        const client = await Client.open(env.url, { storage, crypto, ...FAST })
        try {
          await until(
            async () => (await readIdentity(storage)).deviceToken !== old.deviceToken,
            3000,
            'the token on disk to change',
          )
          const file = join(dir, 'identity.json')
          if (process.platform !== 'win32') {
            assert.equal((await stat(file)).mode & 0o777, 0o600)
          }
          assert.deepEqual(
            (await readdir(dir)).filter((f) => f.endsWith('.tmp')),
            [],
          )
          assert.ok((await readIdentity(storage)).tokenIssuedAtMs! > old.tokenIssuedAtMs!)
        } finally {
          await client.close()
        }
      } finally {
        await rm(dir, { recursive: true, force: true })
        await env.stop()
      }
    })
  })
}

describe('hub 连接层', () => {
  it('没有设备 token 的临时连接收到 token_rotated：丢掉，既不触发 tokenRotated 也不进 control', async () => {
    const env = await startEnv(mockCrypto)
    try {
      const hub = new HubConnection({
        url: env.url.replace('http', 'ws') + '/v1/ws',
        client: { name: 't', kind: 'sdk', platform: 'test', version: '0' },
      })
      const got: string[] = []
      hub.on('tokenRotated', () => got.push('tokenRotated'))
      hub.on('control', (c) => got.push(c.type))
      await hub.start()
      try {
        env.hub.sendControl(hub.deviceId, { type: 'token_rotated', device_token: 'tok_x' })
        await sleep(100)
        assert.deepEqual(got, [])
      } finally {
        hub.close()
      }
    } finally {
      await env.stop()
    }
  })

  it('带设备 token 的连接：token_rotated 只触发 tokenRotated，不进 control', async () => {
    const env = await startEnv(mockCrypto)
    try {
      const device = env.hub.registerDevice('sdk')
      const hub = new HubConnection({
        url: env.url.replace('http', 'ws') + '/v1/ws',
        token: device.token,
        deviceId: device.id,
        client: { name: 't', kind: 'sdk', platform: 'test', version: '0' },
        reconnect: { initialMs: 10, maxMs: 20 },
      })
      const got: string[] = []
      hub.on('tokenRotated', (t) => got.push(`rotated:${t}`))
      hub.on('control', (c) => got.push(c.type))
      await hub.start()
      try {
        env.hub.sendControl(device.id, { type: 'token_rotated', device_token: 'tok_other' })
        await until(() => got.length > 0, 2000, 'the event')
        assert.deepEqual(got, ['rotated:tok_other'])
      } finally {
        hub.close()
      }
    } finally {
      await env.stop()
    }
  })
})
