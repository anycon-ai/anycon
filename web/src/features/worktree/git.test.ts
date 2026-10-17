import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { SessionGitStatus, SessionSyncBaseResult } from '@/protocol'
import { gitStatus, pullRequest } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useGit } from '@/stores/git'
import { dropGit, FILE_CHANGED_DEBOUNCE_MS, lookupPullRequest, refreshGitStatus, scheduleGitRefresh, syncBase } from './git'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

const entry = () => useGit.getState().bySession[SID]
const statusCalls = () => rpc.calls.filter((c) => c.method === 'session.git_status')

beforeEach(() => {
  rpc = new FakeRpc()
  setRpc(rpc)
  useGit.setState({ bySession: {} })
})
afterEach(() => {
  setRpc(null)
  dropGit(SID)
  vi.useRealTimers()
})

describe('refreshGitStatus', () => {
  it('asks for the status without include_pr by default and stores the result', async () => {
    rpc.handlers['session.git_status'] = () => gitStatus({ ahead: 2, uncommitted: 3 })
    await refreshGitStatus(NODE, SID)
    expect(statusCalls().map((c) => c.params)).toEqual([{ session_id: SID }])
    expect(entry()).toMatchObject({ status: { ahead: 2, uncommitted: 3 }, loading: false, error: null, rev: 1, prChecked: false })
  })

  it('looks up the PR only when asked (include_pr) and keeps the last PR through plain refreshes', async () => {
    const pr = pullRequest()
    rpc.handlers['session.git_status'] = (p) => ((p as { include_pr?: boolean }).include_pr ? gitStatus({ pr }) : gitStatus())
    await refreshGitStatus(NODE, SID, { includePr: true })
    expect(statusCalls()[0]!.params).toEqual({ session_id: SID, include_pr: true })
    expect(entry()).toMatchObject({ pr, prError: null, prChecked: true })

    await refreshGitStatus(NODE, SID)
    expect(statusCalls()[1]!.params).toEqual({ session_id: SID })
    expect(entry()).toMatchObject({ pr, prChecked: true, rev: 2 })
  })

  it('records why there is no PR (pr_error), and clears a stale PR when the lookup now says there is none', async () => {
    let next: SessionGitStatus = gitStatus({ pr: pullRequest() })
    rpc.handlers['session.git_status'] = () => next
    await refreshGitStatus(NODE, SID, { includePr: true })
    expect(entry()!.pr).not.toBeNull()
    next = gitStatus({ pr_error: 'no_pr' })
    await refreshGitStatus(NODE, SID, { includePr: true })
    expect(entry()).toMatchObject({ pr: null, prError: 'no_pr', prChecked: true })
  })

  it('keeps the last known status when a refresh fails for a transient reason, and remembers why', async () => {
    rpc.handlers['session.git_status'] = () => gitStatus({ uncommitted: 1 })
    await refreshGitStatus(NODE, SID)
    rpc.handlers['session.git_status'] = () => {
      throw rpcError('internal')
    }
    await refreshGitStatus(NODE, SID)
    expect(entry()!.status).toMatchObject({ uncommitted: 1 })
    expect(entry()!.error).toMatchObject({ code: 'internal' })
    expect(entry()!.loading).toBe(false)
  })

  it('treats not_git as a definite answer: the old status and PR are dropped', async () => {
    rpc.handlers['session.git_status'] = () => gitStatus({ pr: pullRequest() })
    await refreshGitStatus(NODE, SID, { includePr: true })
    rpc.handlers['session.git_status'] = () => {
      throw rpcError('invalid', undefined, 'not_git')
    }
    await refreshGitStatus(NODE, SID)
    expect(entry()).toMatchObject({ status: null, pr: null, error: { reason: 'not_git' } })
  })

  it('never has two requests for one session in flight: a call during a request makes exactly one follow-up', async () => {
    let release: () => void = () => {}
    let n = 0
    rpc.handlers['session.git_status'] = async () => {
      n += 1
      if (n === 1) await new Promise<void>((r) => (release = r))
      return gitStatus({ uncommitted: n })
    }
    const first = refreshGitStatus(NODE, SID)
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
    // 在途期间又来了两次（比如 turn_done 和 git 事件）：并成一次后续
    const second = refreshGitStatus(NODE, SID)
    const third = refreshGitStatus(NODE, SID)
    expect(statusCalls()).toHaveLength(1)
    release()
    await Promise.all([first, second, third])
    expect(statusCalls()).toHaveLength(2)
    expect(entry()!.status).toMatchObject({ uncommitted: 2 })
  })

  it('merges calls made in the same tick (the page and the changes tab both ask on entering) into one request', async () => {
    rpc.handlers['session.git_status'] = () => gitStatus()
    await Promise.all([refreshGitStatus(NODE, SID), refreshGitStatus(NODE, SID), refreshGitStatus(NODE, SID, { includePr: false })])
    expect(statusCalls()).toHaveLength(1)
  })

  it('a same-tick call that wants the PR after a plain request still gets its PR lookup', async () => {
    rpc.handlers['session.git_status'] = () => gitStatus()
    await Promise.all([refreshGitStatus(NODE, SID), refreshGitStatus(NODE, SID, { includePr: true })])
    expect(statusCalls().map((c) => (c.params as { include_pr?: boolean }).include_pr)).toEqual([undefined, true])
  })
})

