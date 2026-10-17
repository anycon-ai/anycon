// 新建会话表单：项目还没有 `git` 元数据时（daemon 冷启动，web.md § 4.5）。
// 选项行里「工作位置」的位置显示占位「正在读取 git 信息…」并补拉一次 `workspace.list`，读完之前不让发送；
// 补拉回来还是没有 `git` 就按非 git 项目处理，之后 `workspace.changed { updated }` 带来的数据让「工作位置」按钮自己出现。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo, Project, SessionCreateParams, WorkspaceChanged, WorkspaceListResult } from '@/protocol'
import i18n from '@/i18n'
import { handleNotification } from '@/features/sync/notifications'
import { PROBE_THROTTLE_MS, probeProjectGit, resetGitProbes, useGitProbe } from '@/features/workspace/gitProbe'
import { gitProject, mockAgent, nodeEntry, nodeInfo, project, resetSeq, summary, worktree } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useDrafts } from '@/stores/drafts'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { NewSessionForm } from './NewSessionForm'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

const cold = (): Project => project({ name: 'app' }) // 没有 `git`：不知道是不是仓库
const listCalls = () => rpc.calls.filter((c) => c.method === 'workspace.list')
const createParams = () => rpc.calls.find((c) => c.method === 'session.create')?.params as SessionCreateParams | undefined
const serveList = (projects: Project[]) => {
  rpc.handlers['workspace.list'] = (): WorkspaceListResult => ({ root: '/workspace', projects })
}
/** 第一次 `workspace.list` 卡住，直到 `release`；之后的请求按 `rest` 回答。 */
function holdFirstList(first: Project[], rest: Project[] = first) {
  let release: () => void = () => {}
  let n = 0
  rpc.handlers['workspace.list'] = (): Promise<WorkspaceListResult> | WorkspaceListResult => {
    n += 1
    if (n > 1) return { root: '/workspace', projects: rest }
    return new Promise((res) => (release = () => res({ root: '/workspace', projects: first })))
  }
  return () => release()
}

function mountForm(opts: { projects?: Project[]; info?: NodeInfo; conn?: 'ready' | 'peer_offline' } = {}) {
  const { projects = [cold()], info = nodeInfo(), conn = 'ready' } = opts
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects, info, conn }) }, order: [NODE], loaded: true })
  return render(<NewSessionForm nodeId={NODE} project="app" />)
}
/** 点开选项行里的「工作位置」按钮，等浮层（里面是 WorkspacePicker）出来；点浮层外面（比如输入框）会关掉它，选择仍留在表单里。 */
async function openWorkspace(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByTestId('new-session-workspace-button'))
  return screen.findByTestId('new-session-workspace-row')
}
async function type(user: ReturnType<typeof userEvent.setup>, text = 'fix the login bug') {
  await user.type(screen.getByTestId('composer-input'), text)
}

