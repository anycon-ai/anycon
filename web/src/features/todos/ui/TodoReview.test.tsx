// 验收页（spec/web.md § 4.12）：结论在前（最后一轮的结果、改动统计、审查器拒绝次数）、文件一个一个看、三个固定动作；
// 通过之后的「提交、推送并开 PR」要先明确确认才会关掉「不 push」。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { DiffFile, SessionDiffParams } from '@/protocol'
import * as db from '@/db'
import { diffFile, ev, gitStatus, nodeEntry, patchFor, resetSeq, summary, todoItem, worktree } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTodoDaemon, linkError } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { resetDrafts } from '@/stores/drafts'
import { useGit } from '@/stores/git'
import { useTodoHistory } from '../history'
import { useReworkDrafts } from '../reworkDraft'
import { resetTodosMemory, syncTodos } from '../service'
import { useTodoView, useTodos } from '../store'
import { TodoReview } from './TodoReview'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const N = 'n'.repeat(32)
const SID = 'sess_review'
let rpc: FakeRpc
let daemon: FakeTodoDaemon

const files: DiffFile[] = [diffFile({ path: 'src/a.ts', additions: 3, deletions: 1 }), diffFile({ path: 'src/b.ts', additions: 10, deletions: 0, status: 'added' })]
const result = (seq: number, text: string, extra: object = {}) => ({ seq, ts_ms: seq * 1000, kind: 'result', session_id: SID, turn: seq, text, truncated: false, ...extra }) as never

function Live({ online = true }: { online?: boolean }) {
  const item = useTodoView([N]).find((v) => v.id === 't1')
  return item ? <TodoReview item={item} online={online} /> : <p>gone</p>
}

async function mount(todo: Parameters<typeof todoItem>[0] = { id: 't1' }, o: { online?: boolean } = {}): Promise<void> {
  daemon.seed(todoItem({ title: 'Fix the redirect', state: 'open', activity: 'review' as never, sessions: [SID] as never, ...todo }))
  await syncTodos(N)
  // 已关闭的项不在没关闭的列表里：直接放进缓存（就像用户刚在这一页点了「通过」）
  useTodos.getState().mergeItems(N, [daemon.todos.get('t1')!])
  render(<Live {...o} />)
  await act(async () => {
    for (let i = 0; i < 15; i++) await new Promise((r) => setTimeout(r, 5))
  })
}

const sent = (): string[] => rpc.calls.filter((c) => c.method === 'session.send').map((c) => (c.params as { content: string }).content)
const settingsCalls = () => rpc.calls.filter((c) => c.method === 'session.set_settings').map((c) => c.params as { settings: { options?: Record<string, unknown> } })

