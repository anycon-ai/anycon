// 会话历史缓存的上限：按这个浏览器最近打开的时间留 KEEP_SESSIONS 个会话、每个最多 MAX_EVENTS_PER_SESSION 条；
// 打开着的会话不动；没有打开记录的（升级前缓存的）最先淘汰；最多每 PRUNE_EVERY_MS 一次。
import { beforeEach, describe, expect, it } from 'vitest'
import * as db from '@/db'
import type { SessionEvent } from '@/protocol'
import { useSessions } from '@/stores/sessions'
import { summary } from '@/test/factories'
import { KEEP_SESSIONS, noteSessionOpened, PRUNE_EVERY_MS, pruneEventCacheNow, resetPruneTimer } from './eventCache'

function events(sid: string, from: number, to: number): SessionEvent[] {
  const out: SessionEvent[] = []
  for (let seq = from; seq <= to; seq++) out.push({ session_id: sid, seq, ts_ms: seq, event: { type: 'turn_start', turn_id: seq, message_id: `m${seq}` } })
  return out
}

function record(id: string, lowSeq: number, lastSeq: number): db.SessionRecord {
  const s = summary({ id, last_seq: lastSeq })
  return { session_id: id, node_id: 'n1', project: null, agent: s.agent, title: s.title, status: s.status, options: s.settings, last_seq: lastSeq, updated_at: 2, low_seq: lowSeq, summary: s }
}

const seqs = async (sid: string): Promise<number[]> => (await db.getEvents(sid)).map((e) => e.seq)

beforeEach(async () => {
  await db.resetDb()
  resetPruneTimer()
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

describe('db.pruneEventCache', () => {
  it('keeps the most recently opened sessions and drops the rest, never-opened ones first', async () => {
    await db.putEvents('legacy', events('legacy', 1, 2))
    for (const [sid, at] of [['old', 10], ['mid', 20], ['new', 30]] as const) {
      await db.putEvents(sid, events(sid, 1, 2))
      await db.touchEventMeta(sid, at)
    }
    await db.putSessions([record('old', 1, 2)])

    expect(await db.pruneEventCache({ keepSessions: 2, maxPerSession: 100, protect: new Set() })).toEqual({ dropped: ['old', 'legacy'], trimmed: [] })
    expect(await seqs('new')).toEqual([1, 2])
    expect(await seqs('mid')).toEqual([1, 2])
    expect(await seqs('old')).toEqual([])
    expect(await seqs('legacy')).toEqual([])
    // 摘要还在，只是本地没有它的事件了
    const [old] = await db.listSessions('n1')
    expect(old?.low_seq).toBe(0)
    // 被淘汰的会话的打开记录也删了：再淘汰一次什么都不动
    expect(await db.pruneEventCache({ keepSessions: 2, maxPerSession: 100, protect: new Set() })).toEqual({ dropped: [], trimmed: [] })
  })

  it('never touches a protected (open) session, however old, and it does not use up a slot', async () => {
    await db.putEvents('open', events('open', 1, 5))
    await db.touchEventMeta('open', 1)
    await db.putEvents('other', events('other', 1, 2))
    await db.touchEventMeta('other', 50)
    expect(await db.pruneEventCache({ keepSessions: 0, maxPerSession: 2, protect: new Set(['open']) })).toEqual({ dropped: ['other'], trimmed: [] })
    expect(await seqs('open')).toEqual([1, 2, 3, 4, 5])
  })

  it('trims a kept session to its newest events, leaving one contiguous range', async () => {
    await db.putEvents('big', events('big', 1, 10))
    await db.touchEventMeta('big', 1)
    await db.putEvents('small', events('small', 1, 3))
    expect(await db.pruneEventCache({ keepSessions: 5, maxPerSession: 3, protect: new Set() })).toEqual({ dropped: [], trimmed: ['big'] })
    expect(await seqs('big')).toEqual([8, 9, 10])
    expect(await seqs('small')).toEqual([1, 2, 3])
  })

  it('deleting a session or clearing the cache removes its opened-at record', async () => {
    await db.putEvents('a', events('a', 1, 1))
    await db.touchEventMeta('a', 1)
    await db.deleteSession('a')
    await db.putEvents('b', events('b', 1, 1))
    await db.touchEventMeta('b', 1)
    await db.clearCaches(new Set())
    const d = await db.getDb()
    expect(await d.count('event_meta')).toBe(0)
  })
})

describe('noteSessionOpened', () => {
  it('records the open and prunes, at most once per interval', async () => {
    for (let i = 0; i < KEEP_SESSIONS; i++) {
      await db.putEvents(`s${i}`, events(`s${i}`, 1, 1))
      await db.touchEventMeta(`s${i}`, 100 + i)
    }
    useSessions.setState({ list: { s0: { ...summary({ id: 's0', last_seq: 1 }), node_id: 'n1', cached_seq: 1 } } })
    // 打开一个新会话：它最新，s0 是最旧的那个，被挤出去
    await db.putEvents('fresh', events('fresh', 1, 1))
    await noteSessionOpened('fresh', 1_000)
    expect(await seqs('s0')).toEqual([])
    expect(await seqs('fresh')).toEqual([1])
    expect(useSessions.getState().list.s0?.cached_seq).toBe(0)

    // 间隔内再打开：只记时间，不淘汰
    await db.putEvents('another', events('another', 1, 1))
    await noteSessionOpened('another', 1_000 + PRUNE_EVERY_MS - 1)
    expect(await seqs('s1')).toEqual([1])
    // 过了间隔：淘汰
    await noteSessionOpened('another', 1_000 + PRUNE_EVERY_MS)
    expect(await seqs('s1')).toEqual([])
  })

  it('leaves sessions open in this tab alone', async () => {
    useSessions.getState().openStart('n1', 'kept', [])
    await db.putEvents('kept', events('kept', 1, 3))
    await db.touchEventMeta('kept', 0)
    for (let i = 0; i < KEEP_SESSIONS; i++) {
      await db.putEvents(`s${i}`, events(`s${i}`, 1, 1))
      await db.touchEventMeta(`s${i}`, 100 + i)
    }
    const r = await pruneEventCacheNow()
    expect(r.dropped).toEqual([])
    expect(await seqs('kept')).toEqual([1, 2, 3])
  })
})
