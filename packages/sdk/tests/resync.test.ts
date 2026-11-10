// 重连之后的补历史（Session.resync）：断线期间发生的事件、补历史的往返期间到达的实时事件，
// 订阅者都按 seq 递增、每个只看到一次。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { connect, type Node, type Session } from '../src/index.js'
import type { FakeDaemon } from './fake-daemon.js'
import { FAST, sleep, startEnv, until } from './harness.js'
import { mockCrypto } from './mock-crypto.js'

/** 一条通道：怎么让节点掉线、怎么让它回来。 */
interface Channel {
  node: Node
  daemon: FakeDaemon
  /** 返回时节点已经离线，并且保持离线直到 `comeBack()`（hub 通道靠客户端自己的重连定时器）。 */
  goOffline(): Promise<void>
  comeBack(): Promise<void>
  /** 立刻掐断连接（daemon 仍然可以重新连上）：客户端会自己重连。 */
  dropLink(): void
}

const historyRequests = (daemon: FakeDaemon): number =>
  daemon.requests.filter((r) => r.method === 'session.history').length

async function open(channel: Channel): Promise<{ session: Session; seqs: number[] }> {
  const session = await channel.node.sessions.create({ agent: 'mock' })
  const seqs: number[] = []
  const iterator = session.events()
  void (async () => {
    for await (const ev of iterator) seqs.push(ev.seq)
  })().catch(() => undefined)
  await until(
    () => channel.daemon.requests.some((r) => r.method === 'session.watch'),
    3000,
    'the first watch',
  )
  return { session, seqs }
}

const say = (daemon: FakeDaemon, session: Session): number =>
  daemon.emitEvent(session.id, { type: 'text_delta', text: 'x' })

/**
 * 三个实时事件 → 掉线 → 离线期间发生 `offline` 个事件 → 重连，补历史的往返里（快照之后）又来一个实时事件。
 * 补历史的答复晚 300 ms 才到，实时事件一定先到客户端。
 */
async function scenario(channel: Channel, offline: number, pageCap: number): Promise<void> {
  const { daemon } = channel
  const { session, seqs } = await open(channel)
  const expected = [say(daemon, session), say(daemon, session), say(daemon, session)]
  await until(() => seqs.length === 3, 3000, 'the live events')

  await channel.goOffline()
  for (let i = 0; i < offline; i++) expected.push(say(daemon, session))

  daemon.historyLateMs = 300
  daemon.historyPageCap = pageCap
  const before = historyRequests(daemon)
  await channel.comeBack()
  await until(() => historyRequests(daemon) > before, 5000, 'the history request')
  expected.push(say(daemon, session)) // 快照之后、答复之前：作为实时事件先到

  await until(() => seqs.length === expected.length, 5000, 'every event')
  assert.deepEqual(seqs, expected, 'every seq once, in order')
  await new Promise((r) => setTimeout(r, 100))
  assert.deepEqual(seqs, expected, 'nothing arrives twice afterwards')
}

/**
 * 补历史的往返里连接又断了（`flaps` 次）：每次在 daemon 取好快照之后，先发生一个实时事件（客户端会扣住它），再掐断连接。
 * 补历史因此以 `ConnectionError` / `NodeOffline` 失败；扣住的事件必须丢掉（不能让 `lastSeq` 越过没补到的历史），
 * 下一次重连的补历史把它们连同断线期间错过的一起补回来。最终每个 seq 一次、按序。
 */
async function flapScenario(channel: Channel, flaps: number): Promise<void> {
  const { daemon } = channel
  const { session, seqs } = await open(channel)
  const expected = [say(daemon, session), say(daemon, session), say(daemon, session)]
  await until(() => seqs.length === 3, 3000, 'the live events')

  await channel.goOffline()
  expected.push(say(daemon, session)) // 断线期间

  const before = historyRequests(daemon)
  daemon.historyHook = async (call) => {
    if (call > flaps) return
    expected.push(say(daemon, session)) // 补历史的往返里到达的实时事件（被扣住）
    await sleep(60)
    channel.dropLink()
  }
  await channel.comeBack()
  await until(() => historyRequests(daemon) >= before + flaps + 1, 15_000, 'the final history')
  await until(() => channel.node.online, 15_000, 'the node to be online again')
  expected.push(say(daemon, session))

  await until(() => seqs.length === expected.length, 5000, 'every event')
  assert.deepEqual(seqs, expected, 'every seq once, in order, nothing lost')
  await sleep(100)
  assert.deepEqual(seqs, expected, 'nothing arrives twice afterwards')
}