beforeEach(async () => {
  resetSeq()
  navigate.mockReset()
  await i18n.changeLanguage('en')
  rpc = new FakeRpc()
  rpc.handlers['workspace.branches'] = () => ({ current: 'main', default_base: 'main', local: ['main'], remote: [] })
  rpc.handlers['session.create'] = () => summary({ id: 's1', project: 'app', worktree: worktree({ state: 'creating' }) })
  setRpc(rpc)
  resetGitProbes()
  useDrafts.setState({ byKey: {}, loaded: {}, inflight: {} })
  usePrefs.setState({ lastUsed: {}, lastWorkspace: {}, customModels: {} })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.setState({ toasts: [] })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('new-session form: git metadata that is not known yet', () => {
  it('shows “Reading git info…” instead of the work-location button, pulls workspace.list once, and does not let a message go out as “current folder” meanwhile', async () => {
    const user = userEvent.setup()
    holdFirstList([cold()])
    mountForm()
    const pending = await screen.findByTestId('new-session-workspace-pending')
    expect(pending).toHaveTextContent('Reading git info…')
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    expect(screen.queryByTestId('new-session-workspace-row')).toBeNull()
    await waitFor(() => expect(listCalls()).toHaveLength(1))
    // 发送被挡着并说明原因，回车也发不出去
    await type(user)
    expect(screen.getByTestId('composer-send')).toBeDisabled()
    expect(screen.getByTestId('composer-blocked')).toHaveTextContent('Waiting for the project’s git info before creating the session…')
    await user.type(screen.getByTestId('composer-input'), '{Enter}')
    expect(createParams()).toBeUndefined()
    expect(rpc.methods()).not.toContain('session.create')
  })

  it('when the pulled list has the git info: the placeholder turns into the work-location button, the base list is fetched, and the message goes out with an explicit workspace', async () => {
    const user = userEvent.setup()
    serveList([gitProject('app')])
    mountForm()
    expect(await screen.findByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'worktree')
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-workspace-worktree')).toHaveAttribute('data-state', 'on')
    await waitFor(() => expect(screen.getByTestId('new-session-base')).toBeEnabled())
    await type(user)
    expect(screen.getByTestId('composer-send')).toBeEnabled()
    expect(screen.queryByTestId('composer-blocked')).toBeNull()
    await user.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()!.workspace).toEqual({ mode: 'worktree', base: 'main' })
    expect(listCalls()).toHaveLength(1)
  })

  it('a pulled list that still has no git info means “not a git project”: no placeholder, no work-location button, the message goes out without a workspace', async () => {
    const user = userEvent.setup()
    serveList([cold()])
    mountForm()
    await waitFor(() => expect(listCalls()).toHaveLength(1))
    await waitFor(() => expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull())
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    await type(user)
    expect(screen.getByTestId('composer-send')).toBeEnabled()
    await user.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()).not.toHaveProperty('workspace')
  })

  it('a pull that fails does not leave the placeholder (or the block) up for good', async () => {
    const user = userEvent.setup()
    rpc.handlers['workspace.list'] = () => {
      throw rpcError('internal')
    }
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    mountForm()
    await waitFor(() => expect(listCalls()).toHaveLength(1))
    await waitFor(() => expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull())
    await type(user)
    expect(screen.getByTestId('composer-send')).toBeEnabled()
  })

  it('the daemon’s workspace.changed { updated } brings the work-location button in afterwards: after the pull came back without git, the form needs no remount', async () => {
    const user = userEvent.setup()
    serveList([cold()])
    mountForm()
    await waitFor(() => expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull())
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    // daemon 在后台把 git 元数据算完了
    serveList([gitProject('app')])
    const changed: WorkspaceChanged = { updated: ['app'] }
    act(() => handleNotification(NODE, 'workspace.changed', changed))
    expect(await screen.findByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'worktree')
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-workspace-worktree')).toHaveAttribute('data-state', 'on')
    expect(listCalls()).toHaveLength(2) // 补拉一次 + `updated` 触发的一次
    expect(screen.queryByTestId('composer-blocked')).toBeNull()
  })

  it('workspace.changed { updated } while the pull is still out: the work-location button comes in at once, and the late (older) answer does not take it away again', async () => {
    const releaseFirst = holdFirstList([cold()], [gitProject('app')])
    mountForm()
    expect(await screen.findByTestId('new-session-workspace-pending')).toBeInTheDocument()
    act(() => handleNotification(NODE, 'workspace.changed', { updated: ['app'] }))
    expect(await screen.findByTestId('new-session-workspace-button')).toBeInTheDocument()
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    // 先发出的那份晚到了，里面还没有 git：不能盖掉新的
    await act(async () => {
      releaseFirst()
    })
    expect(useNodes.getState().byId[NODE]!.projects[0]!.git).toBeDefined()
    expect(screen.getByTestId('new-session-workspace-button')).toBeInTheDocument()
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
  })

  it('writes the placeholder and the reason in Chinese too', async () => {
    await i18n.changeLanguage('zh')
    holdFirstList([cold()])
    mountForm()
    expect(await screen.findByTestId('new-session-workspace-pending')).toHaveTextContent('正在读取 git 信息…')
    expect(await screen.findByTestId('composer-blocked')).toHaveTextContent('正在读取项目的 git 信息，读完才能创建会话…')
  })
})

