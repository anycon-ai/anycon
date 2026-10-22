import { beforeEach, describe, expect, it } from 'vitest'
import { resetDb, type SessionRecord } from '@/db'
import { summary } from '@/test/factories'
import type { SessionChanged } from '@/protocol'
import { useSessions } from './sessions'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'

const changed = (over: Partial<SessionChanged> = {}): SessionChanged => ({
  session_id: SID,
  title: 'hello',
  status: 'idle',
  archived: false,
  pending_permissions: 0,
  updated_at_ms: 9,
  last_seq: 0,
  ...over,
})
const item = () => useSessions.getState().list[SID]

beforeEach(async () => {
  await resetDb()
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

describe('sessions store: background_tasks', () => {
  it('keeps background_tasks from session.list summaries and drops it when the next listing no longer has it', () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, background_tasks: 2 })])
    expect(item()?.background_tasks).toBe(2)
    // 旧 daemon 的列表没有该字段：不留着上一次的值
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID })])
    expect(item()?.background_tasks).toBeUndefined()
  })

  it('session.changed carries the new count into the summary, and the rest of the summary is untouched', () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, project: 'demo', background_tasks: 0 })])
    useSessions.getState().applyChanged(NODE, changed({ background_tasks: 3 }))
    expect(item()).toMatchObject({ background_tasks: 3, project: 'demo', node_id: NODE, status: 'idle' })
    useSessions.getState().applyChanged(NODE, changed({ background_tasks: 1, status: 'running' }))
    expect(item()).toMatchObject({ background_tasks: 1, status: 'running' })
    useSessions.getState().applyChanged(NODE, changed({ background_tasks: 0 }))
    expect(item()?.background_tasks).toBe(0)
  })

  it('treats a session.changed without the field (an older daemon) as 0', () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, background_tasks: 2 })])
    useSessions.getState().applyChanged(NODE, changed())
    expect(item()?.background_tasks).toBe(0)
  })

  it('ignores a session.changed for a session that belongs to another node', () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, background_tasks: 2 })])
    useSessions.getState().applyChanged('o'.repeat(32), changed({ background_tasks: 0 }))
    expect(item()?.background_tasks).toBe(2)
  })

  it('does not trust a cached background_tasks after a reload: it is daemon memory, not history', () => {
    const cached = summary({ id: SID, background_tasks: 2 })
    const rec: SessionRecord = { session_id: SID, node_id: NODE, project: null, agent: 'mock', title: 'hello', status: 'idle', options: {}, last_seq: 0, updated_at: 2, low_seq: 0, summary: cached }
    useSessions.getState().hydrateNode(NODE, [rec])
    expect(item()).toMatchObject({ id: SID, node_id: NODE })
    expect(item()?.background_tasks).toBeUndefined()
  })
})
