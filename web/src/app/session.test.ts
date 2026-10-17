import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type * as Instance from '@/transport/instance'
import * as db from '@/db'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { installFakePush, uninstallFakePush } from '@/test/fakePush'
import { writeSwLanguage } from '@/sw/directory'
import { useDrafts } from '@/stores/drafts'
import { useNodes } from '@/stores/nodes'
import { setRpc } from '@/transport/instance'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { forgetSystemNotifyPrefs, pushEnabledFor } from '@/features/push/prefsCache'
import { resetClientState, signOut, signOutAndRemoveDevice, wipeLocalData } from './session'

// 退出并移除本设备只用 client 的 snapshot 和 stop；真的 AnyconClient 不需要
const client = vi.hoisted(() => ({ snapshot: { paired: true, deviceId: 'dev1' as string | null } }))
vi.mock('@/transport/instance', async (orig) => {
  const mod = await orig<typeof Instance>()
  return { ...mod, getClient: () => ({ snapshot: client.snapshot, stop: () => {} }) }
})

beforeEach(async () => {
  client.snapshot = { paired: true, deviceId: 'dev1' }
  await db.resetDb()
  localStorage.clear()
})

describe('wipeLocalData', () => {
  it('deletes the IndexedDB (private key, token, sessions, drafts) and clears localStorage / sessionStorage', async () => {
    await db.putIdentity({ id: 'self', device_id: 'dev1', device_token: 'tok', static_keypair: { public: new Uint8Array(32), secret: new Uint8Array(32).fill(9) }, device_name: 'x' })
    await db.putNode({ node_id: 'n1', name: 'n', public_key: new Uint8Array(32), paired_at: 1, last_seen: 1 })
    await db.putDraft({ session_id: 's1', text: 'unsent', unsent: [] })
    localStorage.setItem('anycon.prefs', '{"state":{"theme":"dark"}}')
    sessionStorage.setItem('k', 'v')
    useDrafts.setState({ byKey: { s1: { session_id: 's1', text: 'unsent', unsent: [] } } })
    expect((await db.getIdentity())?.device_token).toBe('tok')

    await wipeLocalData()

    expect(localStorage.length).toBe(0)
    expect(sessionStorage.length).toBe(0)
    expect(useDrafts.getState().byKey).toEqual({})
    const names = (await indexedDB.databases()).map((d) => d.name)
    expect(names).not.toContain(db.DB_NAME)
    // 再打开得到的是一个全空的库
    expect(await db.getIdentity()).toBeUndefined()
    expect(await db.listNodes()).toEqual([])
    expect(await db.listDrafts()).toEqual([])
  })

  it('also cancels this browser’s push subscription and forgets the notification language (a shared computer must not keep notifying)', async () => {
    const push = installFakePush({ permission: 'granted' })
    const sub = push.makeSubscription()
    push.subscription = sub
    await writeSwLanguage('zh')
    try {
      await wipeLocalData()
      expect(sub.unsubscribe).toHaveBeenCalledTimes(1)
      expect(push.subscription).toBeNull()
      expect((await indexedDB.databases()).map((d) => d.name)).not.toContain('anycon-sw')
    } finally {
      uninstallFakePush()
    }
  })

  it('a push subscription that refuses to go away does not stop the wipe', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const push = installFakePush({ permission: 'granted' })
    const sub = push.makeSubscription()
    sub.unsubscribe = vi.fn(async () => {
      throw new Error('push service unreachable')
    })
    push.subscription = sub
    try {
      await wipeLocalData()
      expect((await indexedDB.databases()).map((d) => d.name)).not.toContain(db.DB_NAME)
    } finally {
      uninstallFakePush()
    }
  })
})