describe('new-session form: Project.git_pending (the daemon says the git info is still being computed)', () => {
  const pendingProject = (): Project => project({ name: 'app', git_pending: true })

  it('keeps the placeholder after the pull came back — still git_pending — and keeps the send blocked, with no workspace ever sent', async () => {
    const user = userEvent.setup()
    serveList([pendingProject()])
    mountForm({ projects: [pendingProject()] })
    expect(await screen.findByTestId('new-session-workspace-pending')).toBeInTheDocument()
    await waitFor(() => expect(listCalls()).toHaveLength(1))
    await waitFor(() => expect(useGitProbe.getState().byKey[`${NODE}\u0000app`]?.settled).toBe(true))
    // 补拉结束了，但 daemon 还没算完：不能当作非 git 项目
    expect(screen.getByTestId('new-session-workspace-pending')).toHaveTextContent('Reading git info…')
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    await type(user)
    expect(screen.getByTestId('composer-send')).toBeDisabled()
    expect(screen.getByTestId('composer-blocked')).toHaveTextContent('Waiting for the project’s git info before creating the session…')
    await user.type(screen.getByTestId('composer-input'), '{Enter}')
    expect(rpc.methods()).not.toContain('session.create')
  })

  it('stays up across a re-open of the form past the throttle window too (it is the data that decides, not the pull)', async () => {
    const t0 = 2_000_000
    const now = vi.spyOn(Date, 'now').mockReturnValue(t0)
    serveList([pendingProject()])
    const first = mountForm({ projects: [pendingProject()] })
    await waitFor(() => expect(useGitProbe.getState().byKey[`${NODE}\u0000app`]?.settled).toBe(true))
    first.unmount()
    now.mockReturnValue(t0 + PROBE_THROTTLE_MS * 3)
    mountForm({ projects: [pendingProject()] })
    expect(screen.getByTestId('new-session-workspace-pending')).toBeInTheDocument()
    await act(async () => {})
    expect(screen.getByTestId('new-session-workspace-pending')).toBeInTheDocument()
  })

  it('workspace.changed { updated } → the re-read project has its git info: the placeholder turns into the work-location button and the message goes out with an explicit workspace', async () => {
    const user = userEvent.setup()
    serveList([pendingProject()])
    mountForm({ projects: [pendingProject()] })
    await waitFor(() => expect(useGitProbe.getState().byKey[`${NODE}\u0000app`]?.settled).toBe(true))
    expect(screen.getByTestId('new-session-workspace-pending')).toBeInTheDocument()
    serveList([gitProject('app')])
    act(() => handleNotification(NODE, 'workspace.changed', { updated: ['app'] }))
    expect(await screen.findByTestId('new-session-workspace-button')).toBeInTheDocument()
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    expect(screen.queryByTestId('composer-blocked')).toBeNull()
    await openWorkspace(user)
    await waitFor(() => expect(screen.getByTestId('new-session-base')).toBeEnabled())
    await type(user)
    await user.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()!.workspace).toEqual({ mode: 'worktree', base: 'main' })
  })

  it('an `updated` that reveals a non-git project (no git, no git_pending) ends the placeholder: it is a non-git project after all', async () => {
    const user = userEvent.setup()
    serveList([pendingProject()])
    mountForm({ projects: [pendingProject()] })
    await waitFor(() => expect(useGitProbe.getState().byKey[`${NODE}\u0000app`]?.settled).toBe(true))
    serveList([cold()])
    act(() => handleNotification(NODE, 'workspace.changed', { updated: ['app'] }))
    await waitFor(() => expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull())
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    await type(user)
    expect(screen.getByTestId('composer-send')).toBeEnabled()
    await user.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()).not.toHaveProperty('workspace')
  })

  it('a pull that happens to catch the finished metadata also resolves it, without waiting for `updated`', async () => {
    serveList([gitProject('app')])
    mountForm({ projects: [pendingProject()] })
    expect(await screen.findByTestId('new-session-workspace-button')).toBeInTheDocument()
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
  })

  it('an explicit git_pending: false is the old path: the pull settles it as a non-git project', async () => {
    serveList([project({ name: 'app', git_pending: false })])
    mountForm({ projects: [project({ name: 'app', git_pending: false })] })
    expect(screen.getByTestId('new-session-workspace-pending')).toBeInTheDocument()
    await waitFor(() => expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull())
    expect(screen.getByTestId('composer-input')).toBeInTheDocument()
  })

  it('no placeholder when the node is offline or cannot do worktrees (git_pending or not)', async () => {
    mountForm({ projects: [pendingProject()], conn: 'peer_offline' })
    await act(async () => {})
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    expect(listCalls()).toHaveLength(0)
  })

  it('a node without worktree support ignores git_pending', async () => {
    mountForm({ projects: [pendingProject()], info: nodeInfo({ capabilities: { audit: true, blobs: false, fs: true, search: false, workspace: true, worktree: false }, worktree: null }) })
    await act(async () => {})
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    expect(listCalls()).toHaveLength(0)
  })
})

