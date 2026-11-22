// 分身 SDK 通过真实 RPC 和假节点验证参数、通知、错误映射和处理人。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'
import { connect, RpcError, type TwinGetResult } from '../src/index.js'
import { FAST, cryptoImpls, startEnv, until } from './harness.js'

for (const impl of cryptoImpls)
  describe(`分身（${impl.label}）`, () => {
    it('全部方法透传协议字段，事件保留模型理由，清单透传 handler', async () => {
      const env = await startEnv(await impl.load())
      try {
        await env.pair()
        const node = await connect(env.url, {
          storage: env.storage,
          crypto: await impl.load(),
          ...FAST,
        })
        try {
          assert.equal((await node.info()).capabilities.twin, true)
          const status: TwinGetResult = await node.twin.get()
          assert.equal(status.configured, false)
          const changes: string[] = []
          const questions: string[] = []
          node.on('twin_changed', (c) => changes.push(c.state))
          node.on('twin_question', (n) => questions.push(n.question.answer ?? ''))
          const scope = {
            projects: ['web'],
            agent: 'mock',
            mode: 'ask',
            model: 'large',
            max_sessions: 1,
            daily_dispatches: 3,
            paused: true,
          }
          assert.equal((await node.twin.set(scope)).state, 'paused')
          assert.equal(
            (await node.twin.set({ clear: ['agent', 'model', 'mode'] })).scope.agent,
            undefined,
          )
          assert.equal(await node.twin.wake(), undefined)
          const dry = await node.twin.dryRun()
          assert.equal(dry.run.dry_run, true)
          assert.equal(dry.run.decisions[0]?.reason, 'Need a project')
          assert.equal((await node.twin.runs()).runs[0]?.id, dry.run.id)
          assert.deepEqual(await node.twin.runs({ limit: 3, before_ms: 1 }), { runs: [] })
          assert.equal(
            (await node.twin.questions({ open_only: true })).questions[0]?.status,
            'open',
          )
          assert.equal((await node.twin.answer({ id: 'q1', answer: 'web' })).answer, 'web')
          assert.equal((await node.twin.questions()).questions[0]?.status, 'answered')
          await until(() => questions.length === 1 && changes.length >= 3, 3000, 'Twin events')
          assert.deepEqual(questions, ['web'])
          const todo = await node.todos.add({ title: 'Build it', handler: 'twin' })
          assert.equal(todo.handler, 'twin')
          assert.equal((await node.todos.update({ id: todo.id, handler: 'me' })).handler, 'me')
          assert.equal(
            (await node.todos.update({ id: todo.id, reviewed: true, handler: 'twin' })).handler,
            'twin',
          )
          const calls = env.daemon.requests.filter((r) => r.method.startsWith('twin.'))
          assert.deepEqual(
            calls.map((r) => [r.method, r.params]),
            [
              ['twin.get', {}],
              ['twin.set', scope],
              ['twin.set', { clear: ['agent', 'model', 'mode'] }],
              ['twin.wake', {}],
              ['twin.dry_run', {}],
              ['twin.runs', {}],
              ['twin.runs', { limit: 3, before_ms: 1 }],
              ['twin.questions', { open_only: true }],
              ['twin.answer', { id: 'q1', answer: 'web' }],
              ['twin.questions', {}],
            ],
          )
          await assert.rejects(
            node.twin.set({ max_sessions: 99 }),
            (e: unknown) => e instanceof RpcError && e.field === 'max_sessions',
          )
          await assert.rejects(
            node.twin.answer({ id: 'gone', answer: 'web' }),
            (e: unknown) => e instanceof RpcError && e.code === 'not_found',
          )
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })
  })
