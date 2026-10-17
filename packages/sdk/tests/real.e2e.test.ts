// SDK 对真实 hub + 真实 daemon（mock agent）的端到端。默认跳过，`ANYCON_SDK_REAL=1` 才跑：
//
//   cargo build -p anycon-hub -p anycon-cli      # 要用到 target/debug/{anycon-hub,anycon}
//   just gen-wasm                                # 真实 Noise（packages/protocol/wasm）
//   ANYCON_SDK_REAL=1 bun test packages/sdk/tests/real.e2e.test.ts        # 或 `just test-sdk-real`
//   ANYCON_SDK_REAL=1 node --import tsx --conditions source --test packages/sdk/tests/real.e2e.test.ts
//
// 受限链路（最后两个 describe）：在 daemon ↔ hub、SDK ↔ hub 之间插限速 TCP 代理（real-stack.ts），并给 hub 传
// 环境变量（调低 daemon 的速率预算），复现本机回环链路上测不到的乱序 / 逐出问题。
//
// 账号归属（也写在 packages/sdk/README.md）：
//   daemon 第一次连 hub 没有身份、也没有账号；SDK 作为 joiner 同样是无 cookie 的临时连接。两边都没有账号时
//   hub 判 `forbidden`。所以先用「带管理员登录 cookie 的 joiner」（`pair(…, { headers: { Cookie } })`）把 daemon
//   配进管理员账号（这一步等价于浏览器里的首次配对）；之后 daemon 有了账号，不带 cookie 的 SDK 配对会被
//   hub 归到 daemon 的账号下，这才是 CI 里常规的 `pair()`。本测试两步都走一遍。
import assert from 'node:assert/strict'
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { after, before, describe, it } from 'node:test'

import {
  MemoryStorage,
  connect,
  pair,
  type Node,
  type Session,
  type StreamEvent,
} from '../src/index.js'
import { MIB, binariesPresent, startStack, type RealStack } from './real-stack.js'

const textLine = (i: number) => `line ${String(i).padStart(7, '0')} ${'abcdefghij'.repeat(5)}\n`

/** 确定性的 ASCII 文本，恰好 `bytes` 字节（`line 0000001 …` 重复，最后一行截断补齐），能逐字节比对。 */
function lines(bytes: number): string {
  const chunks: string[] = []
  let length = 0
  for (let i = 0; length < bytes; i++) {
    const next = textLine(i)
    chunks.push(next)
    length += next.length
  }
  return chunks.join('').slice(0, bytes)
}

const tail = (log: string) => log.split('\n').slice(-25).join('\n')
const unset = (v: unknown) => v === null || v === undefined

/** 失败时把 hub / daemon 日志的末尾附在错误里（大流量的失败只看 SDK 一侧很难判断是谁先出的问题）。 */
async function withLogs(stack: RealStack, fn: () => Promise<void>): Promise<void> {
  try {
    await fn()
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err)
    throw new Error(
      `${message}\n--- hub log (tail) ---\n${tail(stack.hubLog())}\n--- daemon log (tail) ---\n${tail(stack.daemonLog())}`,
      { cause: err },
    )
  }
}

const enabled = process.env.ANYCON_SDK_REAL === '1'
// bun 的 node:test 兼容层不认钩子 / describe 的 timeout 选项，默认 5 秒；起 hub + daemon + 两次配对在慢机器上不止。
if (enabled && typeof Bun !== 'undefined') {
  const { setDefaultTimeout } = await import('bun:test')
  setDefaultTimeout(180_000)
}
const skip = enabled ? false : 'set ANYCON_SDK_REAL=1 to run the real hub + daemon test'

/** 不带会话元数据的事件字段（用来和 history 里的事件比较）。 */
function bare(ev: StreamEvent): Record<string, unknown> {
  const { session_id: _s, seq: _q, ts_ms: _t, ...rest } = ev as unknown as Record<string, unknown>
  return rest
}

/**
 * 实时事件和落库后读出的事件逐字段一致，只有浮点费用会差最后几位（0.000091 vs 0.00009099999999999999）。
 * 去掉 `turn_id`（历史里挂在外层）并把 `cost_usd` 四舍五入到 9 位小数再比。
 */
function withoutFloatNoise(ev: Record<string, unknown>): Record<string, unknown> {
  const { turn_id: _turn, ...rest } = ev
  const usage = rest.usage as Record<string, unknown> | null | undefined
  if (usage && typeof usage.cost_usd === 'number') {
    return { ...rest, usage: { ...usage, cost_usd: Math.round(usage.cost_usd * 1e9) / 1e9 } }
  }
  return rest
}

function types(events: readonly { type: string }[]): string[] {
  return events.map((e) => e.type)
}

/** 按顺序在 haystack 里找 needle（允许中间夹别的事件，比如 status）。 */
function assertSubsequence(haystack: readonly string[], needle: readonly string[]): void {
  let at = 0
  for (const item of haystack) if (item === needle[at]) at += 1
  assert.equal(
    at,
    needle.length,
    `expected ${needle.join(' → ')} in order, got ${haystack.join(' → ')}`,
  )
}

