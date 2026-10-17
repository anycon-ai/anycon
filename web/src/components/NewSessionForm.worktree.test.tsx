// 新建会话的「工作位置」（ADR-34，web.md § 4.5）：选项行里的按钮 + 点开的浮层（分段控件、base 下拉、分支名）、初始选择、禁用说明、传给 session.create 的 workspace。
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo, Project, SessionCreateParams, WorkspaceBranchesResult } from '@/protocol'
import { gitProject, mockAgent, nodeEntry, nodeInfo, project, resetSeq, summary, worktree } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDb } from '@/db'
import { resetDrafts, useDrafts } from '@/stores/drafts'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { NewSessionForm } from './NewSessionForm'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

const BRANCHES: WorkspaceBranchesResult = { current: 'main', default_base: 'origin/main', local: ['main', 'dev'], remote: ['origin/main', 'origin/release'] }

function mountForm(opts: { projects?: Project[]; info?: NodeInfo | undefined; project?: string | null; conn?: 'ready' | 'peer_offline' } = {}) {
  const { projects = [gitProject('app')], info = nodeInfo(), project: name = 'app', conn = 'ready' } = opts
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects, info, conn }) }, order: [NODE], loaded: true })
  return render(<NewSessionForm nodeId={NODE} project={name} />)
}
const createParams = () => rpc.calls.find((c) => c.method === 'session.create')?.params as SessionCreateParams | undefined
/** 点开选项行里的「工作位置」按钮，等浮层（里面是 WorkspacePicker）出来。点浮层外面（比如输入框）会把它关掉，但选择已经记在表单里。 */
async function openWorkspace(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByTestId('new-session-workspace-button'))
  return screen.findByTestId('new-session-workspace-row')
}
async function send(user: ReturnType<typeof userEvent.setup>, text = 'fix the login bug') {
  await user.type(screen.getByTestId('composer-input'), text)
  await user.click(screen.getByTestId('composer-send'))
}

