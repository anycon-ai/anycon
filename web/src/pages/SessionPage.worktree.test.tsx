// 会话页里的 worktree / git（ADR-34，web.md § 4.6）：分支 chip、主区「改动」视图、git 事件、归档 / 删除确认框、刷新时机。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Event, SessionSummary } from '@/protocol'
import { diffFile, ev, gitProject, gitStatus, mockAgent, nodeEntry, nodeInfo, project, resetSeq, summary, worktree } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts } from '@/stores/drafts'
import { useGit } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
// 不真的去 daemon 拉会话：事件直接放进 store
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

interface Seed {
  summary?: Partial<SessionSummary>
  events?: Event[]
  status?: ReturnType<typeof gitStatus>
  projects?: ReturnType<typeof project>[]
  noWorktreeCapability?: boolean
  conn?: 'ready' | 'peer_offline'
}

function seed({ summary: over = {}, events = [], status = gitStatus(), projects = [gitProject('app', { branch: 'main' })], noWorktreeCapability, conn = 'ready' }: Seed = {}): void {
  rpc.handlers['session.git_status'] = () => status
  rpc.handlers['session.diff'] = () => ({ from_sha: 'x', files: [diffFile({ path: 'src/a.ts' })] })
  rpc.handlers['session.archive'] = () => ({})
  rpc.handlers['session.unarchive'] = () => ({})
  rpc.handlers['session.delete'] = () => ({})
  rpc.handlers['session.send'] = () => ({ queued: false })
  rpc.handlers['session.sync_base'] = () => ({ outcome: 'aborted', commits: 0 })
  const info = noWorktreeCapability ? nodeInfo({ capabilities: { audit: true, blobs: false, fs: true, search: false, workspace: true }, worktree: null }) : nodeInfo()
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects, info, conn }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, project: 'app', ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, events.map((e) => ev(e)))
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
}
const push = (...events: Event[]): void => {
  act(() => {
    useSessions.getState().ingest(SID, events.map((e) => ev(e)))
  })
}
const statusCalls = () => rpc.calls.filter((c) => c.method === 'session.git_status')
const paramsOf = (method: string) => rpc.calls.filter((c) => c.method === method).map((c) => c.params)

beforeEach(() => {
  resetSeq()
  navigate.mockReset()
  rpc = new FakeRpc()
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  useGit.setState({ bySession: {} })
  usePrefs.setState({ infoPanelOpen: true })
  resetDrafts()
})
afterEach(() => setRpc(null))

