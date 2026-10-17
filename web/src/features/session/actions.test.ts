import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { getDraft, putDraft, resetDb } from '@/db'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { resetSeq, summary, worktree } from '@/test/factories'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { pendingMessages, resetDrafts, useDrafts } from '@/stores/drafts'
import { useSessions } from '@/stores/sessions'
import { archiveSession, ATTACHMENTS_TOO_LARGE, BLOB_EXPIRED, BLOB_IN_USE, catchUp, countUnsent, createSession, deleteSession, enqueueLive, flushLive, flushUnsent, loadOlder, openSession, reapEchoed, respondPermission, resendUnsent, sendMessage, setSessionSettings, unarchiveSession } from './actions'
import { handleNotification } from '@/features/sync/notifications'
import { diffSettings } from '@/features/settings/resolve'
import type { AttachmentRef, SessionEvent } from '@/protocol'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

function seqEv(seq: number, event: SessionEvent['event']): SessionEvent {
  return { session_id: SID, seq, ts_ms: 1000 + seq, event }
}
const delta = (seq: number, text = String(seq)) => seqEv(seq, { type: 'text_delta', text })

beforeEach(async () => {
  resetSeq()
  await resetDb()
  rpc = new FakeRpc()
  setRpc(rpc)
  useSessions.setState({ list: {}, pending: {}, open: {} })
  resetDrafts()
  useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, last_seq: 3 })])
})
afterEach(() => {
  setRpc(null)
  vi.useRealTimers()
})

describe('sending messages', () => {
  it('marks the message unsent (and persists it) when the node link is not ready', async () => {
    rpc.ready = false
    const r = await sendMessage(NODE, SID, 'hello')
    expect(r.state).toBe('unsent')
    const d = useDrafts.getState()
    expect(d.inflight).toEqual({})
    expect(d.byKey[SID]!.unsent).toMatchObject([{ content: 'hello', state: 'unsent' }])
    expect(countUnsent(NODE)).toBe(1)
    await vi.waitFor(async () => expect((await getDraft(SID))?.unsent).toHaveLength(1))
  })

  it('treats undeliverable like offline: the message stays local and can be resent by id', async () => {
    rpc.handlers['session.send'] = async () => {
      const { RpcCallError } = await import('@/transport/rpcClient')
      throw new RpcCallError('undeliverable', 'peer offline')
    }
    const r = await sendMessage(NODE, SID, 'hi')
    expect(r.state).toBe('unsent')
    expect(useDrafts.getState().byKey[SID]!.unsent[0]!.state).toBe('unsent')
  })

  it('keeps an accepted message in flight until the echo arrives and reports queueing', async () => {
    rpc.handlers['session.send'] = () => ({ queued: true })
    const r = await sendMessage(NODE, SID, 'later')
    expect(r.state).toBe('queued')
    const inflight = Object.values(useDrafts.getState().inflight)
    expect(inflight).toMatchObject([{ content: 'later', accepted: true, queued: true }])
    expect(useDrafts.getState().byKey[SID]?.unsent ?? []).toEqual([])
    // 事件流里出现回显后清掉
    useSessions.getState().openStart(NODE, SID, [])
    useSessions.getState().ingest(SID, [seqEv(1, { type: 'user_message', message_id: inflight[0]!.messageId, content: 'later', by_device: 'me' })])
    reapEchoed(SID)
    expect(useDrafts.getState().inflight).toEqual({})
  })

  it('marks a daemon rejection as failed with the stable error code', async () => {
    rpc.handlers['session.send'] = () => {
      throw rpcError('agent_unavailable')
    }
    const r = await sendMessage(NODE, SID, 'x')
    expect(r.state).toBe('failed')
    expect(useDrafts.getState().byKey[SID]!.unsent[0]).toMatchObject({ state: 'failed', error: 'agent_unavailable' })
    // failed 不会被自动重发
    rpc.handlers['session.send'] = () => ({ queued: false })
    expect(await flushUnsent(NODE)).toBe(0)
  })

  it('invalid / archived: failed with reason “archived”, the message is kept, and the local session becomes archived', async () => {
    rpc.handlers['session.send'] = () => {
      throw rpcError('invalid', undefined, 'archived')
    }
    expect(useSessions.getState().list[SID]!.archived).toBeFalsy()
    const r = await sendMessage(NODE, SID, 'hello?')
    expect(r).toMatchObject({ state: 'failed', reason: 'archived' })
    expect(useDrafts.getState().byKey[SID]!.unsent[0]).toMatchObject({ content: 'hello?', state: 'failed', error: 'archived' })
    expect(useSessions.getState().list[SID]!.archived).toBe(true)
    // 别的 invalid（或别的节点的同名会话）不算：不带 reason、不改归档标志
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'sess_b' })])
    rpc.handlers['session.send'] = () => {
      throw rpcError('invalid', undefined, 'worktree_not_active')
    }
    const other = await sendMessage(NODE, 'sess_b', 'x')
    expect(other.state).toBe('failed')
    expect(other.reason).toBeUndefined()
    expect(useSessions.getState().list['sess_b']!.archived).toBeFalsy()
    rpc.handlers['session.send'] = () => {
      throw rpcError('invalid')
    }
    expect((await sendMessage(NODE, 'sess_b', 'y')).reason).toBeUndefined()
  })

  it('flushUnsent resends in creation order and stops when the link drops again', async () => {
    const d = useDrafts.getState()
    d.addUnsent(SID, { message_id: 'm2', content: 'second', created_at: 20, state: 'unsent' })
    d.addUnsent(SID, { message_id: 'm1', content: 'first', created_at: 10, state: 'unsent' })
    d.addUnsent(SID, { message_id: 'm3', content: 'third', created_at: 30, state: 'unsent' })
    const sent: string[] = []
    rpc.handlers['session.send'] = (p) => {
      const { message_id: id } = p as { message_id: string }
      if (id === 'm3') throw new Error('link dropped')
      sent.push(id)
      return { queued: false }
    }
    // m3 抛出普通 Error 不是 RpcCallError → deliver 视为不确定，保持 unsent，并且 flush 停止
    await flushUnsent(NODE)
    expect(sent).toEqual(['m1', 'm2'])
  })
})

