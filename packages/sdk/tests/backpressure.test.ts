// 发送方规则（docs/protocol.md § 3.1 粘住规则）：fake hub 按真实 hub 的约定「转发或退回，从不静默丢」，
// 并在某个 seq 被退回后一律退回同一 `(from, to)` 的后续信封，直到那个 seq 的重发成功。SDK 要：
// 保留已发信封的原文；收到 `backpressure` 后暂停该节点、等 `retry_after_ms`（`rate_limited` 给的；没有就
// 100–300 ms + 抖动）、把所有被退回的 seq 按升序原样重发、再继续；保留缓冲超过上限时暂停发送（当作在途窗口），
// 而不是逐出还在路上的；被退回的 seq 不在缓冲里（hub 报了没发过的 seq）→ 重新握手；被退回的 handshake 按
// `retry_after_ms`（没有就 300–1000 ms + 抖动、翻倍）延后再握手。
// 接收方（fake daemon，Noise responder）严格按序：任何乱序 / 重复都会触发 RESET 或解密失败，所以请求能成功
// 且 `resetsSent == 0`，就说明顺序没乱。
import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import { connect, type Node } from '../src/index.js'
import type { FakeHub } from './fake-hub.js'
import { FAST, cryptoImpls, sleep, startEnv, until, type Env } from './harness.js'

/** 一个 N 字节的 JSON 字符串载荷（`test.echo` 原样回显）。 */
const payload = (bytes: number) => ({ data: 'x'.repeat(bytes) })

/** 到达 hub、发往 daemon 的 data 信封的 seq，按到达顺序（含重发）。 */
const arrivedAtHub = (env: Env) =>
  env.hub.arrivals.filter((a) => a.to === env.daemon.id && a.kind === 'data')

/** hub 转发给 daemon 的 data 信封的 seq，按转发顺序。 */
const forwardedToDaemon = (env: Env) =>
  env.hub.log.filter((e) => e.to === env.daemon.id && e.kind === 'data').map((e) => e.seq)

const isAscending = (xs: readonly number[]) => xs.every((x, i) => i === 0 || xs[i - 1]! < x)

/** 以「第 N 个发往 daemon 的 data 信封的目标缓冲满了」为条件退回一次（之后由粘住规则接管）。 */
function bounceNth(
  hub: FakeHub,
  daemonId: string,
  nth: number,
  verdict: 'backpressure' | 'rate_limited' = 'backpressure',
) {
  let seen = 0
  hub.interceptor = (e) => {
    if (e.kind !== 'data' || e.to !== daemonId) return undefined
    seen += 1
    return seen === nth ? verdict : undefined
  }
}

async function withNode(
  impl: (typeof cryptoImpls)[number],
  body: (ctx: { env: Env; node: Node; deviceId: string }) => Promise<void>,
  options: { resendBufferBytes?: number; resendRetainMs?: number } = {},
): Promise<void> {
  const crypto = await impl.load()
  const env = await startEnv(crypto)
  try {
    const paired = await env.pair()
    const node = await connect(env.url, { storage: env.storage, crypto, ...FAST, ...options })
    try {
      await node.info()
      await body({ env, node, deviceId: paired.deviceId })
    } finally {
      await node.close()
    }
  } finally {
    await env.stop()
  }
}

