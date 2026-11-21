// 验收页（spec/web.md § 4.12）：结论在前（最后一轮的结果、改动统计、审查器拒绝次数）、文件一个一个看、三个固定动作；
// 通过之后的「提交、推送并开 PR」要先明确确认才会关掉「不 push」。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { DiffFile, SessionDiffParams } from '@/protocol'
import * as db from '@/db'
import { diffFile, ev, nodeEntry, patchFor, resetSeq, summary, todoItem, worktree } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { resetDrafts } from '@/stores/drafts'
import { useTodoHistory } from '../history'
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
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', info: { capabilities: { todo: true } } as never }) }, order: [N], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, agent: 'claude', status: 'idle', worktree: worktree(), settings: { options: { no_push: true } } }), node_id: N, cached_seq: 0 } as never }, pending: {}, open: {} })
  resetTodosMemory()
  useTodoHistory.getState().reset()
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

  it('Drop with clean-up archives the session removing its worktree; ticking “delete the branch” deletes the session with the branch', async () => {
    const user = userEvent.setup()
    await mount()
    await user.click(screen.getByTestId('review-drop'))
    await screen.findByTestId('review-cleanup-dialog')
    await user.click(screen.getByTestId('review-cleanup-confirm'))
    await waitFor(() => expect(rpc.calls.find((c) => c.method === 'session.archive')?.params).toMatchObject({ session_id: SID, worktree: 'remove' }))
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