describe('restoring unsent messages after a reload', () => {
  it('loadAll brings back unsent messages of sessions that were never opened in this page, and they are counted and resent', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'sess_b', last_seq: 1 })])
    await putDraft({ session_id: SID, text: 'half-written', unsent: [{ message_id: 'a1', content: 'from before', created_at: 1, state: 'unsent' }] })
    await putDraft({ session_id: 'sess_b', text: '', unsent: [{ message_id: 'b1', content: 'other session', created_at: 2, state: 'unsent' }, { message_id: 'b2', content: 'rejected', created_at: 3, state: 'failed', error: 'forbidden' }] })
    await putDraft({ session_id: 'new:somewhere', text: 'a prompt not sent yet', unsent: [] })
    expect(countUnsent(NODE)).toBe(0) // 还没读

    await useDrafts.getState().loadAll()
    expect(Object.keys(useDrafts.getState().byKey).sort()).toEqual(['new:somewhere', 'sess_b', SID].sort())
    expect(useDrafts.getState().byKey['new:somewhere']!.text).toBe('a prompt not sent yet')
    expect(countUnsent(NODE)).toBe(2) // failed 的不算

    const sent: Array<[string, string]> = []
    rpc.handlers['session.send'] = (p) => {
      const { session_id, message_id } = p as { session_id: string; message_id: string }
      sent.push([session_id, message_id])
      return { queued: false }
    }
    expect(await flushUnsent(NODE)).toBe(2)
    expect(sent.sort()).toEqual([[SID, 'a1'], ['sess_b', 'b1']].sort())
  })

  it('flushUnsent loads the saved drafts itself, so it works even if nothing called loadAll yet', async () => {
    await putDraft({ session_id: SID, text: '', unsent: [{ message_id: 'a1', content: 'from before', created_at: 1, state: 'unsent' }] })
    rpc.handlers['session.send'] = () => ({ queued: false })
    expect(await flushUnsent(NODE)).toBe(1)
    expect(rpc.methods()).toEqual(['session.send'])
  })

  it('keeps what was typed in this page when the saved draft is merged in', async () => {
    await putDraft({ session_id: SID, text: 'old text', unsent: [{ message_id: 'a1', content: 'x', created_at: 1, state: 'unsent' }] })
    useDrafts.getState().setText(SID, 'new text')
    await useDrafts.getState().loadAll()
    expect(useDrafts.getState().byKey[SID]).toMatchObject({ text: 'new text', unsent: [{ message_id: 'a1' }] })
  })
})

