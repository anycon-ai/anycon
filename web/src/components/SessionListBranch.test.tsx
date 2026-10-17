// 会话列表里的分支（web.md § 4.5）：项目页的列表行显示 `⑂ anycon/x`，左栏会话树里放进悬停提示。
import { render, screen } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockAgent, nodeEntry, project, resetSeq, summary, worktree } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => ({ data: undefined }), logout: vi.fn() }))

import { Sidebar } from './Sidebar'
import { SessionListItem } from './SessionListItem'

const NODE = 'n'.repeat(32)

beforeEach(() => {
  resetSeq()
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects: [project({ name: 'app' })] }) }, order: [NODE], loaded: true })
  useSessions.setState({
    list: {
      a: { ...summary({ id: 'a', title: 'With branch', project: 'app', worktree: worktree({ branch: 'anycon/fix-login' }) }), node_id: NODE, cached_seq: 0 },
      b: { ...summary({ id: 'b', title: 'No branch', project: 'app' }), node_id: NODE, cached_seq: 0 },
    },
    pending: {},
    open: {},
  })
})

describe('session list rows', () => {
  it('SessionListItem shows the branch of a worktree session, and nothing for one without', () => {
    const list = useSessions.getState().list
    const { rerender } = render(<SessionListItem session={list['a']!} />)
    expect(screen.getByTestId('session-list-branch')).toHaveTextContent('anycon/fix-login')
    expect(screen.getByTestId('session-list-branch')).toHaveAttribute('title', 'anycon/fix-login')
    rerender(<SessionListItem session={list['b']!} />)
    expect(screen.queryByTestId('session-list-branch')).toBeNull()
  })

  it('the sidebar puts the branch in the hover tooltip and leaves the row text alone', () => {
    render(
      <QueryClientProvider client={new QueryClient()}>
        <Sidebar nodeId={NODE} />
      </QueryClientProvider>,
    )
    const withBranch = screen.getByText('With branch').closest('a')!
    expect(withBranch).toHaveAttribute('title', 'With branch\n⑂ anycon/fix-login')
    expect(withBranch).toHaveAttribute('data-branch', 'anycon/fix-login')
    expect(withBranch).not.toHaveTextContent('anycon/fix-login')
    const without = screen.getByText('No branch').closest('a')!
    expect(without).not.toHaveAttribute('title')
  })
})
