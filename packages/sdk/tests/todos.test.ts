// 清单（daemon.md § 21）：`node.todos.*` 是 `todo.*` 的薄封装，`todo_changed` 事件来自 `todo.changed`。
// 断言 SDK 发出的参数（id 缺省时生成 UUID、不传的字段不发）、返回值的类型和错误映射；fake daemon 见 tests/fake-daemon.ts。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import type { Crypto } from '@anycon/protocol'

import { RpcError, connect, type Node } from '../src/index.js'
import type { Env } from './harness.js'
import { FAST, cryptoImpls, startEnv, until } from './harness.js'

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

async function withNode(
  crypto: Crypto,
  fn: (node: Node, env: Env) => Promise<void>,
): Promise<void> {
  const env = await startEnv(crypto)
  try {
    await env.pair()
    const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
    try {
      await fn(node, env)
    } finally {
      await node.close()
    }
  } finally {
    await env.stop()
  }
}

const paramsOf = (env: Env, method: string): Array<Record<string, unknown>> =>
  env.daemon.requests
    .filter((r) => r.method === method)
    .map((r) => r.params as Record<string, unknown>)

for (const impl of cryptoImpls) {
  describe(`清单（${impl.label}）`, () => {
    it('node.info 带 capabilities.todo', async () => {
      await withNode(await impl.load(), async (node) => {
        assert.equal((await node.info()).capabilities.todo, true)
      })
    })

    it('add：缺省生成 UUID，不传的字段不发；传了 id 就原样发，重发得到同一项', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const t = await node.todos.add({ title: 'write the report' })
        assert.match(t.id, UUID_V4)
        assert.deepEqual(paramsOf(env, 'todo.add'), [{ id: t.id, title: 'write the report' }])
        assert.equal(t.title, 'write the report')
        assert.equal(t.state, 'open')
        assert.equal(t.unreviewed, false)

        const id = '11111111-2222-4333-8444-555555555555'
        const first = await node.todos.add({
          id,
          title: 'a',
          body: '# notes',
          project: 'app',
          due_ms: 5,
          agent: true,
          origin_session: 'sess_x',
        })
        assert.deepEqual(paramsOf(env, 'todo.add')[1], {
          id,
          title: 'a',
          body: '# notes',
          project: 'app',
          due_ms: 5,
          agent: true,
          origin_session: 'sess_x',
        })
        assert.equal(first.source.kind, 'agent')
        // 超时后用同一个 id 重试：不会出现第二项
        const again = await node.todos.add({ id, title: 'ignored' })
        assert.deepEqual(again, first)
        assert.equal((await node.todos.list()).todos.filter((x) => x.id === id).length, 1)
      })
    })

    it('list / update / delete：参数原样发出，返回值带类型；不存在的项是 RpcError not_found', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const a = await node.todos.add({ title: 'a' })
        const b = await node.todos.add({ title: 'b' })
        const page = await node.todos.list({ limit: 1 })
        assert.deepEqual(
          page.todos.map((t) => t.id),
          [b.id],
        )
        assert.equal(page.has_more, true)
        assert.deepEqual(paramsOf(env, 'todo.list'), [{ limit: 1 }])
        await node.todos.list()
        assert.deepEqual(paramsOf(env, 'todo.list')[1], {})

        const done = await node.todos.update({ id: a.id, state: 'done', clear: ['project'] })
        assert.equal(done.state, 'done')
        assert.deepEqual(paramsOf(env, 'todo.update'), [
          { id: a.id, state: 'done', clear: ['project'] },
        ])
        assert.deepEqual(
          (await node.todos.list()).todos.map((t) => t.id),
          [b.id],
          'closed items are left out by default',
        )
        assert.equal((await node.todos.list({ include_closed: true })).todos.length, 2)

        assert.equal(await node.todos.delete(b.id), undefined)
        assert.deepEqual(paramsOf(env, 'todo.delete'), [{ id: b.id }])
        for (const p of [node.todos.update({ id: b.id, title: 'x' }), node.todos.delete(b.id)]) {
          await assert.rejects(
            p,
            (err: unknown) => err instanceof RpcError && err.code === 'not_found',
          )
        }
        await assert.rejects(
          node.todos.add({ title: '' }),
          (err: unknown) => err instanceof RpcError && err.code === 'invalid',
        )
      })
    })

    it('dispatch / history：参数原样发出，返回值带类型，错误映射成 RpcError，todo_event 带「经过」的新条目', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const t = await node.todos.add({ title: 'ship it', project: 'app' })
        const seen: Array<{ todo_id: string; seq: number; kind: string }> = []
        node.on('todo_event', (e) =>
          seen.push({ todo_id: e.todo_id, seq: e.event.seq, kind: e.event.kind }),
        )

        // 不传 no_push：发出去的参数里就没有它（缺省值由 daemon 定），agent 没有这个选项 → unsupported / no_push
        await assert.rejects(
          node.todos.dispatch({ id: t.id, agent: 'mock' }),
          (err: unknown) =>
            err instanceof RpcError && err.code === 'unsupported' && err.reason === 'no_push',
        )
        assert.deepEqual(paramsOf(env, 'todo.dispatch'), [{ id: t.id, agent: 'mock' }])

        const r = await node.todos.dispatch({
          id: t.id,
          agent: 'mock',
          no_push: false,
          workspace: { mode: 'local' },
        })
        assert.deepEqual(paramsOf(env, 'todo.dispatch')[1], {
          id: t.id,
          agent: 'mock',
          no_push: false,
          workspace: { mode: 'local' },
        })
        assert.equal(r.todo.state, 'working')
        assert.deepEqual(r.todo.sessions, [r.session.id])
        assert.equal(r.session.todo_id, t.id)

        await until(() => seen.length === 1, 3000, 'one todo_event')
        assert.deepEqual(seen, [{ todo_id: t.id, seq: 1, kind: 'dispatched' }])

        const h = await node.todos.history({ id: t.id })
        assert.equal(h.has_more, false)
        assert.equal(h.events.length, 1)
        const e = h.events[0]
        assert.ok(e !== undefined && e.kind === 'dispatched')
        if (e.kind === 'dispatched') {
          assert.equal(e.session_id, r.session.id)
          assert.equal(e.no_push, false)
        }
        assert.deepEqual(paramsOf(env, 'todo.history'), [{ id: t.id }])
        const none = await node.todos.history({ id: t.id, before_seq: 1, limit: 5 })
        assert.deepEqual(none, { events: [], has_more: false })
        assert.deepEqual(paramsOf(env, 'todo.history')[1], { id: t.id, before_seq: 1, limit: 5 })

        // 已关闭 → invalid / closed；不存在 → not_found；没有项目 → invalid / project
        await node.todos.update({ id: t.id, state: 'done' })
        await assert.rejects(
          node.todos.dispatch({ id: t.id, agent: 'mock', no_push: false }),
          (err: unknown) =>
            err instanceof RpcError && err.code === 'invalid' && err.reason === 'closed',
        )
        await assert.rejects(
          node.todos.history({ id: '11111111-2222-4333-8444-555555555555' }),
          (err: unknown) => err instanceof RpcError && err.code === 'not_found',
        )
        const bare = await node.todos.add({ title: 'no project' })
        await assert.rejects(
          node.todos.dispatch({ id: bare.id, agent: 'mock', no_push: false }),
          (err: unknown) =>
            err instanceof RpcError && err.code === 'invalid' && err.field === 'project',
        )
      })
    })

    it('todo_changed：每次新建 / 修改 / 删除都带事件，包括发起的这一个', async () => {
      await withNode(await impl.load(), async (node) => {
        const seen: Array<{ id: string; deleted: boolean; title?: string }> = []
        node.on('todo_changed', (c) =>
          seen.push({ id: c.id, deleted: c.deleted === true, title: c.todo?.title }),
        )
        const t = await node.todos.add({ title: 'one' })
        await node.todos.update({ id: t.id, title: 'two' })
        await node.todos.delete(t.id)
        await until(() => seen.length === 3, 3000, 'three todo_changed events')
        assert.deepEqual(seen, [
          { id: t.id, deleted: false, title: 'one' },
          { id: t.id, deleted: false, title: 'two' },
          { id: t.id, deleted: true, title: undefined },
        ])
      })
    })
  })
}