beforeEach(async () => {
  resetSeq()
  navigate.mockReset()
  rpc = new FakeRpc()
  rpc.handlers['workspace.branches'] = () => BRANCHES
  rpc.handlers['session.create'] = () => summary({ id: 's1', project: 'app', worktree: worktree({ state: 'creating' }) })
  setRpc(rpc)
  // 草稿会落盘：分支名那组用例在输入框里打过字，不清库的话下一个用例读到的是上一个的草稿
  await resetDb()
  resetDrafts()
  usePrefs.setState({ lastUsed: {}, lastWorkspace: {}, customModels: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.setState({ toasts: [] })
})
afterEach(() => setRpc(null))

describe('new-session form: the work-location button', () => {
  it('sits in the context tray above the input box, not in the options row below it', () => {
    mountForm()
    const button = screen.getByTestId('new-session-workspace-button')
    expect(screen.getByTestId('composer-context')).toContainElement(button)
    expect(screen.getByTestId('option-controls')).not.toContainElement(button)
  })

  it('is not offered for a temporary session, a non-git project, or a node without worktree support', () => {
    const { unmount } = mountForm({ project: null })
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    unmount()
    const b = mountForm({ projects: [project({ name: 'app' })] }) // 不是 git 项目
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    expect(screen.queryByTestId('new-session-workspace-disabled')).toBeNull()
    b.unmount()
    mountForm({ info: nodeInfo({ capabilities: { audit: true, blobs: false, fs: true, search: false, workspace: true, worktree: false }, worktree: null }) })
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    expect(rpc.methods()).not.toContain('workspace.branches')
  })

  it('starts on “New worktree” (the node’s default), lists local and remote branches with the default base selected, and offers an optional branch name', async () => {
    const user = userEvent.setup()
    mountForm()
    // 浮层没点开之前，选项行里只有一个按钮，写着当前的工作位置
    expect(screen.queryByTestId('new-session-workspace-row')).toBeNull()
    expect(screen.getByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'worktree')
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-workspace-worktree')).toHaveAttribute('data-state', 'on')
    expect(screen.getByTestId('new-session-workspace-local')).toHaveAttribute('data-state', 'off')
    const base = screen.getByTestId('new-session-base')
    await waitFor(() => expect(base).toBeEnabled())
    expect(base).toHaveValue('origin/main')
    const groups = Array.from(base.querySelectorAll('optgroup')).map((g) => [g.label, Array.from(g.querySelectorAll('option')).map((o) => o.value)])
    expect(groups).toEqual([
      ['Local branches', ['main', 'dev']],
      ['Remote branches', ['origin/main', 'origin/release']],
    ])
    expect(rpc.calls.find((c) => c.method === 'workspace.branches')!.params).toEqual({ project: 'app' })
    expect(screen.getByTestId('new-session-branch')).toHaveAttribute('placeholder', 'anycon/auto')
    expect(screen.getByTestId('new-session-branch')).toHaveValue('')
  })

  it('puts a default base that is in neither list (e.g. a detached HEAD) in the dropdown too', async () => {
    rpc.handlers['workspace.branches'] = () => ({ default_base: 'abc1234', local: ['dev'], remote: [] })
    const user = userEvent.setup()
    mountForm()
    await openWorkspace(user)
    const base = screen.getByTestId('new-session-base')
    await waitFor(() => expect(base).toBeEnabled())
    expect(base).toHaveValue('abc1234')
  })

  it('creates the session with workspace { mode: "worktree", base } and no branch when the name is left empty', async () => {
    const user = userEvent.setup()
    mountForm()
    // 发送时要带上 base：先点开浮层等分支列表读完（点输入框时浮层会自己关掉）
    await openWorkspace(user)
    await waitFor(() => expect(screen.getByTestId('new-session-base')).toBeEnabled())
    await send(user)
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()).toMatchObject({ project: 'app', agent: 'mock', prompt: 'fix the login bug', workspace: { mode: 'worktree', base: 'origin/main' } })
    expect(createParams()!.workspace).not.toHaveProperty('branch')
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/s/$session', params: { node: 'nnnnnnnn', session: 's1' } }))
    // 创建时返回的摘要带 worktree（creating）：会话页头部据此显示「正在创建 worktree…」
    expect(useSessions.getState().list['s1']!.worktree).toMatchObject({ state: 'creating', branch: 'anycon/fix-login' })
  })

  it('sends the base and the branch name the user chose', async () => {
    const user = userEvent.setup()
    mountForm()
    await openWorkspace(user)
    const base = screen.getByTestId('new-session-base')
    await waitFor(() => expect(base).toBeEnabled())
    await user.selectOptions(base, 'dev')
    await user.type(screen.getByTestId('new-session-branch'), '  anycon/my-fix ')
    // 按钮上显示选的分支名
    expect(screen.getByTestId('new-session-workspace-button')).toHaveTextContent('anycon/my-fix')
    await send(user)
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()!.workspace).toEqual({ mode: 'worktree', base: 'dev', branch: 'anycon/my-fix' })
  })

  it('“Current folder” hides the base and branch fields and sends workspace { mode: "local" }', async () => {
    const user = userEvent.setup()
    mountForm()
    await openWorkspace(user)
    await user.click(screen.getByTestId('new-session-workspace-local'))
    expect(screen.getByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'local')
    expect(screen.queryByTestId('new-session-base')).toBeNull()
    expect(screen.queryByTestId('new-session-branch')).toBeNull()
    await send(user)
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()!.workspace).toEqual({ mode: 'local' })
  })

  it('follows the node’s worktree.default = local, and the browser’s last choice for this project wins over it', async () => {
    const user = userEvent.setup()
    const a = mountForm({ info: nodeInfo({ worktree: { default_mode: 'local', branch_prefix: 'anycon' } }) })
    expect(screen.getByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'local')
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-workspace-local')).toHaveAttribute('data-state', 'on')
    expect(screen.queryByTestId('new-session-base')).toBeNull()
    a.unmount()
    // 上次在这个项目上选的是 worktree
    usePrefs.setState({ lastWorkspace: { [`${NODE}:app`]: 'worktree' } })
    mountForm({ info: nodeInfo({ worktree: { default_mode: 'local', branch_prefix: 'anycon' } }) })
    expect(screen.getByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'worktree')
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-workspace-worktree')).toHaveAttribute('data-state', 'on')
    expect(rpc.methods()).toContain('workspace.branches')
    await user.click(screen.getByTestId('new-session-workspace-local'))
    expect(screen.getByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'local')
  })

  it('remembers the choice per (node, project) after a successful create, and only then', async () => {
    const user = userEvent.setup()
    mountForm()
    await openWorkspace(user)
    await user.click(screen.getByTestId('new-session-workspace-local'))
    expect(usePrefs.getState().lastWorkspace).toEqual({}) // 还没发送
    await send(user)
    await waitFor(() => expect(usePrefs.getState().lastWorkspace).toEqual({ [`${NODE}:app`]: 'local' }))
  })

  it('a failed create leaves the remembered choice alone and puts the text back', async () => {
    rpc.handlers['session.create'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    mountForm()
    await openWorkspace(user)
    await user.click(screen.getByTestId('new-session-workspace-local'))
    await send(user, 'try this')
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true))
    expect(usePrefs.getState().lastWorkspace).toEqual({})
    expect(navigate).not.toHaveBeenCalled()
  })

  it('uses the node’s branch prefix in the placeholder, or just “auto” when the prefix is empty', async () => {
    const user = userEvent.setup()
    const a = mountForm({ info: nodeInfo({ worktree: { default_mode: 'worktree', branch_prefix: 'bots' } }) })
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-branch')).toHaveAttribute('placeholder', 'bots/auto')
    a.unmount()
    mountForm({ info: nodeInfo({ worktree: { default_mode: 'worktree', branch_prefix: '' } }) })
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-branch')).toHaveAttribute('placeholder', 'auto')
  })
})