function hubChannel(
  env: Awaited<ReturnType<typeof startEnv>>,
  node: Node,
  deviceId: string,
): Channel {
  return {
    node,
    daemon: env.daemon,
    async goOffline() {
      env.hub.dropConnection(deviceId)
      await until(() => !node.online, 3000, 'the node to go offline')
    },
    // 客户端的重连定时器（下面的 reconnect 选项）自己会连回来
    async comeBack() {},
    dropLink() {
      env.hub.dropConnection(deviceId)
    },
  }
}

/** 起一套 hub + 假 daemon，配对一台设备，用给定的重连间隔连上，把 hub 通道交给 `run`。 */
async function withHub(reconnectMs: number, run: (channel: Channel) => Promise<void>): Promise<void> {
  const env = await startEnv(mockCrypto)
  try {
    const paired = await env.pair()
    const node = await connect(env.url, {
      storage: env.storage,
      crypto: mockCrypto,
      ...FAST,
      reconnect: { initialMs: reconnectMs, maxMs: reconnectMs },
    })
    try {
      await run(hubChannel(env, node, paired.deviceId))
    } finally {
      await node.close()
    }
  } finally {
    await env.stop()
  }
}

describe('重连后补历史：hub 通道', () => {
  for (const [offline, pageCap] of [
    [2, 0],
    [4, 2],
  ] as const) {
    it(`断线期间 ${offline} 个事件、补历史期间 1 个实时事件${pageCap ? `（一页 ${pageCap} 条）` : ''}`, async () => {
      const env = await startEnv(mockCrypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, {
          storage: env.storage,
          crypto: mockCrypto,
          ...FAST,
          // 离线窗口要够测试在重连之前发出断线期间的事件
          reconnect: { initialMs: 500, maxMs: 500 },
        })
        try {
          await scenario(hubChannel(env, node, paired.deviceId), offline, pageCap)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })
  }
})

describe('补历史的往返里连接又断了：hub 通道', () => {
  for (const flaps of [1, 2]) {
    it(`断 ${flaps} 次，最终每个 seq 一次、按序`, async () => {
      const env = await startEnv(mockCrypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, {
          storage: env.storage,
          crypto: mockCrypto,
          ...FAST,
          reconnect: { initialMs: 300, maxMs: 300 },
        })
        try {
          await flapScenario(hubChannel(env, node, paired.deviceId), flaps)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })
  }
})

describe('补历史失败而连接还在：hub 通道', () => {
  it('补历史失败：扣住的实时事件照样按序放出，没补上的留下空档，之后可以用 history() 取回', () =>
    withHub(500, async (channel) => {
      const { daemon, node } = channel
      const { session, seqs } = await open(channel)
      say(daemon, session)
      say(daemon, session)
      say(daemon, session)
      await until(() => seqs.length === 3, 3000, 'the live events')
      await channel.goOffline()
      const missed = say(daemon, session)

      daemon.historyLateMs = 200
      daemon.historyFail = true
      const before = historyRequests(daemon)
      await channel.comeBack()
      await until(() => historyRequests(daemon) > before, 5000, 'the history request')
      const live = say(daemon, session)
      await until(() => seqs.includes(live), 5000, 'the held live event')
      assert.deepEqual(seqs, [1, 2, 3, live], 'in order, once each; the missed one is a gap')
      assert.equal(node.online, true, 'a failed resync does not take the node down')

      daemon.historyFail = false
      daemon.historyLateMs = 0
      const page = await session.history({ afterSeq: 3 })
      assert.deepEqual(
        page.events.map((e) => e.seq),
        [missed, live],
      )
    }))
})
