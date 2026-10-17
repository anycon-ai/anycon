// 节点设置 → Workspace → Worktree 一节（web.md § 4.8，ADR-34）：列表、会话链接、删除确认框（force / delete_branch）。
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { WorkspaceRemoveWorktreeParams } from '@/protocol'
import { nodeEntry, nodeInfo, resetSeq, summary, worktreeInfo } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { WorkspaceTab } from './WorkspaceTab'
import { WorktreeSection } from './WorktreeSection'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

function node(over: Parameters<typeof nodeEntry>[0] extends infer T ? Partial<T> : never = {}) {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, info: nodeInfo(), ...over }) }, order: [NODE], loaded: true })
}
const removeCalls = () => rpc.calls.filter((c) => c.method === 'workspace.remove_worktree').map((c) => c.params as WorkspaceRemoveWorktreeParams)

beforeEach(() => {
  resetSeq()
  rpc = new FakeRpc()
  setRpc(rpc)
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.setState({ toasts: [] })
  node()
})
afterEach(() => setRpc(null))

describe('node settings: worktrees', () => {
  it('lists each worktree: project, branch ← base, state, session link (or “No session”), uncommitted, own commits, size', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'sess_a', title: 'Fix the login', archived: true })])
    rpc.handlers['workspace.worktrees'] = () => ({
      worktrees: [
        worktreeInfo({ id: 'wt_1', state: 'kept', uncommitted: 2, ahead: 3, bytes: 5 * 1024 * 1024, session_id: 'sess_a', session_title: 'Fix the login' }),
        worktreeInfo({ id: 'wt_2', branch: 'anycon/old', state: 'missing', session_id: null }),
      ],
    })
    render(<WorktreeSection nodeId={NODE} />)
    const rows = await screen.findAllByTestId('worktree-row')
    expect(rows).toHaveLength(2)
    const first = within(rows[0]!)
    expect(first.getByTestId('worktree-project')).toHaveTextContent('proj')
    expect(first.getByTestId('worktree-branch')).toHaveTextContent('anycon/fix-login')
    expect(first.getByTestId('worktree-branch')).toHaveTextContent('← main')
    expect(first.getByTestId('worktree-state')).toHaveTextContent('Kept')
    expect(rows[0]).toHaveAttribute('data-state', 'kept')
    expect(first.getByTestId('worktree-session')).toHaveTextContent('Fix the login')
    expect(first.getByTestId('worktree-session')).toHaveAttribute('href', '/n/nnnnnnnn/s/sess_a')
    expect(first.getByTestId('worktree-uncommitted')).toHaveTextContent('2 uncommitted')
    expect(first.getByTestId('worktree-ahead')).toHaveTextContent('3 own commits')
    expect(rows[0]).toHaveTextContent('5.0 MiB')
    const second = within(rows[1]!)
    expect(second.getByTestId('worktree-no-session')).toHaveTextContent('No session')
    expect(second.getByTestId('worktree-state')).toHaveTextContent('Missing')
    expect(rpc.calls.find((c) => c.method === 'workspace.worktrees')!.params).toEqual({})
  })

  it('shows a worktree whose session was deleted, and falls back to the title the daemon sent when the session is not in the local list', async () => {
    rpc.handlers['workspace.worktrees'] = () => ({ worktrees: [worktreeInfo({ session_id: 'sess_gone', session_title: 'Old title' })] })
    render(<WorktreeSection nodeId={NODE} />)
    expect(await screen.findByTestId('worktree-session')).toHaveTextContent('Old title')
  })

  it('says so when there are none, and shows the error when the list cannot be loaded', async () => {
    rpc.handlers['workspace.worktrees'] = () => ({ worktrees: [] })
    const a = render(<WorktreeSection nodeId={NODE} />)
    expect(await screen.findByTestId('worktree-empty')).toHaveTextContent('No worktrees.')
    a.unmount()
    rpc.handlers['workspace.worktrees'] = () => {
      throw rpcError('internal')
    }
    render(<WorktreeSection nodeId={NODE} />)
    expect(await screen.findByTestId('worktree-error')).toHaveTextContent('Something went wrong on the node.')
  })

  it('is not shown on a node without worktree support, and does not ask it for the list', () => {
    node({ info: nodeInfo({ capabilities: { audit: true, blobs: false, fs: true, search: false, workspace: true }, worktree: null }) })
    render(<WorktreeSection nodeId={NODE} />)
    expect(screen.queryByTestId('worktree-section')).toBeNull()
    expect(rpc.methods()).not.toContain('workspace.worktrees')
  })

  it('is part of the Workspace tab, below the trash', async () => {
    rpc.handlers['workspace.trash_list'] = () => ({ entries: [] })
    rpc.handlers['workspace.worktrees'] = () => ({ worktrees: [worktreeInfo()] })
    render(<WorkspaceTab nodeId={NODE} />)
    expect(await screen.findByTestId('worktree-section')).toBeInTheDocument()
  })

  it('refresh asks the node again', async () => {
    let n = 0
    rpc.handlers['workspace.worktrees'] = () => ({ worktrees: n++ === 0 ? [] : [worktreeInfo()] })
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await screen.findByTestId('worktree-empty')
    await user.click(screen.getByTestId('worktree-refresh'))
    expect(await screen.findByTestId('worktree-row')).toBeInTheDocument()
  })
})