describe('new-session form: when the project cannot have worktrees', () => {
  it('disables the switch, explains why, never asks for branches, and sends no workspace', async () => {
    const user = userEvent.setup()
    mountForm({ projects: [gitProject('app', { worktree_ready: false })] })
    expect(screen.getByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'local')
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-workspace-worktree')).toBeDisabled()
    expect(screen.getByTestId('new-session-workspace-local')).toBeDisabled()
    expect(screen.getByTestId('new-session-workspace-local')).toHaveAttribute('data-state', 'on')
    expect(screen.getByTestId('new-session-workspace-disabled')).toHaveTextContent('Worktrees need the project folder to be the root of a git repository with at least one commit.')
    expect(screen.queryByTestId('new-session-base')).toBeNull()
    await send(user)
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()).not.toHaveProperty('workspace')
    expect(rpc.methods()).not.toContain('workspace.branches')
    expect(usePrefs.getState().lastWorkspace).toEqual({}) // 没得选，不记
  })

  it('treats a missing worktree_ready (an older daemon) as not ready', async () => {
    const user = userEvent.setup()
    mountForm({ projects: [project({ name: 'app', git: { branch: 'main', dirty: false } })] })
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-workspace-disabled')).toBeInTheDocument()
  })
})

describe('new-session form: the branch name', () => {
  it.each(['has space', 'a..b', 'x~1', '-bad', 'ends.lock'])('rejects %j before sending: red hint, send disabled', async (name) => {
    const user = userEvent.setup()
    mountForm()
    await user.type(screen.getByTestId('composer-input'), 'go')
    await openWorkspace(user)
    await user.type(screen.getByTestId('new-session-branch'), name)
    // 浮层里的红字，选项行里的按钮标红，输入框上方说明为什么发不了
    expect(screen.getByTestId('new-session-branch')).toHaveAttribute('aria-invalid', 'true')
    expect(screen.getByTestId('new-session-branch-error')).toHaveTextContent('That is not a valid git branch name.')
    expect(screen.getByTestId('new-session-workspace-button')).toHaveClass('text-danger')
    expect(screen.getByTestId('composer-send')).toBeDisabled()
    expect(screen.getByTestId('composer-blocked')).toHaveTextContent('That is not a valid git branch name.')
    // 改对之后又能发了
    await user.clear(screen.getByTestId('new-session-branch'))
    await user.type(screen.getByTestId('new-session-branch'), 'ok-name')
    expect(screen.queryByTestId('new-session-branch-error')).toBeNull()
    expect(screen.queryByTestId('composer-blocked')).toBeNull()
    expect(screen.getByTestId('composer-send')).toBeEnabled()
  })

  it('rejects main / master / HEAD as the name of a new branch', async () => {
    const user = userEvent.setup()
    mountForm()
    await openWorkspace(user)
    await user.type(screen.getByTestId('new-session-branch'), 'master')
    expect(screen.getByTestId('new-session-branch-error')).toHaveTextContent('main, master and HEAD can’t be used as the new branch name.')
  })

  it('does not validate a branch name in “Current folder” mode (the field is not even there)', async () => {
    const user = userEvent.setup()
    mountForm()
    await openWorkspace(user)
    await user.type(screen.getByTestId('new-session-branch'), 'bad name')
    await user.click(screen.getByTestId('new-session-workspace-local'))
    expect(screen.queryByTestId('new-session-branch-error')).toBeNull()
    await user.type(screen.getByTestId('composer-input'), 'go')
    expect(screen.queryByTestId('composer-blocked')).toBeNull()
    expect(screen.getByTestId('composer-send')).toBeEnabled()
  })
})

