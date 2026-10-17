// data 信封分片（ADR-30）：大消息在两个方向都被切成 ≤ 200 KiB 的 `data` 信封，hub 的信封上限不再丢消息；
// 传输失步（解密失败 / 乱序分片 / RESET）时直接重新 IK 握手；hub 退回（`backpressure`，粘住规则）时按升序原样重发；
// hub 的 `dropped`（每秒至多一条）不据此重发，是 error 事件 + 重新握手；`rate_limited` 只是暂停多久的提示。
// 发送方规则（保留缓冲、暂停、升序重发、多轮、缓冲外的 seq）的专项用例在 backpressure.test.ts。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { FRAME_FRAGMENT, MAX_FRAGMENT, encodeFrames, resetBody } from '@anycon/protocol'

import { AnyconError, NodeOffline, connect } from '../src/index.js'
import { FAST, cryptoImpls, startEnv, until } from './harness.js'

/** 一个 N 字节的 JSON 字符串载荷（`test.echo` 原样回显）。 */
const payload = (bytes: number) => ({ data: 'x'.repeat(bytes) })

for (const impl of cryptoImpls) {
  describe(`分片（${impl.label}）`, () => {
    it('1 MB 的请求和 1 MB 的回复都分片通过 hub（上限 256 KiB），内容完整', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          assert.ok(
            env.hub.maxEnvelopeBytes < 1024 * 1024,
            'the payload is bigger than one envelope',
          )
          const logBefore = env.hub.log.length
          const sent = payload(1024 * 1024)
          const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 15_000 })
          assert.equal(echoed.data.length, sent.data.length)
          assert.equal(echoed.data, sent.data)

          // 经过 hub 的 data 信封：每封都不超限，两个方向都是多片
          const data = env.hub.log.slice(logBefore).filter((e) => e.kind === 'data')
          const toDaemon = data.filter((e) => e.to === env.daemon.id)
          const toClient = data.filter((e) => e.to === paired.deviceId)
          assert.ok(toDaemon.length >= 5, `request fragments: ${toDaemon.length}`)
          assert.ok(toClient.length >= 5, `reply fragments: ${toClient.length}`)
          for (const e of data) assert.ok(e.size <= env.hub.maxEnvelopeBytes, `envelope ${e.size}`)
          assert.deepEqual(env.hub.dropped, [], 'the hub dropped nothing')
          // daemon 侧确实是分片收到的，没有任何一封超过片大小 + 头
          const received = env.daemon.dataIn.get(paired.deviceId)!
          assert.ok(received.maxBody <= MAX_FRAGMENT + 9)
          assert.equal(env.daemon.resetsSent, 0)
          assert.ok(env.daemon.dataOut.maxBody <= MAX_FRAGMENT + 9)

          // 小消息仍然是单个 WHOLE，不会因为分片改变
          const before = env.hub.log.length
          assert.equal((await node.info({ refresh: true })).name, 'fake-node')
          const small = env.hub.log.slice(before).filter((e) => e.kind === 'data')
          assert.equal(small.length, 2) // 一个请求 + 一个回复
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('一次连发多个大小不一的请求：msg_id 递增，各自重组，互不串', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          const sizes = [10, 300 * 1024, 5, 450 * 1024, 200 * 1024, 200 * 1024 + 1]
          const results = await Promise.all(
            sizes.map((n) =>
              node.rpc<{ data: string }>('test.echo', payload(n), { timeoutMs: 15_000 }),
            ),
          )
          assert.deepEqual(
            results.map((r) => r.data.length),
            sizes,
          )
          assert.equal(env.daemon.resetsSent, 0)
          assert.deepEqual(env.hub.dropped, [])
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('分片中间被背压（粘住规则）：按升序原样重发各一次，daemon 既没见到缺口也没见到重复，请求成功', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          const handshakes = env.daemon.handshakes.get(paired.deviceId)
          // 目标的发送缓冲在第 2 片满了：这一片被退回，之后每一片都被 hub 的粘住规则退回，
          // 直到被退回的那片按序重发成功——和真实 hub 一致。
          let seen = 0
          env.hub.interceptor = (e) => {
            if (e.kind !== 'data' || e.to !== env.daemon.id) return undefined
            seen += 1
            return seen === 2 ? 'backpressure' : undefined
          }
          const sent = payload(900 * 1024)
          const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 15_000 })
          assert.equal(echoed.data, sent.data)
          const bounced = env.hub.bounced.filter((b) => b.to === env.daemon.id).map((b) => b.seq)
          assert.ok(bounced.length >= 3, `bounced ${bounced.length} fragments`)
          assert.equal(env.daemon.resetsSent, 0)
          assert.equal(env.daemon.handshakes.get(paired.deviceId), handshakes, 'no re-handshake')
          // 重发的是同一个 seq（原样字节），不是新编码的信封；每片只转发了一次
          const resent = env.hub.log.filter(
            (e) => e.kind === 'data' && e.to === env.daemon.id && bounced.includes(e.seq),
          )
          assert.equal(resent.length, bounced.length, 'each bounced fragment was forwarded once')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('hub 限速（单独到达的 rate_limited）：只暂停发送，到点再发；刚发过 data 也不重新握手、不报错', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          const handshakes = env.daemon.handshakes.get(paired.deviceId) ?? 0
          const errors: Error[] = []
          node.on('error', (e) => errors.push(e))
          // 刚发过 data（不到 1 秒）：hub 不丢，`rate_limited` 只是暂停提示，不代表丢了一封、传输已失步
          await node.info({ refresh: true })
          env.hub.sendControl(paired.deviceId, { type: 'rate_limited', retry_after_ms: 400 })
          await new Promise((r) => setTimeout(r, 30)) // 让通知先到
          const before = env.daemon.requests.length
          const started = Date.now()
          const info = await node.info({ refresh: true })
          assert.equal(info.name, 'fake-node')
          assert.ok(Date.now() - started >= 300, `sent after the pause (${Date.now() - started}ms)`)
          assert.equal(env.daemon.requests.length, before + 1)
          assert.equal(env.daemon.handshakes.get(paired.deviceId), handshakes, 'no re-handshake')
          assert.equal(env.daemon.resetsSent, 0)
          assert.deepEqual(errors, [])
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('hub 的信封上限比一片还小（dropped too_large）：请求立刻失败而不是挂着，随后重新握手恢复', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          const handshakes = env.daemon.handshakes.get(paired.deviceId) ?? 0
          const errors: Error[] = []
          node.on('error', (e) => errors.push(e))
          env.hub.maxEnvelopeBytes = 64 * 1024
          await assert.rejects(
            node.rpc('test.echo', payload(300 * 1024), { timeoutMs: 15_000 }),
            (err: unknown) => {
              assert.ok(err instanceof AnyconError)
              assert.equal(err.code, 'envelope_too_large')
              return true
            },
          )
          assert.ok(env.hub.dropped.some((d) => d.reason === 'too_large'))
          // hub 每秒至多回一条 dropped：SDK 只看到一条，也只报一次 error，不逐片重发
          assert.equal(errors.length, 1)
          assert.equal((errors[0] as AnyconError).code, 'envelope_too_large')
          // 上限恢复后，通道重新握手，小请求照常
          env.hub.maxEnvelopeBytes = 256 * 1024
          await until(
            () => (env.daemon.handshakes.get(paired.deviceId) ?? 0) > handshakes && node.online,
            3000,
            're-handshake',
          )
          assert.equal((await node.info({ refresh: true })).name, 'fake-node')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('解密失败：丢弃传输、重新 IK 握手，进行中的请求以 NodeOffline 失败，之后恢复', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          const handshakes = env.daemon.handshakes.get(paired.deviceId) ?? 0
          const offline: string[] = []
          node.on('offline', () => offline.push('offline'))
          env.daemon.sendGarbage(paired.deviceId)
          await until(
            () =>
              (env.daemon.handshakes.get(paired.deviceId) ?? 0) === handshakes + 1 && node.online,
            3000,
            're-handshake after decrypt failure',
          )
          assert.deepEqual(offline, ['offline'])
          assert.equal((await node.info({ refresh: true })).name, 'fake-node')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('分片乱序（从 index 1 开始）：当作传输失步，重新握手', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          const handshakes = env.daemon.handshakes.get(paired.deviceId) ?? 0
          const [, second] = encodeFrames(new Uint8Array(MAX_FRAGMENT + 100).fill(3), 1)
          assert.equal(second![0], FRAME_FRAGMENT)
          env.daemon.sendRawData(paired.deviceId, second!)
          await until(
            () =>
              (env.daemon.handshakes.get(paired.deviceId) ?? 0) === handshakes + 1 && node.online,
            3000,
            're-handshake after out-of-order fragment',
          )
          assert.equal((await node.info({ refresh: true })).name, 'fake-node')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('daemon 发 RESET：重新握手；未知 flags 的帧同样当作失步', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          let handshakes = env.daemon.handshakes.get(paired.deviceId) ?? 0
          env.daemon.sendRawData(paired.deviceId, resetBody())
          await until(
            () =>
              (env.daemon.handshakes.get(paired.deviceId) ?? 0) === handshakes + 1 && node.online,
            3000,
            're-handshake after RESET',
          )
          assert.equal((await node.info({ refresh: true })).name, 'fake-node')

          handshakes = env.daemon.handshakes.get(paired.deviceId) ?? 0
          env.daemon.sendRawData(paired.deviceId, Uint8Array.of(0x7f, 1, 2, 3))
          await until(
            () =>
              (env.daemon.handshakes.get(paired.deviceId) ?? 0) === handshakes + 1 && node.online,
            3000,
            're-handshake after unknown flags',
          )
          assert.equal((await node.info({ refresh: true })).name, 'fake-node')
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('重组到一半断线：旧分片不会混进新传输的消息里', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        const paired = await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        try {
          await node.info()
          // 先塞一片「半条消息」，再让 hub 掉线重连并重新握手
          const [first] = encodeFrames(new Uint8Array(MAX_FRAGMENT + 100).fill(5), 9)
          env.daemon.sendRawData(paired.deviceId, first!)
          env.hub.dropConnection(paired.deviceId)
          await until(() => !node.online, 4000, 'offline')
          await until(() => node.online, 4000, 'back online')
          const sent = payload(400 * 1024)
          const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 15_000 })
          assert.equal(echoed.data, sent.data)
        } finally {
          await node.close()
        }
      } finally {
        await env.stop()
      }
    })

    it('close() 之后再请求仍然以 NodeOffline 失败（分片路径也不遗留）', async () => {
      const crypto = await impl.load()
      const env = await startEnv(crypto)
      try {
        await env.pair()
        const node = await connect(env.url, { storage: env.storage, crypto, ...FAST })
        await node.close()
        await assert.rejects(node.rpc('test.echo', payload(300 * 1024)), NodeOffline)
      } finally {
        await env.stop()
      }
    })
  })
}