describe('opening a session', () => {
  it('watches first, then pulls the most recent page of history', async () => {
    const events = [delta(1, 'a'), delta(2, 'b'), delta(3, 'c')]
    rpc.handlers['session.watch'] = () => ({})
    rpc.handlers['session.history'] = () => ({ events, has_more: true })
    await openSession(NODE, SID)
    expect(rpc.methods()).toEqual(['session.watch', 'session.history'])
    expect(rpc.calls[1]!.params).toMatchObject({ session_id: SID, limit: 200 })
    const o = useSessions.getState().open[SID]!
    expect(o.view.lastSeq).toBe(3)
    expect(o.phase).toBe('ready')
    expect(o.hasMoreOlder).toBe(true)
    expect(o.watching).toBe(true)
    expect(o.view.blocks).toMatchObject([{ type: 'assistant', text: 'abc' }])
  })

  it('applies live events, and fetches the missing range when there is a gap', async () => {
    rpc.handlers['session.watch'] = () => ({})
    rpc.handlers['session.history'] = (p) => {
      const { after_seq } = p as { after_seq?: number; before_seq?: number }
      if (after_seq === undefined) return { events: [delta(1), delta(2), delta(3)], has_more: false }
      return { events: [delta(4), delta(5)], has_more: false }
    }
    await openSession(NODE, SID)
    enqueueLive(delta(4))
    flushLive()
    expect(useSessions.getState().open[SID]!.view.lastSeq).toBe(4)
    // 5 被补回来了，6 是永久缺口（被合并掉的 delta），7 紧跟其后：补完 5 之后接受缺口，不会无限追赶
    enqueueLive(delta(7))
    flushLive()
    await vi.waitFor(() => expect(useSessions.getState().open[SID]!.view.lastSeq).toBe(7))
    const afters = rpc.calls.filter((c) => c.method === 'session.history').map((c) => (c.params as { after_seq?: number }).after_seq)
    expect(afters).toEqual([undefined, 4, 5])
  })

  it('holds live events that arrive during catch-up and applies them afterwards', async () => {
    let release!: (v: unknown) => void
    rpc.handlers['session.watch'] = () => ({})
    rpc.handlers['session.history'] = () => new Promise((r) => (release = r))
    const opening = openSession(NODE, SID)
    await vi.waitFor(() => expect(rpc.methods()).toContain('session.history'))
    enqueueLive(delta(4)) // 追赶途中到的实时事件
    flushLive()
    expect(useSessions.getState().open[SID]!.held).toHaveLength(1)
    release({ events: [delta(1), delta(2), delta(3)], has_more: false })
    await opening
    await vi.waitFor(() => expect(useSessions.getState().open[SID]!.view.lastSeq).toBe(4))
    expect(useSessions.getState().open[SID]!.held).toEqual([])
  })

  it('surfaces not_found for a session that no longer exists', async () => {
    rpc.handlers['session.watch'] = () => {
      throw rpcError('not_found')
    }
    await openSession(NODE, SID)
    expect(useSessions.getState().open[SID]).toMatchObject({ phase: 'error', error: 'not_found' })
  })

  it('pulls older history when scrolling up and merges it before the cached events', async () => {
    rpc.handlers['session.watch'] = () => ({})
    rpc.handlers['session.history'] = (p) => {
      const { before_seq } = p as { before_seq?: number }
      if (before_seq === Number.MAX_SAFE_INTEGER) return { events: [delta(5, 'e'), delta(6, 'f')], has_more: true }
      return { events: [delta(3, 'c'), delta(4, 'd')], has_more: false }
    }
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, last_seq: 6 })])
    await openSession(NODE, SID)
    await loadOlder(SID)
    const o = useSessions.getState().open[SID]!
    expect(o.lowSeq).toBe(3)
    expect(o.hasMoreOlder).toBe(false)
    expect(o.view.blocks).toMatchObject([{ type: 'assistant', text: 'cdef' }])
    expect(rpc.calls.at(-1)!.params).toMatchObject({ before_seq: 5 })
  })

  it('can run an explicit catch-up', async () => {
    rpc.handlers['session.watch'] = () => ({})
    rpc.handlers['session.history'] = (p) => ((p as { after_seq?: number }).after_seq === undefined ? { events: [delta(1)], has_more: false } : { events: [delta(2)], has_more: false })
    await openSession(NODE, SID)
    await catchUp(NODE, SID)
    expect(useSessions.getState().open[SID]!.view.lastSeq).toBe(2)
  })
})

