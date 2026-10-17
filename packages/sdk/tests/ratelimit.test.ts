// `rate_limited { retry_after_ms }` 是整条 hub 连接（所有节点、含重发）的暂停提示（docs/protocol.md § 3.1）：
// 记在 `HubConnection.rateUntil` 上，各节点共用；封顶 30 s（RATE_WAIT_MAX_MS）；换一条 WebSocket 就清零。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { Client, pair } from '../src/index.js'
import { buildClientInfo } from '../src/client.js'
import { HubConnection, RATE_WAIT_MAX_MS } from '../src/hub.js'
import { FakeDaemon } from './fake-daemon.js'
import { FakeHub } from './fake-hub.js'
import { FAST, cryptoImpls, sleep, startEnv, until } from './harness.js'

describe('HubConnection.rateUntil', () => {
  it('取到的等待时间封顶 30 s；迟到的较短提示不会缩短已有的暂停；重连清零', async () => {
    const hub = await FakeHub.start()
    const device = hub.registerDevice('sdk')
    const conn = new HubConnection({
      url: hub.url.replace('http', 'ws') + '/v1/ws',
      token: device.token,
      deviceId: device.id,
      client: buildClientInfo(),
      reconnect: { initialMs: 15, maxMs: 60 },
    })
    try {
      await conn.start()
      assert.equal(conn.rateUntil, 0)

      // 荒谬的值（1 小时）按 30 s 算
      const t0 = Date.now()
      hub.sendControl(device.id, { type: 'rate_limited', retry_after_ms: 3_600_000 })
      await until(() => conn.rateUntil > 0, 2000, 'rate_limited to arrive')
      assert.ok(
        conn.rateUntil >= t0 + RATE_WAIT_MAX_MS,
        `rateUntil ${conn.rateUntil - t0} ms ahead`,
      )
      assert.ok(
        conn.rateUntil <= Date.now() + RATE_WAIT_MAX_MS + 100,
        `rateUntil ${conn.rateUntil - t0} ms ahead`,
      )

      // 较短的提示不缩短
      const long = conn.rateUntil
      hub.sendControl(device.id, { type: 'rate_limited', retry_after_ms: 200 })
      await sleep(60)
      assert.equal(conn.rateUntil, long)

      // 掉线重连之后是一条新连接，预算重新开始
      const connected = new Promise<void>((resolve) => conn.once('authed', () => resolve()))
      hub.dropConnection(device.id)
      await connected
      assert.equal(conn.rateUntil, 0)
    } finally {
      conn.close()
      await hub.stop()
    }
  })
})

for (const impl of cryptoImpls) {
  describe(`rate_limited 是整条 hub 连接共享的（${impl.label}）`, () => {
    it('之后才连上的节点也要等；hub 重连后一起恢复', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      let daemon2: FakeDaemon | undefined
      try {
        await env.pair()
        const device2 = env.hub.registerDevice('daemon')
        daemon2 = new FakeDaemon({
          hubUrl: env.url,
          crypto,
          device: device2,
          name: 'second-node',
        })
        await daemon2.start()
        const code = await daemon2.beginPairing()
        await pair(env.url, { code, name: 'ci-runner', storage: env.storage, crypto })

        const client = await Client.open(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const a = await client.node('fake-node')
          // hub 说这条连接要停 700 ms。此刻 b 的通道还不存在：它不是「收到通知的那个节点」，也得等。
          env.hub.sendControl(client.deviceId, { type: 'rate_limited', retry_after_ms: 700 })
          await sleep(40) // 让通知先到
          const started = Date.now()
          const b = await client.node('second-node') // 握手之后的 node.info() 要等到暂停到点
          const waited = Date.now() - started
          assert.ok(waited >= 550, `second node's first request went out after ${waited} ms`)
          assert.equal((await b.info()).node_id, device2.id)

          // 已有的节点同样在等
          env.hub.sendControl(client.deviceId, { type: 'rate_limited', retry_after_ms: 500 })
          await sleep(40)
          const t1 = Date.now()
          assert.equal((await a.info({ refresh: true })).name, 'fake-node')
          assert.ok(Date.now() - t1 >= 400, `first node waited ${Date.now() - t1} ms`)

          // 长暂停之后掉线重连：新连接的预算重新开始，不需要等
          env.hub.sendControl(client.deviceId, { type: 'rate_limited', retry_after_ms: 20_000 })
          await sleep(40)
          env.hub.dropConnection(client.deviceId)
          await until(() => !a.online && !b.online, 2000, 'both nodes to notice the drop')
          await until(() => a.online && b.online, 4000, 'both nodes back online')
          const t2 = Date.now()
          assert.equal((await a.info({ refresh: true })).name, 'fake-node')
          assert.equal((await b.info({ refresh: true })).node_id, device2.id)
          assert.ok(Date.now() - t2 < 2000, `waited ${Date.now() - t2} ms after reconnecting`)
        } finally {
          await client.close()
        }
      } finally {
        daemon2?.stop()
        await env.stop()
      }
    })
  })
}
