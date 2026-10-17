// 连接层：节点离线 / 上线、hub 掉线重连、daemon 重启重新握手、握手被拒、背压重发、undeliverable。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import {
  Client,
  NodeOffline,
  NodeRejected,
  PairingRequired,
  connect,
  type Node,
} from '../src/index.js'
import { FakeDaemon } from './fake-daemon.js'
import { FAST, collect, cryptoImpls, sleep, startEnv, until } from './harness.js'

/** 在 `node` 上等到 `online` 变成 `want`。 */
async function untilOnline(node: Node, want: boolean, ms = 3000): Promise<void> {
  await until(() => node.online === want, ms, `node.online === ${want}`)
}

for (const impl of cryptoImpls) {
  describe(`连接管理（${impl.label}）`, () => {
    it('节点不在线：connect 成功但 online=false，调用抛 NodeOffline，上线后 waitOnline 返回', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        env.daemon.stop()
        await until(() => !env.hub.isOnline(env.daemon.id), 2000, 'daemon to leave the hub')

        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          assert.equal(node.online, false)
          assert.equal(node.state, 'peer_offline')
          await assert.rejects(node.info(), NodeOffline)
          await assert.rejects(node.waitOnline(60), NodeOffline)

          // daemon 回来（同一身份、同一公钥、认识这台设备）
          const back = new FakeDaemon({
            hubUrl: env.url,
            crypto,
            device: {
              id: env.daemon.id,
              token: [...env.hub.devices.values()].find((d) => d.id === env.daemon.id)!.token,
            },
            keypair: env.daemon.keypair,
            peers: [...env.daemon.peers.values()],
          })
          await back.start()
          try {
            await node.waitOnline(3000)
            assert.equal(node.online, true)
            assert.equal((await node.info()).name, 'fake-node')
          } finally {
            back.stop()
          }
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('hub 连接掉线：自动重连并重新握手，之后调用恢复；未完成的请求以 NodeOffline 失败', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          const offlineEvents: string[] = []
          node.on('offline', () => offlineEvents.push('offline'))
          node.on('online', () => offlineEvents.push('online'))
          const handshakesBefore = env.daemon.handshakes.get(paired.deviceId) ?? 0

          env.hub.dropConnection(paired.deviceId)
          await until(() => offlineEvents.includes('online'), 4000, 'node to come back online')
          assert.deepEqual(offlineEvents, ['offline', 'online'])
          assert.equal(env.daemon.handshakes.get(paired.deviceId), handshakesBefore + 1)
          assert.equal((await node.info({ refresh: true })).name, 'fake-node')
          assert.equal(env.hub.authCount.get(paired.deviceId), 3) // pair 里取节点名 + connect + 重连
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('掉线重连后，进行中的 send() 迭代用 history 补齐事件并走完', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const session = await node.sessions.create({ agent: 'mock' })
          const stream = session.send('slow resume me')
          const seen: string[] = []
          let dropped = false
          for await (const ev of stream) {
            seen.push(ev.type)
            if (ev.type === 'thinking_delta' && !dropped) {
              dropped = true
              env.hub.dropConnection(paired.deviceId) // 掉线时 turn 还在跑
            }
          }
          assert.equal(seen.at(-1), 'turn_done')
          assert.equal(seen.filter((t) => t === 'turn_done').length, 1)
          // 没有重复、没有缺失：seq 连续
          const history = (await session.history()).events.map((e) => e.event.type)
          assert.deepEqual(seen.slice(0, 3), ['user_message', 'turn_start', 'thinking_delta'])
          assert.equal(history.at(-1), 'turn_done')
          assert.equal(seen.length, history.length)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('daemon 重启（peer_offline → peer_online）：重新握手，旧 Transport 丢弃', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          env.daemon.dropConnection()
          await untilOnline(node, false)
          await assert.rejects(node.info({ refresh: true }), NodeOffline)

          const device = [...env.hub.devices.values()].find((d) => d.id === env.daemon.id)!
          const restarted = new FakeDaemon({
            hubUrl: env.url,
            crypto,
            device: { id: device.id, token: device.token },
            keypair: env.daemon.keypair,
            peers: [...env.daemon.peers.values()],
          })
          await restarted.start()
          try {
            await untilOnline(node, true)
            assert.equal(restarted.handshakes.get(paired.deviceId), 1)
            assert.equal((await node.info({ refresh: true })).node_id, device.id)
          } finally {
            restarted.stop()
          }
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('daemon 不认识这台设备的公钥：握手无应答，connect 抛 NodeRejected', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        env.daemon.peers.clear() // daemon 重装 / 吊销了这台设备
        await assert.rejects(
          connect(env.url, {
            storage: env.storage,
            crypto,
            ...FAST,
            handshakeTimeoutMs: 80,
            handshakeAttempts: 2,
          }),
          NodeRejected,
        )
      } finally {
        await env.stop()
      }
    })

    it('hub 吊销设备：auth_fail revoked → NodeRejected 并清掉本地凭据，之后 connect 抛 PairingRequired', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        env.hub.revoke(paired.deviceId)
        await assert.rejects(
          connect(env.url, { storage: env.storage, crypto, ...FAST }),
          NodeRejected,
        )
        assert.equal(await env.storage.get('identity'), undefined) // 作废的 token 不再留着
        await assert.rejects(
          connect(env.url, { storage: env.storage, crypto, ...FAST }),
          PairingRequired,
        )
      } finally {
        await env.stop()
      }
    })

    it('已连接时被吊销（close 4004）：Client 收到 error 并清掉凭据，不再重连', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const client = await Client.open(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const errors: Error[] = []
          client.on('error', (e) => errors.push(e))
          env.hub.revoke(paired.deviceId)
          await until(() => errors.length === 1, 2000, 'revoked error')
          assert.ok(errors[0] instanceof NodeRejected)
          await until(
            async () => (await env.storage.get('identity')) === undefined,
            2000,
            'identity cleared',
          )
          await assert.rejects(client.node(), NodeRejected)
        } finally {
          await client.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('token 无效（hub 不认）：PairingRequired，本地凭据保留', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const stored = JSON.parse((await env.storage.get('identity'))!) as { deviceToken: string }
        stored.deviceToken = 'tok_wrong'
        await env.storage.set('identity', JSON.stringify(stored))
        await assert.rejects(
          connect(env.url, { storage: env.storage, crypto, ...FAST }),
          PairingRequired,
        )
        assert.ok(await env.storage.get('identity'))
      } finally {
        await env.stop()
      }
    })

    it('同一设备的第二条连接把第一条顶掉（4001）：第一个 client 收到 error，不再重连', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const first = await Client.open(env.url, { storage: env.storage, crypto, ...FAST })
        const errors: Error[] = []
        first.on('error', (e) => errors.push(e))
        const second = await Client.open(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await until(() => errors.length === 1, 2000, 'superseded error')
          assert.match(errors[0]!.message, /4001|took over/)
          await sleep(150) // 没有重连风暴
          assert.equal(env.hub.authCount.get(paired.deviceId), 3) // pair 里取节点名 + first + second
          assert.ok(second.connected)
          await assert.rejects(first.node(), /took over/)
        } finally {
          await first.close()
          await second.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('背压：hub 回 backpressure 后 SDK 按原序重发，请求最终成功', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          let hit = 0
          env.hub.interceptor = (e) => {
            // 目标的发送缓冲满了：这一小段时间里发给它的信封都被拒（和真实 hub 一样，每封各回一条 backpressure）。
            if (e.kind === 'data' && e.to === env.daemon.id && hit++ < 2) return 'backpressure'
            return undefined
          }
          const handshakes = env.daemon.handshakes.get(paired.deviceId)
          const started = Date.now()
          // 两个请求连发：都被背压，重发时必须按原序、各一次（Noise 计数器要求按序，重复的一封对端解不开）
          const [a, b] = await Promise.all([
            node.info({ refresh: true }),
            node.agents.list({ refresh: true }),
          ])
          assert.equal(a.name, 'fake-node')
          assert.equal(b[0]?.id, 'mock')
          assert.ok(Date.now() - started >= 100, 'retry waited a bit')
          assert.equal(env.daemon.resetsSent, 0, 'the daemon never saw a gap or a duplicate')
          assert.equal(env.daemon.handshakes.get(paired.deviceId), handshakes, 'no re-handshake')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('undeliverable：请求立刻以 NodeOffline 失败', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          env.hub.interceptor = (e) => (e.kind === 'data' ? 'drop' : undefined) // 请求挂起
          const pending = node.agents.list({ refresh: true })
          const settled = assert.rejects(pending, NodeOffline)
          await sleep(30)
          env.hub.sendControl(paired.deviceId, { type: 'undeliverable', to: env.daemon.id, seq: 1 })
          await settled
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('一个 Client 同时连两个节点；connect() 在多节点时要求指定', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        // 第二个节点
        const device2 = env.hub.registerDevice('daemon')
        const daemon2 = new FakeDaemon({
          hubUrl: env.url,
          crypto,
          device: device2,
          name: 'second-node',
        })
        await daemon2.start()
        try {
          const code = await daemon2.beginPairing()
          const { pair } = await import('../src/index.js')
          await pair(env.url, { code, name: 'ci-runner', storage: env.storage, crypto })

          await assert.rejects(
            connect(env.url, { storage: env.storage, crypto, ...FAST }),
            /several nodes are paired/,
          )
          await assert.rejects(
            connect(env.url, { storage: env.storage, crypto, node: 'nope', ...FAST }),
            /no paired node matches/,
          )

          const client = await Client.open(env.url, { storage: env.storage, crypto, ...FAST })
          try {
            const a = await client.node('fake-node')
            const b = await client.node('second-node')
            assert.notEqual(a, b)
            assert.equal(await client.node('fake-node'), a)
            assert.equal((await a.info()).node_id, env.daemon.id)
            assert.equal((await b.info()).node_id, device2.id)
            const viaPrefix = await client.node(device2.id.slice(0, 6))
            assert.equal(viaPrefix, b)
          } finally {
            await client.close()
          }
        } finally {
          daemon2.stop()
        }
      } finally {
        await env.stop()
      }
    })

    it('close() 之后事件流和请求都立刻失败，不遗留定时器', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        const session = await node.sessions.create({ agent: 'mock' })
        const stream = session.send('slow close me')
        await stream.next()
        await node.close()
        await assert.rejects(collect(stream), NodeOffline)
        await assert.rejects(node.info({ refresh: true }), NodeOffline)
      } finally {
        await env.stop()
      }
    })
  })
}