describe('SessionPage: the branch chip', () => {
  it('shows ⑂ branch ← base with ahead / behind / uncommitted once the status is in, above the input on the right', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ ahead: 3, behind: 1, uncommitted: 5 }) })
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('session-branch')
    await waitFor(() => expect(within(chip).getByTestId('session-branch-ahead')).toHaveTextContent('↑3'))
    expect(chip).toHaveAttribute('data-state', 'active')
    expect(within(chip).getByTestId('session-branch-name')).toHaveTextContent('anycon/fix-login')
    expect(within(chip).getByTestId('session-branch-base')).toHaveTextContent('← main')
    expect(within(chip).getByTestId('session-branch-behind')).toHaveTextContent('↓1')
    expect(within(chip).getByTestId('session-branch-uncommitted')).toHaveTextContent('5 uncommitted')
    expect(chip).toHaveAccessibleName(/anycon\/fix-login based on main 3 ahead 1 behind 5 uncommitted/)
    // 宽屏在输入框上方靠右，不在下面的选项行里；顶栏只剩状态 chip
    expect(screen.getByTestId('composer-aside')).toContainElement(chip)
    expect(screen.getByTestId('option-controls')).not.toContainElement(chip)
    expect(chip.compareDocumentPosition(screen.getByTestId('composer-box')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
  })

  it('on a narrow screen the chip sits with the other options in the ⚙ sheet', async () => {
    const mm = vi.spyOn(window, 'matchMedia').mockImplementation(
      (query: string) => ({ matches: false, media: query, onchange: null, addEventListener: () => {}, removeEventListener: () => {}, addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false }) as unknown as MediaQueryList,
    )
    try {
      seed({ summary: { worktree: worktree() }, status: gitStatus() })
      const user = userEvent.setup()
      render(<SessionPage sessionId={SID} />)
      await user.click(await screen.findByTestId('composer-options-button'))
      expect(await screen.findByTestId('options-sheet')).toContainElement(await screen.findByTestId('session-branch'))
      expect(screen.queryByTestId('composer-aside')).toBeNull()
    } finally {
      mm.mockRestore()
    }
  })

  it('the top bar has Chat / Changes with icons, and Changes shows how many files are uncommitted', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ uncommitted: 5 }) })
    render(<SessionPage sessionId={SID} />)
    const header = (await screen.findByTestId('main-view')).closest('header')!
    expect(header).toContainElement(screen.getByTestId('session-status'))
    expect(header).toContainElement(screen.getByTestId('session-title'))
    expect(screen.getByTestId('main-view-chat').querySelector('svg')).not.toBeNull()
    expect(screen.getByTestId('main-view-changes').querySelector('svg')).not.toBeNull()
    await waitFor(() => expect(screen.getByTestId('main-view-changes-count')).toHaveTextContent('5'))
    expect(header).not.toContainElement(screen.getByTestId('session-branch'))
  })

  it('leaves out the parts that are zero', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus() })
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('session-branch')
    await waitFor(() => expect(statusCalls().length).toBeGreaterThan(0))
    expect(within(chip).queryByTestId('session-branch-ahead')).toBeNull()
    expect(within(chip).queryByTestId('session-branch-behind')).toBeNull()
    expect(within(chip).queryByTestId('session-branch-uncommitted')).toBeNull()
  })

  it('creating: shows a spinner and “Creating worktree…”, and does not call git_status yet', async () => {
    seed({ summary: { worktree: worktree({ state: 'creating' }) } })
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('session-branch')
    expect(chip).toHaveAttribute('data-state', 'creating')
    expect(chip).toHaveTextContent('Creating worktree…')
    await act(async () => {})
    expect(statusCalls()).toHaveLength(0)
  })

  it('creating → active: pulls the status as soon as the daemon reports the worktree as active (session.changed)', async () => {
    seed({ summary: { worktree: worktree({ state: 'creating' }) } })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-branch')
    act(() => {
      useSessions.getState().applyChanged(NODE, { session_id: SID, title: 'hello', status: 'idle', archived: false, pending_permissions: 0, updated_at_ms: 3, last_seq: 0, worktree: worktree({ state: 'active' }) })
    })
    await waitFor(() => expect(screen.getByTestId('session-branch')).toHaveAttribute('data-state', 'active'))
    await waitFor(() => expect(statusCalls().filter((c) => !(c.params as { include_pr?: boolean }).include_pr)).toHaveLength(1))
  })

  it.each([
    ['missing', 'Worktree missing'],
    ['failed', 'Worktree failed'],
  ] as const)('%s: a warning with an icon and the words, not just a colour; no git_status calls', async (state, text) => {
    seed({ summary: { worktree: worktree({ state }) } })
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('session-branch')
    expect(chip).toHaveAttribute('data-state', state)
    expect(chip).toHaveTextContent(text)
    await act(async () => {})
    expect(statusCalls()).toHaveLength(0)
  })

  it('kept (archived with uncommitted changes): neutral, with the word “Kept”', async () => {
    seed({ summary: { archived: true, worktree: worktree({ state: 'kept' }) }, status: gitStatus({ uncommitted: 2 }) })
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('session-branch')
    await waitFor(() => expect(chip).toHaveTextContent('2 uncommitted'))
    expect(chip).toHaveAttribute('data-state', 'kept')
    expect(chip).toHaveTextContent('Kept')
  })

  it('removed: shows the branch and “Removed”, and does not ask the node for a status (the folder is gone)', async () => {
    seed({ summary: { archived: true, worktree: worktree({ state: 'removed' }) } })
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('session-branch')
    expect(chip).toHaveAttribute('data-state', 'removed')
    expect(chip).toHaveTextContent('anycon/fix-login')
    expect(chip).toHaveTextContent('Removed')
    await act(async () => {})
    expect(statusCalls()).toHaveLength(0)
  })

  it('a session working in the project folder shows only the branch and the uncommitted count (no base)', async () => {
    seed({ status: gitStatus({ branch: 'main', base: null, base_sha: null, ahead: null, behind: null, uncommitted: 2 }) })
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('session-branch')
    await waitFor(() => expect(within(chip).getByTestId('session-branch-name')).toHaveTextContent('main'))
    expect(chip).toHaveAttribute('data-state', 'local')
    expect(within(chip).queryByTestId('session-branch-base')).toBeNull()
    expect(within(chip).getByTestId('session-branch-uncommitted')).toHaveTextContent('2 uncommitted')
  })

  it('shows a merge in progress as a warning with the word “merging”', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ merging: true, conflicts: 2 }) })
    render(<SessionPage sessionId={SID} />)
    const chip = await screen.findByTestId('session-branch')
    await waitFor(() => expect(chip).toHaveTextContent('merging'))
  })

  it('clicking the chip switches the main area from the chat to the Changes view', async () => {
    seed({ summary: { worktree: worktree() } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    expect(screen.queryByTestId('changes-view')).toBeNull()
    expect(await screen.findByTestId('composer')).toBeInTheDocument()
    await user.click(await screen.findByTestId('session-branch'))
    expect(await screen.findByTestId('changes-tab')).toBeInTheDocument()
    expect(screen.getByTestId('main-view-changes')).toHaveAttribute('data-state', 'on')
    // 「改动」占满聊天区：消息列表和输入框都让出位置
    expect(screen.queryByTestId('message-list')).toBeNull()
    expect(screen.queryByTestId('composer')).toBeNull()
  })

  it('is not there without worktree support on the node, for a scratch session, or in a non-git project', async () => {
    seed({ noWorktreeCapability: true, summary: { worktree: worktree() } })
    const a = render(<SessionPage sessionId={SID} />)
    await act(async () => {})
    expect(screen.queryByTestId('session-branch')).toBeNull()
    expect(screen.queryByTestId('main-view')).toBeNull()
    expect(statusCalls()).toHaveLength(0)
    a.unmount()

    seed({ summary: { project: null } })
    const b = render(<SessionPage sessionId={SID} />)
    await act(async () => {})
    expect(screen.queryByTestId('session-branch')).toBeNull()
    b.unmount()

    seed({ projects: [project({ name: 'app' })] })
    render(<SessionPage sessionId={SID} />)
    await act(async () => {})
    expect(screen.queryByTestId('session-branch')).toBeNull()
    expect(screen.queryByTestId('main-view')).toBeNull()
    expect(statusCalls()).toHaveLength(0)
  })

  it('does not call the node while it is offline', async () => {
    seed({ summary: { worktree: worktree() }, conn: 'peer_offline' })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-branch')
    expect(statusCalls()).toHaveLength(0)
  })
})