beforeEach(async () => {
  resetSeq()
  await db.resetDb()
  resetDrafts()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  rpc.handlers['todo.history'] = () => ({ events: [], has_more: false })
  rpc.handlers['session.diff'] = (p) => {
    const { path } = p as SessionDiffParams
    if (path) {
      const f = files.find((x) => x.path === path)!
      return { from_sha: 'a'.repeat(40), files: [{ ...f, patch: patchFor(path) }] }
    }
    return { from_sha: 'a'.repeat(40), files }
  }
  rpc.handlers['session.send'] = () => ({ queued: false })
  rpc.handlers['session.set_settings'] = (p) => ({ settings: { ...(p as { settings: object }).settings } })
  rpc.handlers['session.archive'] = () => ({})
  rpc.handlers['session.delete'] = () => ({})
  rpc.handlers['workspace.remove_worktree'] = () => ({})
  rpc.handlers['session.git_status'] = () => gitStatus()
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', info: { capabilities: { todo: true } } as never }) }, order: [N], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, agent: 'claude', status: 'idle', worktree: worktree(), settings: { options: { no_push: true } } }), node_id: N, cached_seq: 0 } as never }, pending: {}, open: {} })
  resetTodosMemory()
  useTodoHistory.getState().reset()
  useReworkDrafts.getState().reset()
  useGit.setState({ bySession: {} })
  useUi.setState({ toasts: [] })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('the conclusion comes first', () => {
  it('shows the latest result (sanitised Markdown), the changed files with line counts, and links to the session', async () => {
    rpc.handlers['todo.history'] = () => ({ events: [result(2, 'Fixed it. <script>alert(1)</script> **bold**'), result(1, 'older')], has_more: false })
    await mount()
    expect(await screen.findByTestId('review-result')).toHaveTextContent('Fixed it.')
    expect(screen.getByTestId('review-result').querySelector('script')).toBeNull()
    expect(screen.getByTestId('review-result')).not.toHaveTextContent('older')
    expect(await screen.findByTestId('review-changes')).toHaveTextContent('2 files changed')
    expect(screen.getByTestId('review-changes')).toHaveTextContent('+13')
    expect(screen.getByTestId('review-changes')).toHaveTextContent('−1')
    expect(screen.getByTestId('review-summary')).toContainElement(screen.getByTestId('history-session-link'))
  })

  it('with no cached events the denials are unknown: it says so and links to the session', async () => {
    await mount()
    expect(await screen.findByTestId('review-denials-unknown')).toBeInTheDocument()
  })

  it('counts the reviewer’s denials from the events this browser already has (permission_denied and auto_review only)', async () => {
    await db.putEvents(SID, [ev({ type: 'notice', code: 'permission_denied', level: 'info', message: 'Bash: no' } as never), ev({ type: 'notice', code: 'auto_review', level: 'warning', message: 'x' } as never), ev({ type: 'notice', code: 'rate_limit', level: 'info', message: 'y' } as never), ev({ type: 'text_delta', text: 'hi' } as never)])
    await mount()
    expect(await screen.findByTestId('review-denials')).toHaveTextContent('denied 2 actions')
  })

  it('a session that is gone leaves nothing to diff and says so', async () => {
    useSessions.setState({ list: {} })
    await mount()
    expect(await screen.findByTestId('review-no-session')).toBeInTheDocument()
    expect(screen.queryByTestId('review-files')).toBeNull()
  })

  it('an item that is not in review says so, but the actions stay (the person decides)', async () => {
    await mount({ id: 't1', activity: 'working' as never })
    expect(screen.getByTestId('review-not-ready')).toBeInTheDocument()
    expect(screen.getByTestId('review-approve')).toBeEnabled()
  })
})

describe('the files', () => {
  it('open one at a time with previous / next; the diff is the same view as the session page', async () => {
    const user = userEvent.setup()
    await mount()
    expect(await screen.findByTestId('review-file-name')).toHaveTextContent('src/a.ts (1 / 2)')
    expect(screen.getByTestId('review-prev')).toBeDisabled()
    await waitFor(() => expect(screen.getByTestId('review-diff').querySelector('[data-testid="diff-file"]')).not.toBeNull())
    await user.click(screen.getByTestId('review-next'))
    expect(screen.getByTestId('review-file-name')).toHaveTextContent('src/b.ts (2 / 2)')
    expect(screen.getByTestId('review-next')).toBeDisabled()
    await waitFor(() => expect(screen.getByTestId('review-diff').querySelector('[data-path="src/b.ts"]')).not.toBeNull())
    const patchCalls = rpc.calls.filter((c) => c.method === 'session.diff' && (c.params as SessionDiffParams).path)
    expect(patchCalls.map((c) => (c.params as SessionDiffParams).path)).toEqual(['src/a.ts', 'src/b.ts'])
  })

  it('a list click jumps to that file', async () => {
    const user = userEvent.setup()
    await mount()
    await screen.findByTestId('review-file-name')
    await user.click(screen.getAllByTestId('review-file')[1]!)
    expect(screen.getByTestId('review-file-name')).toHaveTextContent('src/b.ts')
  })
})

describe('the fixed actions', () => {
  it('Approve sets the state to done', async () => {
    const user = userEvent.setup()
    await mount()
    await user.click(screen.getByTestId('review-approve'))
    await waitFor(() => expect(daemon.todos.get('t1')!.state).toBe('done'))
  })

  it('Needs more work sends the typed text to the latest linked session only, and nothing without text', async () => {
    const user = userEvent.setup()
    await mount()
    await user.click(screen.getByTestId('review-rework'))
    expect(screen.getByTestId('review-rework-send')).toBeDisabled()
    await user.type(screen.getByTestId('review-rework-input'), 'also handle the 404 case')
    await user.click(screen.getByTestId('review-rework-send'))
    await waitFor(() => expect(sent()).toEqual(['also handle the 404 case']))
    expect(rpc.calls.find((c) => c.method === 'session.send')!.params).toMatchObject({ session_id: SID })
    // 状态没有被改：activity 由 daemon 随会话改回处理中
    expect(daemon.todos.get('t1')!.state).toBe('open')
  })

  it('Needs more work targets the latest linked session that still exists', async () => {
    const user = userEvent.setup()
    useSessions.setState({ list: { ...useSessions.getState().list, sess_old: { ...summary({ id: 'sess_old', status: 'idle' }), node_id: N, cached_seq: 0 } as never } })
    await mount({ id: 't1', sessions: ['sess_old', SID, 'sess_deleted'] as never })
    await user.click(screen.getByTestId('review-rework'))
    await user.type(screen.getByTestId('review-rework-input'), 'again')
    await user.click(screen.getByTestId('review-rework-send'))
    await waitFor(() => expect(sent()).toEqual(['again']))
    expect(rpc.calls.find((c) => c.method === 'session.send')!.params).toMatchObject({ session_id: SID })
  })

  it('Drop sets the state to dropped and then asks whether to clean up the worktree and branch; “keep them” leaves everything', async () => {
    const user = userEvent.setup()
    await mount()
    await user.click(screen.getByTestId('review-drop'))
    await waitFor(() => expect(daemon.todos.get('t1')!.state).toBe('dropped'))
    expect(await screen.findByTestId('review-cleanup-dialog')).toBeInTheDocument()
    await user.click(screen.getByTestId('review-cleanup-keep'))
    expect(rpc.calls.some((c) => c.method === 'session.archive' || c.method === 'session.delete')).toBe(false)
  })

  // 清理：会话和它的记录永远留着（只归档，从不 session.delete）；缺省什么都不丢
  const called = (m: string) => rpc.calls.filter((c) => c.method === m).map((c) => c.params as Record<string, unknown>)
  const dropAndOpenCleanup = async (user: ReturnType<typeof userEvent.setup>): Promise<void> => {
    await user.click(screen.getByTestId('review-drop'))
    await screen.findByTestId('review-cleanup-dialog')
    await waitFor(() => expect(screen.getByTestId('review-cleanup-status')).toBeInTheDocument())
  }

  it('the default clean-up is the safe one: archive with worktree “auto” (the node keeps anything with uncommitted changes), no remove_worktree, never session.delete', async () => {
    const user = userEvent.setup()
    await mount()
    await dropAndOpenCleanup(user)
    expect(screen.getByTestId('review-cleanup-auto')).toBeChecked()
    await user.click(screen.getByTestId('review-cleanup-confirm'))
    await waitFor(() => expect(called('session.archive')).toEqual([{ session_id: SID, worktree: 'auto' }]))
    expect(called('workspace.remove_worktree')).toEqual([])
    expect(called('session.delete')).toEqual([])
  })

  it('it shows the worktree’s git status when it opens: uncommitted files, commits not on the base, commits not pushed', async () => {
    rpc.handlers['session.git_status'] = () => gitStatus({ uncommitted: 3, ahead: 2, unpushed: 1 })
    const user = userEvent.setup()
    await mount()
    await dropAndOpenCleanup(user)
    await waitFor(() => expect(screen.getByTestId('review-cleanup-status')).toHaveTextContent('3 uncommitted files'))
    expect(screen.getByTestId('review-cleanup-status')).toHaveTextContent('2 commits on the branch that are not on its base')
    expect(screen.getByTestId('review-cleanup-status')).toHaveTextContent('1 commit not pushed')
    expect(called('session.git_status')).toEqual([{ session_id: SID }])
  })

  it('“keep” archives the session and keeps the worktree; “remove” archives keeping it first, then calls workspace.remove_worktree (the session stays)', async () => {
    const user = userEvent.setup()
    await mount()
    await dropAndOpenCleanup(user)
    await user.click(screen.getByTestId('review-cleanup-option-keep'))
    await user.click(screen.getByTestId('review-cleanup-confirm'))
    await waitFor(() => expect(called('session.archive')).toEqual([{ session_id: SID, worktree: 'keep' }]))
    expect(called('workspace.remove_worktree')).toEqual([])
  })

  it('a clean worktree can be removed without the discard tick: remove_worktree has no force', async () => {
    const user = userEvent.setup()
    await mount()
    await dropAndOpenCleanup(user)
    await user.click(screen.getByTestId('review-cleanup-option-remove'))
    expect(screen.queryByTestId('review-cleanup-force')).toBeNull()
    await user.click(screen.getByTestId('review-cleanup-confirm'))
    await waitFor(() => expect(called('workspace.remove_worktree')).toEqual([{ id: 'wt_1' }]))
  })

  it('a dirty worktree cannot be removed until the per-worktree “discard N uncommitted files” box is ticked; then remove_worktree gets force', async () => {
    rpc.handlers['session.git_status'] = () => gitStatus({ uncommitted: 4 })
    const user = userEvent.setup()
    await mount()
    await dropAndOpenCleanup(user)
    await waitFor(() => expect(screen.getByTestId('review-cleanup-status')).toHaveTextContent('4 uncommitted files'))
    await user.click(screen.getByTestId('review-cleanup-option-remove'))
    expect(screen.getByTestId('review-cleanup-force')).not.toBeChecked()
    expect(screen.getByTestId('review-cleanup-confirm')).toBeDisabled()
    await user.click(screen.getByTestId('review-cleanup-force'))
    expect(screen.getByTestId('review-cleanup-confirm')).toBeEnabled()
    await user.click(screen.getByTestId('review-cleanup-confirm'))
    await waitFor(() => expect(called('workspace.remove_worktree')).toEqual([{ id: 'wt_1', force: true }]))
    expect(called('session.archive')).toEqual([{ session_id: SID, worktree: 'keep' }])
    expect(called('session.delete')).toEqual([])
  })

  it('“also delete the branch” sends delete_branch and names the commits that would be lost; the session is never deleted', async () => {
    rpc.handlers['session.git_status'] = () => gitStatus({ ahead: 2, unpushed: 2 })
    const user = userEvent.setup()
    await mount()
    await dropAndOpenCleanup(user)
    await user.click(screen.getByTestId('review-cleanup-option-remove'))
    await user.click(screen.getByTestId('review-cleanup-delete-branch'))
    expect(screen.getByTestId('review-cleanup-commits-lost')).toHaveTextContent('2 commits')
    await user.click(screen.getByTestId('review-cleanup-confirm'))
    await waitFor(() => expect(called('workspace.remove_worktree')).toEqual([{ id: 'wt_1', delete_branch: true }]))
    expect(called('session.delete')).toEqual([])
  })

  it('removing needs the git status first: while it is unknown (or failed to load) the confirm stays disabled', async () => {
    rpc.handlers['session.git_status'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    await mount()
    await user.click(screen.getByTestId('review-drop'))
    await screen.findByTestId('review-cleanup-dialog')
    await user.click(screen.getByTestId('review-cleanup-option-remove'))
    expect(screen.getByTestId('review-cleanup-confirm')).toBeDisabled()
    // 「自动」仍然可以：由 daemon 决定，不会丢东西
    await user.click(screen.getByTestId('review-cleanup-auto'))
    expect(screen.getByTestId('review-cleanup-confirm')).toBeEnabled()
  })

  it('lists the worktree of every linked session (not only the latest), each with its own choice', async () => {
    const user = userEvent.setup()
    useSessions.setState({ list: { ...useSessions.getState().list, sess_b: { ...summary({ id: 'sess_b', status: 'idle', worktree: worktree({ id: 'wt_2', branch: 'anycon/other' }) }), node_id: N, cached_seq: 0 } as never } })
    await mount({ id: 't1', sessions: ['sess_b', SID] as never })
    await user.click(screen.getByTestId('review-drop'))
    await screen.findByTestId('review-cleanup-dialog')
    expect(screen.getAllByTestId('review-cleanup-row')).toHaveLength(2)
    const rowB = screen.getAllByTestId('review-cleanup-row').find((r) => r.getAttribute('data-session') === 'sess_b')!
    await user.click(rowB.querySelector('[data-testid="review-cleanup-option-keep"]')!)
    await user.click(screen.getByTestId('review-cleanup-confirm'))
    await waitFor(() => expect(called('session.archive')).toHaveLength(2))
    expect(called('session.archive')).toEqual(expect.arrayContaining([{ session_id: 'sess_b', worktree: 'keep' }, { session_id: SID, worktree: 'auto' }]))
  })

  it('without a worktree there is no clean-up question', async () => {
    const user = userEvent.setup()
    useSessions.setState({ list: { [SID]: { ...summary({ id: SID, status: 'idle' }), node_id: N, cached_seq: 0 } as never } })
    await mount()
    await user.click(screen.getByTestId('review-drop'))
    await waitFor(() => expect(daemon.todos.get('t1')!.state).toBe('dropped'))
    expect(screen.queryByTestId('review-cleanup-dialog')).toBeNull()
  })

  it('offline: rework is disabled', async () => {
    await mount({ id: 't1' }, { online: false })
    expect(screen.getByTestId('review-rework')).toBeDisabled()
  })
})

describe('after approving: commit, push and open a pull request', () => {
  it('never turns “no push” off silently: it asks first, and only a confirmation sets no_push to false (then the fixed message is sent)', async () => {
    const user = userEvent.setup()
    await mount({ id: 't1', state: 'done' as never, activity: undefined })
    await user.click(screen.getByTestId('review-push'))
    expect(await screen.findByTestId('review-push-dialog')).toHaveTextContent(/lets the agent push|push the branch/i)
    expect(settingsCalls()).toEqual([])
    expect(sent()).toEqual([])
    // 取消：什么都没发生
    await user.click(screen.getByTestId('review-push-cancel'))
    expect(settingsCalls()).toEqual([])
    expect(sent()).toEqual([])
    // 确认：先关掉 no_push（设置里 no_push = false），再发消息
    await user.click(screen.getByTestId('review-push'))
    await user.click(await screen.findByTestId('review-push-confirm'))
    await waitFor(() => expect(sent()).toHaveLength(1))
    expect(settingsCalls()).toHaveLength(1)
    expect(settingsCalls()[0]!.settings.options).toEqual({ no_push: false })
    expect(sent()[0]).toMatch(/anycon\/fix-login/)
    expect(sent()[0]).toMatch(/push the branch to the remote and open a pull request/)
    // 先设置、后发消息
    const order = rpc.methods().filter((m) => m === 'session.set_settings' || m === 'session.send')
    expect(order).toEqual(['session.set_settings', 'session.send'])
  })

  it('a refused settings change sends nothing', async () => {
    const user = userEvent.setup()
    rpc.handlers['session.set_settings'] = () => {
      throw rpcError('busy')
    }
    await mount({ id: 't1', state: 'done' as never, activity: undefined })
    await user.click(screen.getByTestId('review-push'))
    await user.click(await screen.findByTestId('review-push-confirm'))
    await waitFor(() => expect(useUi.getState().toasts.some((x) => x.kind === 'error')).toBe(true))
    expect(sent()).toEqual([])
  })

  it('a session that already allows pushing is just asked, without a settings change', async () => {
    const user = userEvent.setup()
    useSessions.setState({ list: { [SID]: { ...summary({ id: SID, status: 'idle', worktree: worktree(), settings: { options: { no_push: false } } }), node_id: N, cached_seq: 0 } as never } })
    await mount({ id: 't1', state: 'done' as never, activity: undefined })
    await user.click(screen.getByTestId('review-push'))
    await waitFor(() => expect(sent()).toHaveLength(1))
    expect(settingsCalls()).toEqual([])
    expect(screen.queryByTestId('review-push-dialog')).toBeNull()
  })

  it('the quick action only appears once the item is approved', async () => {
    await mount()
    expect(screen.queryByTestId('review-push')).toBeNull()
  })
})

describe('Needs more work: honest about what was sent', () => {
  it('a link failure (unsent) is not reported as sent: the text and the box stay and the toast says it was not sent', async () => {
    rpc.handlers['session.send'] = () => {
      throw linkError('timeout')
    }
    const user = userEvent.setup()
    await mount()
    await user.click(screen.getByTestId('review-rework'))
    await user.type(screen.getByTestId('review-rework-input'), 'try the other approach')
    await user.click(screen.getByTestId('review-rework-send'))
    await waitFor(() => expect(useUi.getState().toasts.some((t) => /Not sent/.test(t.text))).toBe(true))
    expect(useUi.getState().toasts.some((t) => /Sent to the agent/.test(t.text))).toBe(false)
    expect(screen.getByTestId('review-rework-input')).toHaveValue('try the other approach')
    expect(useReworkDrafts.getState().byKey[`${N}:t1`]).toBe('try the other approach')
  })

  it('a sent message clears the draft and closes the box', async () => {
    const user = userEvent.setup()
    await mount()
    await user.click(screen.getByTestId('review-rework'))
    await user.type(screen.getByTestId('review-rework-input'), 'again')
    expect(useReworkDrafts.getState().byKey[`${N}:t1`]).toBe('again')
    await user.click(screen.getByTestId('review-rework-send'))
    await waitFor(() => expect(sent()).toEqual(['again']))
    await waitFor(() => expect(screen.queryByTestId('review-rework-input')).toBeNull())
    expect(useReworkDrafts.getState().byKey).toEqual({})
  })

  it('the text survives leaving the page and coming back (the box opens with it)', async () => {
    const user = userEvent.setup()
    await mount()
    await user.click(screen.getByTestId('review-rework'))
    await user.type(screen.getByTestId('review-rework-input'), 'half a thought')
    const first = screen.getByTestId('todo-review')
    first.ownerDocument.body.innerHTML = ''
    render(<Live />)
    expect(await screen.findByTestId('review-rework-input')).toHaveValue('half a thought')
  })

  it('is disabled with the reason while the latest linked session is still working', async () => {
    useSessions.setState({ list: { [SID]: { ...useSessions.getState().list[SID]!, status: 'running' } as never } })
    await mount()
    expect(screen.getByTestId('review-rework')).toBeDisabled()
    expect(screen.getByTestId('review-rework-why')).toHaveTextContent('still working')
  })

  it('does not fall back to an older session while the node has not finished syncing: the actions wait and say why; once synced it uses the older one', async () => {
    const user = userEvent.setup()
    useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', synced: false, info: { capabilities: { todo: true } } as never }) } })
    await mount({ id: 't1', sessions: [SID, 'sess_not_loaded_yet'] as never })
    expect(screen.getByTestId('review-resolving')).toBeInTheDocument()
    expect(screen.getByTestId('review-approve')).toBeDisabled()
    expect(screen.getByTestId('review-rework')).toBeDisabled()
    expect(screen.getByTestId('review-drop')).toBeDisabled()
    expect(screen.queryByTestId('review-files')).toBeNull()
    expect(rpc.calls.some((c) => c.method === 'session.diff')).toBe(false)
    // 同步完了、它仍然不在：才算被删掉，退到更早的那个
    await act(async () => useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', synced: true, info: { capabilities: { todo: true } } as never }) } }))
    await waitFor(() => expect(screen.getByTestId('review-rework')).toBeEnabled())
    await user.click(screen.getByTestId('review-rework'))
    await user.type(screen.getByTestId('review-rework-input'), 'hi')
    await user.click(screen.getByTestId('review-rework-send'))
    await waitFor(() => expect(rpc.calls.find((c) => c.method === 'session.send')!.params).toMatchObject({ session_id: SID }))
  })
})

describe('the denial count is only exact when the cache covers the whole session', () => {
  const denial = (n: number) => ev({ type: 'notice', code: 'permission_denied', level: 'info', message: `no ${n}` } as never)
  it('a cache that covers [1, last_seq] gives the exact count', async () => {
    resetSeq()
    const evs = [denial(1), ev({ type: 'text_delta', text: 'x' } as never), denial(2)]
    await db.putEvents(SID, evs)
    useSessions.setState({ list: { [SID]: { ...useSessions.getState().list[SID]!, last_seq: evs.length } as never } })
    await mount()
    expect(await screen.findByTestId('review-denials')).toHaveAttribute('data-complete', 'true')
    expect(screen.getByTestId('review-denials')).toHaveTextContent('denied 2 actions')
  })

  it('a partial cache says “at least N” and points at the session', async () => {
    resetSeq()
    const evs = [denial(1), denial(2)]
    await db.putEvents(SID, evs)
    // 会话已经有 50 个事件，这个浏览器只读到过 2 个
    useSessions.setState({ list: { [SID]: { ...useSessions.getState().list[SID]!, last_seq: 50 } as never } })
    await mount()
    const el = await screen.findByTestId('review-denials')
    expect(el).toHaveAttribute('data-complete', 'false')
    expect(el).toHaveTextContent('at least 2 actions')
  })

  it('a partial cache with no denial in it is unknown, not “0”', async () => {
    resetSeq()
    await db.putEvents(SID, [ev({ type: 'text_delta', text: 'x' } as never)])
    useSessions.setState({ list: { [SID]: { ...useSessions.getState().list[SID]!, last_seq: 50 } as never } })
    await mount()
    expect(await screen.findByTestId('review-denials-unknown')).toBeInTheDocument()
    expect(screen.queryByTestId('review-denials')).toBeNull()
  })
})

describe('the page does not re-read on every event', () => {
  it('a new event (last_seq) does not re-run session.diff; the turn ending (status change) and the Refresh button do', async () => {
    const user = userEvent.setup()
    await mount()
    await screen.findByTestId('review-changes')
    const diffs = () => rpc.calls.filter((c) => c.method === 'session.diff' && !(c.params as { path?: string }).path).length
    const before = diffs()
    await act(async () => useSessions.setState({ list: { [SID]: { ...useSessions.getState().list[SID]!, last_seq: 99 } as never } }))
    await act(async () => {
      for (let i = 0; i < 6; i++) await new Promise((r) => setTimeout(r, 5))
    })
    expect(diffs()).toBe(before)
    await act(async () => useSessions.setState({ list: { [SID]: { ...useSessions.getState().list[SID]!, status: 'running' } as never } }))
    await waitFor(() => expect(diffs()).toBe(before + 1))
    await act(async () => useSessions.setState({ list: { [SID]: { ...useSessions.getState().list[SID]!, status: 'idle' } as never } }))
    await waitFor(() => expect(diffs()).toBe(before + 2))
    await user.click(screen.getByTestId('review-refresh'))
    await waitFor(() => expect(diffs()).toBe(before + 3))
  })
})
