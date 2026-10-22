// 后台任务相关：agent 自己开始的 turn（`turn_start.message_id` 为空串）不会被任何一次 `send()` 认领；
// `session.changed` 里的 `background_tasks` 合并进会话摘要。
// 认领逻辑用桩 Node 直接喂事件（精确控制到达顺序），摘要合并走 fake daemon 的完整链路。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import type { Event, SessionEvent, SessionSendResult, SessionSummary } from '@anycon/protocol'

import { connect, type StreamEvent } from '../src/index.js'
import type { Node } from '../src/node.js'
import { Session } from '../src/session.js'
import { FAST, collect, cryptoImpls, sleep, startEnv, until } from './harness.js'

const SESSION_ID = 'sess_bg'

const baseSummary = (): SessionSummary => ({
  id: SESSION_ID,
  agent: 'mock',
  title: 'bg',
  status: 'idle',
  settings: {},
  pending_permissions: 0,
  created_at_ms: 0,
  updated_at_ms: 0,
  last_seq: 0,
})

interface Harness {
  session: Session
  /** 按顺序喂一个事件（seq 自动递增）。 */
  feed(event: Event): void
  /** 让桩 `session.send` 回复；不调用就一直挂着（模拟事件先于答复到达）。 */
  reply(result: SessionSendResult): void
}

/** 桩 Node：只实现 `Session.send()` 用到的部分，`session.send` 的答复由测试决定何时给。 */
function harness(): Harness {
  let reply!: (r: SessionSendResult) => void
  const replied = new Promise<SessionSendResult>((resolve) => {
    reply = resolve
  })
  const node = {
    name: 'stub',
    hasPermissionPolicy: false,
    on: () => () => undefined,
    observePermission: () => undefined,
    call: async (method: string) => (method === 'session.send' ? replied : {}),
  } as unknown as Node
  const session = new Session(node, baseSummary())
  let seq = 0
  return {
    session,
    reply,
    feed(event) {
      seq += 1
      const se: SessionEvent = { session_id: SESSION_ID, seq, ts_ms: seq, event }
      session.dispatch(se)
    },
  }
}

const userMessage = (messageId: string): Event => ({
  type: 'user_message',
  message_id: messageId,
  content: 'hi',
  by_device: 'dev',
})
const turnStart = (turnId: number, messageId: string): Event => ({
  type: 'turn_start',
  turn_id: turnId,
  message_id: messageId,
})
const text = (t: string): Event => ({ type: 'text_delta', text: t })
const turnDone = (turnId: number): Event => ({
  type: 'turn_done',
  turn_id: turnId,
  reason: 'completed',
})

const types = (events: StreamEvent[]): string[] => events.map((e) => e.type)