describe('SessionPage: the Changes view', () => {
  it('a worktree session opens on the chat; the Chat / Changes switch moves between them, and Overview stays in the right panel', async () => {
    seed({ summary: { worktree: worktree() } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('info-overview')).toBeInTheDocument()
    expect(screen.queryByTestId('changes-tab')).toBeNull()
    expect(screen.getByTestId('main-view-chat')).toHaveAttribute('data-state', 'on')
    expect(screen.getByTestId('info-branch')).toHaveTextContent('anycon/fix-login')
    expect(screen.getByTestId('info-branch')).toHaveTextContent('← main')
    await user.click(screen.getByTestId('main-view-changes'))
    expect(await screen.findByTestId('changes-tab')).toBeInTheDocument()
    expect(screen.getByTestId('info-overview')).toBeInTheDocument()
    await user.click(screen.getByTestId('main-view-chat'))
    expect(await screen.findByTestId('composer')).toBeInTheDocument()
    expect(screen.queryByTestId('changes-tab')).toBeNull()
  })

  it('a session in a git project (no worktree) has the Changes view too, with only the uncommitted view', async () => {
    seed({ status: gitStatus({ branch: 'main', base: null, base_sha: null, ahead: null, behind: null, uncommitted: 1 }) })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('main-view-changes'))
    expect(await screen.findByTestId('diff-uncommitted-only')).toBeInTheDocument()
    expect(screen.queryByTestId('diff-against')).toBeNull()
    expect(await screen.findByTestId('diff-file-row')).toHaveAttribute('data-path', 'src/a.ts')
    expect(paramsOf('session.diff')).toEqual([{ session_id: SID, against: 'uncommitted' }])
  })

  it('shows the worktree folder, not the project folder, as the working directory', async () => {
    seed({ summary: { worktree: worktree() }, projects: [gitProject('app')] })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByText('/workspace/app/.anycon/worktrees/fix-login')).toBeInTheDocument()
  })
})

