// 页面连着 hub 但在后台时的系统通知（spec/web.md § 7）：什么时候弹、弹什么。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import type { NotifyBody } from '@/protocol'
import { forgetSystemNotifyPrefs } from '@/features/push/prefsCache'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { installFakePush, uninstallFakePush, type FakePush } from '@/test/fakePush'
import { nodeEntry, summary } from '@/test/factories'
import { defaultEnv, showSystemNotification, textForNotify, type SystemNotifyEnv } from './systemNotify'

const NODE = 'n'.repeat(32)
const body = (over: Partial<NotifyBody> = {}): NotifyBody => ({ category: 'permission_requested', session_id: 'sess_abc', ...over })

function fakeEnv(over: Partial<SystemNotifyEnv> = {}) {
  const shown: Array<{ title: string; options: unknown }> = []
  const env: SystemNotifyEnv = {
    visibility: () => 'hidden',
    permission: () => 'granted',
    registration: async () => ({ pushManager: { getSubscription: async () => ({}) }, showNotification: async (title, options) => void shown.push({ title, options }) }),
    pushEnabled: async () => true,
    t: i18n.getFixedT('en'),
    nodeName: (id) => (id === NODE ? 'mac-mini' : undefined),
    sessionTitle: (_n, s) => (s === 'sess_abc' ? 'Fix login' : undefined),
    nodeIds: () => [NODE],
    ...over,
  }
  return { env, shown }
}

describe('showSystemNotification — when', () => {
  it('page hidden + permission granted + category on: shows the same notification a push would (text, tag, click URL)', async () => {
    const { env, shown } = fakeEnv()
    expect(await showSystemNotification(NODE, body(), env)).toBe(true)
    expect(shown).toHaveLength(1)
    expect(shown[0]!.title).toBe('“Fix login” is waiting for your approval')
    expect(shown[0]!.options).toMatchObject({ body: 'On mac-mini', tag: 'session:sess_abc', renotify: true, icon: '/icon-192.png', data: { url: '/n/nnnnnnnn/s/sess_abc' } })
  })

  it('page in the foreground: no system notification (the toast / banner covers it)', async () => {
    const { env, shown } = fakeEnv({ visibility: () => 'visible' })
    expect(await showSystemNotification(NODE, body(), env)).toBe(false)
    expect(shown).toHaveLength(0)
  })

  it.each(['default', 'denied', 'unsupported'] as const)('permission %s: nothing', async (permission) => {
    const { env, shown } = fakeEnv({ permission: () => permission })
    expect(await showSystemNotification(NODE, body(), env)).toBe(false)
    expect(shown).toHaveLength(0)
  })

  it('the account turned push off for that category: nothing, and the check is for the right category', async () => {
    const asked: string[] = []
    const { env, shown } = fakeEnv({
      pushEnabled: async (c) => {
        asked.push(c)
        return false
      },
    })
    expect(await showSystemNotification(NODE, body({ category: 'turn_finished' }), env)).toBe(false)
    expect(asked).toEqual(['turn_finished'])
    expect(shown).toHaveLength(0)
  })

  it('the page came back to the foreground while the preferences were loading: nothing', async () => {
    let calls = 0
    const { env, shown } = fakeEnv({ visibility: () => (++calls === 1 ? 'hidden' : 'visible') })
    expect(await showSystemNotification(NODE, body(), env)).toBe(false)
    expect(shown).toHaveLength(0)
  })

  it('push is off in this browser (no subscription): nothing, even though the notification permission is still granted', async () => {
    const asked: string[] = []
    const { env, shown } = fakeEnv({
      registration: async () => ({ pushManager: { getSubscription: async () => null }, showNotification: async (title, options) => void shown.push({ title, options }) }),
      pushEnabled: async (c) => {
        asked.push(c)
        return true
      },
    })
    expect(await showSystemNotification(NODE, body(), env)).toBe(false)
    expect(shown).toHaveLength(0)
    expect(asked).toEqual([]) // 连偏好都不用去读
  })

  it('a subscription that cannot be read counts as "push is off"', async () => {
    vi.spyOn(console, 'debug').mockImplementation(() => undefined)
    const { env, shown } = fakeEnv({
      registration: async () => ({
        pushManager: {
          getSubscription: async () => {
            throw new Error('no push manager')
          },
        },
        showNotification: async (title, options) => void shown.push({ title, options }),
      }),
    })
    expect(await showSystemNotification(NODE, body(), env)).toBe(false)
    expect(shown).toHaveLength(0)
  })

  it('no service worker (dev server): nothing, no error', async () => {
    const { env } = fakeEnv({ registration: async () => null })
    expect(await showSystemNotification(NODE, body(), env)).toBe(false)
  })
})