describe('new-session form: failures', () => {
  it('a base list that cannot be loaded does not block creating: the dropdown falls back to “Default” and no base is sent', async () => {
    rpc.handlers['workspace.branches'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    mountForm()
    await waitFor(() => expect(rpc.methods()).toContain('workspace.branches'))
    await openWorkspace(user)
    const base = screen.getByTestId('new-session-base')
    await waitFor(() => expect(within(base).getByRole('option')).toHaveTextContent('Default'))
    expect(base).toBeDisabled()
    await send(user)
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()!.workspace).toEqual({ mode: 'worktree' })
  })

  it.each([
    [rpcError('exists'), 'That branch already exists. Pick another name, or leave it empty.'],
    [rpcError('invalid', undefined, 'not_worktree_ready'), 'This project can’t have worktrees: its folder is not the repository root, or it has no commits yet.'],
    [rpcError('invalid', undefined, 'bad_base'), 'That base does not exist in the repository.'],
  ])('explains %# in its own words and keeps the text', async (err, text) => {
    rpc.handlers['session.create'] = () => {
      throw err
    }
    const user = userEvent.setup()
    mountForm()
    await send(user, 'keep me')
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.text)).toContain(text))
    await waitFor(() => expect(useDrafts.getState().byKey[`new:${NODE}:app`]?.text).toBe('keep me'))
    expect(navigate).not.toHaveBeenCalled()
  })

  it('keeps the generic wording for an `exists` that is not about a branch (project folder mode)', async () => {
    rpc.handlers['session.create'] = () => {
      throw rpcError('exists')
    }
    const user = userEvent.setup()
    mountForm()
    await openWorkspace(user)
    await user.click(screen.getByTestId('new-session-workspace-local'))
    await send(user)
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.text)).toContain('That name already exists.'))
  })

  it('does not ask for branches while the node is offline', () => {
    mountForm({ conn: 'peer_offline' })
    expect(screen.getByTestId('new-session-workspace-button')).toBeInTheDocument()
    expect(rpc.methods()).not.toContain('workspace.branches')
  })
})