describe('SessionPage: git status refresh timing', () => {
  it('pulls on entering (once, no include_pr from the page itself) and again after turn_done — never on a timer', async () => {
    seed({ summary: { worktree: worktree() }, events: [{ type: 'turn_start', turn_id: 1, message_id: 'm1' }] })
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(statusCalls()).toHaveLength(1))
    expect((statusCalls()[0]!.params as { include_pr?: boolean }).include_pr).toBeUndefined()
    push({ type: 'text_delta', text: 'hi' })
    await act(async () => {})
    expect(statusCalls()).toHaveLength(1)
    push({ type: 'turn_done', turn_id: 1, reason: 'completed' })
    await waitFor(() => expect(statusCalls()).toHaveLength(2))
    expect((statusCalls()[1]!.params as { include_pr?: boolean }).include_pr).toBeUndefined() // PR 不在这时查
  })

  it('opening the Changes view looks up the PR once', async () => {
    seed({ summary: { worktree: worktree() } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(statusCalls()).toHaveLength(1))
    await user.click(await screen.findByTestId('main-view-changes'))
    await screen.findByTestId('changes-tab')
    await waitFor(() => expect(statusCalls().map((c) => (c.params as { include_pr?: boolean }).include_pr)).toEqual([undefined, true]))
  })
})

