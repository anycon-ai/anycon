// 会话信息面板里的工作目录可以点，打开它的文件浏览（docs/spec/web.md § 4.6 信息面板、§ 4.7）：
// worktree 会话进 `<项目>/.anycon/worktrees/<名字>`，项目目录里的会话进项目，scratch 会话进 `.scratch/<会话 id>`。
import { configure, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { SessionSummary } from '@/protocol'
import { gitProject, mockAgent, nodeEntry, nodeInfo, project, resetSeq, summary, worktree } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts } from '@/stores/drafts'
import { useGit } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

// 数据是异步读的（渲染整页、shiki 都不快）；机器忙的时候默认的 1 秒等待和 5 秒用例超时不够
configure({ asyncUtilTimeout: 5000 })
vi.setConfig({ testTimeout: 20_000 })

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

function seed(over: Partial<SessionSummary>, projects = [gitProject('app', { branch: 'main' })]): void {
  rpc.handlers['session.git_status'] = () => ({ branch: 'main', head_sha: 'b'.repeat(40), ahead: 0, behind: 0, uncommitted: 0, conflicts: 0 })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects, info: nodeInfo() }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, [])
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
}

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

const open = async (): Promise<void> => {
  await userEvent.setup().click(await screen.findByTestId('info-workdir-link'))
}

describe('SessionPage: working directory link', () => {
  it('a worktree session opens its worktree folder', async () => {
    seed({ project: 'app', worktree: worktree({ path: '.anycon/worktrees/fix-login' }) })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('info-workdir')).toHaveTextContent('app')
    await open()
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: 'app/.anycon/worktrees/fix-login' } })
  })

  it('a session working in the project folder opens the project', async () => {
    seed({ project: 'app' })
    render(<SessionPage sessionId={SID} />)
    await open()
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: 'app' } })
  })

  it('a worktree that does not exist (removed) falls back to the project folder', async () => {
    seed({ project: 'app', worktree: worktree({ state: 'removed' }) })
    render(<SessionPage sessionId={SID} />)
    await open()
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: 'app' } })
  })

  it('a scratch session opens `.scratch/<session id>`, which the daemon allows (daemon.md § 10)', async () => {
    seed({ project: null })
    render(<SessionPage sessionId={SID} />)
    const link = await screen.findByTestId('info-workdir-link')
    expect(link).toHaveTextContent('Temporary workspace')
    await open()
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: '.scratch/sess_a' } })
  })

  it('has no link when the project folder is gone, or the project is not in the list any more', async () => {
    seed({ project: 'app' }, [project({ name: 'app', missing: true })])
    const { unmount } = render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('info-workdir')).toHaveTextContent('app')
    expect(screen.queryByTestId('info-workdir-link')).toBeNull()
    unmount()
    seed({ project: 'app' }, [])
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('info-workdir')).toBeInTheDocument()
    expect(screen.queryByTestId('info-workdir-link')).toBeNull()
  })
})
