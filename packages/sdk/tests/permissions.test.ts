// 权限策略不会悄悄升级成 allow_always；自动放行默认不含 mode_change / plan；策略出错不再被吞掉。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import type { PermissionRequest } from '@anycon/protocol'

import { AnyconError, connect, policies, type StreamEvent } from '../src/index.js'
import { decisionToParams } from '../src/permissions.js'
import { FAST, cryptoImpls, startEnv, until } from './harness.js'

const baseRequest = (kind: PermissionRequest['kind']): PermissionRequest => ({
  session_id: 's',
  request_id: 'r1',
  kind,
  title: 't',
  detail: {},
  options: [
    { id: 'a', label: 'A', kind: 'allow_once' },
    { id: 's', label: 'S', kind: 'allow_always' },
    { id: 'd', label: 'D', kind: 'reject_once' },
  ],
  created_at_ms: 0,
})

describe('decisionToParams：不悄悄升级', () => {
  const alwaysOnly = {
    request_id: 'r1',
    options: [
      { id: 's', label: 'S', kind: 'allow_always' as const },
      { id: 'd', label: 'D', kind: 'reject_once' as const },
    ],
  }
  const onceOnly = {
    request_id: 'r1',
    options: [
      { id: 'a', label: 'A', kind: 'allow_once' as const },
      { id: 'd', label: 'D', kind: 'reject_once' as const },
    ],
  }
  it("'allow' 只选 allow_once；只有 allow_always 时没有可用 option（返回 undefined）", () => {
    assert.deepEqual(decisionToParams(onceOnly, 'allow'), { request_id: 'r1', option_id: 'a' })
    assert.equal(decisionToParams(alwaysOnly, 'allow'), undefined)
  })
  it("'allow_session' 优先 allow_always，没有就退回 allow_once（只会收窄）", () => {
    assert.deepEqual(decisionToParams(alwaysOnly, 'allow_session'), {
      request_id: 'r1',
      option_id: 's',
    })
    assert.deepEqual(decisionToParams(onceOnly, 'allow_session'), {
      request_id: 'r1',
      option_id: 'a',
    })
  })
})

describe('policies.allowAll / allowAllWith', () => {
  const ctx = { session: undefined, node: undefined as never }
  const run = (policy: (r: PermissionRequest, c: typeof ctx) => unknown, kind: string) =>
    policy(baseRequest(kind as PermissionRequest['kind']), ctx)

  it('allowAll 放行 tool / other，不放行 mode_change / plan / question', () => {
    assert.equal(run(policies.allowAll, 'tool'), 'allow')
    assert.equal(run(policies.allowAll, 'other'), 'allow')
    assert.equal(run(policies.allowAll, 'mode_change'), undefined)
    assert.equal(run(policies.allowAll, 'plan'), undefined)
    assert.equal(run(policies.allowAll, 'question'), undefined)
  })

  it('allowAllWith 要显式打开 plan / modeChange', () => {
    const both = policies.allowAllWith({ plan: true, modeChange: true })
    assert.equal(run(both, 'plan'), 'allow')
    assert.equal(run(both, 'mode_change'), 'allow')
    assert.equal(run(both, 'question'), undefined)
    const onlyPlan = policies.allowAllWith({ plan: true })
    assert.equal(run(onlyPlan, 'plan'), 'allow')
    assert.equal(run(onlyPlan, 'mode_change'), undefined)
    assert.equal(run(policies.allowAllWith(), 'plan'), undefined)
  })
})

for (const impl of cryptoImpls) {
  describe(`权限策略端到端（${impl.label}）`, () => {
    it("策略说 'allow' 但请求里没有 allow_once：不升级成 allow_always，发 permission_error，超时 deny", async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, {
          storage: env.storage,
          crypto,
          onPermission: policies.allowAll,
          permissionTimeoutMs: 250,
          ...FAST,
        })
        try {
          const errors: Array<{ error: unknown; request: PermissionRequest }> = []
          node.on('permission_error', (error, request) => errors.push({ error, request }))
          const session = await node.sessions.create({ agent: 'mock' })
          const result = await session.run('permission always-only')
          assert.equal(result.text, 'Hello skipped') // 被超时 deny，不是被放行
          assert.equal(errors.length, 1)
          assert.ok(errors[0]!.error instanceof AnyconError)
          assert.equal((errors[0]!.error as AnyconError).code, 'no_matching_option')
          const responds = env.daemon.requests.filter((r) => r.method === 'permission.respond')
          assert.equal(responds.length, 1)
          const sent = responds[0]!.params as { option_id?: string }
          assert.equal(sent.option_id, 'deny')
          assert.ok(
            !responds.some(
              (r) => (r.params as { option_id?: string }).option_id === 'allow_session',
            ),
          )
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('事件上的 allow() 在没有 allow_once 时报错（no_matching_option），allowSession() 可以', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const events: StreamEvent[] = []
          let allowError: unknown
          for await (const ev of session.send('permission always-only')) {
            events.push(ev)
            if (ev.type === 'permission') {
              await ev.allow().catch((e: unknown) => (allowError = e))
              assert.deepEqual(
                env.daemon.requests.filter((r) => r.method === 'permission.respond'),
                [],
              )
              await ev.allowSession()
            }
          }
          assert.ok(allowError instanceof AnyconError)
          assert.equal(allowError.code, 'no_matching_option')
          assert.ok(events.some((e) => e.type === 'tool_result')) // allow_session 放行了
          const sent = env.daemon.requests.find((r) => r.method === 'permission.respond')!
          assert.equal((sent.params as { option_id?: string }).option_id, 'allow_session')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('allowAll 不自动批准 mode_change / plan（留给调用方或超时 deny）；显式 allowAllWith 才放行', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, {
          storage: env.storage,
          crypto,
          onPermission: policies.allowAll,
          permissionTimeoutMs: 250,
          ...FAST,
        })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          assert.equal((await session.run('permission mode_change')).text, 'Hello skipped')
          assert.equal((await session.run('permission plan')).text, 'Hello skipped')
          assert.equal((await session.run('permission tool')).text, 'Hello done') // 普通工具调用照常放行

          session.policy = policies.allowAllWith({ modeChange: true })
          assert.equal((await session.run('permission mode_change')).text, 'Hello done')
          assert.equal((await session.run('permission plan')).text, 'Hello skipped') // plan 没开

          session.policy = policies.allowAllWith({ modeChange: true, plan: true })
          assert.equal((await session.run('permission plan')).text, 'Hello done')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('策略回调抛异常：发 permission_error（不再悄悄吞掉），请求照常走超时 deny', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const boom = new Error('policy exploded')
        const node = await connect(env.url, {
          storage: env.storage,
          crypto,
          onPermission: () => {
            throw boom
          },
          permissionTimeoutMs: 250,
          ...FAST,
        })
        try {
          const errors: unknown[] = []
          node.on('permission_error', (error) => errors.push(error))
          const session = await node.sessions.create({ agent: 'mock' })
          const result = await session.run('permission please')
          assert.equal(result.text, 'Hello skipped')
          await until(() => errors.length === 1, 1000, 'permission_error')
          assert.equal(errors[0], boom)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })
  })
}