describe('SDK ↔ 真实 hub + daemon（mock agent）', { skip, timeout: 180_000 }, () => {
  let stack: RealStack
  let node: Node
  const storage = new MemoryStorage()

  before(async () => {
    assert.ok(binariesPresent(), 'build target/debug/anycon-hub and target/debug/anycon first')
    stack = await startStack()
    try {
      await setup()
    } catch (err) {
      await stack.stop() // before 失败时不保证会跑 after
      throw err
    }
  })

  async function setup(): Promise<void> {
    // 1) 首次配对：带管理员 cookie 的临时设备，让 daemon 有账号和 device token。
    const first = await stack.beginPair()
    const bootstrap = await pair(stack.hubUrl, {
      code: first.code,
      name: 'sdk-bootstrap',
      storage: new MemoryStorage(),
      headers: { Cookie: stack.adminCookie },
    })
    assert.equal(bootstrap.nodeName, 'sdk-node')
    const firstDone = await first.done
    assert.equal((firstDone as { paired: { name: string } }).paired.name, 'sdk-bootstrap')

    // 2) 常规配对：不带 cookie，被 hub 归到 daemon 的账号。
    const second = await stack.beginPair()
    const paired = await pair(stack.hubUrl, { code: second.code, name: 'sdk-test', storage })
    assert.equal(paired.nodeName, 'sdk-node')
    assert.match(paired.nodeFingerprint, /^([0-9a-f]{2}:){7}[0-9a-f]{2}$/)
    const secondDone = await second.done
    assert.equal((secondDone as { paired: { kind: string } }).paired.kind, 'sdk')

    node = await connect(stack.hubUrl, { storage, node: 'sdk-node' })
  }

  it('配对：#code= 链接（短码在 fragment 里，不经过 hub），pair.ack 校验后落盘', async () => {
    const offered = await stack.beginPair()
    // CLI 打印的链接形如 ${hub}/pair#code=…：整条链接可以直接交给 pair()
    const link = offered.link ?? `${stack.hubUrl}/pair#code=${offered.code}`
    const url = new URL(link)
    assert.equal(url.search, '', 'the code must not be in the query string')
    assert.equal(
      new URLSearchParams(url.hash.slice(1)).get('code')?.replaceAll('-', ''),
      offered.code.replaceAll('-', ''),
    )
    const viaLink = await pair(stack.hubUrl, {
      code: link,
      name: 'sdk-link',
      storage: new MemoryStorage(),
    })
    assert.equal(viaLink.nodeName, 'sdk-node')
    assert.match(viaLink.nodeFingerprint, /^([0-9a-f]{2}:){7}[0-9a-f]{2}$/)
    await offered.done
    assert.ok(
      !stack.hubLog().includes(offered.code.replaceAll('-', '')),
      'the hub log must not contain a pair code',
    )
  })

  it('配对：默认就是 16 位短码（anycon pair）', async () => {
    const offered = await stack.beginPair()
    assert.match(offered.code, /^([2-9A-HJKMNP-Z]{4}-){3}[2-9A-HJKMNP-Z]{4}$/)
    const viaDefault = await pair(stack.hubUrl, {
      code: offered.code,
      name: 'sdk-default-code',
      storage: new MemoryStorage(),
    })
    assert.equal(viaDefault.nodeName, 'sdk-node')
    assert.equal(
      ((await offered.done) as { paired: { name: string } }).paired.name,
      'sdk-default-code',
    )
  })

  it('配对：8 位短码（anycon pair --short，只适合自建 hub）', async () => {
    const offered = await stack.beginPair({ short: true })
    assert.match(offered.code, /^[2-9A-HJKMNP-Z]{4}-[2-9A-HJKMNP-Z]{4}$/)
    const viaShort = await pair(stack.hubUrl, {
      code: offered.code,
      name: 'sdk-short-code',
      storage: new MemoryStorage(),
    })
    assert.equal(viaShort.nodeName, 'sdk-node')
    assert.equal(
      ((await offered.done) as { paired: { name: string } }).paired.name,
      'sdk-short-code',
    )
  })

  after(async () => {
    await node?.close()
    await stack?.stop()
  })

  it('没有账号的节点 + 没有 cookie 的 joiner：hub 判 forbidden（账号规则）', async () => {
    // 另起一个全新的 hub + daemon，故意不带 cookie。
    const fresh = await startStack()
    try {
      const p = await fresh.beginPair()
      await assert.rejects(
        pair(fresh.hubUrl, { code: p.code, name: 'no-cookie', storage: new MemoryStorage() }),
        (err: Error & { reason?: string }) => {
          assert.equal(err.name, 'PairingFailed')
          assert.equal(err.reason, 'forbidden')
          return true
        },
      )
    } finally {
      await fresh.stop()
    }
  })

  it('node.info / agents.list', async () => {
    const info = await node.info()
    assert.equal(info.name, 'sdk-node')
    assert.equal(info.protocol_version, 1)

    const agents = await node.agents.list()
    const mock = agents.find((a) => a.id === 'mock')
    assert.ok(mock, `mock agent missing: ${agents.map((a) => a.id).join(',')}`)
    assert.equal(mock.status, 'ready')
    assert.deepEqual(mock.models?.map((m) => m.id).toSorted(), ['mock-large', 'mock-small'])
    assert.equal(agents.find((a) => a.id === 'claude-code')?.status, 'disabled')
    // etag 缓存：再取一次得到同一份
    assert.deepEqual(await node.agents.list(), agents)

    // daemon 侧看得到这台 SDK 设备
    const devices = JSON.parse(await stack.cli('devices', '--json')) as {
      devices: { name: string; kind: string }[]
    }
    assert.ok(devices.devices.some((d) => d.name === 'sdk-test' && d.kind === 'sdk'))
  })

  describe('会话', () => {
    let session: Session

    before(async () => {
      const project = await node.projects.create('sdk-proj')
      assert.equal(project.name, 'sdk-proj')
      session = await node.sessions.create({
        project: 'sdk-proj',
        agent: 'mock',
        settings: { model: 'mock-large', thinking: 'low', mode: 'ask' },
      })
    })

    it('创建时的设置原样生效', () => {
      assert.match(session.id, /^sess_/)
      assert.equal(session.settings.model, 'mock-large')
      assert.equal(session.settings.thinking, 'low')
      assert.equal(session.settings.mode, 'ask')
    })

    let streamed: StreamEvent[] = []

    it('send("run permission")：事件流 + 权限答复 + 用量', async () => {
      streamed = []
      let answered = 0
      for await (const ev of session.send('run permission')) {
        streamed.push(ev)
        if (ev.type === 'permission') {
          assert.equal(ev.kind, 'tool')
          assert.deepEqual(
            ev.options.map((o) => o.kind),
            ['allow_once', 'allow_always', 'reject_once'],
          )
          // 用 session.respond 而不是事件上的快捷方法，两条路径各覆盖一次（策略回调在下一个用例里）
          const allow = ev.options.find((o) => o.kind === 'allow_once')
          assert.ok(allow)
          await session.respond(ev.request_id, allow.id)
          answered += 1
          // 重复答复：daemon 说已经有人答过了
          await assert.rejects(
            session.respond(ev.request_id, allow.id),
            (err: Error & { code?: string }) => {
              assert.equal(err.code, 'already_resolved')
              return true
            },
          )
        }
      }
      assert.equal(answered, 1)

      const t = types(streamed)
      assertSubsequence(t, [
        'user_message',
        'turn_start',
        'thinking_delta',
        'text_delta',
        'permission',
        'permission_resolved',
        'tool_call',
        'tool_result',
        'turn_done',
      ])
      assert.equal(t[0], 'user_message')
      assert.equal(t.at(-1), 'turn_done')

      const user = streamed[0]
      assert.ok(user?.type === 'user_message')
      assert.equal(user.content, 'run permission')

      const text = streamed.flatMap((e) => (e.type === 'text_delta' ? [e.text] : [])).join('')
      assert.ok(
        text.includes('[model=mock-large thinking=low mode=ask]'),
        `settings echo missing in: ${text}`,
      )

      const resolved = streamed.find((e) => e.type === 'permission_resolved')
      assert.ok(resolved?.type === 'permission_resolved')
      assert.equal(resolved.option_id, 'allow')

      const call = streamed.find((e) => e.type === 'tool_call')
      assert.ok(call?.type === 'tool_call')
      const result = streamed.find((e) => e.type === 'tool_result')
      assert.ok(result?.type === 'tool_result')
      assert.equal(result.id, call.id)

      const done = streamed.at(-1)
      assert.ok(done?.type === 'turn_done')
      assert.equal(done.reason, 'completed')
      assert.ok(done.usage, 'turn_done carries usage')
      assert.equal(done.usage.input_tokens, 'run permission'.length) // mock：输入 = prompt 字符数
      assert.ok((done.usage.output_tokens ?? 0) > 0)

      // seq 单调递增
      const seqs = streamed.map((e) => e.seq)
      assert.deepEqual(
        seqs,
        seqs.toSorted((a, b) => a - b),
      )
      assert.equal(new Set(seqs).size, seqs.length)
    })

    it('history() 与刚才流里的事件一致', async () => {
      const { events, has_more } = await session.history({ afterSeq: 0 })
      assert.equal(has_more, false)
      // 创建时的第一个事件：完整的有效设置
      assert.equal(events[0]?.event.type, 'settings_changed')
      const bySeq = new Map(events.map((e) => [e.seq, e]))
      for (const ev of streamed) {
        const h = bySeq.get(ev.seq)
        assert.ok(h, `seq ${ev.seq} (${ev.type}) is not in history`)
        assert.equal(h.event.type, ev.type)
        assert.deepEqual(
          withoutFloatNoise(h.event as unknown as Record<string, unknown>),
          withoutFloatNoise(bare(ev)),
          `event at seq ${ev.seq} differs`,
        )
      }
      // 分页：after_seq 之后没有更多
      const last = events.at(-1)
      assert.ok(last)
      assert.deepEqual((await session.history({ afterSeq: last.seq })).events, [])
      // 同一个会话再 get 一遍，last_seq 对得上
      const fresh = await node.sessions.get(session.id)
      assert.equal(fresh.summary.last_seq, last.seq)
    })

    it('策略回调代答：deny → agent 看到 skipped', async () => {
      const denied = await node.sessions.create({
        project: 'sdk-proj',
        agent: 'mock',
        settings: { model: 'mock-large', thinking: 'high', mode: 'ask' },
        onPermission: (req) => (req.kind === 'tool' ? { deny: 'not today' } : undefined),
      })
      const result = await denied.run('run permission')
      assert.equal(result.reason, 'completed')
      assert.ok(result.text.includes('[model=mock-large thinking=high mode=ask]'), result.text)
      assert.ok(result.text.includes('skipped'), result.text)
      assert.equal(
        result.events.some((e) => e.type === 'tool_call'),
        false,
      )
    })

    it('setSettings(model: mock-small)：settings_changed，thinking 回落', async () => {
      const applied = await session.setSettings({ model: 'mock-small' })
      assert.equal(applied.model, 'mock-small')
      assert.equal(applied.mode, 'ask')
      // mock-small 没有 thinking：不能保留 low
      assert.ok(
        applied.thinking === null || applied.thinking === undefined,
        `thinking: ${applied.thinking}`,
      )

      const { events } = await session.history({ afterSeq: 0 })
      const changed = events.filter((e) => e.event.type === 'settings_changed')
      assert.ok(changed.length >= 2)
      const last = changed.at(-1)?.event
      assert.ok(last?.type === 'settings_changed')
      assert.equal(last.settings.model, 'mock-small')
      assert.ok(last.settings.thinking === null || last.settings.thinking === undefined)

      // 下一个 turn 用新设置
      const result = await session.run('hello')
      assert.ok(result.text.includes('[model=mock-small thinking=none mode=ask]'), result.text)
      assert.equal(result.reason, 'completed')

      // 非法设置被拒绝，错误码稳定
      await assert.rejects(
        session.setSettings({ model: 'no-such-model' }),
        (err: Error & { field?: string }) => {
          assert.equal(err.name, 'SettingsInvalid')
          assert.equal(err.field, 'model')
          return true
        },
      )
    })

    it('restart()：真的换了一个 agent 进程（mock 的 allow_always 只记在进程里，重启后要重新问）', async () => {
      // 下面的 clear 联测要先 restart 再 clear，它的前提是 restart 之后确实没有存活进程、下一次 send 才 resume 出新进程。
      // mock 用 `allow_session` 标志记「本会话一直允许」，这个标志只活在进程里（crates/adapters/src/mock/session.rs）：
      // 同一个进程里第二次不再问；重启后新进程要重新问。这就证明了 restart 之后跑的是另一个进程。
      const fresh = await node.sessions.create({
        project: 'sdk-proj',
        agent: 'mock',
        settings: { model: 'mock-large', thinking: 'low', mode: 'ask' },
      })
      const asked = async (): Promise<number> => {
        let n = 0
        for await (const ev of fresh.send('run permission')) {
          if (ev.type !== 'permission') continue
          n += 1
          const always = ev.options.find((o) => o.kind === 'allow_always')
          assert.ok(always, 'the mock offers an allow-for-this-session option')
          await fresh.respond(ev.request_id, always.id)
        }
        return n
      }
      assert.equal(await asked(), 1, 'first time: asks, and we pick allow-for-session')
      assert.equal(await asked(), 0, 'same process: remembered, no question')
      await fresh.restart()
      assert.equal(await asked(), 1, 'after restart() a new process has forgotten it')
    })

    // 这条能抓到：restart 之后确实没有存活进程；此时 clear 走的是 daemon 的「无进程 → 改写设置 + launch_args」路径
    // （持久化、`settings_changed` 事件、返回值），下一次 send 拉起的新进程拿到的是清除后的设置，旧值没有复活。
    // 抓不到：旧 `launch_args` 复活的那个窗口本身——mock 不读 `resume.launch_args`，只看拉起请求里的 `settings`，所以就算 daemon 的
    // `sync_launch_args` 被删掉，这条也是绿的；那一层由 adapters 的 claude_process 测试和 daemon 的 sessions 测试守着。
    it('setSettings + clear：先 restart 让进程退出，再 clear，resume 后旧值不复活（mock 不读 launch_args，旧值复活的窗口由 adapters / daemon 单测守着）', async () => {
      // mock 的回显把 system_prompt_append / max_turns / max_budget_usd 追加在 `[model=…]` 之后
      const applied = await session.setSettings({
        system_prompt_append: 'Be terse.',
        max_turns: 3,
        max_budget_usd: 1.5,
      })
      assert.equal(applied.system_prompt_append, 'Be terse.')
      assert.equal(applied.max_turns, 3)
      assert.equal(applied.max_budget_usd, 1.5)
      const withValues = await session.run('hello')
      assert.ok(withValues.text.includes('prompt_append=Be terse.'), withValues.text)
      assert.ok(withValues.text.includes('max_turns=3'), withValues.text)
      assert.ok(withValues.text.includes('budget=1.5'), withValues.text)

      // 只发 settings 不带字段 = 保持；清除要点名
      const kept = await session.setSettings({})
      assert.equal(kept.system_prompt_append, 'Be terse.')

      // 先让 agent 进程退出（restart 后没有存活进程，直到下一次 send 才 resume），再 clear：
      // 这样 clear 走的是「无进程 → 改写 launch_args」的路径，下一次 send 走 resume
      await session.restart()
      const cleared = await session.setSettings(
        {},
        { clear: ['system_prompt_append', 'max_turns', 'max_budget_usd'] },
      )
      assert.ok(
        unset(cleared.system_prompt_append),
        `system_prompt_append: ${cleared.system_prompt_append}`,
      )
      assert.ok(unset(cleared.max_turns), `max_turns: ${cleared.max_turns}`)
      assert.ok(unset(cleared.max_budget_usd), `max_budget_usd: ${cleared.max_budget_usd}`)

      // 结果里的完整设置和 session.get 一致
      const fetched = await session.refresh()
      assert.ok(unset(fetched.settings.system_prompt_append))
      assert.ok(unset(fetched.settings.max_turns))

      // 下一个 turn 走 resume：清掉的值不能从旧的 launch_args 复活
      const afterClear = await session.run('hello again')
      assert.ok(!afterClear.text.includes('prompt_append='), afterClear.text)
      assert.ok(!afterClear.text.includes('max_turns='), afterClear.text)
      assert.ok(!afterClear.text.includes('budget='), afterClear.text)
      // 再重启一次（进程活着时 clear 过的值也不能复活）
      await session.restart()
      const resumed = await session.run('after resume')
      assert.ok(!resumed.text.includes('prompt_append='), resumed.text)
      assert.ok(!resumed.text.includes('max_turns='), resumed.text)
      assert.ok(!resumed.text.includes('budget='), resumed.text)
      assert.ok(resumed.text.includes('[model=mock-small'), resumed.text)
    })

    it('cancel() 慢 turn：turn_done(cancelled)', async () => {
      const slow = await node.sessions.create({
        project: 'sdk-proj',
        agent: 'mock',
        settings: { model: 'mock-large', thinking: 'low', mode: 'yolo' },
      })
      const events: StreamEvent[] = []
      let cancelled = false
      for await (const ev of slow.send('slow please')) {
        events.push(ev)
        if (!cancelled && ev.type === 'text_delta') {
          cancelled = true
          await slow.cancel()
        }
      }
      assert.ok(cancelled, 'got a text_delta before the turn ended')
      const done = events.at(-1)
      assert.ok(done?.type === 'turn_done')
      assert.equal(done.reason, 'cancelled')
      // 取消后会话还能继续用
      const next = await slow.run('hello again')
      assert.equal(next.reason, 'completed')
    })

    it('大输出：4 次 300 KB 的 tool_result 之后 history() 分页取全，节点不掉线（ADR-30）', async () => {
      const big = await node.sessions.create({
        project: 'sdk-proj',
        agent: 'mock',
        settings: { model: 'mock-large', thinking: 'low', mode: 'yolo' },
      })
      const offline: string[] = []
      node.on('offline', () => offline.push('offline'))
      for (let i = 0; i < 4; i++) {
        const result = await big.run(`please big #${i}`)
        assert.equal(result.reason, 'completed')
        const out = result.events.find((e) => e.type === 'tool_result')
        assert.ok(out?.type === 'tool_result')
        const text = typeof out.output === 'string' ? out.output : JSON.stringify(out.output)
        // daemon 把超过 64 KiB 的输出截成前 48 KiB + 后 16 KiB（中间放截断标记），而不是整条发出去
        assert.ok(text.length < 100 * 1024, `tool output was not truncated (${text.length} bytes)`)
        assert.ok(text.includes('BIGSTART'), 'head kept')
        assert.ok(text.includes('BIGEND'), 'tail kept')
        assert.ok(text.includes('[truncated'), 'truncation marker present')
      }
      // history 按页取全（每页按编码后字节截断，has_more 提示还有）
      const events = []
      let cursor = 0
      for (let page = 0; page < 50; page++) {
        const r = await big.history({ afterSeq: cursor })
        events.push(...r.events)
        if (!r.has_more || r.events.length === 0) break
        cursor = r.events.at(-1)!.seq
      }
      assert.equal(events.filter((e) => e.event.type === 'tool_result').length, 4)
      assert.equal(events.filter((e) => e.event.type === 'turn_done').length, 4)
      assert.deepEqual(offline, [], 'the node never dropped')
      assert.equal(node.online, true)
      assert.equal((await node.info({ refresh: true })).name, 'sdk-node')
    })

    it('delete() 之后会话不在列表里', async () => {
      const tmp = await node.sessions.create({ project: 'sdk-proj', agent: 'mock' })
      assert.ok((await node.sessions.list()).some((s) => s.id === tmp.id))
      await tmp.delete()
      assert.equal(
        (await node.sessions.list()).some((s) => s.id === tmp.id),
        false,
      )
    })
  })
})