describe('lookupPullRequest (entering the changes tab)', () => {
  it('asks with include_pr, fills only the PR fields, and leaves status and rev alone (the file list must not reload for it)', async () => {
    rpc.handlers['session.git_status'] = (p) => ((p as { include_pr?: boolean }).include_pr ? gitStatus({ pr: pullRequest(), uncommitted: 9 }) : gitStatus({ uncommitted: 1 }))
    await refreshGitStatus(NODE, SID)
    const rev = entry()!.rev
    await lookupPullRequest(NODE, SID)
    expect(statusCalls().at(-1)!.params).toEqual({ session_id: SID, include_pr: true })
    expect(entry()).toMatchObject({ pr: { number: 12 }, prChecked: true, prLoading: false, rev, status: { uncommitted: 1 } })
  })

  it('does not run twice at once, and a failure only turns into “could not look it up”', async () => {
    let release: () => void = () => {}
    rpc.handlers['session.git_status'] = async () => {
      await new Promise<void>((r) => (release = r))
      throw rpcError('internal')
    }
    const a = lookupPullRequest(NODE, SID)
    const b = lookupPullRequest(NODE, SID)
    await vi.waitFor(() => expect(entry()!.prLoading).toBe(true))
    release()
    await Promise.all([a, b])
    expect(statusCalls()).toHaveLength(1)
    expect(entry()).toMatchObject({ prLoading: false, prChecked: true, prError: 'error', pr: null })
  })

  it('ignores a result that arrives after the page was left', async () => {
    let release: () => void = () => {}
    rpc.handlers['session.git_status'] = async () => {
      await new Promise<void>((r) => (release = r))
      return gitStatus({ pr: pullRequest() })
    }
    const p = lookupPullRequest(NODE, SID)
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
    dropGit(SID)
    release()
    await p
    expect(entry()).toBeUndefined()
  })
})

describe('scheduleGitRefresh (file_changed debounce)', () => {
  it('waits 2 seconds after the last call and refreshes once', async () => {
    vi.useFakeTimers()
    rpc.handlers['session.git_status'] = () => gitStatus()
    scheduleGitRefresh(NODE, SID)
    await vi.advanceTimersByTimeAsync(FILE_CHANGED_DEBOUNCE_MS - 500)
    scheduleGitRefresh(NODE, SID) // 又改了一个文件：重新计时
    await vi.advanceTimersByTimeAsync(FILE_CHANGED_DEBOUNCE_MS - 1)
    expect(statusCalls()).toHaveLength(0)
    await vi.advanceTimersByTimeAsync(1)
    expect(statusCalls()).toHaveLength(1)
    await vi.advanceTimersByTimeAsync(10_000) // 不轮询
    expect(statusCalls()).toHaveLength(1)
  })

  it('dropGit cancels a pending refresh and forgets the session', async () => {
    vi.useFakeTimers()
    rpc.handlers['session.git_status'] = () => gitStatus()
    useGit.getState().patch(SID, () => ({ status: gitStatus() }))
    scheduleGitRefresh(NODE, SID)
    dropGit(SID)
    await vi.advanceTimersByTimeAsync(5000)
    expect(statusCalls()).toHaveLength(0)
    expect(entry()).toBeUndefined()
  })
})

describe('syncBase', () => {
  it('sends session.sync_base, remembers the conflicting files, and refreshes the status afterwards', async () => {
    rpc.handlers['session.sync_base'] = (): SessionSyncBaseResult => ({ outcome: 'conflicts', commits: 0, conflicts: ['a.rs', 'b.rs'] })
    rpc.handlers['session.git_status'] = () => gitStatus({ merging: true, conflicts: 2 })
    const res = await syncBase(NODE, SID)
    expect(res.outcome).toBe('conflicts')
    expect(rpc.calls.find((c) => c.method === 'session.sync_base')!.params).toEqual({ session_id: SID })
    expect(entry()!.conflictFiles).toEqual(['a.rs', 'b.rs'])
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
  })

  it('abort sends abort: true and clears the remembered conflicts', async () => {
    useGit.getState().patch(SID, () => ({ conflictFiles: ['a.rs'] }))
    rpc.handlers['session.sync_base'] = (): SessionSyncBaseResult => ({ outcome: 'aborted', commits: 0 })
    rpc.handlers['session.git_status'] = () => gitStatus()
    await syncBase(NODE, SID, { abort: true })
    expect(rpc.calls.find((c) => c.method === 'session.sync_base')!.params).toEqual({ session_id: SID, abort: true })
    expect(entry()!.conflictFiles).toEqual([])
  })

  it('rethrows the daemon’s error untouched, and still refreshes (the state may have changed)', async () => {
    rpc.handlers['session.sync_base'] = () => {
      throw rpcError('invalid', undefined, 'uncommitted_changes')
    }
    rpc.handlers['session.git_status'] = () => gitStatus({ uncommitted: 2 })
    await expect(syncBase(NODE, SID)).rejects.toMatchObject({ reason: 'uncommitted_changes' })
    await vi.waitFor(() => expect(statusCalls()).toHaveLength(1))
  })
})
