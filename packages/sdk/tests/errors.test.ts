// 错误类：RPC 稳定码映射、配对失败、版本不兼容。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { AnyconRpcError } from '@anycon/protocol'

import {
  AgentUnavailable,
  AnyconError,
  NodeBusy,
  PairingFailed,
  PairingRequired,
  ProtocolMismatch,
  RpcError,
  SettingsInvalid,
  connect,
  pair,
} from '../src/index.js'
import { mapRpcError } from '../src/errors.js'
import { FAST, cryptoImpls, startEnv } from './harness.js'

describe('mapRpcError', () => {
  it('按稳定码映射到子类，字段保留', () => {
    const settings = mapRpcError(
      new AnyconRpcError(-32001, 'invalid settings', {
        code: 'invalid_settings',
        field: 'thinking',
        reason: 'nope',
      }),
    )
    assert.ok(settings instanceof SettingsInvalid)
    assert.equal(settings.field, 'thinking')
    assert.equal(settings.reason, 'nope')
    assert.equal(settings.code, 'invalid_settings')
    assert.ok(settings instanceof RpcError && settings instanceof AnyconRpcError)

    const agent = mapRpcError(
      new AnyconRpcError(-32002, 'agent unavailable', {
        code: 'agent_unavailable',
        reason: 'not_authenticated',
      }),
    )
    assert.ok(agent instanceof AgentUnavailable)
    assert.equal(agent.reason, 'not_authenticated')

    assert.ok(mapRpcError(AnyconRpcError.of('node_busy', 'x')) instanceof NodeBusy)
    const mismatch = mapRpcError(AnyconRpcError.of('protocol_mismatch', 'x'), 1)
    assert.ok(mismatch instanceof ProtocolMismatch)
    assert.equal(mismatch.ours, 1)

    const other = mapRpcError(AnyconRpcError.of('not_found', 'no such thing'))
    assert.ok(other instanceof RpcError && !(other instanceof SettingsInvalid))
    assert.equal(other.code, 'not_found')
  })

  it('非 RPC 错误和已经映射过的错误原样返回', () => {
    const plain = new Error('x')
    assert.equal(mapRpcError(plain), plain)
    const mapped = new SettingsInvalid(-32001, 'x', { code: 'invalid_settings' })
    assert.equal(mapRpcError(mapped), mapped)
  })

  it('SDK 本地错误带稳定 code', () => {
    assert.equal(new PairingRequired().code, 'pairing_required')
    assert.equal(new PairingFailed('not_found').code, 'pairing_not_found')
    assert.ok(new PairingFailed('timeout') instanceof AnyconError)
  })
})

