import { describe, expect, it } from 'vitest'
import i18n from '@/i18n'
import { enrollPushText, genericPushText, nodeParam, pushText } from './pushText'

describe('push texts for the two enrollment categories', () => {
  const en = i18n.getFixedT('en')
  const zh = i18n.getFixedT('zh')
  const node = { id: 'n'.repeat(32), name: 'mac-mini' }

  it('enroll_requested: no session, opens the Access tab and pops the approval dialog; merged per node by tag', () => {
    const p = enrollPushText(en, 'enroll_requested', node)
    expect(p.title).toBe('A device wants to join mac-mini')
    expect(p.url).toBe('/n/nnnnnnnn/settings?tab=enroll&review=1')
    expect(p.tag).toBe(`enroll_requested:${node.id}`)
    expect(enrollPushText(zh, 'enroll_requested', node).title).toBe('有设备请求接入 mac-mini')
  })

  it('security: the same tab without the dialog', () => {
    const p = enrollPushText(en, 'security', node)
    expect(p.title).toBe('Security alert for mac-mini')
    expect(p.url).toBe('/n/nnnnnnnn/settings?tab=enroll')
    expect(p.tag).toBe(`security:${node.id}`)
  })

  it('falls back to "a node" when the cache has no name (and cleans a hostile one)', () => {
    expect(enrollPushText(en, 'security', { id: 'x'.repeat(32) }).title).toBe('Security alert for a node')
    expect(enrollPushText(en, 'security', { id: 'x'.repeat(32), name: 'a\u{202e}b' }).title).toBe('Security alert for a b')
  })
})

describe('push texts for the session categories and the test push', () => {
  const en = i18n.getFixedT('en')
  const zh = i18n.getFixedT('zh')
  const node = { id: 'n'.repeat(32), name: 'mac-mini' }
  const session = { id: 'sess_abc123', title: 'Fix login' }

  it('permission_requested: names the session and the node, opens the session; one tag per session', () => {
    const p = pushText(en, { category: 'permission_requested', node, session })
    expect(p.title).toBe('“Fix login” is waiting for your approval')
    expect(p.body).toBe('On mac-mini')
    expect(p.url).toBe('/n/nnnnnnnn/s/sess_abc123')
    expect(p.tag).toBe('session:sess_abc123')
    expect(pushText(zh, { category: 'permission_requested', node, session }).title).toBe('“Fix login” 在等你审批')
  })

  it('the other three session categories have their own sentence but share the session tag', () => {
    const titles = (['turn_finished', 'error', 'session_idle'] as const).map((category) => pushText(en, { category, node, session }))
    expect(titles.map((p) => p.title)).toEqual(['“Fix login” finished', '“Fix login” ran into an error', '“Fix login” is idle'])
    expect(new Set(titles.map((p) => p.tag))).toEqual(new Set(['session:sess_abc123']))
  })

  it('falls back to "a session" / "a node" when the cache has no names (and cleans hostile ones)', () => {
    const p = pushText(en, { category: 'permission_requested', node: { id: 'x'.repeat(32) }, session: { id: 'sess_1' } })
    expect(p.title).toBe('A session is waiting for your approval')
    expect(p.body).toBe('On a node')
    const hostile = pushText(en, { category: 'error', node: { id: 'x'.repeat(32), name: 'a\u{202e}b' }, session: { id: 'sess_1', title: 'c\nd' } })
    expect(hostile.title).toBe('“c d” ran into an error')
    expect(hostile.body).toBe('On a b')
  })

  it('a session category without a session opens the node home and merges per node', () => {
    const p = pushText(en, { category: 'error', node })
    expect(p.url).toBe('/n/nnnnnnnn')
    expect(p.tag).toBe(`error:${node.id}`)
  })

  it('test: a fixed title, no node needed, opens the notification settings', () => {
    const p = pushText(en, { category: 'test' })
    expect(p).toMatchObject({ title: 'Anycon test push', tag: 'test', url: '/settings/notifications' })
    expect(pushText(zh, { category: 'test' }).title).toBe('Anycon 测试推送')
  })

  it('the enrollment categories still go through enrollPushText (node settings, review dialog)', () => {
    expect(pushText(en, { category: 'enroll_requested', node })).toEqual(enrollPushText(en, 'enroll_requested', node))
    expect(pushText(en, { category: 'security', node }).url).toBe('/n/nnnnnnnn/settings?tab=enroll')
  })

  it('a session id is encoded into the path, never spliced as-is', () => {
    expect(pushText(en, { category: 'error', node, session: { id: 'a/b?c' } }).url).toBe('/n/nnnnnnnn/s/a%2Fb%3Fc')
  })

  it('the node parameter falls back to the full id when two nodes share the first 8 characters', () => {
    expect(nodeParam('abcdefgh1', ['abcdefgh1', 'abcdefgh2'])).toBe('abcdefgh1')
    expect(nodeParam('abcdefgh1', ['abcdefgh1', 'zzzzzzzz'])).toBe('abcdefgh')
    expect(pushText(en, { category: 'error', node: { id: 'abcdefgh1', param: nodeParam('abcdefgh1', ['abcdefgh1', 'abcdefgh2']) } }).url).toBe('/n/abcdefgh1')
  })

  it('the generic text is what an unreadable push shows', () => {
    expect(genericPushText(en)).toEqual({ title: 'Anycon has something new', tag: 'anycon', url: '/' })
  })
})