for (const impl of cryptoImpls) {
  describe(`发送方规则：背压与重发（${impl.label}）`, () => {
    it('退回第 N 片、之后全部被粘住退回：暂停后按升序原样重发，daemon 按序收齐，请求成功', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const handshakes = env.daemon.handshakes.get(deviceId)
        const before = arrivedAtHub(env).length
        bounceNth(env.hub, env.daemon.id, 2)
        const sent = payload(1200 * 1024)
        const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 15_000 })
        assert.equal(echoed.data, sent.data)

        const arrivals = arrivedAtHub(env).slice(before)
        const originals = arrivals.filter(
          (a, i) => arrivals.findIndex((b) => b.seq === a.seq) === i,
        )
        assert.ok(originals.length >= 6, `fragments ${originals.length}`)
        // 第 1 片被转发，第 2 片起全部被退回（第 2 片是缓冲满，其余是粘住）
        const bounced = env.hub.bounced.filter((b) => b.to === env.daemon.id).map((b) => b.seq)
        assert.deepEqual(
          bounced,
          originals.slice(1).map((a) => a.seq),
          'everything after the first fragment bounced',
        )
        // 先是原来的各片，然后恰好是被退回的 seq，按升序，各一次，原样（同一个 seq）
        assert.deepEqual(
          arrivals.map((a) => a.seq),
          [...originals.map((a) => a.seq), ...bounced],
        )
        assert.ok(isAscending(bounced))
        // 暂停了一会儿再重发：不早于第一条退回之后 ~100 ms
        const resentAt = arrivals[originals.length]!.at
        const firstBounceAt = env.hub.bounced[0]!.at
        assert.ok(resentAt - firstBounceAt >= 90, `waited ${resentAt - firstBounceAt} ms`)
        // hub 转发给 daemon 的顺序严格升序、每片一次（没有重复、没有缺口）
        const forwarded = forwardedToDaemon(env).slice(-originals.length)
        assert.deepEqual(
          forwarded,
          originals.map((a) => a.seq),
        )
        assert.equal(env.daemon.resetsSent, 0)
        assert.equal(env.daemon.handshakes.get(deviceId), handshakes, 'no re-handshake')
        assert.equal((await node.info({ refresh: true })).name, 'fake-node')
      })
    })

    it('重发时缓冲还是满的：再退回一轮、再等、再按升序重发（连续被退回时退避），最终按序到达', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const handshakes = env.daemon.handshakes.get(deviceId)
        // 第 2 片：第一次到达被退回，第一次重发还是被退回，第二次重发才收下
        let seen = 0
        let headSeq: number | undefined
        let headAttempts = 0
        env.hub.interceptor = (e) => {
          if (e.kind !== 'data' || e.to !== env.daemon.id) return undefined
          seen += 1
          if (seen === 2) headSeq = e.seq
          if (e.seq === headSeq) {
            headAttempts += 1
            return headAttempts <= 2 ? 'backpressure' : undefined
          }
          return undefined
        }
        const before = arrivedAtHub(env).length
        const sent = payload(900 * 1024)
        const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 20_000 })
        assert.equal(echoed.data, sent.data)

        const arrivals = arrivedAtHub(env).slice(before)
        const head = arrivals.filter((a) => a.seq === headSeq)
        assert.equal(head.length, 3, 'the head fragment arrived at the hub three times')
        // 每一轮之间都等了（后一轮的退避不短于第一轮的下限）
        assert.ok(head[1]!.at - head[0]!.at >= 90)
        assert.ok(head[2]!.at - head[1]!.at >= 180, `second wait ${head[2]!.at - head[1]!.at} ms`)
        // 每一轮重发内部都按升序
        const rounds: number[][] = [[]]
        for (const a of arrivals) {
          const cur = rounds[rounds.length - 1]!
          if (cur.length > 0 && a.seq < cur[cur.length - 1]!) rounds.push([a.seq])
          else cur.push(a.seq)
        }
        for (const r of rounds) assert.ok(isAscending(r), `round ${r.join(',')}`)
        assert.ok(isAscending(forwardedToDaemon(env).slice(-5)))
        assert.equal(env.daemon.resetsSent, 0)
        assert.equal(env.daemon.handshakes.get(deviceId), handshakes, 'no re-handshake')
      })
    })

    it('暂停期间的新请求排在重发之后：不插队，顺序不乱', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const handshakes = env.daemon.handshakes.get(deviceId)
        const before = arrivedAtHub(env).length
        bounceNth(env.hub, env.daemon.id, 2)
        const big = node.rpc<{ data: string }>('test.echo', payload(900 * 1024), {
          timeoutMs: 15_000,
        })
        await until(() => env.hub.bounced.length >= 3, 3000, 'bounces')
        await sleep(20) // 还在 100–300 ms 的暂停里
        const small = node.rpc<{ data: string }>(
          'test.echo',
          { data: 'late' },
          { timeoutMs: 15_000 },
        )
        assert.equal((await small).data, 'late')
        assert.equal((await big).data.length, 900 * 1024)

        const arrivals = arrivedAtHub(env).slice(before)
        const seqs = arrivals.map((a) => a.seq)
        const lateSeq = Math.max(...seqs)
        // 晚到的小请求只出现一次，且在所有重发之后
        assert.equal(seqs.filter((s) => s === lateSeq).length, 1)
        assert.equal(seqs[seqs.length - 1], lateSeq)
        const bouncedCount = env.hub.bounced.filter((b) => b.to === env.daemon.id).length
        const resends = seqs.slice(seqs.length - 1 - bouncedCount, seqs.length - 1)
        assert.ok(isAscending(resends), `resends ${resends.join(',')}`)
        assert.ok(isAscending(forwardedToDaemon(env).slice(-6)))
        assert.equal(env.daemon.resetsSent, 0)
        assert.equal(env.daemon.handshakes.get(deviceId), handshakes, 'no re-handshake')
      })
    })

    it('hub 在退回的同时给出 rate_limited：按 retry_after_ms 暂停再重发，不失败请求、不重新握手、不报错', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const handshakes = env.daemon.handshakes.get(deviceId)
        const errors: Error[] = []
        node.on('error', (e) => errors.push(e))
        env.hub.retryAfterMs = 450
        env.hub.receiptIntervalMs = 0
        bounceNth(env.hub, env.daemon.id, 2, 'rate_limited')
        const before = arrivedAtHub(env).length
        const sent = payload(900 * 1024)
        const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 15_000 })
        assert.equal(echoed.data, sent.data)

        const arrivals = arrivedAtHub(env).slice(before)
        const firstBounceAt = env.hub.bounced[0]!.at
        const resend = arrivals.find(
          (a) =>
            a.at > firstBounceAt && env.hub.bounced.some((b) => b.seq === a.seq && b.at < a.at),
        )
        assert.ok(resend, 'a bounced fragment was resent')
        assert.ok(resend.at - firstBounceAt >= 400, `resent after ${resend.at - firstBounceAt} ms`)
        assert.deepEqual(errors, [], 'rate_limited is a pause hint, not an error')
        assert.equal(env.daemon.resetsSent, 0)
        assert.equal(env.daemon.handshakes.get(deviceId), handshakes, 'no re-handshake')
      })
    })

    it('rate_limited 比 backpressure 晚到：重发等到 rate_limited 给的时间点之后', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const handshakes = env.daemon.handshakes.get(deviceId)
        let seen = 0
        env.hub.interceptor = (e) => {
          if (e.kind !== 'data' || e.to !== env.daemon.id) return undefined
          seen += 1
          if (seen === 2) {
            // backpressure 先到，rate_limited 30 ms 后才到（默认的 100–300 ms 等待还没走完）
            setTimeout(
              () => env.hub.sendControl(deviceId, { type: 'rate_limited', retry_after_ms: 600 }),
              30,
            )
            return 'backpressure'
          }
          return undefined
        }
        const sent = payload(900 * 1024)
        const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 15_000 })
        assert.equal(echoed.data, sent.data)
        const firstBounceAt = env.hub.bounced[0]!.at
        const resendAt = arrivedAtHub(env).find(
          (a) => a.seq === env.hub.bounced[0]!.seq && a.at > firstBounceAt,
        )!.at
        assert.ok(resendAt - firstBounceAt >= 550, `resent after ${resendAt - firstBounceAt} ms`)
        assert.equal(env.daemon.resetsSent, 0)
        assert.equal(env.daemon.handshakes.get(deviceId), handshakes, 'no re-handshake')
      })
    })

    it('目标缓冲按窗口限量（模拟 4 MiB 缓冲）：一条超过缓冲的大消息分几轮重发，仍然按序到达', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const handshakes = env.daemon.handshakes.get(deviceId)
        // 每个 150 ms 窗口只收 1 MiB：超出的退回，粘住规则保证顺序
        const WINDOW_MS = 150
        const WINDOW_BYTES = 1024 * 1024
        let windowStart = 0
        let used = 0
        env.hub.interceptor = (e, _from) => {
          if (e.kind !== 'data' || e.to !== env.daemon.id) return undefined
          const now = Date.now()
          if (now - windowStart > WINDOW_MS) {
            windowStart = now
            used = 0
          }
          if (used + e.body.length > WINDOW_BYTES) return 'backpressure'
          used += e.body.length
          return undefined
        }
        const before = arrivedAtHub(env).length
        const sent = payload(2560 * 1024)
        const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 20_000 })
        assert.equal(echoed.data.length, sent.data.length)
        assert.equal(echoed.data, sent.data)
        const arrivals = arrivedAtHub(env).slice(before)
        assert.ok(
          arrivals.length > new Set(arrivals.map((a) => a.seq)).size,
          'some fragments were resent',
        )
        const forwarded = forwardedToDaemon(env).slice(-new Set(arrivals.map((a) => a.seq)).size)
        assert.ok(isAscending(forwarded), 'the daemon saw the fragments in order')
        assert.equal(env.daemon.resetsSent, 0)
        assert.equal(env.daemon.handshakes.get(deviceId), handshakes, 'no re-handshake')
      })
    })

    it('被退回的 seq 不在缓冲里（hub 说了我们没发过的 seq）：丢弃传输、重新 IK 握手，之后恢复', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const handshakes = env.daemon.handshakes.get(deviceId) ?? 0
        env.hub.sendControl(deviceId, { type: 'backpressure', to: env.daemon.id, seq: 999_999 })
        await until(
          () => (env.daemon.handshakes.get(deviceId) ?? 0) === handshakes + 1 && node.online,
          3000,
          're-handshake after an unbuffered backpressure',
        )
        assert.equal((await node.info({ refresh: true })).name, 'fake-node')
      })
    })

    it('在途窗口（保留缓冲）满了：暂停发新的、不逐出还在路上的片；被退回的片仍在缓冲里，按序重发，不重新握手', async () => {
      // 缓冲上限约一片半、保留期 300 ms：发出的第 2 片之后窗口就满了，要等最老的一片过了保留期才能继续发。
      // 第 2 片被退回——它此刻远超上限也不能被逐出，否则只能重新握手（旧行为）。
      await withNode(
        impl,
        async ({ env, node, deviceId }) => {
          const handshakes = env.daemon.handshakes.get(deviceId) ?? 0
          const errors: Error[] = []
          node.on('error', (e) => errors.push(e))
          bounceNth(env.hub, env.daemon.id, 2)
          const before = arrivedAtHub(env).length
          const sent = payload(900 * 1024)
          const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 15_000 })
          assert.equal(echoed.data, sent.data)

          const arrivals = arrivedAtHub(env).slice(before)
          const originals = arrivals.filter(
            (a, i) => arrivals.findIndex((b) => b.seq === a.seq) === i,
          )
          assert.ok(originals.length >= 5, `fragments ${originals.length}`)
          // 被退回的第 2 片（及粘住的后续）确实原样重发过
          const bounced = env.hub.bounced.filter((b) => b.to === env.daemon.id).map((b) => b.seq)
          assert.ok(bounced.length >= 1)
          assert.ok(isAscending(bounced))
          assert.ok(
            arrivals.filter((a) => a.seq === bounced[0]).length >= 2,
            'the bounced fragment was resent from the buffer',
          )
          // 窗口满了就暂停：各片不是一口气发完的，而是等最老的过了保留期才放下一片
          const spread = originals.at(-1)!.at - originals[0]!.at
          assert.ok(spread >= 350, `all fragments went out within ${spread} ms: no pause`)
          // daemon 按严格升序、每片一次收齐
          const forwarded = forwardedToDaemon(env).slice(-originals.length)
          assert.deepEqual(
            forwarded,
            originals.map((a) => a.seq),
          )
          assert.deepEqual(errors, [])
          assert.equal(env.daemon.resetsSent, 0)
          assert.equal(env.daemon.handshakes.get(deviceId) ?? 0, handshakes, 'no re-handshake')
          assert.equal(node.online, true)
        },
        { resendBufferBytes: 250 * 1024, resendRetainMs: 300 },
      )
    })

    it('保留缓冲没有逐出这一说：缓冲上限很小，大消息也只是发得慢，仍按序送达', async () => {
      await withNode(
        impl,
        async ({ env, node, deviceId }) => {
          const handshakes = env.daemon.handshakes.get(deviceId) ?? 0
          const sent = payload(700 * 1024)
          const echoed = await node.rpc<{ data: string }>('test.echo', sent, { timeoutMs: 15_000 })
          assert.equal(echoed.data, sent.data)
          assert.equal(env.daemon.resetsSent, 0)
          assert.equal(env.daemon.handshakes.get(deviceId) ?? 0, handshakes, 'no re-handshake')
        },
        { resendBufferBytes: 100 * 1024, resendRetainMs: 150 },
      )
    })

    it('handshake 被退回：按退避延后再握手（不立即重发、不重置、不占握手重试次数），之后恢复', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const errors: Error[] = []
        node.on('error', (e) => errors.push(e))
        const handshakeAt: number[] = []
        env.hub.interceptor = (e) => {
          if (e.kind !== 'handshake' || e.to !== env.daemon.id) return undefined
          handshakeAt.push(Date.now())
          // 前两封握手被退回（目标缓冲满）；FAST 的握手回复超时只有 400 ms、只允许 2 次，
          // 所以如果还是靠超时重发，早就 noise_failed 了
          return handshakeAt.length <= 2 ? 'backpressure' : undefined
        }
        env.daemon.sendReset(deviceId) // 让 SDK 重新握手
        await until(() => handshakeAt.length >= 3 && node.online, 4500, 'the third handshake')
        const gap1 = handshakeAt[1]! - handshakeAt[0]!
        const gap2 = handshakeAt[2]! - handshakeAt[1]!
        assert.ok(gap1 >= 280, `first retry after ${gap1} ms (>= 300 ms)`)
        assert.ok(gap1 <= 1500, `first retry after ${gap1} ms (<= ~1000 ms)`)
        assert.ok(gap2 >= 560, `second retry after ${gap2} ms: doubled (>= 600 ms)`)
        assert.deepEqual(errors, [])
        assert.equal((await node.info({ refresh: true })).name, 'fake-node')
      })
    })

    it('handshake 被退回的同时给出 rate_limited：按 retry_after_ms 再握手', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        env.hub.retryAfterMs = 1500 // 比默认的 300–1000 ms 退避上限还长
        env.hub.receiptIntervalMs = 0
        const handshakeAt: number[] = []
        env.hub.interceptor = (e) => {
          if (e.kind !== 'handshake' || e.to !== env.daemon.id) return undefined
          handshakeAt.push(Date.now())
          return handshakeAt.length === 1 ? 'rate_limited' : undefined
        }
        env.daemon.sendReset(deviceId)
        await until(() => handshakeAt.length >= 2 && node.online, 4500, 'the second handshake')
        const gap = handshakeAt[1]! - handshakeAt[0]!
        assert.ok(gap >= 1400, `re-handshake after ${gap} ms (retry_after_ms = 1500)`)
        assert.equal((await node.info({ refresh: true })).name, 'fake-node')
      })
    })

    it('上一个会话的迟到的 backpressure 不会再触发重新握手', async () => {
      await withNode(impl, async ({ env, node, deviceId }) => {
        const oldSeq = arrivedAtHub(env).at(-1)!.seq
        // 强制重新握手（daemon 发 RESET）
        let handshakes = env.daemon.handshakes.get(deviceId) ?? 0
        env.daemon.sendReset(deviceId)
        await until(
          () => (env.daemon.handshakes.get(deviceId) ?? 0) === handshakes + 1 && node.online,
          3000,
          're-handshake after RESET',
        )
        handshakes = env.daemon.handshakes.get(deviceId) ?? 0
        env.hub.sendControl(deviceId, { type: 'backpressure', to: env.daemon.id, seq: oldSeq })
        await sleep(150)
        assert.equal(env.daemon.handshakes.get(deviceId), handshakes, 'stale backpressure ignored')
        assert.equal((await node.info({ refresh: true })).name, 'fake-node')
      })
    })
  })
}