/** 给 hub 开 debug 日志：它把每一次退回（缓冲满 / 超速率预算 / 粘住）记在 debug 级别，只有这样才断言得到「确实退回过」。 */
const HUB_DEBUG = 'warn,anycon_hub=debug'

/**
 * hub 日志里「退回」的行（`backpressure: sender over its rate budget` / `target buffer full` / `sticky`）；
 * `rate` = 只数因为发送方超速率预算而退回的。
 */
function bounceLines(log: string, rate = false): string[] {
  return log.split('\n').filter((l) => /backpressure/i.test(l) && (!rate || /rate/i.test(l)))
}

/** 两步配对（管理员 cookie 引导 daemon 入账号 → 常规配对）后连上节点；用 `clientUrl`，所以经得起 SDK 侧的限速代理。 */
async function bootstrapNode(stack: RealStack, name: string): Promise<Node> {
  const first = await stack.beginPair()
  await pair(stack.clientUrl, {
    code: first.code,
    name: 'sdk-bootstrap',
    storage: new MemoryStorage(),
    headers: { Cookie: stack.adminCookie },
  })
  await first.done
  const storage = new MemoryStorage()
  const second = await stack.beginPair()
  await pair(stack.clientUrl, { code: second.code, name, storage })
  await second.done
  return await connect(stack.clientUrl, { storage, node: 'sdk-node' })
}