describe('notifications', () => {
  it('applies session.changed to the list and drops deleted sessions', () => {
    handleNotification(NODE, 'session.changed', { session_id: SID, title: 'New title', status: 'running', pending_permissions: 2, updated_at_ms: 99, last_seq: 9 })
    expect(useSessions.getState().list[SID]).toMatchObject({ title: 'New title', status: 'running', pending_permissions: 2, last_seq: 9 })
    handleNotification(NODE, 'session.changed', { session_id: SID, title: '', status: 'ended', deleted: true, pending_permissions: 0, updated_at_ms: 100, last_seq: 9 })
    expect(useSessions.getState().list[SID]).toBeUndefined()
  })

  it('tracks pending permission requests across sessions and clears them when resolved', () => {
    const req = { session_id: SID, request_id: 'r1', kind: 'tool', title: 'Run', detail: {}, options: [], created_at_ms: 1 }
    handleNotification(NODE, 'permission.request', req)
    expect(Object.keys(useSessions.getState().pending)).toEqual(['r1'])
    handleNotification(NODE, 'permission.resolved', { session_id: SID, request_id: 'r1', option_id: 'allow', by_device: 'd' })
    expect(useSessions.getState().pending).toEqual({})
  })

  it('answers a permission and clears the pending badge', async () => {
    const req = { session_id: SID, request_id: 'r1', kind: 'tool', title: 'Run', detail: {}, options: [], created_at_ms: 1 }
    handleNotification(NODE, 'permission.request', req)
    rpc.handlers['permission.respond'] = () => ({})
    await respondPermission(NODE, { request_id: 'r1', option_id: 'allow' })
    expect(rpc.calls.at(-1)).toMatchObject({ method: 'permission.respond', params: { request_id: 'r1', option_id: 'allow' } })
    expect(useSessions.getState().pending).toEqual({})
  })

  describe('permission.respond → not_found (two different meanings, told apart by data.reason)', () => {
    const req = { session_id: SID, request_id: 'r1', kind: 'tool', title: 'Run', detail: {}, options: [], created_at_ms: 1 }

    it('data.reason = project_dir_changed: the folder changed and the daemon already denied the request — an outcome, not a failure; no retry, pending cleared', async () => {
      handleNotification(NODE, 'permission.request', req)
      rpc.handlers['permission.respond'] = () => {
        throw rpcError('not_found', undefined, 'project_dir_changed')
      }
      await expect(respondPermission(NODE, { request_id: 'r1', option_id: 'allow' })).resolves.toBe('denied_folder_changed')
      expect(rpc.calls.filter((c) => c.method === 'permission.respond')).toHaveLength(1)
      expect(useSessions.getState().pending).toEqual({})
    })

    it.each([
      ['no reason (unknown request id, session not found)', undefined],
      ['some other reason', 'something_else'],
      ['a reason that only looks similar', 'project_dir_changed_maybe'],
    ])('%s: only “the request no longer exists” — it must not be taken for a changed folder; no retry, pending cleared', async (_name, reason) => {
      handleNotification(NODE, 'permission.request', req)
      rpc.handlers['permission.respond'] = () => {
        throw rpcError('not_found', undefined, reason)
      }
      await expect(respondPermission(NODE, { request_id: 'r1', option_id: 'allow' })).resolves.toBe('request_gone')
      expect(rpc.calls.filter((c) => c.method === 'permission.respond')).toHaveLength(1)
      expect(useSessions.getState().pending).toEqual({})
    })

    it('a project_dir_changed reason on any other error code does not count', async () => {
      rpc.handlers['permission.respond'] = () => {
        throw rpcError('internal', undefined, 'project_dir_changed')
      }
      await expect(respondPermission(NODE, { request_id: 'r1', option_id: 'allow' })).rejects.toMatchObject({ code: 'internal' })
    })
  })

  it('returns “answered” for a normal answer, and still throws every other error', async () => {
    rpc.handlers['permission.respond'] = () => ({})
    await expect(respondPermission(NODE, { request_id: 'r1', option_id: 'allow' })).resolves.toBe('answered')
    for (const code of ['internal', 'forbidden', 'invalid'] as const) {
      rpc.handlers['permission.respond'] = () => {
        throw rpcError(code)
      }
      await expect(respondPermission(NODE, { request_id: 'r1', option_id: 'allow' })).rejects.toMatchObject({ code })
    }
  })

  it('propagates already_resolved so the UI can say another device answered', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('already_resolved')
    }
    await expect(respondPermission(NODE, { request_id: 'r1', option_id: 'allow' })).rejects.toMatchObject({ code: 'already_resolved' })
  })
})

