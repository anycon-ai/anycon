// 端到端主流程：pair → connect → create → send → respond → turn_done。
// fake hub（真实 WebSocket，只转发）+ fake daemon（TS，Noise responder）。同一组场景对每种 Noise 实现跑一遍。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { RpcError, connect, policies, type StreamEvent } from '../src/index.js'
import { FAST, collect, cryptoImpls, sleep, startEnv, until } from './harness.js'

for (const impl of cryptoImpls) {
  describe(`主流程（${impl.label}）`, () => {
    it('pair → connect → agents/projects → create → send → respond → turn_done', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair({ name: 'ci-runner' })
        assert.equal(paired.nodeId, env.daemon.id)
        assert.equal(paired.nodeName, 'fake-node') // pair 之后顺手取了节点名
        assert.match(paired.nodeFingerprint, /^([0-9a-f]{2}:){7}[0-9a-f]{2}$/)

        // daemon 侧记下了对端公钥和握手第 3 条里自报的设备信息
        const peer = env.daemon.peers.get(paired.deviceId)
        assert.ok(peer)
        assert.equal(peer.info?.name, 'ci-runner')
        assert.equal(peer.info?.kind, 'sdk')

        const node = await connect(env.url, {
          storage: env.storage,
          crypto,
          node: 'fake-node',
          ...FAST,
        })
        try {
          assert.equal(node.online, true)
          assert.equal(node.name, 'fake-node')
          const info = await node.info()
          assert.equal(info.protocol_version, 1)

          const agents = await node.agents.list()
          assert.equal(agents[0]?.id, 'mock')
          // etag 缓存：第二次 daemon 回 not_modified，SDK 返回缓存
          assert.deepEqual(await node.agents.list(), agents)
          assert.equal(env.daemon.requests.filter((r) => r.method === 'agent.list').length, 2)

          const created = await node.projects.create('demo', { gitInit: true })
          assert.equal(created.name, 'demo')
          assert.deepEqual(
            (await node.projects.list()).map((p) => p.name),
            ['app', 'demo'],
          )
          // 返回类型来自 schema/methods.json 生成的 method-types：workspace.rename → `{ from, to }`
          const renamed = await node.projects.rename('demo', 'demo2')
          assert.deepEqual(renamed, { from: 'demo', to: 'demo2' })
          const trashed = await node.projects.trash('demo2')
          assert.equal(trashed.original_name, 'demo2')
          assert.match(trashed.id, /^demo2-/)
          assert.deepEqual(
            (await node.projects.trashList()).map((t) => t.id),
            [trashed.id],
          )
          const restored = await node.projects.restore(trashed.id)
          assert.equal(restored.name, 'demo2')
          await node.projects.trash('demo2')
          await node.projects.purge((await node.projects.trashList())[0]!.id)
          assert.deepEqual(await node.projects.trashList(), [])
          // 关联已有目录：link / browse / unlink（参数 camelCase → 线上 snake_case，不传的字段不发）
          const browsed = await node.projects.browse()
          assert.deepEqual(
            browsed.entries.map((e) => e.name),
            ['app'],
          )
          assert.deepEqual(
            (await node.projects.browse({ path: '/home/me', showHidden: true })).entries.map(
              (e) => e.name,
            ),
            ['.config', 'app'],
          )
          // 不能关联的目录带 `blocked` 原因（linkable 为 false），能关联的没有
          const hiddenOnes = (await node.projects.browse({ showHidden: true })).entries
          assert.deepEqual(
            hiddenOnes.map((e) => [e.name, e.linkable, e.blocked ?? null]),
            [
              ['.config', false, 'hidden'],
              ['app', true, null],
            ],
          )
          const browseCalls = env.daemon.requests.filter((r) => r.method === 'workspace.browse')
          assert.deepEqual(
            browseCalls.map((r) => r.params),
            [{}, { path: '/home/me', show_hidden: true }, { show_hidden: true }],
          )
          const linkedProject = await node.projects.link('/home/me/notes')
          assert.equal(linkedProject.name, 'notes')
          assert.equal(linkedProject.kind, 'linked')
          assert.equal((await node.projects.link('/home/me/x', { name: 'other' })).name, 'other')
          const linkCalls = env.daemon.requests.filter((r) => r.method === 'workspace.link')
          assert.deepEqual(
            linkCalls.map((r) => r.params),
            [{ path: '/home/me/notes' }, { path: '/home/me/x', name: 'other' }],
          )
          await assert.rejects(node.projects.link('/etc/passwd.d'), RpcError)
          await assert.rejects(node.projects.link('/home/me/notes'), RpcError)
          assert.deepEqual(await node.projects.unlink('notes'), {})
          await node.projects.unlink('other')
          await assert.rejects(node.projects.unlink('app'), RpcError)
          assert.deepEqual(
            (await node.projects.list()).map((p) => p.name),
            ['app'],
          )
          const job = await node.projects.clone('https://example.com/r.git')
          const stages: string[] = []
          for await (const p of job.progress()) stages.push(p.stage)
          assert.deepEqual(stages, ['receiving'])
          assert.equal((await job.wait()).ok, true)

          const session = await node.sessions.create({
            project: 'app',
            agent: 'mock',
            settings: { model: 'mock-large', thinking: 'low', mode: 'ask' },
          })
          assert.match(session.id, /^sess_/)
          assert.equal(session.settings.thinking, 'low')

          const events: StreamEvent[] = []
          for await (const ev of session.send('run permission please')) {
            events.push(ev)
            if (ev.type === 'permission') {
              assert.equal(ev.kind, 'tool')
              assert.equal(ev.options.length, 3)
              await ev.allow()
            }
          }
          assert.deepEqual(
            events.map((e) => e.type),
            [
              'user_message',
              'turn_start',
              'thinking_delta',
              'text_delta',
              'permission',
              'permission_resolved',
              'tool_call',
              'tool_result',
              'text_delta',
              'turn_done',
            ],
          )
          // 每个事件带会话元数据，seq 严格递增
          assert.ok(events.every((e) => e.session_id === session.id))
          assert.deepEqual(
            events.map((e) => e.seq),
            events.map((_, i) => i + 1),
          )
          const done = events.at(-1)
          assert.ok(done?.type === 'turn_done')
          assert.equal(done.reason, 'completed')
          assert.equal(done.usage?.output_tokens, 20)

          // 迭代结束后自动 unwatch
          await until(
            () => env.daemon.requests.some((r) => r.method === 'session.unwatch'),
            2000,
            'unwatch',
          )

          // 第二个 turn 复用同一个会话
          const second = await collect(session.send('hi'))
          assert.equal(second.at(-1)?.type, 'turn_done')
          const turnIds = second
            .filter((e) => e.type === 'turn_start')
            .map((e) => (e.type === 'turn_start' ? e.turn_id : -1))
          assert.deepEqual(turnIds, [2])

          // 重新拿到已有会话
          const again = await node.sessions.get(session.id)
          assert.equal(again, session)
          assert.equal((await node.sessions.list()).length, 1)
          const history = await session.history({ afterSeq: 0 })
          assert.ok(history.events.length > 10)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('onPermission 策略自动答复；run() 返回文本和 usage', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, {
          storage: env.storage,
          crypto,
          onPermission: policies.allowReadsDenyRest, // shell 工具 → deny
          ...FAST,
        })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const result = await session.run('please run permission')
          assert.equal(result.text, 'Hello skipped')
          assert.equal(result.reason, 'completed')
          assert.equal(result.usage?.input_tokens, 10)
          assert.ok(result.events.some((e) => e.type === 'permission'))

          // 会话级策略覆盖节点级
          session.policy = policies.allowAll
          const allowed = await session.run('permission again')
          assert.equal(allowed.text, 'Hello done')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('session.rename / archive / unarchive / restart 的答复是 `{}`，句柄方法照常完成', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          await session.rename('release notes')
          assert.equal(session.summary.title, 'release notes')
          await session.archive()
          await session.unarchive()
          await session.restart()
          assert.deepEqual(
            env.daemon.requests
              .filter((r) => r.method.startsWith('session.') && r.method !== 'session.create')
              .map((r) => r.method),
            ['session.rename', 'session.archive', 'session.unarchive', 'session.restart'],
          )
          const [fresh] = (await node.sessions.list()).filter((s) => s.id === session.id)
          assert.equal(fresh?.title, 'release notes')
          assert.equal(fresh?.archived, false)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('已归档的会话 send() 抛 invalid / archived，unarchive() 后能发；运行 / 等审批时 archive() 抛 busy', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          await session.archive()
          await assert.rejects(collect(session.send('hello')), (err: unknown) => {
            assert.ok(err instanceof RpcError)
            assert.equal(err.code, 'invalid')
            assert.equal(err.reason, 'archived')
            return true
          })
          // 被拒的 send 不会在 daemon 上留下 turn
          assert.equal(env.daemon.sessions.get(session.id)?.events.length, 0)

          await session.unarchive()
          const events = await collect(session.send('hello'))
          assert.equal(events.at(-1)?.type, 'turn_done')

          // 等审批（waiting_permission）时不能归档：busy；答复之后可以
          const busy: unknown[] = []
          for await (const ev of session.send('permission')) {
            if (ev.type === 'permission') {
              await session.archive().catch((err: unknown) => busy.push(err))
              await ev.allow()
            }
          }
          assert.equal(busy.length, 1)
          assert.ok(busy[0] instanceof RpcError)
          assert.equal(busy[0].code, 'busy')
          assert.equal(env.daemon.sessions.get(session.id)?.summary.archived, false)
          await session.archive()
          assert.equal(env.daemon.sessions.get(session.id)?.summary.archived, true)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('setSettings：clear 原样放进 params，settings 缺省 = 保持；返回生效后的完整设置', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({
            agent: 'mock',
            settings: { model: 'mock-large', thinking: 'low', mode: 'ask', max_turns: 5 },
          })
          const calls = () =>
            env.daemon.requests
              .filter((r) => r.method === 'session.set_settings')
              .map((r) => r.params as Record<string, unknown>)

          // 只改 model：没有 clear 键，其余字段保持
          const kept = await session.setSettings({ model: 'mock-small' })
          assert.equal(kept.model, 'mock-small')
          assert.equal(kept.thinking, 'low')
          assert.equal(kept.max_turns, 5)
          assert.ok(!('clear' in calls().at(-1)!), 'no clear key when nothing is cleared')
          assert.deepEqual(calls().at(-1), {
            session_id: session.id,
            settings: { model: 'mock-small' },
          })

          // 清除 thinking 和 max_turns，同时改 mode：clear 在 payload 里，结果是生效后的完整设置
          const cleared = await session.setSettings(
            { mode: 'yolo' },
            { clear: ['thinking', 'max_turns'] },
          )
          assert.deepEqual(calls().at(-1), {
            session_id: session.id,
            settings: { mode: 'yolo' },
            clear: ['thinking', 'max_turns'],
          })
          assert.equal(cleared.mode, 'yolo')
          assert.equal(cleared.model, 'mock-small')
          assert.ok(cleared.thinking === undefined || cleared.thinking === null)
          assert.ok(cleared.max_turns === undefined || cleared.max_turns === null)
          assert.deepEqual(session.settings, cleared) // 句柄上的设置也更新了

          // 空的 clear 数组不发
          await session.setSettings({ mode: 'ask' }, { clear: [] })
          assert.ok(!('clear' in calls().at(-1)!))
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('没有策略时 run() 直接 deny，不会挂住', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const result = await session.run('permission')
          assert.equal(result.text, 'Hello skipped')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('respond(requestId, optionId, message) 手动答复；重复答复得到 already_resolved', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const seen: string[] = []
          for await (const ev of session.send('permission')) {
            seen.push(ev.type)
            if (ev.type === 'permission') {
              await session.respond(ev.request_id, 'deny', 'not now')
              await assert.rejects(session.respond(ev.request_id, 'deny'), (err: unknown) => {
                assert.equal((err as { code?: string }).code, 'already_resolved')
                return true
              })
            }
          }
          assert.ok(!seen.includes('tool_call'))
          assert.equal(seen.at(-1), 'turn_done')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('SDK 对无人答复的权限请求超时后代答 deny', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, {
          storage: env.storage,
          crypto,
          permissionTimeoutMs: 80,
          ...FAST,
        })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const events = await collect(session.send('permission'))
          const text = events.flatMap((e) => (e.type === 'text_delta' ? [e.text] : [])).join('')
          assert.equal(text, 'Hello skipped')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('不替没在使用的会话做决定：别的设备的会话不会被代答', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, {
          storage: env.storage,
          crypto,
          onPermission: policies.denyAll,
          permissionTimeoutMs: 50,
          ...FAST,
        })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          // 没有进行中的 send()/events()：session.active === false
          const requests: string[] = []
          node.on('permission', (req) => requests.push(req.request_id))
          // 用另一条路径触发 daemon 的权限请求：直接让 daemon 脚本跑一个 turn（没人 watch）
          const rpc = (node as unknown as { call: typeof node.call }).call.bind(node)
          await rpc('session.send', {
            session_id: session.id,
            message_id: 'm1',
            content: 'permission',
          })
          await until(() => requests.length === 1, 2000, 'permission.request notification')
          await sleep(200) // 超过 permissionTimeoutMs
          assert.equal(
            env.daemon.requests.filter((r) => r.method === 'permission.respond').length,
            0,
          )
          // 现在 SDK 之外有人答复它，流程收尾
          const pending = await node.permissions.list(session.id)
          assert.equal(pending.length, 1)
          await rpc('permission.respond', {
            request_id: pending[0]!.request_id,
            option_id: 'allow',
          })
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('events() 观察整个会话；cancel() 结束 turn；setSettings 回显生效设置', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const observed: string[] = []
          const watcher = (async () => {
            for await (const ev of session.events()) {
              observed.push(ev.type)
              if (ev.type === 'turn_done') return ev.reason
            }
            return undefined
          })()

          const stream = session.send('slow slow slow')
          const first = await stream.next()
          assert.equal(first.value?.type, 'user_message')
          await session.cancel()
          const rest = await collect(stream)
          const done = rest.at(-1)
          assert.ok(done?.type === 'turn_done')
          assert.equal(done.reason, 'cancelled')
          assert.equal(await watcher, 'cancelled')
          assert.ok(observed.includes('turn_start'))

          const settings = await session.setSettings({ thinking: 'high' })
          assert.equal(settings.thinking, 'high')
          assert.equal(session.settings.thinking, 'high')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('提前 break 不取消 turn，之后可以 history() 取回全部事件；重发同一 message_id 是幂等的', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          for await (const ev of session.send('slow break-early', { messageId: 'fixed-id' })) {
            if (ev.type === 'text_delta') break
          }
          await until(
            () => env.daemon.sessions.get(session.id)?.events.at(-1)?.event.type === 'turn_done',
            3000,
            'daemon to finish the turn on its own',
          )
          const types = (await session.history()).events.map((e) => e.event.type)
          assert.equal(types.at(-1), 'turn_done')

          // 幂等重发：daemon 回 duplicate，SDK 从历史里回放出那一轮
          const replay = await collect(session.send('slow break-early', { messageId: 'fixed-id' }))
          assert.equal(replay.at(-1)?.type, 'turn_done')
          const turns = env.daemon.sessions.get(session.id)?.turn
          assert.equal(turns, 1)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('不可恢复的 error 事件结束迭代', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const events = await collect(session.send('trigger error'))
          const last = events.at(-1)
          assert.ok(last?.type === 'error')
          assert.equal(last.recoverable, false)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('AbortSignal 中止迭代但不取消 turn', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const ctl = new AbortController()
          const stream = session.send('slow abort', { signal: ctl.signal })
          await stream.next()
          ctl.abort(new Error('stop iterating'))
          await assert.rejects(collect(stream), /stop iterating/)
          await until(
            () => env.daemon.sessions.get(session.id)?.events.at(-1)?.event.type === 'turn_done',
            3000,
            'turn to complete anyway',
          )
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })
  })
}