for (const impl of cryptoImpls) {
  describe(`协议错误端到端（${impl.label}）`, () => {
    it('daemon 的 RPC 错误以具体子类抛出', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await assert.rejects(
            node.sessions.create({ agent: 'mock', settings: { thinking: 'bogus' } }),
            (err: unknown) => {
              assert.ok(err instanceof SettingsInvalid)
              assert.equal(err.field, 'thinking')
              assert.equal(err.reason, 'not supported by this model')
              return true
            },
          )
          await assert.rejects(node.sessions.create({ agent: 'missing' }), (err: unknown) => {
            assert.ok(err instanceof AgentUnavailable)
            assert.equal(err.reason, 'not_installed')
            return true
          })
          await assert.rejects(
            node.sessions.create({ agent: 'mock', settings: { mode: 'busy' } }),
            NodeBusy,
          )
          await assert.rejects(node.projects.rename('nope', 'x'), (err: unknown) => {
            assert.ok(err instanceof RpcError)
            assert.equal(err.code, 'not_found')
            return true
          })
          // 会话进行中改设置 → busy
          const session = await node.sessions.create({ agent: 'mock' })
          const stream = session.send('slow slow')
          await stream.next()
          await assert.rejects(session.setSettings({ model: 'mock-small' }), (err: unknown) => {
            assert.ok(err instanceof RpcError)
            assert.equal(err.code, 'busy')
            return true
          })
          await session.cancel()
          for await (const _ of stream) void _
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('节点协议版本差太多：connect 抛 ProtocolMismatch', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto, { daemon: { protocolVersion: 7 } })
      try {
        await env.pair()
        await assert.rejects(
          connect(env.url, { storage: env.storage, crypto, ...FAST }),
          (err: unknown) => {
            assert.ok(err instanceof ProtocolMismatch)
            assert.equal(err.ours, 1)
            assert.equal(err.theirs, 7)
            return true
          },
        )
      } finally {
        await env.stop()
      }
    })

    it('没有本地身份：connect 抛 PairingRequired', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await assert.rejects(
          connect(env.url, { storage: env.storage, crypto, ...FAST }),
          PairingRequired,
        )
      } finally {
        await env.stop()
      }
    })

    it('配对失败：短码不存在 / 格式不对 / 已被用掉 / 配对中被取消 / 超时', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await assert.rejects(
          pair(env.url, { code: 'ZZZZ-ZZZZ', name: 'x', storage: env.storage, crypto }),
          (err: unknown) => {
            assert.ok(err instanceof PairingFailed)
            assert.equal(err.reason, 'not_found')
            return true
          },
        )
        await assert.rejects(
          pair(env.url, { code: '0O1I-L!', name: 'x', storage: env.storage, crypto }),
          PairingFailed,
        )
        assert.equal(await env.storage.get('identity'), undefined) // 失败不留任何身份

        const code = await env.daemon.beginPairing()
        await pair(env.url, { code, name: 'first', storage: env.storage, crypto })
        // 短码一次性
        await assert.rejects(
          pair(env.url, {
            code,
            name: 'second',
            crypto,
            storage: new (await import('../src/index.js')).MemoryStorage(),
          }),
          (err: unknown) => {
            assert.ok(err instanceof PairingFailed)
            assert.equal(err.reason, 'not_found')
            return true
          },
        )

        // 取消
        const ctl = new AbortController()
        const code2 = await env.daemon.beginPairing()
        env.daemon.setPairCode('AAAA-BBBB') // daemon 用错的 PSK：握手第 3 条校验失败，daemon 不确认，pair 一直等
        const waiting = pair(env.url, {
          code: code2,
          name: 'third',
          crypto,
          storage: new (await import('../src/index.js')).MemoryStorage(),
          signal: ctl.signal,
          timeoutMs: 5000,
        })
        setTimeout(() => ctl.abort(), 150)
        await assert.rejects(waiting, (err: unknown) => {
          assert.ok(err instanceof PairingFailed)
          assert.equal(err.reason, 'cancelled')
          return true
        })
        // 超时
        const code3 = await env.daemon.beginPairing()
        env.daemon.setPairCode('AAAA-BBBB')
        await assert.rejects(
          pair(env.url, {
            code: code3,
            name: 'fourth',
            crypto,
            storage: new (await import('../src/index.js')).MemoryStorage(),
            timeoutMs: 200,
          }),
          (err: unknown) => {
            assert.ok(err instanceof PairingFailed)
            assert.equal(err.reason, 'timeout')
            return true
          },
        )
      } finally {
        await env.stop()
      }
    })

    it('hub 不可达：connect / pair 抛 ConnectionError，不会无限重试', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      const url = env.url
      await env.stop() // 端口立刻没人听了
      const { ConnectionError } = await import('../src/index.js')
      await assert.rejects(
        pair(url, {
          code: '7K3P-Q2MX',
          name: 'x',
          crypto,
          storage: new (await import('../src/index.js')).MemoryStorage(),
        }),
        ConnectionError,
      )
      const storage = new (await import('../src/index.js')).MemoryStorage()
      await storage.set(
        'identity',
        JSON.stringify({
          deviceId: 'a',
          deviceToken: 't',
          secretKey: '00'.repeat(32),
          name: 'n',
          createdAtMs: 1,
        }),
      )
      await assert.rejects(connect(url, { storage, crypto, ...FAST }), ConnectionError)
    })
  })
}