describe('session settings', () => {
  it('sends set_settings and stores the effective settings', async () => {
    rpc.handlers['session.set_settings'] = () => ({ settings: { model: 'mock-small', mode: 'ask' } })
    const eff = await setSessionSettings(NODE, SID, { settings: { model: 'mock-small' }, clear: [] })
    expect(eff).toEqual({ model: 'mock-small', mode: 'ask' })
    expect(useSessions.getState().list[SID]!.settings).toEqual({ model: 'mock-small', mode: 'ask' })
    // 没有要清的字段时不带 clear
    expect(rpc.calls.at(-1)).toEqual({ nodeId: NODE, method: 'session.set_settings', params: { session_id: SID, settings: { model: 'mock-small' } } })
  })

  it('emptying a field sends it in `clear` (and not in settings), and the effective settings from the reply replace the local ones', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, last_seq: 3, settings: { model: 'mock-large', thinking: 'high', max_turns: 5 } })])
    rpc.handlers['session.set_settings'] = () => ({ settings: { model: 'mock-large' } })
    const update = diffSettings({ model: 'mock-large', thinking: 'high', max_turns: 5, system_prompt_append: 'x' }, { model: 'mock-large', system_prompt_append: '' })
    const eff = await setSessionSettings(NODE, SID, update)
    expect(rpc.calls.at(-1)).toEqual({
      nodeId: NODE,
      method: 'session.set_settings',
      params: { session_id: SID, settings: {}, clear: ['thinking', 'system_prompt_append', 'max_turns'] },
    })
    expect(eff).toEqual({ model: 'mock-large' })
    expect(useSessions.getState().list[SID]!.settings).toEqual({ model: 'mock-large' })
  })
})

describe('pending message list', () => {
  it('merges persisted unsent and in-flight messages and hides ones already echoed', () => {
    const draft = { session_id: SID, text: '', unsent: [{ message_id: 'u1', content: 'offline msg', created_at: 5, state: 'unsent' as const }, { message_id: 'u2', content: 'seen', created_at: 6, state: 'unsent' as const }] }
    const inflight = { f1: { messageId: 'f1', sessionId: SID, content: 'sending', createdAt: 7, queued: false, accepted: false }, f2: { messageId: 'f2', sessionId: SID, content: 'queued', createdAt: 8, queued: true, accepted: true }, other: { messageId: 'o', sessionId: 'other', content: 'x', createdAt: 1, queued: false, accepted: false } }
    const list = pendingMessages(draft, inflight, SID, new Set(['u2']))
    expect(list.map((m) => [m.messageId, m.state])).toEqual([['u1', 'unsent'], ['f1', 'sending'], ['f2', 'queued']])
  })
})