describe('signOut (plain sign-out keeps the device, but not the push subscription)', () => {
  const row = (id: string, device_id: string) => ({ id, device_id, endpoint_host: 'fcm.googleapis.com', created_at_ms: 1 })
  afterEach(() => uninstallFakePush())

  it('cancels the browser subscription and deletes the hub row BEFORE ending the session (deleting needs the login), then logs out', async () => {
    const push = installFakePush({ permission: 'granted' })
    const sub = push.makeSubscription()
    push.subscription = sub
    const order: string[] = []
    sub.unsubscribe = vi.fn(async () => {
      order.push('browser unsubscribe')
      push.subscription = null
      return true
    })
    const api = installFakeApi({
      'GET /api/push/subscriptions': () => [row('mine', 'dev1'), row('theirs', 'dev2')],
      'DELETE /api/push/subscriptions/*': (c) => {
        order.push(`DELETE ${c.path}`)
        return undefined
      },
      'POST /auth/logout': () => {
        order.push('POST /auth/logout')
        return undefined
      },
    })
    await signOut()
    expect(order.at(-1)).toBe('POST /auth/logout')
    expect(order).toContain('browser unsubscribe')
    expect(order).toContain('DELETE /api/push/subscriptions/mine')
    expect(order).not.toContain('DELETE /api/push/subscriptions/theirs')
    expect(api.callsTo('POST /auth/logout')).toHaveLength(1)
    // 设备本身、密钥、缓存都还在（这是「退出登录」，不是「退出并移除本设备」）
    expect(push.subscription).toBeNull()
  })

  it('a push cleanup that fails does not keep you signed in', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const push = installFakePush({ permission: 'granted' })
    push.subscription = push.makeSubscription()
    const api = installFakeApi({ 'GET /api/push/subscriptions': () => ({ status: 500, body: errorBody('internal') }), 'POST /auth/logout': () => undefined })
    await signOut()
    expect(api.callsTo('POST /auth/logout')).toHaveLength(1)
  })

  it('a failing logout is reported to the caller (the page says "you are still signed in")', async () => {
    installFakeApi({ 'POST /auth/logout': () => ({ status: 500, body: errorBody('internal') }) })
    await expect(signOut()).rejects.toMatchObject({ status: 500 })
  })

  it('works in a browser without push (no requests but the logout)', async () => {
    const api = installFakeApi({ 'POST /auth/logout': () => undefined })
    await signOut()
    expect(api.calls.map((c) => c.path)).toEqual(['/auth/logout'])
  })

  it('looks up the device id in the saved identity when the client has not got one yet', async () => {
    client.snapshot = { paired: true, deviceId: null }
    await db.putIdentity({ id: 'self', device_id: 'dev1', static_keypair: { public: new Uint8Array(32), secret: new Uint8Array(32) }, device_name: 'x' })
    const push = installFakePush({ permission: 'granted' })
    push.subscription = push.makeSubscription()
    const api = installFakeApi({ 'GET /api/push/subscriptions': () => [row('mine', 'dev1')], 'DELETE /api/push/subscriptions/mine': () => undefined, 'POST /auth/logout': () => undefined })
    await signOut()
    expect(api.callsTo('DELETE /api/push/subscriptions/mine')).toHaveLength(1)
  })
})

describe('resetClientState', () => {
  it('forgets the cached notification preferences (they belong to the account that just signed out)', async () => {
    installFakeApi({ 'GET /api/me/notifications': () => ({ categories: [{ category: 'error', push: false, email: false }], email_available: false, push_available: true }) })
    forgetSystemNotifyPrefs()
    expect(await pushEnabledFor('error')).toBe(false)
    installFakeApi({ 'GET /api/me/notifications': () => ({ categories: [{ category: 'error', push: true, email: false }], email_available: false, push_available: true }) })
    expect(await pushEnabledFor('error')).toBe(false) // 还在缓存里
    await resetClientState()
    expect(await pushEnabledFor('error')).toBe(true) // 重新读，是新账号的值
    forgetSystemNotifyPrefs()
  })
})

describe('signOutAndRemoveDevice', () => {
  let rpc: FakeRpc
  beforeEach(() => {
    rpc = new FakeRpc()
    setRpc(rpc)
    useNodes.setState({ order: ['n1', 'n2'], byId: {}, loaded: true })
    vi.stubGlobal('fetch', vi.fn(async () => new Response(null, { status: 204 })))
  })
  afterEach(() => {
    setRpc(null)
    useNodes.setState({ byId: {}, order: [], loaded: false })
    vi.unstubAllGlobals()
  })

  it('a node answering not_found (the key was already removed by an earlier attempt) counts as revoked, not as "still holds the key"', async () => {
    rpc.handlers['node.revoke_device'] = (_p, nodeId) => {
      if (nodeId === 'n1') throw rpcError('not_found')
      return {}
    }
    const report = await signOutAndRemoveDevice()
    expect(report).toEqual({ revokedOnNodes: ['n1', 'n2'], skippedNodes: [] })
    expect(rpc.calls.map((c) => c.nodeId)).toEqual(['n1', 'n2'])
  })

  it('any other node error still stops before the hub and the wipe', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    rpc.handlers['node.revoke_device'] = () => {
      throw rpcError('internal')
    }
    await expect(signOutAndRemoveDevice()).rejects.toMatchObject({ step: 'nodes', failedNodes: ['n1', 'n2'] })
    expect(fetch).not.toHaveBeenCalled()
  })
})