// 大消息 / 大流量：一条 5 MiB 的 `fs.read` 回复要经 hub 的发送缓冲（4 MiB）和粘住规则才能按序到达；
// 连续 30 MiB 之后节点仍然可用、没有 30 秒超时。这两个用例依赖 hub 的粘住规则、hub 的 daemon 速率预算 100 MiB/分钟
// （docs/spec/hub.md § 3.4）和 daemon 的升序重发，缺一边都可能失败；
// 放在最后，这样即使失败也不影响上面的用例（失败时节点可能被 hub 断开）。
describe('SDK ↔ 真实 hub + daemon：大消息与大流量', { skip, timeout: 240_000 }, () => {
  let stack: RealStack
  let node: Node

  before(async () => {
    stack = await startStack({ hubLog: HUB_DEBUG })
    try {
      node = await bootstrapNode(stack, 'sdk-big')
    } catch (err) {
      await stack.stop()
      throw err
    }
  })

  after(async () => {
    await node?.close()
    await stack?.stop()
  })

  it('5 MiB 的 fs.read 经 hub 粘住规则按序到达，内容逐字节一致', () =>
    withLogs(stack, async () => {
      const content = lines(5 * MIB)
      writeFileSync(join(stack.workspace, 'five-mib.txt'), content)
      const offline: string[] = []
      node.on('offline', () => offline.push('offline'))
      const started = Date.now()
      const got = await node.call(
        'fs.read',
        { path: 'five-mib.txt', max_bytes: 10 * MIB },
        { timeoutMs: 25_000 },
      )
      assert.ok(Date.now() - started < 20_000, `fs.read took ${Date.now() - started} ms`)
      assert.equal(got.binary, false)
      assert.equal(got.truncated, false)
      assert.equal(got.bytes, content.length)
      assert.equal(got.text.length, content.length)
      assert.ok(got.text === content, 'the 5 MiB text differs from the file')
      assert.deepEqual(offline, [], 'the node never dropped (no desync / re-handshake)')
      assert.equal((await node.info({ refresh: true })).name, 'sdk-node')
    }))

  it('连续 30 MiB（10 × 3 MiB fs.read）之后节点仍然可用，没有 30 秒超时', () =>
    withLogs(stack, async () => {
      const content = lines(3 * MIB)
      writeFileSync(join(stack.workspace, 'three-mib.txt'), content)
      const offline: string[] = []
      node.on('offline', () => offline.push('offline'))
      for (let i = 0; i < 10; i++) {
        const started = Date.now()
        const got = await node.call(
          'fs.read',
          { path: 'three-mib.txt', max_bytes: 10 * MIB },
          { timeoutMs: 25_000 },
        )
        const took = Date.now() - started
        assert.ok(took < 20_000, `read #${i} took ${took} ms`)
        assert.equal(got.text.length, content.length, `read #${i}`)
        assert.ok(got.text === content, `read #${i} content differs`)
      }
      assert.deepEqual(offline, [], 'the node never dropped (no desync / re-handshake)')
      // 30 MiB 之后：节点在线，常规请求照常
      assert.equal(node.online, true)
      assert.equal((await node.info({ refresh: true })).name, 'sdk-node')
      assert.ok((await node.agents.list({ refresh: true })).some((a) => a.id === 'mock'))
    }))

  it('SDK 作为发送方超过客户端速率预算（16 × 900 KiB session.send）：暂停、等 retry_after_ms、升序重发，全部完成且不掉线，hub 确实退回过', () =>
    withLogs(stack, async () => {
      // 客户端连接默认 10 MiB / 分钟：16 × 900 KiB ≈ 14.1 MiB 越过它（令牌桶一分钟只回 10 MiB，这 16 条在几秒内发完）。
      // hub 不丢信封，改为退回 backpressure + rate_limited；SDK 按发送方规则暂停、等、按升序原样重发，请求最终完成，
      // 通道不失步、不被 hub 断开。
      const project = await node.projects.create('sdk-send-proj')
      const session = await node.sessions.create({
        project: project.name,
        agent: 'mock',
        settings: { model: 'mock-large', thinking: 'low', mode: 'yolo' },
      })
      const offline: string[] = []
      const errors: Error[] = []
      node.on('offline', () => offline.push('offline'))
      node.on('error', (e) => errors.push(e))
      const bouncedBefore = bounceLines(stack.hubLog(), true).length
      for (let i = 0; i < 16; i++) {
        const started = Date.now()
        const result = await session.run(`message ${i} ${'z'.repeat(900 * 1024)}`)
        assert.equal(result.reason, 'completed', `send #${i}`)
        assert.ok(Date.now() - started < 25_000, `send #${i} took ${Date.now() - started} ms`)
      }
      assert.deepEqual(offline, [], 'the node never dropped (no desync / re-handshake)')
      assert.deepEqual(errors, [])
      assert.equal(node.online, true)
      assert.equal((await node.info({ refresh: true })).name, 'sdk-node')
      // 没有这条断言，「超预算」可能根本没发生（令牌桶一直够用），用例就什么都没测到
      const bounced = bounceLines(stack.hubLog(), true).length - bouncedBefore
      assert.ok(
        bounced > 0,
        'the hub never bounced an SDK envelope for the rate budget: the budget was not exceeded',
      )
    }))
})

