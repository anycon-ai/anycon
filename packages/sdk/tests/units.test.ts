import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { describe, it } from 'node:test'

import { AsyncQueue } from '../src/async-queue.js'
import { selectNode } from '../src/client.js'
import { decisionToParams } from '../src/permissions.js'
import { SDK_VERSION, toWsUrl, type StoredNode } from '../src/index.js'

describe('toWsUrl', () => {
  it('http(s) → ws(s)，追加 /v1/ws，不重复追加', () => {
    assert.equal(toWsUrl('https://hub.example.com'), 'wss://hub.example.com/v1/ws')
    assert.equal(toWsUrl('https://hub.example.com/'), 'wss://hub.example.com/v1/ws')
    assert.equal(toWsUrl('http://127.0.0.1:8080'), 'ws://127.0.0.1:8080/v1/ws')
    assert.equal(toWsUrl('ws://127.0.0.1:8080/v1/ws'), 'ws://127.0.0.1:8080/v1/ws')
    assert.equal(toWsUrl('wss://h.example/prefix'), 'wss://h.example/prefix/v1/ws')
    assert.equal(toWsUrl('hub.example.com'), 'wss://hub.example.com/v1/ws')
    assert.equal(toWsUrl('https://hub.example.com/?x=1#y'), 'wss://hub.example.com/v1/ws')
    assert.throws(() => toWsUrl('ftp://h'), /unsupported hub URL scheme/)
  })
})

describe('selectNode', () => {
  const nodes: StoredNode[] = [
    { id: 'aaaa1111', name: 'mac-mini', publicKey: '00', pairedAtMs: 1 },
    { id: 'aaaa2222', name: 'linux-box', publicKey: '00', pairedAtMs: 2 },
    { id: 'bbbb3333', publicKey: '00', pairedAtMs: 3 },
  ]
  it('名字精确匹配，或 id 前缀', () => {
    assert.equal(selectNode(nodes, 'mac-mini').id, 'aaaa1111')
    assert.equal(selectNode(nodes, 'bbbb').id, 'bbbb3333')
    assert.equal(selectNode(nodes, 'aaaa2').id, 'aaaa2222')
  })
  it('省略时：恰好一个才行', () => {
    assert.equal(selectNode([nodes[0]!]).id, 'aaaa1111')
    assert.throws(() => selectNode(nodes), /several nodes are paired/)
    assert.throws(() => selectNode([]), /pair\(\)/)
  })
  it('歧义和不存在', () => {
    assert.throws(() => selectNode(nodes, 'aaaa'), /matches 2/)
    assert.throws(() => selectNode(nodes, 'zzzz'), /no paired node matches/)
  })
})

describe('AsyncQueue', () => {
  it('先 push 后 next，先 next 后 push，end 后取完再结束', async () => {
    const q = new AsyncQueue<number>()
    q.push(1)
    q.push(2)
    assert.deepEqual(await q.next(), { value: 1, done: false })
    const waiting = q.next()
    q.push(3) // 2 还在队列里，waiting 拿到 2
    assert.deepEqual(await waiting, { value: 2, done: false })
    q.end()
    assert.deepEqual(await q.next(), { value: 3, done: false })
    assert.deepEqual(await q.next(), { value: undefined, done: true })
  })

  it('fail：已入队的先消费完，再抛错；等待者直接拒绝', async () => {
    const q = new AsyncQueue<number>()
    q.push(1)
    q.fail(new Error('boom'))
    assert.deepEqual(await q.next(), { value: 1, done: false })
    await assert.rejects(q.next(), /boom/)
    assert.deepEqual(await q.next(), { value: undefined, done: true })

    const q2 = new AsyncQueue<number>()
    const w = q2.next()
    q2.fail(new Error('later'))
    await assert.rejects(w, /later/)
  })

  it('消费者提前 break 触发 onDispose，且只触发一次', async () => {
    let disposed = 0
    const q = new AsyncQueue<number>(() => disposed++)
    q.push(1)
    for await (const _ of q) {
      void _
      break
    }
    q.end()
    assert.equal(disposed, 1)
    assert.ok(q.closed)
  })
})

describe('decisionToParams', () => {
  const request = {
    request_id: 'r1',
    options: [
      { id: 'a', label: 'A', kind: 'allow_once' as const },
      { id: 's', label: 'S', kind: 'allow_always' as const },
      { id: 'd', label: 'D', kind: 'reject_once' as const },
    ],
  }
  it('字符串决定 → 对应 kind 的 option', () => {
    assert.deepEqual(decisionToParams(request, 'allow'), { request_id: 'r1', option_id: 'a' })
    assert.deepEqual(decisionToParams(request, 'allow_session'), {
      request_id: 'r1',
      option_id: 's',
    })
    assert.deepEqual(decisionToParams(request, 'deny'), { request_id: 'r1', option_id: 'd' })
    assert.deepEqual(decisionToParams(request, { deny: 'nope' }), {
      request_id: 'r1',
      option_id: 'd',
      message: 'nope',
    })
  })
  it('显式 option / answers / 无决定 / 没有可用 option', () => {
    assert.deepEqual(decisionToParams(request, { option: 'zz', message: 'm' }), {
      request_id: 'r1',
      option_id: 'zz',
      message: 'm',
    })
    assert.deepEqual(decisionToParams(request, { answers: { q: 'x' } }), {
      request_id: 'r1',
      answers: { q: 'x' },
    })
    assert.equal(decisionToParams(request, undefined), undefined)
    assert.equal(decisionToParams({ request_id: 'r', options: [] }, 'allow'), undefined)
  })
})

describe('package', () => {
  it('SDK_VERSION 与 package.json 一致', () => {
    const pkg = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')) as {
      version: string
    }
    assert.equal(SDK_VERSION, pkg.version)
  })
})