describe('new-session form: where the placeholder does not belong', () => {
  it('a project that already has git info is never pulled for', async () => {
    mountForm({ projects: [gitProject('app')] })
    expect(screen.getByTestId('new-session-workspace-button')).toBeInTheDocument()
    await act(async () => {})
    expect(listCalls()).toHaveLength(0)
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
  })

  it('a node without worktree support has no work-location button, so there is nothing to wait for or pull', async () => {
    mountForm({ info: nodeInfo({ capabilities: { audit: true, blobs: false, fs: true, search: false, workspace: true, worktree: false }, worktree: null }) })
    await act(async () => {})
    expect(screen.queryByTestId('new-session-workspace-button')).toBeNull()
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    expect(listCalls()).toHaveLength(0)
  })

  it('an offline node: no pull (it would fail), no placeholder — the form already says it can’t create offline', async () => {
    mountForm({ conn: 'peer_offline' })
    await act(async () => {})
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    expect(listCalls()).toHaveLength(0)
    expect(screen.getByTestId('composer-input')).toBeInTheDocument()
  })

  it('a temporary session (no project) has no work-location button either', async () => {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects: [cold()], info: nodeInfo() }) }, order: [NODE], loaded: true })
    render(<NewSessionForm nodeId={NODE} project={null} />)
    await act(async () => {})
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    expect(listCalls()).toHaveLength(0)
  })

  it('a git project that is not worktree-ready still shows the disabled explanation, not the placeholder (the git info is there)', async () => {
    const user = userEvent.setup()
    mountForm({ projects: [gitProject('app', { worktree_ready: false })] })
    expect(screen.getByTestId('new-session-workspace-button')).toHaveAttribute('data-mode', 'local')
    await openWorkspace(user)
    expect(screen.getByTestId('new-session-workspace-disabled')).toBeInTheDocument()
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    expect(listCalls()).toHaveLength(0)
  })
})

describe('the pull is throttled per (node, project)', () => {
  it('opening the form again within 5 s pulls nothing more, and after that a repeat pull is quiet (no placeholder again)', async () => {
    const t0 = 1_000_000
    const now = vi.spyOn(Date, 'now').mockReturnValue(t0)
    serveList([cold()])
    const first = mountForm()
    await waitFor(() => expect(listCalls()).toHaveLength(1))
    await waitFor(() => expect(useGitProbe.getState().byKey[`${NODE}\u0000app`]?.settled).toBe(true))
    first.unmount()

    now.mockReturnValue(t0 + PROBE_THROTTLE_MS - 1)
    const second = mountForm()
    await act(async () => {})
    expect(listCalls()).toHaveLength(1)
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    second.unmount()

    now.mockReturnValue(t0 + PROBE_THROTTLE_MS)
    holdFirstList([cold()])
    // 上面的 holdFirstList 换了处理函数，计数从头开始：这一次是静默补拉（卡住也不出现占位）
    mountForm()
    await waitFor(() => expect(listCalls()).toHaveLength(2))
    expect(screen.queryByTestId('new-session-workspace-pending')).toBeNull()
    expect(screen.getByTestId('composer-send')).toBeDisabled() // 没有文字
  })

  it('probeProjectGit: at most one pull in flight, and one per window; another project has its own window', async () => {
    const release = holdFirstList([cold()])
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, projects: [cold(), project({ name: 'other' })] }) }, order: [NODE], loaded: true })
    probeProjectGit(NODE, 'app', 10)
    probeProjectGit(NODE, 'app', 11) // 在途
    expect(listCalls()).toHaveLength(1)
    act(() => release())
    await waitFor(() => expect(useGitProbe.getState().byKey[`${NODE}\u0000app`]?.inflight).toBe(false))
    probeProjectGit(NODE, 'app', 10 + PROBE_THROTTLE_MS - 1) // 窗口内
    expect(listCalls()).toHaveLength(1)
    probeProjectGit(NODE, 'other', 12) // 另一个项目
    expect(listCalls()).toHaveLength(2)
    probeProjectGit(NODE, 'app', 10 + PROBE_THROTTLE_MS)
    expect(listCalls()).toHaveLength(3)
  })
})