// 受限链路：daemon → hub 上行限速 6 MiB/s（TCP 代理），daemon 的速率预算调到 32 MiB/分钟。
// 本机回环链路上，全部在途数据几毫秒内就被 hub 处理完，回执总是早于 100 ms 的重发计时；一旦链路受限，在途数据远多于
// 一轮重发排得空的量，退回的回执就跨了好几轮重发，粘住规则不够严的 hub 会放过乱序的信封（接收方看到 `out_of_order`，
// 传输重置，请求以 `NodeOffline` 失败）。修复前 8 次 9 MiB 读里约 5 次失败。
// 32 MiB/分钟：令牌桶容量 32 MiB、回填约 0.53 MiB/s，代理 6 MiB/s 把它在约 6 s 内抽干（约第 4 次读），
// 之后 daemon 持续超出自己的预算，hub 退回 + `rate_limited`，最后两次读以 0.5 MiB/s 左右的速度完成（约 40 s）。
// 预算要在链路比标称慢得多时也一定被打到：传输耗时 T 秒，桶里最多能过 32 + 0.53·T MiB，要小于 54 MiB，即 T < 41 s；
// 36 MiB 时 T 的上限只有约 30 s，慢的共享 CI 机器上补充量会把余下的数据全放过去，「hub 确实退回过」的断言就失败。
describe('SDK ↔ 真实 hub + daemon：受限链路', { skip, timeout: 300_000 }, () => {
  let stack: RealStack
  let node: Node

  before(async () => {
    stack = await startStack({
      hubLog: HUB_DEBUG,
      hubEnv: { ANYCON_RATE_DAEMON_BYTES_PER_MIN: String(32 * MIB) },
      daemonLink: { up: 6 * MIB },
    })
    try {
      node = await bootstrapNode(stack, 'sdk-limited')
    } catch (err) {
      await stack.stop()
      throw err
    }
  })

  after(async () => {
    await node?.close()
    await stack?.stop()
  })

  it('daemon 上行 6 MiB/s、预算 32 MiB/分钟：连续 6 次 9 MiB fs.read 全部成功，不掉线，hub 确实退回过（缓冲满 / 超速）', () =>
    withLogs(stack, async () => {
      const content = lines(9 * MIB)
      writeFileSync(join(stack.workspace, 'nine-mib.txt'), content)
      const offline: string[] = []
      const errors: Error[] = []
      node.on('offline', () => offline.push('offline'))
      node.on('error', (e) => errors.push(e))
      for (let i = 0; i < 6; i++) {
        const started = Date.now()
        const got = await node.call(
          'fs.read',
          { path: 'nine-mib.txt', max_bytes: 10 * MIB },
          { timeoutMs: 120_000 },
        )
        assert.equal(got.truncated, false, `read #${i}`)
        assert.equal(
          got.text.length,
          content.length,
          `read #${i} (after ${Date.now() - started} ms)`,
        )
        assert.ok(got.text === content, `read #${i} content differs`)
      }
      assert.deepEqual(offline, [], 'the node never dropped (no desync / re-handshake)')
      assert.deepEqual(errors, [])
      assert.equal(node.online, true)
      assert.equal((await node.info({ refresh: true })).name, 'sdk-node')

      // 流量确实走了代理，并且 hub 确实退回过：否则链路没有被真正限住，用例什么都没测到
      assert.ok(
        (stack.daemonProxy?.bytesUp ?? 0) >= 6 * 9 * MIB,
        `the daemon's traffic did not go through the proxy (${stack.daemonProxy?.bytesUp} bytes)`,
      )
      const log = stack.hubLog()
      assert.ok(bounceLines(log).length > 0, 'the hub never bounced an envelope (backpressure)')
      assert.ok(
        bounceLines(log, true).length > 0,
        "the hub never bounced an envelope for the daemon's rate budget",
      )
    }))
})