describe('SessionPage: git events in the message stream', () => {
  const created: Event = { type: 'git', action: 'worktree_created', branch: 'anycon/fix-login', base: 'main', count: 2 }

  it('renders worktree_created, kept, removed, restored, missing and failed as centred system lines', async () => {
    seed({
      summary: { worktree: worktree() },
      events: [
        created,
        { type: 'git', action: 'worktree_failed', branch: 'anycon/x', note: 'git worktree add failed' },
        { type: 'git', action: 'worktree_kept', branch: 'anycon/x' },
        { type: 'git', action: 'worktree_removed', branch: 'anycon/x', note: 'branch_deleted' },
        { type: 'git', action: 'worktree_removed', branch: 'anycon/y', note: 'branch_kept' },
        { type: 'git', action: 'worktree_removed', branch: 'anycon/z' },
        { type: 'git', action: 'worktree_restored', branch: 'anycon/x' },
        { type: 'git', action: 'worktree_missing', branch: 'anycon/x' },
      ],
    })
    render(<SessionPage sessionId={SID} />)
    const lines = await screen.findAllByTestId('git-event')
    expect(lines.map((l) => l.textContent)).toEqual([
      'Started in worktree anycon/fix-login (based on main, copied 2 files)',
      'Could not create worktree anycon/x: git worktree add failed',
      'Worktree anycon/x has uncommitted changes, so it was kept',
      'Deleted worktree anycon/x (branch deleted)',
      'Deleted worktree anycon/y (branch kept)',
      'Deleted worktree anycon/z',
      'Rebuilt worktree anycon/x from its branch',
      'The folder of worktree anycon/x is gone',
    ])
    expect(lines[0]).toHaveAttribute('data-action', 'worktree_created')
  })

  it('renders the sync results: up to date, merged N commits, aborted', async () => {
    seed({
      summary: { worktree: worktree() },
      events: [
        { type: 'git', action: 'sync_up_to_date', branch: 'b', base: 'main' },
        { type: 'git', action: 'sync_merged', branch: 'b', base: 'main', count: 3 },
        { type: 'git', action: 'sync_merged', branch: 'b', base: 'main', count: 1 },
        { type: 'git', action: 'sync_aborted', branch: 'b', base: 'main' },
      ],
    })
    render(<SessionPage sessionId={SID} />)
    const lines = await screen.findAllByTestId('git-event')
    expect(lines.map((l) => l.textContent)).toEqual(['main has no new commits', 'Merged 3 commits from main', 'Merged 1 commit from main', 'Merge of main abandoned'])
  })

  it('conflicts: lists the files and offers “Let the agent resolve” and “Abandon merge” on the latest conflict', async () => {
    seed({
      summary: { worktree: worktree() },
      status: gitStatus({ merging: true, conflicts: 2 }),
      events: [{ type: 'git', action: 'sync_conflicts', branch: 'anycon/fix-login', base: 'main', files: ['a.rs', 'b.rs'] }],
    })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    const line = await screen.findByTestId('git-event')
    // 系统提示里列文件名没问题（只是显示）；发给 agent 的用户消息里不能有
    expect(line).toHaveTextContent('Merging main: 2 files have conflicts (a.rs, b.rs)')
    await user.click(within(line).getByTestId('git-event-resolve'))
    await waitFor(() => expect(paramsOf('session.send')).toHaveLength(1))
    const content = (paramsOf('session.send')[0] as { content: string }).content
    expect(content).toBe('Merging main into anycon/fix-login hit conflicts. Resolve the files that `git status` lists as conflicted and finish the merge commit.')
    expect(content).not.toMatch(/a\.rs|b\.rs/)
    await user.click(within(line).getByTestId('git-event-abort'))
    await waitFor(() => expect(paramsOf('session.sync_base')).toEqual([{ session_id: SID, abort: true }]))
  })

  it('after the merge was abandoned (a newer sync event) the old conflict line has no buttons; neither does it once the status says the merge is over', async () => {
    seed({
      summary: { worktree: worktree() },
      status: gitStatus(), // 合并已经不在进行
      events: [{ type: 'git', action: 'sync_conflicts', branch: 'b', base: 'main', files: ['a.rs'] }],
    })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('git-event')
    await waitFor(() => expect(statusCalls().length).toBeGreaterThan(0))
    await waitFor(() => expect(screen.queryByTestId('git-event-conflict-actions')).toBeNull())
  })

  it('a newer sync event takes the buttons away from an older conflict line', async () => {
    seed({
      summary: { worktree: worktree() },
      status: gitStatus({ merging: true, conflicts: 1 }),
      events: [{ type: 'git', action: 'sync_conflicts', branch: 'b', base: 'main', files: ['a.rs'] }],
    })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('git-event-conflict-actions')
    push({ type: 'git', action: 'sync_aborted', branch: 'b', base: 'main' })
    await waitFor(() => expect(screen.queryByTestId('git-event-conflict-actions')).toBeNull())
    expect(screen.getAllByTestId('git-event')).toHaveLength(2)
  })

  it('a git event refreshes the status', async () => {
    seed({ summary: { worktree: worktree() }, events: [created] })
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(statusCalls().length).toBeGreaterThanOrEqual(1))
    const before = statusCalls().length
    push({ type: 'git', action: 'sync_merged', branch: 'b', base: 'main', count: 1 })
    await waitFor(() => expect(statusCalls().length).toBeGreaterThan(before))
  })
})

