// 决策档案（daemon.md § 23）：`node.rules.*` 是 `rule.*` 的薄封装，`rule_changed` 事件来自 `rule.changed`。
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
  describe(`决策档案（${impl.label}）`, () => {
    it('node.info 带 capabilities.rules', async () => {
      await withNode(await impl.load(), async (node) => {
        assert.equal((await node.info()).capabilities.rules, true)
      })
    })

    it('add：缺省生成 UUID，不传的字段不发；传了 id 就原样发，重发得到同一条', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const r = await node.rules.add({ text: 'write the failing test first' })
        assert.match(r.id, UUID_V4)
        assert.deepEqual(paramsOf(env, 'rule.add'), [
          { id: r.id, text: 'write the failing test first' },
        ])
        assert.equal(r.status, 'active')
        assert.equal(r.source.kind, 'manual')

        const id = '11111111-2222-4333-8444-555555555555'
        const first = await node.rules.add({
          id,
          text: 'ask first',
          project: 'app',
          status: 'pending',
          source: { kind: 'answer', session_id: 'sess_x', request_id: 'req_1' },
        })
        assert.deepEqual(paramsOf(env, 'rule.add')[1], {
          id,
          text: 'ask first',
          project: 'app',
          status: 'pending',
          source: { kind: 'answer', session_id: 'sess_x', request_id: 'req_1' },
        })
        // 超时后用同一个 id 重试：不会出现第二条
        const again = await node.rules.add({ id, text: 'ignored' })
        assert.deepEqual(again, first)
        assert.equal((await node.rules.list()).rules.filter((x) => x.id === id).length, 1)
      })
    })

    it('agent 标记：状态强制为 pending，来源是 agent', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const r = await node.rules.add({ text: 'from an agent', agent: true, status: 'active' })
        assert.equal(r.status, 'pending')
        assert.equal(r.source.kind, 'agent')
        assert.deepEqual(paramsOf(env, 'rule.add')[0], {
          id: r.id,
          text: 'from an agent',
          agent: true,
          status: 'active',
        })
      })
    })

    it('list / update / delete：参数原样发出，返回值带类型；条件和不存在的错误映射成 RpcError', async () => {
      await withNode(await impl.load(), async (node, env) => {
        const a = await node.rules.add({ text: 'a' })
        const b = await node.rules.add({ text: 'b', project: 'web' })
        assert.deepEqual(
          (await node.rules.list()).rules.map((r) => r.id),
          [a.id, b.id],
        )
        assert.deepEqual(paramsOf(env, 'rule.list'), [{}])
        const l = await node.rules.list({ status: 'active', project: 'web' })
        assert.deepEqual(
          l.rules.map((r) => r.id),
          [b.id],
        )
        assert.deepEqual(paramsOf(env, 'rule.list')[1], { status: 'active', project: 'web' })
        assert.equal(l.budget.cap, 300)

        const off = await node.rules.update({ id: a.id, status: 'disabled', clear: ['project'] })
        assert.equal(off.status, 'disabled')
        assert.deepEqual(paramsOf(env, 'rule.update'), [
          { id: a.id, status: 'disabled', clear: ['project'] },
        ])
        // 过期的版本：RpcError invalid / changed，带现在的 updated_ms
        await assert.rejects(
          node.rules.update({ id: a.id, text: 'x', if_updated_ms: a.updated_ms }),
          (err: unknown) =>
            err instanceof RpcError &&
            err.code === 'invalid' &&
            err.reason === 'changed' &&
            err.updatedMs === off.updated_ms,
        )
        await assert.rejects(
          node.rules.delete(a.id, { ifUpdatedMs: a.updated_ms }),
          (err: unknown) => err instanceof RpcError && err.reason === 'changed',
        )
        assert.equal(await node.rules.delete(a.id, { ifUpdatedMs: off.updated_ms }), undefined)
        assert.deepEqual(paramsOf(env, 'rule.delete').at(-1), {
          id: a.id,
          if_updated_ms: off.updated_ms,
        })
        assert.equal(await node.rules.delete(b.id), undefined)
        assert.deepEqual(paramsOf(env, 'rule.delete').at(-1), { id: b.id })
        for (const p of [node.rules.update({ id: b.id, text: 'x' }), node.rules.delete(b.id)]) {
          await assert.rejects(
            p,
            (err: unknown) => err instanceof RpcError && err.code === 'not_found',
          )
        }
        await assert.rejects(
          node.rules.add({ text: '' }),
          (err: unknown) => err instanceof RpcError && err.code === 'invalid',
        )
      })
    })

    it('超出字数上限：RpcError invalid / over_budget，带 bytes 和 cap', async () => {
      await withNode(await impl.load(), async (node) => {
        await node.rules.add({ text: 'x'.repeat(200) })
        await assert.rejects(
          node.rules.add({ text: 'y'.repeat(200) }),
          (err: unknown) =>
            err instanceof RpcError &&
            err.code === 'invalid' &&
            err.field === 'text' &&
            err.reason === 'over_budget' &&
            err.bytes === 406 &&
            err.cap === 300,
        )
        // 待确认的不占字数；确认的时候才检查
        const p = await node.rules.add({ text: 'z'.repeat(200), status: 'pending' })
        await assert.rejects(
          node.rules.update({ id: p.id, status: 'active' }),
          (err: unknown) => err instanceof RpcError && err.reason === 'over_budget',
        )
      })
    })

    it('preview：参数原样发出，返回文字、规则 id 和字节数', async () => {
      await withNode(await impl.load(), async (node, env) => {
        assert.deepEqual(await node.rules.preview(), { text: '', rule_ids: [], bytes: 0 })
        assert.deepEqual(paramsOf(env, 'rule.preview'), [{}])
        const a = await node.rules.add({ text: 'node wide' })
        await node.rules.add({ text: 'in api', project: 'api' })
        const p = await node.rules.preview({ project: 'web' })
        assert.deepEqual(p.rule_ids, [a.id])
        assert.equal(p.bytes, new TextEncoder().encode(p.text).length)
        assert.deepEqual(paramsOf(env, 'rule.preview')[1], { project: 'web' })
      })
    })

    it('todos.dispatchPreview 带 rules_text', async () => {
      await withNode(await impl.load(), async (node) => {
        const t = await node.todos.add({ title: 'ship it', project: 'app' })
        assert.equal((await node.todos.dispatchPreview({ id: t.id })).rules_text, '')
        await node.rules.add({ text: 'be brief', project: 'app' })
        assert.equal(
          (await node.todos.dispatchPreview({ id: t.id })).rules_text,
          'Standing rules:\n- be brief',
        )
      })
    })

    it('rule_changed：每次新建 / 修改 / 删除都带事件，包括发起的这一个', async () => {
      await withNode(await impl.load(), async (node) => {
        const seen: Array<{ id: string; deleted: boolean; text?: string }> = []
        node.on('rule_changed', (c) =>
          seen.push({ id: c.id, deleted: c.deleted === true, text: c.rule?.text }),
        )
        const r = await node.rules.add({ text: 'one' })
        await node.rules.update({ id: r.id, text: 'two' })
        await node.rules.delete(r.id)
        await until(() => seen.length === 3, 3000, 'three rule_changed events')
        assert.deepEqual(seen, [
          { id: r.id, deleted: false, text: 'one' },
          { id: r.id, deleted: false, text: 'two' },
          { id: r.id, deleted: true, text: undefined },
        ])
      })
    })
  })
}