describe('agent 自己开始的 turn 不属于任何一次 send()', () => {
  // 我的 turn 的 message_id 用 'm0'（排在前一条消息后面、合并进同一个 turn 的情形），
  // 这样认领只能靠「user_message 之后的第一个 turn_start」这条兜底，正是 agent 的 turn 容易被误认的地方。

  it('我的 user_message 之后先来 agent 的 turn：流只含我的 turn（答复没带 turn_id，走兜底匹配）', async () => {
    const h = harness()
    const stream = h.session.send('hi', { messageId: 'm1' })
    h.reply({ queued: true }) // 排队：没有立即开始的 turn_id
    await stream.sent

    h.feed(userMessage('m1'))
    h.feed(turnStart(7, '')) // agent 自己开的一轮
    h.feed(text('background result'))
    h.feed(turnDone(7))
    h.feed(turnStart(8, 'm0'))
    h.feed(text('mine'))
    h.feed(turnDone(8))

    const events = await collect(stream)
    assert.deepEqual(types(events), ['user_message', 'turn_start', 'text_delta', 'turn_done'])
    const [, start, delta, done] = events
    assert.ok(start?.type === 'turn_start' && start.turn_id === 8 && start.message_id === 'm0')
    assert.ok(delta?.type === 'text_delta' && delta.text === 'mine')
    assert.ok(done?.type === 'turn_done' && done.turn_id === 8)
  })

  it('事件先于 session.send 的答复到达（还不知道 turn_id）：同样跳过 agent 的 turn', async () => {
    const h = harness()
    const stream = h.session.send('hi', { messageId: 'm1' })
    h.feed(userMessage('m1'))
    h.feed(turnStart(7, ''))
    h.feed(text('background result'))
    h.feed(turnDone(7))
    h.feed(turnStart(8, 'm0'))
    h.feed(text('mine'))
    h.feed(turnDone(8))
    h.reply({ queued: false, turn_id: 8 })

    const events = await collect(stream)
    assert.deepEqual(types(events), ['user_message', 'turn_start', 'text_delta', 'turn_done'])
    assert.ok(events[1]?.type === 'turn_start' && events[1].turn_id === 8)
    assert.ok(events[2]?.type === 'text_delta' && events[2].text === 'mine')
  })

  it('答复里带 turn_id：按 turn_id 精确认领，空串 message_id 的 turn 不会误配', async () => {
    const h = harness()
    const stream = h.session.send('hi', { messageId: 'm1' })
    h.reply({ queued: false, turn_id: 8 })
    await stream.sent

    h.feed(userMessage('m1'))
    h.feed(turnStart(7, ''))
    h.feed(text('background result'))
    h.feed(turnDone(7))
    h.feed(turnStart(8, 'm0')) // message_id 对不上，但 turn_id 就是答复里的那个
    h.feed(text('mine'))
    h.feed(turnDone(8))

    const events = await collect(stream)
    assert.deepEqual(types(events), ['user_message', 'turn_start', 'text_delta', 'turn_done'])
    assert.ok(events[1]?.type === 'turn_start' && events[1].turn_id === 8)
  })

  it('只有 agent 的 turn、没有我的 turn：流不会被它结束', async () => {
    const h = harness()
    const stream = h.session.send('hi', { messageId: 'm1' })
    h.reply({ queued: true })
    await stream.sent

    h.feed(userMessage('m1'))
    h.feed(turnStart(7, ''))
    h.feed(text('background result'))
    h.feed(turnDone(7))

    const pending = stream.next()
    const outcome = await Promise.race([
      pending.then(() => 'delivered'),
      sleep(60).then(() => 'idle'),
    ])
    assert.equal(outcome, 'idle', 'agent 自己的 turn 不应该产出事件，也不应该结束流')

    // 我的 turn 到了才开始转发，并以它的 turn_done 结束。
    h.feed(turnStart(8, 'm1'))
    h.feed(turnDone(8))
    const first = await pending
    assert.ok(!first.done && first.value.type === 'user_message')
    const rest = await collect(stream)
    assert.deepEqual(types(rest), ['turn_start', 'turn_done'])
  })

  it('agent 的 turn 排在我的 user_message 之前：同样不被认领', async () => {
    const h = harness()
    const stream = h.session.send('hi', { messageId: 'm1' })
    h.reply({ queued: true })
    await stream.sent

    h.feed(turnStart(7, ''))
    h.feed(turnDone(7))
    h.feed(userMessage('m1'))
    h.feed(turnStart(8, 'm1'))
    h.feed(turnDone(8))

    const events = await collect(stream)
    assert.deepEqual(types(events), ['user_message', 'turn_start', 'turn_done'])
  })
})

for (const impl of cryptoImpls) {
  describe(`session.changed 带 background_tasks（${impl.label}）`, () => {
    it('合并进会话摘要；数值回到 0 时清掉；旧 daemon 不发该字段时按 0 处理', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          assert.equal(session.summary.background_tasks ?? 0, 0)

          env.daemon.setBackgroundTasks(session.id, 2)
          await until(() => session.summary.background_tasks === 2, 3000, 'background_tasks = 2')

          env.daemon.setBackgroundTasks(session.id, 0)
          await until(() => session.summary.background_tasks === 0, 3000, 'background_tasks = 0')

          // 通知里没有这个字段（旧 daemon）：按 0 处理，不留着上一次的值
          env.daemon.setBackgroundTasks(session.id, 3)
          await until(() => session.summary.background_tasks === 3, 3000, 'background_tasks = 3')
          env.daemon.setBackgroundTasks(session.id, undefined)
          await until(() => session.summary.background_tasks === 0, 3000, 'background_tasks reset')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })
  })
}
