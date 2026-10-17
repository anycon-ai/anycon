import { describe, expect, it } from 'vitest'
import { isNodeId, isSessionId, parsePushPayload, readPushPayload } from './payload'

describe('push payload', () => {
  it('accepts the six categories and the test push', () => {
    for (const category of ['permission_requested', 'turn_finished', 'error', 'session_idle', 'enroll_requested', 'security']) {
      expect(parsePushPayload({ category, node: 'n'.repeat(32), session: null })).toEqual({ category, node: 'n'.repeat(32), session: null })
    }
    expect(parsePushPayload({ category: 'test' })).toEqual({ category: 'test', node: null, session: null })
  })

  it('keeps the session id of a session notification', () => {
    expect(parsePushPayload({ category: 'error', node: 'nn', session: 'sess_aB3' })).toEqual({ category: 'error', node: 'nn', session: 'sess_aB3' })
  })

  it('rejects what it does not understand', () => {
    for (const raw of [null, undefined, 'x', 3, [], {}, { category: 'nope', node: 'n' }, { category: 7, node: 'n' }, { category: 'error' }, { category: 'error', node: '' }]) {
      expect(parsePushPayload(raw)).toBeNull()
    }
  })

  it('treats ids from the hub as untrusted: only opaque-id characters survive', () => {
    for (const bad of ['../x', 'a/b', 'a?b', 'a#b', 'a b', 'a\tb', 'a\nb', '//evil.example', 'a%2Fb', 'a.b', 'x'.repeat(129), '', 5, null]) expect(isNodeId(bad)).toBe(false)
    for (const good of ['sess_aB3', 'a-b_C', 'd'.repeat(32), '0123456789abcdef']) expect(isNodeId(good)).toBe(true)
    for (const bad of ['../x', 'a/b', 'a?b', 'a#b', 'a b', 'a\tb', 'a\nb', '//evil.example', 'a%2Fb', 'x'.repeat(129), '', 5, null]) expect(isSessionId(bad)).toBe(false)
    for (const good of ['sess_aB3', 'a-b_C', 'd'.repeat(32), '0123456789abcdef', 'x'.repeat(128)]) expect(isSessionId(good)).toBe(true)
    // 坏的 session 只是被丢掉（仍显示通知，只是没有会话）；坏的 node 让整条载荷读不懂
    expect(parsePushPayload({ category: 'error', node: 'nn', session: '../x' })).toEqual({ category: 'error', node: 'nn', session: null })
    expect(parsePushPayload({ category: 'error', node: '../x', session: 'sess_1' })).toBeNull()
  })

  it('session ids use the hub’s character set: a dot is allowed, nothing else new', () => {
    // hub 放行 [A-Za-z0-9._-]{1,128}（clean_session_id）；SW 以前不认 `.`，带点的会话 id 会被当成没有会话
    expect(isSessionId('sess.v2_1-a')).toBe(true)
    expect(parsePushPayload({ category: 'error', node: 'nn', session: 'sess.v2_1-a' })).toEqual({ category: 'error', node: 'nn', session: 'sess.v2_1-a' })
    // 节点 id 是 daemon 的设备 id，字符集不变
    expect(parsePushPayload({ category: 'error', node: 'n.n', session: 'sess_1' })).toBeNull()
  })

  it('reads PushEvent.data: no data or non-JSON data is "unreadable", not an exception', () => {
    expect(readPushPayload(null)).toBeNull()
    expect(
      readPushPayload({
        json: () => {
          throw new SyntaxError('Unexpected token')
        },
      }),
    ).toBeNull()
    expect(readPushPayload({ json: () => ({ category: 'test' }) })).toEqual({ category: 'test', node: null, session: null })
  })
})
