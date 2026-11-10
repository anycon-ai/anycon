// 重连之后的补历史（Session.resync）：断线期间发生的事件、补历史的往返期间到达的实时事件，
// 订阅者都按 seq 递增、每个只看到一次。hub 通道和本机通道各跑一遍。
import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { describe, it } from 'node:test'

import { connect, connectLocal, type Node, type Session } from '../src/index.js'
import { FakeDaemon } from './fake-daemon.js'
import { FAST, startEnv, until } from './harness.js'
import { mockCrypto } from './mock-crypto.js'

/** 一条通道：怎么让节点掉线、怎么让它回来。 */
interface Channel {
  node: Node
  daemon: FakeDaemon
  /** 返回时节点已经离线，并且保持离线直到 `comeBack()`（hub 通道靠客户端自己的重连定时器）。 */
  goOffline(): Promise<void>
  comeBack(): Promise<void>
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

async function withLocal(run: (channel: Channel) => Promise<void>): Promise<void> {
  const dir = await mkdtemp(join(tmpdir(), 'anycon-rs-'))
  const sock = join(dir, 'd.sock')
  const daemon = new FakeDaemon({
    hubUrl: 'http://127.0.0.1:1',
    crypto: mockCrypto,
    device: { id: 'node-local-rs', token: '-' },
  })
  await daemon.serveLocal(sock)
  const node = await connectLocal({
    socketPath: sock,
    reconnect: { initialMs: 15, maxMs: 60 },
    requestTimeoutMs: 4000,
  })
  try {
    await run({
      node,
      daemon,
      async goOffline() {
        await daemon.stopLocal()
        await until(() => !node.online, 3000, 'the node to go offline')
      },
      async comeBack() {
        await daemon.serveLocal(sock)
      },
    })
  } finally {
    await node.close()
    await daemon.stopLocal()
    await rm(dir, { recursive: true, force: true })
  }
}

describe('重连后补历史：本机通道', () => {
  for (const [offline, pageCap] of [
    [2, 0],
    [4, 2],
  ] as const) {
    it(`断线期间 ${offline} 个事件、补历史期间 1 个实时事件${pageCap ? `（一页 ${pageCap} 条）` : ''}`, () =>
      withLocal((channel) => scenario(channel, offline, pageCap)))
  }

  it('补历史失败：扣住的实时事件照样按序放出，没补上的留下空档，之后可以用 history() 取回', () =>
    withLocal(async (channel) => {
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
