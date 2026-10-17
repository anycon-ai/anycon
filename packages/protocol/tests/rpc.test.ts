import assert from 'node:assert/strict'
import { describe, it } from 'node:test'

import {
  AnyconRpcError,
  ProtocolError,
  RpcPeer,
  RpcTimeoutError,
  decodeRpc,
  encodeRpc,
  isNotification,
  isRequest,
  isResponse,
  makeError,
  makeNotification,
  makeRequest,
  makeResult,
  type RpcMessage,
} from '../src/index.js'

const enc = (m: RpcMessage) => encodeRpc(m)
const text = (b: Uint8Array) => new TextDecoder().decode(b)
const utf8 = (s: string) => new TextEncoder().encode(s)

/** 两个 peer 内存直连，异步投递（模拟真实传输的异步性）。 */
function pair() {
  let a!: RpcPeer
  let b!: RpcPeer
  const sentByA: RpcMessage[] = []
  a = new RpcPeer({
    send: (bytes) => {
      sentByA.push(decodeRpc(bytes))
      queueMicrotask(() => b.handle(bytes))
    },
    timeoutMs: 1000,
  })
  b = new RpcPeer({ send: (bytes) => queueMicrotask(() => a.handle(bytes)), timeoutMs: 1000 })
  return { a, b, sentByA }
}

describe('guards / codec', () => {
  it('三种消息的判别', () => {
    const req = makeRequest(1, 'node.info')
    const note = makeNotification('session.event', { x: 1 })
    const ok = makeResult(1, { a: 1 })
    const err = makeError('s', { code: -32007, message: 'no', data: { code: 'not_found' } })
    assert.deepEqual([isRequest(req), isNotification(req), isResponse(req)], [true, false, false])
    assert.deepEqual(
      [isRequest(note), isNotification(note), isResponse(note)],
      [false, true, false],
    )
    assert.deepEqual([isRequest(ok), isNotification(ok), isResponse(ok)], [false, false, true])
    assert.deepEqual([isRequest(err), isNotification(err), isResponse(err)], [false, false, true])
  })

  it('空 params 编成 {}，不是 null（Rust 的 Empty 是 braced struct）', () => {
    assert.equal(
      text(enc(makeRequest(1, 'node.info'))),
      '{"jsonrpc":"2.0","id":1,"method":"node.info","params":{}}',
    )
  })

  it('decode 拒绝非法消息', () => {
    assert.throws(() => decodeRpc(utf8('not json')), ProtocolError)
    assert.throws(() => decodeRpc(utf8('[]')), ProtocolError)
    assert.throws(() => decodeRpc(utf8('{"jsonrpc":"1.0","id":1,"method":"x"}')), /jsonrpc/)
    assert.throws(() => decodeRpc(utf8('{"jsonrpc":"2.0","method":5}')), /method/)
    assert.throws(() => decodeRpc(utf8('{"jsonrpc":"2.0","result":1}')), /id/)
  })

  it('字符串 id 与非 ASCII 内容往返', () => {
    const msg = makeRequest('c3b1', 'session.send', { content: '跑一下测试 ✅' })
    assert.deepEqual(decodeRpc(enc(msg)), msg)
  })
})

describe('AnyconRpcError', () => {
  it('code 取 data.code，field / reason 透传', () => {
    const e = AnyconRpcError.fromWire({
      code: -32001,
      message: 'invalid settings',
      data: { code: 'invalid_settings', field: 'thinking', reason: 'unsupported' },
    })
    assert.equal(e.code, 'invalid_settings')
    assert.equal(e.field, 'thinking')
    assert.equal(e.reason, 'unsupported')
    assert.equal(e.rpcCode, -32001)
  })

  it('没有 data 时按数字 code 兜底', () => {
    assert.equal(new AnyconRpcError(-32007, 'x').code, 'not_found')
    assert.equal(new AnyconRpcError(-32602, 'x').code, 'invalid')
    assert.equal(new AnyconRpcError(-1, 'x').code, 'internal')
  })

  it('of() / toWire() 往返', () => {
    const wire = AnyconRpcError.of('busy', 'turn in progress').toWire()
    assert.deepEqual(wire, { code: -32004, message: 'turn in progress', data: { code: 'busy' } })
  })
})

