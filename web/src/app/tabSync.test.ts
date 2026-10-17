// 多标签页 wipe 同步：发起 wipe 的标签页广播，其它标签页收到后停止写库、停传输层并跳到登录页。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import * as db from '@/db'
import { announceWiped, watchForWipe } from './tabSync'

afterEach(() => vi.restoreAllMocks())

describe('tabSync', () => {
  it('a wipe announced by this tab reaches a watcher in another tab over BroadcastChannel', async () => {
    expect(typeof BroadcastChannel).toBe('function')
    const handler = vi.fn()
    // 「另一个标签页」：同名的另一个 BroadcastChannel 实例（同一个实例收不到自己发的）
    const other = new BroadcastChannel('anycon.tabs')
    const got = new Promise<void>((resolve) => {
      other.onmessage = (ev: MessageEvent<unknown>) => {
        if ((ev.data as { type?: string }).type === 'wiped') resolve()
      }
    })
    const stop = watchForWipe(handler)
    announceWiped()
    await got
    other.close()
    stop()
  })

  it('a watcher is told when another tab broadcasts', async () => {
    const handler = vi.fn()
    const stop = watchForWipe(handler)
    const other = new BroadcastChannel('anycon.tabs')
    other.postMessage({ type: 'wiped' })
    await vi.waitFor(() => expect(handler).toHaveBeenCalledTimes(1))
    // 不认识的消息不算
    other.postMessage({ type: 'something-else' })
    other.postMessage('wiped')
    await new Promise((r) => setTimeout(r, 30))
    expect(handler).toHaveBeenCalledTimes(1)
    stop() // 取消监听之后不再通知
    other.postMessage({ type: 'wiped' })
    await new Promise((r) => setTimeout(r, 30))
    expect(handler).toHaveBeenCalledTimes(1)
    other.close()
  })

  it('falls back to the storage event (set, then remove: nothing is left behind)', () => {
    const handler = vi.fn()
    const stop = watchForWipe(handler)
    window.dispatchEvent(new StorageEvent('storage', { key: 'anycon.wipe_signal', newValue: '123' }))
    expect(handler).toHaveBeenCalledTimes(1)
    // 删除信号键时的事件（newValue = null）和无关的键都不触发
    window.dispatchEvent(new StorageEvent('storage', { key: 'anycon.wipe_signal', newValue: null }))
    window.dispatchEvent(new StorageEvent('storage', { key: 'anycon.prefs', newValue: 'x' }))
    expect(handler).toHaveBeenCalledTimes(1)
    announceWiped()
    expect(localStorage.getItem('anycon.wipe_signal')).toBeNull()
    expect(localStorage.length).toBe(0)
    stop()
  })
})

describe('handleWipedElsewhere', () => {
  beforeEach(async () => {
    vi.resetModules()
    await db.resetDb()
  })

  it('disables the local database, stops the transport and drafts, and leaves for /login — only once', async () => {
    const stopClient = vi.fn()
    vi.doMock('@/transport/instance', () => ({ getClient: () => ({ stop: stopClient }), rpc: () => ({}) }))
    const dbMod = await import('@/db')
    const draftsMod = await import('@/stores/drafts')
    const { handleWipedElsewhere } = await import('./bootstrap')
    await dbMod.putDraft({ session_id: 's1', text: 'x', unsent: [] }) // 库正常可写
    draftsMod.useDrafts.setState({ byKey: { s1: { session_id: 's1', text: 'x', unsent: [] } } })
    const leave = vi.fn()

    handleWipedElsewhere(leave)
    handleWipedElsewhere(leave) // 重复通知只处理一次

    expect(leave).toHaveBeenCalledTimes(1)
    expect(stopClient).toHaveBeenCalledTimes(1)
    expect(draftsMod.useDrafts.getState().byKey).toEqual({})
    // 之后写库一律失败，不会把被删掉的库又建出来
    await expect(dbMod.putIdentity({ id: 'self', static_keypair: { public: new Uint8Array(32), secret: new Uint8Array(32) }, device_name: 'x' })).rejects.toBeInstanceOf(dbMod.DbDisabledError)
    await expect(dbMod.putDraft({ session_id: 's2', text: 'y', unsent: [] })).rejects.toBeInstanceOf(dbMod.DbDisabledError)
    vi.doUnmock('@/transport/instance')
  })

  it('an IndexedDB versionchange (another tab deleting the database) also triggers it, even without the broadcast', async () => {
    const dbMod = await import('@/db')
    const handler = vi.fn()
    dbMod.setBlockedHandler(handler)
    await dbMod.getDb() // 打开连接
    // 另一个标签页要删库：本标签页的连接收到 versionchange，库被禁用，回调被调用
    const deleting = indexedDB.deleteDatabase(dbMod.DB_NAME)
    await new Promise<void>((resolve) => {
      deleting.onsuccess = () => resolve()
      deleting.onerror = () => resolve()
    })
    expect(handler).toHaveBeenCalledTimes(1)
    await expect(dbMod.getDb()).rejects.toBeInstanceOf(dbMod.DbDisabledError)
    expect(await indexedDB.databases().then((l) => l.map((d) => d.name))).not.toContain(dbMod.DB_NAME) // 没有被重新建出来
    dbMod.setBlockedHandler(null)
  })
})

describe('the wiping tab', () => {
  it('announces before it deletes', async () => {
    const order: string[] = []
    const ch = new BroadcastChannel('anycon.tabs')
    ch.onmessage = () => order.push('message')
    vi.resetModules()
    vi.doMock('@/transport/instance', () => ({ getClient: () => ({ stop: () => {} }), rpc: () => ({}) }))
    const { wipeLocalData } = await import('./session')
    await wipeLocalData()
    await vi.waitFor(() => expect(order).toEqual(['message']))
    ch.close()
    vi.doUnmock('@/transport/instance')
  })
})
