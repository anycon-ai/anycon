// service worker 读页面 IndexedDB 的那一层：读得到缓存、绝不创建主库、用完就放手。
import { afterEach, describe, expect, it } from 'vitest'
import { getDb, putIdentity, putNode, putSessions, resetDb, type NodeRecord, type SessionRecord } from '@/db'
import { readDeviceId, readPushDirectory, readSwLanguage, syncSwLanguage, writeSwLanguage } from './directory'

afterEach(async () => {
  await resetDb()
  indexedDB.deleteDatabase('anycon-sw')
})

const node = (id: string, name: string): NodeRecord => ({ node_id: id, name, public_key: new Uint8Array(32), paired_at: 1, last_seen: 1 })
const session = (id: string, nodeId: string, title: string): SessionRecord =>
  ({ session_id: id, node_id: nodeId, project: null, agent: 'claude', title, status: 'idle', options: {}, last_seq: 0, updated_at: 1, low_seq: 0, summary: {} }) as unknown as SessionRecord

async function databaseNames(): Promise<string[]> {
  return (await indexedDB.databases()).map((d) => d.name ?? '')
}

describe('readPushDirectory', () => {
  it('reads the node name, the session title and the paired node ids from the page database', async () => {
    await putNode(node('a'.repeat(32), 'mac-mini'))
    await putNode(node('b'.repeat(32), 'nas'))
    await putSessions([session('sess_1', 'a'.repeat(32), 'Fix login')])
    expect(await readPushDirectory('a'.repeat(32), 'sess_1')).toEqual({ nodeName: 'mac-mini', sessionTitle: 'Fix login', nodeIds: ['a'.repeat(32), 'b'.repeat(32)] })
  })

  it('a session that belongs to another node gets no title: the notification says "a session"', async () => {
    await putNode(node('a'.repeat(32), 'mac-mini'))
    await putNode(node('b'.repeat(32), 'nas'))
    await putSessions([session('sess_b', 'b'.repeat(32), 'Secret plan'), session('sess_a', 'a'.repeat(32), 'Fix login')])
    // 载荷说「节点 A 的会话 sess_b」，但 sess_b 在缓存里属于 B
    expect(await readPushDirectory('a'.repeat(32), 'sess_b')).toEqual({ nodeName: 'mac-mini', nodeIds: ['a'.repeat(32), 'b'.repeat(32)] })
    expect(await readPushDirectory('b'.repeat(32), 'sess_b')).toMatchObject({ nodeName: 'nas', sessionTitle: 'Secret plan' })
    // 没有节点就没法核对：不给标题
    expect(await readPushDirectory(null, 'sess_a')).toEqual({ nodeIds: ['a'.repeat(32), 'b'.repeat(32)] })
  })

  it('a cached session without a node_id (older page version) gets no title either', async () => {
    await putNode(node('a'.repeat(32), 'mac-mini'))
    const db = await getDb()
    await db.put('sessions', { session_id: 'sess_odd', title: 'Odd one' } as unknown as SessionRecord)
    expect(await readPushDirectory('a'.repeat(32), 'sess_odd')).toEqual({ nodeName: 'mac-mini', nodeIds: ['a'.repeat(32)] })
  })

  it('leaves out what is not cached (unknown node / session)', async () => {
    await putNode(node('a'.repeat(32), 'mac-mini'))
    expect(await readPushDirectory('z'.repeat(32), 'sess_nope')).toEqual({ nodeIds: ['a'.repeat(32)] })
    expect(await readPushDirectory(null, null)).toEqual({ nodeIds: ['a'.repeat(32)] })
  })

  it('ignores a record that is not shaped like the page writes it', async () => {
    const db = await getDb()
    await db.put('nodes', { node_id: 'odd', name: 42 } as unknown as NodeRecord)
    expect(await readPushDirectory('odd', null)).toEqual({ nodeIds: ['odd'] })
  })

  it('never creates the page database: with no database it returns nothing and none exists afterwards', async () => {
    expect(await databaseNames()).not.toContain('anycon')
    expect(await readPushDirectory('a'.repeat(32), 'sess_1')).toEqual({ nodeIds: [] })
    expect(await readDeviceId()).toBeNull()
    expect(await databaseNames()).not.toContain('anycon')
    // 页面之后照常建库（升级回调还会跑，stores 都在）
    await putNode(node('a'.repeat(32), 'mac-mini'))
    expect(await readPushDirectory('a'.repeat(32), null)).toMatchObject({ nodeName: 'mac-mini' })
  })

  it('does not hold the connection: the page can delete its database right after a read', async () => {
    await putNode(node('a'.repeat(32), 'mac-mini'))
    await readPushDirectory('a'.repeat(32), null)
    await readDeviceId()
    // 页面自己的连接先让出，再删库——如果 service worker 的连接还开着，deleteDB 会一直等
    const done = await Promise.race([resetDb().then(() => 'deleted'), new Promise<string>((r) => setTimeout(() => r('blocked'), 1000))])
    expect(done).toBe('deleted')
  })
})

describe('readDeviceId', () => {
  it('returns the hub-assigned web device id of this browser', async () => {
    await putIdentity({ id: 'self', device_id: 'dev123', static_keypair: { public: new Uint8Array(32), secret: new Uint8Array(32) }, device_name: 'Chrome' })
    expect(await readDeviceId()).toBe('dev123')
  })
  it('is null for a browser that never paired', async () => {
    await putIdentity({ id: 'self', static_keypair: { public: new Uint8Array(32), secret: new Uint8Array(32) }, device_name: 'Chrome' })
    expect(await readDeviceId()).toBeNull()
  })
})

describe('language store', () => {
  it('round-trips the language the page wrote, in a database of its own', async () => {
    expect(await readSwLanguage()).toBeUndefined()
    await writeSwLanguage('zh')
    expect(await readSwLanguage()).toBe('zh')
    await writeSwLanguage('en')
    expect(await readSwLanguage()).toBe('en')
    expect(await databaseNames()).not.toContain('anycon')
  })

  it('reading never creates the database (nothing was stored for "auto")', async () => {
    expect(await readSwLanguage()).toBeUndefined()
    expect(await databaseNames()).not.toContain('anycon-sw')
  })

  it('syncSwLanguage stores a picked language, and "auto" removes it (the service worker then follows navigator.language)', async () => {
    await syncSwLanguage('zh')
    expect(await readSwLanguage()).toBe('zh')
    await syncSwLanguage('auto')
    expect(await readSwLanguage()).toBeUndefined()
    expect(await databaseNames()).not.toContain('anycon-sw')
    await syncSwLanguage('auto') // 本来就没有：不报错
    expect(await databaseNames()).not.toContain('anycon-sw')
  })
})