describe('RpcPeer', () => {
  it('request / response 关联，并发请求按 id 对应', async () => {
    const { a, b } = pair()
    b.onRequest<{ n: number }, { double: number }>('math.double', async ({ n }) => {
      await new Promise((r) => setTimeout(r, n === 1 ? 20 : 1))
      return { double: n * 2 }
    })
    const [r1, r2] = await Promise.all([
      a.request<{ double: number }>('math.double', { n: 1 }),
      a.request<{ double: number }>('math.double', { n: 2 }),
    ])
    assert.equal(r1.double, 2)
    assert.equal(r2.double, 4)
    assert.equal(a.pending, 0)
  })

  it('错误响应变成 AnyconRpcError', async () => {
    const { a, b } = pair()
    b.onRequest('session.send', () => {
      throw AnyconRpcError.of('invalid_settings', 'bad', {
        field: 'model',
        reason: 'unknown model',
      })
    })
    await assert.rejects(a.request('session.send', {}), (err: unknown) => {
      assert.ok(err instanceof AnyconRpcError)
      assert.equal(err.code, 'invalid_settings')
      assert.equal(err.field, 'model')
      return true
    })
  })

  it('对端没有处理器：unsupported', async () => {
    const { a } = pair()
    await assert.rejects(a.request('nope.nope'), (err: unknown) => {
      assert.ok(err instanceof AnyconRpcError)
      assert.equal(err.code, 'unsupported')
      return true
    })
  })

  it('处理器抛普通 Error：internal', async () => {
    const { a, b } = pair()
    b.onRequest('boom', () => {
      throw new Error('kaput')
    })
    await assert.rejects(a.request('boom'), (err: unknown) => {
      assert.ok(err instanceof AnyconRpcError)
      assert.equal(err.code, 'internal')
      assert.match(err.message, /kaput/)
      return true
    })
  })

  it('通知：按方法订阅、通配订阅、取消订阅；订阅者异常不影响其他订阅者', async () => {
    const { a, b } = pair()
    const got: string[] = []
    const off = a.onNotification<{ text: string }>('session.event', (p) => got.push(`s:${p.text}`))
    a.onNotification('*', (_p, method) => got.push(`*:${method}`))
    a.onNotification('session.event', () => {
      throw new Error('subscriber bug')
    })
    b.notify('session.event', { text: 'hi' })
    b.notify('other', {})
    await new Promise((r) => setTimeout(r, 5))
    off()
    b.notify('session.event', { text: 'again' })
    await new Promise((r) => setTimeout(r, 5))
    assert.deepEqual(got, ['s:hi', '*:session.event', '*:other', '*:session.event'])
  })

  it('请求超时', async () => {
    const peer = new RpcPeer({ send: () => {}, timeoutMs: 15 })
    await assert.rejects(peer.request('slow'), RpcTimeoutError)
    assert.equal(peer.pending, 0)
    // 单次覆盖
    await assert.rejects(peer.request('slow', {}, { timeoutMs: 5 }), /timed out after 5ms/)
  })

  it('AbortSignal 取消', async () => {
    const peer = new RpcPeer({ send: () => {} })
    const ctl = new AbortController()
    const p = peer.request('x', {}, { signal: ctl.signal })
    ctl.abort(new Error('user cancelled'))
    await assert.rejects(p, /user cancelled/)
    assert.equal(peer.pending, 0)
    await assert.rejects(peer.request('x', {}, { signal: ctl.signal }), /user cancelled/)
  })

  it('send 同步抛错：request 拒绝，不留悬挂项', async () => {
    const peer = new RpcPeer({
      send: () => {
        throw new Error('offline')
      },
    })
    await assert.rejects(peer.request('x'), /offline/)
    assert.equal(peer.pending, 0)
  })

  it('failAll 拒绝所有未答复请求', async () => {
    const peer = new RpcPeer({ send: () => {}, timeoutMs: 0 })
    const p1 = peer.request('a')
    const p2 = peer.request('b')
    peer.failAll(new Error('disconnected'))
    await assert.rejects(p1, /disconnected/)
    await assert.rejects(p2, /disconnected/)
  })

  it('未知 id 的响应与重复响应被忽略；非法字节抛 ProtocolError', async () => {
    const peer = new RpcPeer({ send: () => {} })
    peer.handle(enc(makeResult(999, {})))
    assert.throws(() => peer.handle(new TextEncoder().encode('{')), ProtocolError)
  })

  it('请求 id 递增，参数默认 {}', async () => {
    const { a, sentByA } = pair()
    void a.request('one').catch(() => {})
    void a.request('two', { k: 1 }).catch(() => {})
    await new Promise((r) => setTimeout(r, 5))
    assert.deepEqual(
      sentByA.map((m) => (isRequest(m) ? [m.id, m.method, m.params] : null)),
      [
        [1, 'one', {}],
        [2, 'two', { k: 1 }],
      ],
    )
  })
})