describe('textForNotify', () => {
  const env = fakeEnv().env

  it('enrollment categories: no session, the Access tab (with the review dialog for enroll_requested), merged per node', () => {
    expect(textForNotify(env, NODE, { category: 'enroll_requested', session_id: null })).toEqual({ title: 'A device wants to join mac-mini', tag: `enroll_requested:${NODE}`, url: '/n/nnnnnnnn/settings?tab=enroll&review=1' })
    expect(textForNotify(env, NODE, { category: 'security', session_id: null }).url).toBe('/n/nnnnnnnn/settings?tab=enroll')
  })

  it('falls back to "a session" when the title is not cached', () => {
    expect(textForNotify(env, NODE, body({ session_id: 'sess_unknown' })).title).toBe('A session is waiting for your approval')
  })

  it('uses the full node id in the URL when two paired nodes share the first 8 characters', () => {
    const other = `nnnnnnnn${'z'.repeat(24)}`
    const clash = fakeEnv({ nodeIds: () => [NODE, other] }).env
    expect(textForNotify(clash, NODE, body()).url).toBe(`/n/${NODE}/s/sess_abc`)
  })
})

describe('defaultEnv — real stores, real service worker registration', () => {
  let push: FakePush
  beforeEach(() => {
    forgetSystemNotifyPrefs()
    push = installFakePush({ permission: 'granted' })
    push.subscription = push.makeSubscription() // 本浏览器开着推送
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' })
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'mac-mini' }) }, order: [NODE], loaded: true })
    useSessions.setState({ list: { sess_abc: { ...summary({ id: 'sess_abc', title: 'Fix login' }), node_id: NODE, cached_seq: 0 } } })
    installFakeApi({ 'GET /api/me/notifications': () => ({ categories: [{ category: 'permission_requested', push: true, email: true }, { category: 'turn_finished', push: false, email: false }], email_available: true, push_available: true }) })
  })
  afterEach(() => {
    Reflect.deleteProperty(document, 'visibilityState')
    uninstallFakePush()
    useSessions.setState({ list: {} })
    useNodes.setState({ byId: {}, order: [] })
  })

  it('shows through registration.showNotification with the cached node name and session title', async () => {
    expect(await showSystemNotification(NODE, body(), defaultEnv())).toBe(true)
    expect(push.shown).toHaveLength(1)
    expect(push.shown[0]!.title).toBe('“Fix login” is waiting for your approval')
    expect(push.shown[0]!.options).toMatchObject({ body: 'On mac-mini', tag: 'session:sess_abc', data: { url: '/n/nnnnnnnn/s/sess_abc' } })
  })

  it('respects the account’s push switch for the category', async () => {
    expect(await showSystemNotification(NODE, body({ category: 'turn_finished' }), defaultEnv())).toBe(false)
    expect(push.shown).toHaveLength(0)
  })

  it('does not borrow a title from a session of another node', async () => {
    const other = 'o'.repeat(32)
    useSessions.setState({ list: { sess_abc: { ...summary({ id: 'sess_abc', title: 'Secret plan' }), node_id: other, cached_seq: 0 } } })
    await showSystemNotification(NODE, body(), defaultEnv())
    expect(push.shown[0]!.title).toBe('A session is waiting for your approval')
  })

  it('turning push off in this browser (the subscription is gone) silences it, although the permission is still granted', async () => {
    push.subscription = null
    expect(await showSystemNotification(NODE, body(), defaultEnv())).toBe(false)
    expect(push.shown).toHaveLength(0)
    push.subscription = push.makeSubscription()
    expect(await showSystemNotification(NODE, body(), defaultEnv())).toBe(true)
    expect(push.shown).toHaveLength(1)
  })

  it('stays quiet when the page is in the foreground', async () => {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'visible' })
    expect(await showSystemNotification(NODE, body(), defaultEnv())).toBe(false)
    expect(push.shown).toHaveLength(0)
  })

  it('a hub that cannot be asked for the preferences still gets the notification', async () => {
    forgetSystemNotifyPrefs()
    vi.spyOn(console, 'debug').mockImplementation(() => undefined)
    installFakeApi({ 'GET /api/me/notifications': () => ({ status: 500, body: errorBody('internal') }) })
    expect(await showSystemNotification(NODE, body({ category: 'turn_finished' }), defaultEnv())).toBe(true)
  })
})
