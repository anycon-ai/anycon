import { describe, expect, it, vi } from 'vitest'
import { pushTranslator } from './i18n'
import type { PushDirectory } from './directory'
import { handlePush, type PushDeps } from './push'
import type { ShowNotificationOptions } from './types'

const NODE = 'n'.repeat(32)

interface Shown {
  title: string
  options: ShowNotificationOptions
}

function setup(over: Partial<PushDeps> & { dir?: PushDirectory; stored?: string; navLang?: string } = {}) {
  const shown: Shown[] = []
  const deps: PushDeps = {
    directory: over.directory ?? (async () => over.dir ?? { nodeIds: [] }),
    language: over.language ?? (async () => over.stored),
    navigatorLanguage: () => over.navLang ?? 'en-US',
    translator: pushTranslator,
    show: async (title, options) => void shown.push({ title, options }),
  }
  return { deps, shown }
}
const data = (payload: unknown) => ({ json: () => payload })

describe('handlePush', () => {
  it('builds the notification from the cached node name and session title (text, tag, click URL)', async () => {
    const { deps, shown } = setup({ dir: { nodeName: 'mac-mini', sessionTitle: 'Fix login', nodeIds: [NODE] } })
    await handlePush(data({ category: 'permission_requested', node: NODE, session: 'sess_abc' }), deps)
    expect(shown).toHaveLength(1)
    expect(shown[0]!.title).toBe('“Fix login” is waiting for your approval')
    expect(shown[0]!.options).toMatchObject({ body: 'On mac-mini', tag: 'session:sess_abc', renotify: true, icon: '/icon-192.png', lang: 'en', data: { url: '/n/nnnnnnnn/s/sess_abc' } })
  })

  it('asks the cache about exactly the node and session in the payload', async () => {
    const directory = vi.fn(async () => ({ nodeIds: [] }))
    const { deps } = setup({ directory })
    await handlePush(data({ category: 'error', node: NODE, session: 'sess_abc' }), deps)
    expect(directory).toHaveBeenCalledWith(NODE, 'sess_abc')
  })

  it('speaks the language the page stored, else the browser language', async () => {
    const stored = setup({ stored: 'zh', navLang: 'en-US', dir: { sessionTitle: 'Fix login', nodeIds: [] } })
    await handlePush(data({ category: 'turn_finished', node: NODE, session: 'sess_abc' }), stored.deps)
    expect(stored.shown[0]!.title).toBe('“Fix login” 已完成')
    expect(stored.shown[0]!.options.lang).toBe('zh')
    const nav = setup({ navLang: 'zh-CN' })
    await handlePush(data({ category: 'session_idle', node: NODE, session: 'sess_abc' }), nav.deps)
    expect(nav.shown[0]!.title).toBe('一个会话 空闲了')
  })

  it('falls back to "a session" / "a node" when nothing is cached', async () => {
    const { deps, shown } = setup()
    await handlePush(data({ category: 'permission_requested', node: NODE, session: 'sess_abc' }), deps)
    expect(shown[0]!.title).toBe('A session is waiting for your approval')
    expect(shown[0]!.options.body).toBe('On a node')
  })

  it('still shows a notification when the cache cannot be read', async () => {
    const { deps, shown } = setup({
      directory: async () => {
        throw new Error('IndexedDB is gone')
      },
      language: async () => {
        throw new Error('IndexedDB is gone')
      },
    })
    await handlePush(data({ category: 'error', node: NODE, session: 'sess_abc' }), deps)
    expect(shown).toHaveLength(1)
    expect(shown[0]!.title).toBe('A session ran into an error')
  })

  it('test push: a fixed title, tag "test", opens the notification settings, no node needed', async () => {
    const { deps, shown } = setup()
    await handlePush(data({ category: 'test' }), deps)
    expect(shown[0]).toMatchObject({ title: 'Anycon test push', options: { body: 'Push notifications work on this browser.', tag: 'test', data: { url: '/settings/notifications' } } })
  })

  it('enrollment categories: merged per node, the Access tab (with the approval dialog for enroll_requested)', async () => {
    const { deps, shown } = setup({ dir: { nodeName: 'mac-mini', nodeIds: [NODE] } })
    await handlePush(data({ category: 'enroll_requested', node: NODE, session: null }), deps)
    await handlePush(data({ category: 'security', node: NODE, session: null }), deps)
    expect(shown.map((s) => s.title)).toEqual(['A device wants to join mac-mini', 'Security alert for mac-mini'])
    expect(shown.map((s) => s.options.tag)).toEqual([`enroll_requested:${NODE}`, `security:${NODE}`])
    expect(shown.map((s) => s.options.data)).toEqual([{ url: '/n/nnnnnnnn/settings?tab=enroll&review=1' }, { url: '/n/nnnnnnnn/settings?tab=enroll' }])
  })

  it('uses the full node id in the URL when two paired nodes share the first 8 characters', async () => {
    const other = `nnnnnnnn${'x'.repeat(24)}`
    const { deps, shown } = setup({ dir: { nodeIds: [NODE, other] } })
    await handlePush(data({ category: 'error', node: NODE, session: 'sess_abc' }), deps)
    expect(shown[0]!.options.data).toEqual({ url: `/n/${NODE}/s/sess_abc` })
  })

  it.each([
    ['no data', null],
    ['bad JSON', { json: () => { throw new SyntaxError('x') } }],
    ['an unknown category', data({ category: 'launch_missiles', node: NODE })],
    ['a hostile node id', data({ category: 'error', node: '../../x', session: 'sess_1' })],
    ['not an object', data('hello')],
  ])('%s: still shows the generic notification (every push must be visible)', async (_name, d) => {
    const { deps, shown } = setup()
    await handlePush(d, deps)
    expect(shown).toHaveLength(1)
    expect(shown[0]).toMatchObject({ title: 'Anycon has something new', options: { tag: 'anycon', data: { url: '/' } } })
  })

  it('a session id with a dot (the hub allows [A-Za-z0-9._-]) is kept: its own tag and click URL', async () => {
    const { deps, shown } = setup({ dir: { sessionTitle: 'Fix login', nodeIds: [NODE] } })
    await handlePush(data({ category: 'error', node: NODE, session: 'sess.v2_1' }), deps)
    expect(shown[0]!.title).toBe('“Fix login” ran into an error')
    expect(shown[0]!.options).toMatchObject({ tag: 'session:sess.v2_1', data: { url: '/n/nnnnnnnn/s/sess.v2_1' } })
  })

  it('a hostile session id only loses the session (the node home opens instead of a crafted path)', async () => {
    const { deps, shown } = setup({ dir: { nodeIds: [NODE] } })
    await handlePush(data({ category: 'error', node: NODE, session: '../../etc' }), deps)
    expect(shown[0]!.options.data).toEqual({ url: '/n/nnnnnnnn' })
  })
})