// 停顿的客户端 + 并发大读：daemon 上行 6 MiB/s，SDK 下行每 10 s 里停读 5 s（模拟手机切后台再回来），
// 同时发 3 个 9 MiB 的 `fs.read`；daemon 预算调高到 10 GiB/分钟以排除速率限制，只剩目标缓冲满的退回。
// 停读要足够久，缓冲才一定会满：停读期间 hub 要收下的量 = daemon 的实际发送速率 × 停读时长，得超过 hub → SDK 那条 TCP 的
// 内核缓冲加上 hub 自己的 4 MiB 发送缓冲。共享 CI 机器上 debug 构建的 daemon 只发得出约 3 MiB/s，2.5 s 的停读只攒
// 约 7 MiB，装得下，hub 一次都没退回；停读 5 s（占空比不变，总耗时不变）攒得出约 15 MiB。
// 修复前 6 轮全部失败：daemon 的保留缓冲逐出了「已发出、hub 还没处理」的旧分片，它们随后被退回时找不到原文，
// 只能重置传输（daemon 日志 `the hub bounced an envelope that is no longer retained`）。
describe('SDK ↔ 真实 hub + daemon：停顿的客户端与并发大读', { skip, timeout: 300_000 }, () => {
  let stack: RealStack
  let node: Node

  before(async () => {
    stack = await startStack({
      hubLog: HUB_DEBUG,
      hubEnv: { ANYCON_RATE_DAEMON_BYTES_PER_MIN: String(10 * 1024 * MIB) },
      daemonLink: { up: 6 * MIB },
      clientLink: { stallDown: { periodMs: 10_000, stallMs: 5000 } },
    })
    try {
      node = await bootstrapNode(stack, 'sdk-stalled')
    } catch (err) {
      await stack.stop()
      throw err
    }
  })

  after(async () => {
    await node?.close()
    await stack?.stop()
  })

  it('3 个并发的 9 MiB fs.read × 6 轮全部成功，不掉线，hub 确实因为目标缓冲满退回过', () =>
    withLogs(stack, async () => {
      const content = lines(9 * MIB)
      writeFileSync(join(stack.workspace, 'nine-mib.txt'), content)
      const offline: string[] = []
      node.on('offline', () => offline.push('offline'))
      for (let round = 0; round < 6; round++) {
        const reads = await Promise.all(
          [0, 1, 2].map(() =>
            node.call(
              'fs.read',
              { path: 'nine-mib.txt', max_bytes: 10 * MIB },
              { timeoutMs: 120_000 },
            ),
          ),
        )
        for (const [i, got] of reads.entries()) {
          assert.equal(got.text.length, content.length, `round ${round} read #${i}`)
          assert.ok(got.text === content, `round ${round} read #${i} content differs`)
        }
      }
      assert.deepEqual(offline, [], 'the node never dropped (no desync / re-handshake)')
      assert.equal((await node.info({ refresh: true })).name, 'sdk-node')
      const bufferFull = bounceLines(stack.hubLog()).filter((l) => /buffer full/i.test(l))
      assert.ok(bufferFull.length > 0, 'the hub never bounced an envelope for a full target buffer')
    }))
})

// 没设 ANYCON_SDK_REAL 时的占位：让 `bun test` 的输出里明确写出「跳过」的原因。
if (!enabled) {
  describe('SDK ↔ 真实 hub + daemon', () => {
    it.skip('skipped: set ANYCON_SDK_REAL=1 (see tests/real.e2e.test.ts)', () => {})
  })
}