describe('worktree parameters (ADR-34)', () => {
  const callOf = (method: string) => rpc.calls.find((c) => c.method === method)!.params

  it('createSession sends workspace only when given, and keeps the worktree of the returned summary', async () => {
    rpc.handlers['session.create'] = () => summary({ id: 'sess_new', worktree: worktree({ state: 'creating' }) })
    await createSession(NODE, { project: 'app', agent: 'mock', prompt: 'hi', settings: {}, workspace: { mode: 'worktree', base: 'main', branch: 'anycon/x' } })
    expect(callOf('session.create')).toMatchObject({ project: 'app', agent: 'mock', prompt: 'hi', workspace: { mode: 'worktree', base: 'main', branch: 'anycon/x' } })
    expect(useSessions.getState().list['sess_new']!.worktree).toMatchObject({ state: 'creating' })

    rpc.calls = []
    await createSession(NODE, { project: 'app', agent: 'mock', prompt: 'hi', settings: {} })
    expect(callOf('session.create')).not.toHaveProperty('workspace')
  })

  it('archiveSession: only the session id by default; worktree is added when the user chose; un-archiving never carries it', async () => {
    rpc.handlers['session.archive'] = () => ({})
    rpc.handlers['session.unarchive'] = () => ({})
    await archiveSession(NODE, SID, true)
    expect(callOf('session.archive')).toEqual({ session_id: SID })
    rpc.calls = []
    await archiveSession(NODE, SID, true, { worktree: 'keep' })
    expect(callOf('session.archive')).toEqual({ session_id: SID, worktree: 'keep' })
    rpc.calls = []
    await archiveSession(NODE, SID, false, { worktree: 'remove' })
    expect(callOf('session.unarchive')).toEqual({ session_id: SID })
  })

  it('unarchiveSession clears the local archived flag after the daemon accepted, and leaves it alone when the daemon refused', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, archived: true })])
    rpc.handlers['session.unarchive'] = () => {
      throw rpcError('internal', undefined, 'git_failed')
    }
    await expect(unarchiveSession(NODE, SID)).rejects.toMatchObject({ code: 'internal', reason: 'git_failed' })
    expect(useSessions.getState().list[SID]!.archived).toBe(true)
    rpc.handlers['session.unarchive'] = () => ({})
    await unarchiveSession(NODE, SID)
    expect(callOf('session.unarchive')).toEqual({ session_id: SID })
    expect(useSessions.getState().list[SID]!.archived).toBe(false)
  })

  it('deleteSession: only the session id by default; worktree and delete_branch only when asked for; the session leaves the list', async () => {
    rpc.handlers['session.delete'] = () => ({})
    await deleteSession(NODE, SID)
    expect(callOf('session.delete')).toEqual({ session_id: SID })
    expect(useSessions.getState().list[SID]).toBeUndefined()

    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID })])
    rpc.calls = []
    await deleteSession(NODE, SID, { worktree: 'remove', deleteBranch: true })
    expect(callOf('session.delete')).toEqual({ session_id: SID, worktree: 'remove', delete_branch: true })

    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID })])
    rpc.calls = []
    await deleteSession(NODE, SID, { worktree: 'keep', deleteBranch: false })
    expect(callOf('session.delete')).toEqual({ session_id: SID, worktree: 'keep' })
  })

  it('session.changed updates the worktree state in the list and keeps the old one when the notification has none', () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, worktree: worktree({ state: 'creating' }) })])
    const base = { session_id: SID, title: 'hello', status: 'idle' as const, archived: false, pending_permissions: 0, updated_at_ms: 5, last_seq: 4 }
    handleNotification(NODE, 'session.changed', { ...base, worktree: worktree({ state: 'active' }) })
    expect(useSessions.getState().list[SID]!.worktree?.state).toBe('active')
    handleNotification(NODE, 'session.changed', { ...base, archived: true, worktree: worktree({ state: 'kept' }) })
    expect(useSessions.getState().list[SID]).toMatchObject({ archived: true, worktree: { state: 'kept' } })
    handleNotification(NODE, 'session.changed', { ...base, updated_at_ms: 6 })
    expect(useSessions.getState().list[SID]!.worktree?.state).toBe('kept')
  })
})