describe('SessionPage: archive and delete', () => {
  const archiveCall = () => rpc.calls.find((c) => c.method === 'session.archive')?.params
  const deleteCall = () => rpc.calls.find((c) => c.method === 'session.delete')?.params

  it('a session without a worktree archives straight away, with just the session id, exactly as before', async () => {
    seed({ projects: [project({ name: 'app' })] })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-archive'))
    await waitFor(() => expect(archiveCall()).toEqual({ session_id: SID }))
    expect(screen.queryByTestId('archive-worktree-dialog')).toBeNull()
  })

  it('a session in a git project but in the project folder (no worktree) also archives straight away', async () => {
    seed()
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-archive'))
    await waitFor(() => expect(archiveCall()).toEqual({ session_id: SID }))
  })

  it('with a worktree it asks first: automatic is preselected, and the dialog explains what happens', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ uncommitted: 3 }) })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-archive'))
    const dialog = await screen.findByTestId('archive-worktree-dialog')
    expect(dialog).toHaveTextContent('anycon/fix-login')
    expect(within(dialog).getByTestId('archive-option-auto')).toHaveAttribute('data-state', 'checked')
    expect(archiveCall()).toBeUndefined() // 还没确认
    await user.click(within(dialog).getByTestId('archive-confirm'))
    await waitFor(() => expect(archiveCall()).toEqual({ session_id: SID, worktree: 'auto' }))
    await waitFor(() => expect(screen.queryByTestId('archive-worktree-dialog')).toBeNull())
  })

  it('“Keep the worktree” sends worktree: keep', async () => {
    seed({ summary: { worktree: worktree() } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-archive'))
    const dialog = await screen.findByTestId('archive-worktree-dialog')
    await user.click(within(dialog).getByTestId('archive-option-keep'))
    await user.click(within(dialog).getByTestId('archive-confirm'))
    await waitFor(() => expect(archiveCall()).toEqual({ session_id: SID, worktree: 'keep' }))
  })

  it('“Delete the worktree” names how many uncommitted changes are discarded (N from git status) and sends worktree: remove', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ uncommitted: 4 }) })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    // 等状态到了再开对话框，N 才是对的
    await waitFor(() => expect(useGit.getState().bySession[SID]?.status?.uncommitted).toBe(4))
    await user.click(await screen.findByTestId('session-archive'))
    const dialog = await screen.findByTestId('archive-worktree-dialog')
    const remove = within(dialog).getByTestId('archive-option-remove')
    expect(dialog).toHaveTextContent('Delete the worktree (discard 4 uncommitted changes)')
    await user.click(remove)
    await user.click(within(dialog).getByTestId('archive-confirm'))
    await waitFor(() => expect(archiveCall()).toEqual({ session_id: SID, worktree: 'remove' }))
  })

  it('cancelling the dialog sends nothing', async () => {
    seed({ summary: { worktree: worktree() } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-archive'))
    const dialog = await screen.findByTestId('archive-worktree-dialog')
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }))
    await waitFor(() => expect(screen.queryByTestId('archive-worktree-dialog')).toBeNull())
    expect(archiveCall()).toBeUndefined()
  })

  it('refreshes the git status when the dialog opens, so the uncommitted count is current', async () => {
    seed({ summary: { worktree: worktree() } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(statusCalls().length).toBeGreaterThan(0))
    const before = statusCalls().length
    await user.click(await screen.findByTestId('session-archive'))
    await waitFor(() => expect(statusCalls().length).toBeGreaterThan(before))
  })

  it('un-archiving never asks (the daemon rebuilds the worktree from its branch)', async () => {
    seed({ summary: { worktree: worktree({ state: 'kept' }), archived: true } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-archive'))
    await waitFor(() => expect(rpc.calls.find((c) => c.method === 'session.unarchive')?.params).toEqual({ session_id: SID }))
    expect(screen.queryByTestId('archive-worktree-dialog')).toBeNull()
  })

  it('a session whose worktree is already removed archives without asking', async () => {
    seed({ summary: { worktree: worktree({ state: 'removed' }) } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-archive'))
    await waitFor(() => expect(archiveCall()).toEqual({ session_id: SID }))
  })

  it('delete without a worktree: the old confirmation and just the session id', async () => {
    seed({ projects: [project({ name: 'app' })] })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-delete'))
    const dialog = await screen.findByTestId('delete-dialog')
    expect(within(dialog).queryByTestId('delete-picker')).toBeNull()
    await user.click(screen.getByTestId('session-delete-confirm'))
    await waitFor(() => expect(deleteCall()).toEqual({ session_id: SID }))
  })

  it('delete with a worktree: the same three choices plus “also delete the branch”', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ ahead: 0 }) })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-delete'))
    const dialog = await screen.findByTestId('delete-dialog')
    expect(within(dialog).getByTestId('delete-option-auto')).toHaveAttribute('data-state', 'checked')
    expect(within(dialog).getByTestId('delete-delete-branch')).toHaveAttribute('data-state', 'unchecked')
    // 默认：不删分支
    await user.click(screen.getByTestId('session-delete-confirm'))
    await waitFor(() => expect(deleteCall()).toEqual({ session_id: SID, worktree: 'auto' }))
  })

  it('delete + remove the worktree + delete the branch → worktree: remove, delete_branch: true', async () => {
    seed({ summary: { worktree: worktree() } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-delete'))
    const dialog = await screen.findByTestId('delete-dialog')
    await user.click(within(dialog).getByTestId('delete-option-remove'))
    await user.click(within(dialog).getByTestId('delete-delete-branch'))
    await user.click(screen.getByTestId('session-delete-confirm'))
    await waitFor(() => expect(deleteCall()).toEqual({ session_id: SID, worktree: 'remove', delete_branch: true }))
    await waitFor(() => expect(navigate).toHaveBeenCalled())
  })

  it('warns “N commits would be lost” next to the branch checkbox when the branch has commits of its own or unpushed ones', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ ahead: 3, unpushed: 1 }) })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(useGit.getState().bySession[SID]?.status?.ahead).toBe(3))
    await user.click(await screen.findByTestId('session-delete'))
    const dialog = await screen.findByTestId('delete-dialog')
    await user.click(within(dialog).getByTestId('delete-option-remove'))
    expect(within(dialog).getByTestId('delete-commits-lost')).toHaveTextContent('3 commits on this branch would be lost')
    await user.click(within(dialog).getByTestId('delete-delete-branch'))
    expect(within(dialog).getByTestId('delete-commits-lost')).toHaveAttribute('role', 'alert')
  })

  it('no warning when the branch has nothing of its own', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ ahead: 0, unpushed: 0 }) })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-delete'))
    const dialog = await screen.findByTestId('delete-dialog')
    expect(within(dialog).queryByTestId('delete-commits-lost')).toBeNull()
  })

  it('“keep the worktree” disables and unticks the branch checkbox (the branch stays with it), and never sends delete_branch', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ ahead: 2 }) })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('session-delete'))
    const dialog = await screen.findByTestId('delete-dialog')
    await user.click(within(dialog).getByTestId('delete-option-remove'))
    await user.click(within(dialog).getByTestId('delete-delete-branch'))
    await user.click(within(dialog).getByTestId('delete-option-keep'))
    expect(within(dialog).getByTestId('delete-delete-branch')).toBeDisabled()
    expect(within(dialog).queryByTestId('delete-commits-lost')).toBeNull()
    await user.click(screen.getByTestId('session-delete-confirm'))
    await waitFor(() => expect(deleteCall()).toEqual({ session_id: SID, worktree: 'keep' }))
  })

  it('automatic with uncommitted changes means the worktree will be kept, so the branch cannot be deleted either', async () => {
    seed({ summary: { worktree: worktree() }, status: gitStatus({ uncommitted: 2 }) })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await waitFor(() => expect(useGit.getState().bySession[SID]?.status?.uncommitted).toBe(2))
    await user.click(await screen.findByTestId('session-delete'))
    const dialog = await screen.findByTestId('delete-dialog')
    expect(within(dialog).getByTestId('delete-delete-branch')).toBeDisabled()
  })
})