describe('node settings: deleting a worktree', () => {
  function serve(list = [worktreeInfo({ id: 'wt_1', state: 'kept' })]): void {
    let items = list
    rpc.handlers['workspace.worktrees'] = () => ({ worktrees: items })
    rpc.handlers['workspace.remove_worktree'] = (p) => {
      items = items.filter((w) => w.id !== (p as WorkspaceRemoveWorktreeParams).id)
      return {}
    }
  }

  it('a clean worktree: confirm, and the request carries just the id; the row goes away', async () => {
    serve()
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await user.click(await screen.findByTestId('worktree-remove'))
    const dialog = await screen.findByTestId('worktree-remove-dialog')
    expect(within(dialog).queryByTestId('worktree-remove-force')).toBeNull() // 没有未提交的改动，不用勾
    await user.click(within(dialog).getByTestId('worktree-remove-confirm'))
    await waitFor(() => expect(removeCalls()).toEqual([{ id: 'wt_1' }]))
    await waitFor(() => expect(screen.queryByTestId('worktree-row')).toBeNull())
    expect(screen.getByTestId('worktree-empty')).toBeInTheDocument()
  })

  it('uncommitted changes: the delete button stays off until “discard the uncommitted changes” is ticked, then force: true is sent', async () => {
    serve([worktreeInfo({ id: 'wt_1', state: 'kept', uncommitted: 3 })])
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await user.click(await screen.findByTestId('worktree-remove'))
    const dialog = await screen.findByTestId('worktree-remove-dialog')
    const confirm = within(dialog).getByTestId('worktree-remove-confirm')
    expect(confirm).toBeDisabled()
    const force = within(dialog).getByTestId('worktree-remove-force')
    expect(dialog).toHaveTextContent('Discard 3 uncommitted changes')
    await user.click(force)
    expect(confirm).toBeEnabled()
    await user.click(confirm)
    await waitFor(() => expect(removeCalls()).toEqual([{ id: 'wt_1', force: true }]))
  })

  it('“also delete the branch” sends delete_branch: true, and warns about the commits that would be lost', async () => {
    serve([worktreeInfo({ id: 'wt_1', state: 'kept', ahead: 2 })])
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await user.click(await screen.findByTestId('worktree-remove'))
    const dialog = await screen.findByTestId('worktree-remove-dialog')
    expect(within(dialog).getByTestId('worktree-remove-commits-lost')).toHaveTextContent('2 commits on this branch would be lost')
    expect(within(dialog).getByTestId('worktree-remove-commits-lost')).not.toHaveAttribute('role')
    await user.click(within(dialog).getByTestId('worktree-remove-delete-branch'))
    expect(within(dialog).getByTestId('worktree-remove-commits-lost')).toHaveAttribute('role', 'alert')
    await user.click(within(dialog).getByTestId('worktree-remove-confirm'))
    await waitFor(() => expect(removeCalls()).toEqual([{ id: 'wt_1', delete_branch: true }]))
  })

  it('no warning when the branch has no commits of its own', async () => {
    serve([worktreeInfo({ id: 'wt_1', ahead: 0 })])
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await user.click(await screen.findByTestId('worktree-remove'))
    expect(screen.queryByTestId('worktree-remove-commits-lost')).toBeNull()
  })

  it('a worktree whose session is not archived yet cannot be deleted: the button is off and says to archive first', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'sess_a', archived: false })])
    serve([worktreeInfo({ id: 'wt_1', state: 'active', session_id: 'sess_a' })])
    render(<WorktreeSection nodeId={NODE} />)
    const row = within(await screen.findByTestId('worktree-row'))
    expect(row.getByTestId('worktree-remove')).toBeDisabled()
    expect(row.getByTestId('worktree-archive-first')).toHaveTextContent('Archive the session first.')
  })

  it('an archived session’s worktree can be deleted', async () => {
    useSessions.getState().upsertSummaries(NODE, [summary({ id: 'sess_a', archived: true })])
    serve([worktreeInfo({ id: 'wt_1', state: 'kept', session_id: 'sess_a' })])
    render(<WorktreeSection nodeId={NODE} />)
    const row = within(await screen.findByTestId('worktree-row'))
    expect(row.getByTestId('worktree-remove')).toBeEnabled()
    expect(row.queryByTestId('worktree-archive-first')).toBeNull()
  })

  it('busy from the node (the session was un-archived meanwhile): says so, closes the dialog and reloads the list', async () => {
    serve()
    rpc.handlers['workspace.remove_worktree'] = () => {
      throw rpcError('busy')
    }
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await user.click(await screen.findByTestId('worktree-remove'))
    await user.click(within(await screen.findByTestId('worktree-remove-dialog')).getByTestId('worktree-remove-confirm'))
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.text)).toContain('The agent is busy with a turn. Try again when it finishes.'))
    await waitFor(() => expect(screen.queryByTestId('worktree-remove-dialog')).toBeNull())
    expect(rpc.calls.filter((c) => c.method === 'workspace.worktrees')).toHaveLength(2)
  })

  it('uncommitted_changes from the node (they appeared after the list was loaded): explains, and reloads so the checkbox shows up', async () => {
    serve()
    rpc.handlers['workspace.remove_worktree'] = () => {
      throw rpcError('invalid', undefined, 'uncommitted_changes')
    }
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await user.click(await screen.findByTestId('worktree-remove'))
    await user.click(within(await screen.findByTestId('worktree-remove-dialog')).getByTestId('worktree-remove-confirm'))
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.text)).toContain('There are uncommitted changes. Commit or discard them first.'))
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'workspace.worktrees')).toHaveLength(2))
  })

  it('any other failure keeps the dialog open so the user can retry', async () => {
    serve()
    rpc.handlers['workspace.remove_worktree'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await user.click(await screen.findByTestId('worktree-remove'))
    await user.click(within(await screen.findByTestId('worktree-remove-dialog')).getByTestId('worktree-remove-confirm'))
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true))
    expect(screen.getByTestId('worktree-remove-dialog')).toBeInTheDocument()
  })

  it('cancel sends nothing', async () => {
    serve()
    const user = userEvent.setup()
    render(<WorktreeSection nodeId={NODE} />)
    await user.click(await screen.findByTestId('worktree-remove'))
    await user.click(within(await screen.findByTestId('worktree-remove-dialog')).getByRole('button', { name: 'Cancel' }))
    await waitFor(() => expect(screen.queryByTestId('worktree-remove-dialog')).toBeNull())
    expect(removeCalls()).toEqual([])
  })

  it('offline: nothing to delete with', async () => {
    serve()
    node({ conn: 'peer_offline' })
    render(<WorktreeSection nodeId={NODE} />)
    expect(screen.getByTestId('worktree-refresh')).toBeDisabled()
    expect(rpc.methods()).not.toContain('workspace.worktrees')
  })
})