describe('image attachments (ADR-35)', () => {
  const callsOf = (method: string) => rpc.calls.filter((c) => c.method === method).map((c) => c.params as Record<string, unknown>)
  const a = (n: number): AttachmentRef => ({ blob_id: String(n).repeat(32), name: `p${n}.png`, mime: 'image/png', bytes: 100 * n, width: 10 * n, height: 5 * n })

  it('session.send carries the attachment references (and only when there are some); the in-flight bubble knows them', async () => {
    rpc.handlers['session.send'] = () => ({ queued: false })
    await sendMessage(NODE, SID, 'what is this?', [a(1), a(2)])
    expect(callsOf('session.send')[0]).toMatchObject({ session_id: SID, content: 'what is this?', attachments: [a(1), a(2)] })
    const [inflight] = Object.values(useDrafts.getState().inflight)
    expect(inflight!.attachments).toEqual([a(1), a(2)])
    expect(pendingMessages(undefined, useDrafts.getState().inflight, SID, new Set())[0]!.attachments).toEqual([a(1), a(2)])

    await sendMessage(NODE, SID, 'plain')
    expect(callsOf('session.send')[1]).not.toHaveProperty('attachments')
  })

  it('an image-only message goes out with an empty content', async () => {
    rpc.handlers['session.send'] = () => ({ queued: false })
    const r = await sendMessage(NODE, SID, '', [a(1)])
    expect(r.state).toBe('accepted')
    expect(callsOf('session.send')[0]).toMatchObject({ content: '', attachments: [a(1)] })
  })

  it('an unsent message keeps its attachment references on disk; resending and flushUnsent send them again, in order', async () => {
    rpc.ready = false
    const r = await sendMessage(NODE, SID, 'offline', [a(1), a(2)])
    expect(r.state).toBe('unsent')
    expect(useDrafts.getState().byKey[SID]!.unsent[0]).toMatchObject({ content: 'offline', state: 'unsent', attachments: [a(1), a(2)] })
    await vi.waitFor(async () => expect((await getDraft(SID))?.unsent[0]?.attachments).toEqual([a(1), a(2)]))

    rpc.ready = true
    rpc.handlers['session.send'] = () => ({ queued: false })
    expect(await flushUnsent(NODE)).toBe(1)
    expect(callsOf('session.send').at(-1)).toMatchObject({ message_id: r.messageId, content: 'offline', attachments: [a(1), a(2)] })
    expect(useDrafts.getState().inflight[r.messageId]!.attachments).toEqual([a(1), a(2)])
    expect(useDrafts.getState().byKey[SID]!.unsent).toEqual([])
  })

  it('not_found with reason “blob” (the node has dropped an attachment): failed as “blob_expired”, the message and its references stay for editing', async () => {
    rpc.handlers['session.send'] = () => {
      throw rpcError('not_found', undefined, 'blob')
    }
    const r = await sendMessage(NODE, SID, 'x', [a(1)])
    expect(r).toMatchObject({ state: 'failed', reason: BLOB_EXPIRED })
    expect(useDrafts.getState().byKey[SID]!.unsent[0]).toMatchObject({ state: 'failed', error: 'blob_expired', attachments: [a(1)] })
    // 只是「重发」得到同样的结果，不会丢东西
    const again = await resendUnsent(NODE, SID, r.messageId)
    expect(again).toMatchObject({ state: 'failed', reason: BLOB_EXPIRED })
    expect(useDrafts.getState().byKey[SID]!.unsent).toHaveLength(1)
  })

  it('invalid with reason “blob_in_use” (the images already belong to another session): failed as “blob_in_use”, references kept', async () => {
    rpc.handlers['session.send'] = () => {
      throw rpcError('invalid', undefined, 'blob_in_use')
    }
    const r = await sendMessage(NODE, SID, 'x', [a(1)])
    expect(r).toMatchObject({ state: 'failed', reason: BLOB_IN_USE })
    expect(useDrafts.getState().byKey[SID]!.unsent[0]).toMatchObject({ state: 'failed', error: 'blob_in_use', attachments: [a(1)] })
  })

  it('invalid with reason “too_large” (the images of one message add up to too much): failed as “attachments_too_large”', async () => {
    rpc.handlers['session.send'] = () => {
      throw rpcError('invalid', undefined, 'too_large')
    }
    const r = await sendMessage(NODE, SID, 'x', [a(1), a(2)])
    expect(r).toMatchObject({ state: 'failed', reason: ATTACHMENTS_TOO_LARGE })
    expect(useDrafts.getState().byKey[SID]!.unsent[0]).toMatchObject({ state: 'failed', error: 'attachments_too_large', attachments: [a(1), a(2)] })
  })

  it('an invalid without one of those reasons is an ordinary failure', async () => {
    rpc.handlers['session.send'] = () => {
      throw rpcError('invalid', undefined, 'archived_nope')
    }
    const r = await sendMessage(NODE, SID, 'x', [a(1)])
    expect(r.state).toBe('failed')
    expect(r.reason).toBeUndefined()
    expect(useDrafts.getState().byKey[SID]!.unsent[0]!.error).toBe('invalid')
  })

  it('a not_found without that reason is an ordinary failure', async () => {
    rpc.handlers['session.send'] = () => {
      throw rpcError('not_found')
    }
    const r = await sendMessage(NODE, SID, 'x', [a(1)])
    expect(r.state).toBe('failed')
    expect(r.reason).toBeUndefined()
    expect(useDrafts.getState().byKey[SID]!.unsent[0]!.error).toBe('not_found')
  })

  it('createSession sends the attachments with the first message, even without a prompt, and a message id', async () => {
    rpc.handlers['session.create'] = () => summary({ id: 'sess_new' })
    await createSession(NODE, { project: 'app', agent: 'mock', attachments: [a(1)], settings: {} })
    const params = callsOf('session.create')[0]!
    expect(params).toMatchObject({ project: 'app', agent: 'mock', attachments: [a(1)] })
    expect(params.prompt).toBeUndefined()
    expect(typeof params.message_id).toBe('string')

    rpc.calls = []
    await createSession(NODE, { project: 'app', agent: 'mock', prompt: 'hi', attachments: [a(1), a(2)], settings: {} })
    expect(callsOf('session.create')[0]).toMatchObject({ prompt: 'hi', attachments: [a(1), a(2)] })

    rpc.calls = []
    await createSession(NODE, { project: 'app', agent: 'mock', settings: {} })
    const bare = callsOf('session.create')[0]!
    expect(bare).not.toHaveProperty('attachments')
    expect(bare.message_id).toBeUndefined()
  })

  describe('createSession after a failure keeps the message_id of the identical request', () => {
    const input = { project: 'app', agent: 'mock', prompt: 'hi', attachments: [a(1)], settings: {} }
    let attempts = 0
    beforeEach(() => {
      attempts = 0
      rpc.handlers['session.create'] = () => {
        attempts += 1
        if (attempts === 1) throw new RpcCallError('timeout', 'session.create: timed out')
        return summary({ id: 'sess_new' })
      }
    })
    const ids = () => callsOf('session.create').map((c) => c.message_id)

    it('same project, agent, text, images and settings → the same id (the node answers with the session it already made); then the id is forgotten', async () => {
      await expect(createSession(NODE, input)).rejects.toMatchObject({ kind: 'timeout' })
      await createSession(NODE, input)
      const [first, second] = ids()
      expect(typeof first).toBe('string')
      expect(second).toBe(first)

      // 成功之后忘掉：同样的请求再发一次是新的一次
      rpc.handlers['session.create'] = () => summary({ id: 'sess_other' })
      await createSession(NODE, input)
      expect(ids()[2]).not.toBe(first)
    })

    it.each([
      ['another text', { ...input, prompt: 'hello' }],
      ['another image', { ...input, attachments: [a(2)] }],
      ['another agent', { ...input, agent: 'other' }],
      ['another project', { ...input, project: 'lib' }],
      ['other settings', { ...input, settings: { model: 'm2' } }],
      ['a worktree', { ...input, workspace: { mode: 'worktree' as const } }],
    ])('%s → a new id (the node would otherwise hand back a session that is not what was asked for)', async (_name, changed) => {
      await expect(createSession(NODE, input)).rejects.toMatchObject({ kind: 'timeout' })
      await createSession(NODE, changed)
      const [first, second] = ids()
      expect(second).not.toBe(first)
    })

    it('a request with neither text nor images has no message_id at all', async () => {
      await expect(createSession(NODE, { project: 'app', agent: 'mock', settings: {} })).rejects.toBeDefined()
      await createSession(NODE, { project: 'app', agent: 'mock', settings: {} })
      expect(ids()).toEqual([undefined, undefined])
    })
  })
})
