import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Project } from '@/protocol'
import { mockAgent, nodeEntry, project } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { ProjectPage } from './ProjectPage'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

function mount(projects: Project[], name: string, conn: 'ready' | 'peer_offline' = 'ready') {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn, projects, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  return render(<ProjectPage project={name} />)
}

beforeEach(() => {
  navigate.mockReset()
  rpc = new FakeRpc()
  rpc.handlers['workspace.list'] = () => ({ root: '/workspace', projects: [] })
  setRpc(rpc)
  useSessions.setState({ list: {} })
})
afterEach(() => setRpc(null))

const openMenu = async (user: ReturnType<typeof userEvent.setup>): Promise<void> => {
  await user.click(screen.getByTestId('project-menu'))
}

describe('ProjectPage: linked project', () => {
  const linked = project({ name: 'ext', path: '/home/me/code/ext', kind: 'linked' })

  it('is marked as linked and shows the folder path', () => {
    mount([linked], 'ext')
    expect(screen.getByTestId('project-linked')).toHaveTextContent('Linked')
    expect(screen.getByTestId('project-path')).toHaveTextContent('/home/me/code/ext')
    expect(screen.queryByTestId('project-missing')).toBeNull()
    expect(screen.getByTestId('project-new-session')).toBeInTheDocument()
  })

  it('has no composer on the page: the header button opens the new-session page of this project', async () => {
    const user = userEvent.setup()
    mount([linked], 'ext')
    // 新建会话在单独的空白页，项目页只有会话历史和管理
    expect(screen.queryByTestId('new-session-form')).toBeNull()
    expect(screen.queryByTestId('composer')).toBeNull()
    expect(screen.queryByTestId('composer-input')).toBeNull()
    await user.click(screen.getByTestId('project-new-session'))
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/new', params: { node: 'nnnnnnnn' }, search: { project: 'ext' } })
  })

  it('offers “Remove from list” instead of “Move to trash”, and keeps rename', async () => {
    const user = userEvent.setup()
    mount([linked], 'ext')
    await openMenu(user)
    expect(await screen.findByTestId('project-remove')).toHaveTextContent('Remove from list')
    expect(screen.getByRole('menuitem', { name: 'Rename project' })).toBeInTheDocument()
    expect(screen.queryByRole('menuitem', { name: 'Move to trash' })).toBeNull()
  })

  it('confirms with a text saying no files are deleted, then unlinks and returns to the node page', async () => {
    rpc.handlers['workspace.unlink'] = () => ({})
    const user = userEvent.setup()
    mount([linked], 'ext')
    await openMenu(user)
    await user.click(await screen.findByTestId('project-remove'))
    const dialog = await screen.findByTestId('project-remove-dialog')
    expect(dialog).toHaveTextContent('not deleted')
    expect(dialog).toHaveTextContent('/home/me/code/ext')
    // 会话怎么样——同一个目录重新关联回来能继续，这个名字指向别的目录就不能
    expect(dialog).toHaveTextContent('Existing sessions stay in your history')
    expect(dialog).toHaveTextContent('If you link this same folder again they can continue')
    expect(dialog).toHaveTextContent('if this name later points to a different folder, the old sessions cannot continue')
    expect(rpc.methods()).not.toContain('workspace.unlink') // 确认之前什么也不做
    await user.click(within(dialog).getByTestId('project-remove-confirm'))
    await waitFor(() => expect(rpc.calls.find((c) => c.method === 'workspace.unlink')?.params).toEqual({ name: 'ext' }))
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: '/n/$node', params: { node: 'nnnnnnnn' } }))
    expect(rpc.methods()).not.toContain('workspace.trash')
  })

  it('cancelling the confirmation does nothing', async () => {
    const user = userEvent.setup()
    mount([linked], 'ext')
    await openMenu(user)
    await user.click(await screen.findByTestId('project-remove'))
    await user.click(within(await screen.findByTestId('project-remove-dialog')).getByRole('button', { name: 'Cancel' }))
    expect(rpc.methods()).toEqual([])
    expect(navigate).not.toHaveBeenCalled()
  })

  it('explains that running sessions block the removal (busy) and stays on the page', async () => {
    rpc.handlers['workspace.unlink'] = () => {
      throw rpcError('busy')
    }
    const user = userEvent.setup()
    mount([linked], 'ext')
    await openMenu(user)
    await user.click(await screen.findByTestId('project-remove'))
    await user.click(within(await screen.findByTestId('project-remove-dialog')).getByTestId('project-remove-confirm'))
    expect(await screen.findByTestId('project-remove-error')).toHaveTextContent('has running sessions')
    expect(navigate).not.toHaveBeenCalled()
    // 对话框还在，处理完之后可以直接重试
    rpc.handlers['workspace.unlink'] = () => ({})
    await user.click(screen.getByTestId('project-remove-confirm'))
    await waitFor(() => expect(navigate).toHaveBeenCalled())
  })
})

describe('ProjectPage: missing linked folder', () => {
  const missing = project({ name: 'gone', path: '/mnt/disk/gone', kind: 'linked', missing: true, modified_at_ms: 0 })

  it('says the folder is not found, disables new sessions and offers only “Remove from list”', async () => {
    const user = userEvent.setup()
    mount([missing], 'gone')
    expect(screen.getByTestId('project-missing')).toHaveTextContent('Folder not found')
    const notice = screen.getByTestId('project-missing-notice')
    expect(notice).toHaveTextContent('/mnt/disk/gone')
    expect(notice).toHaveTextContent('new sessions are turned off')
    expect(screen.queryByTestId('project-new-session')).toBeNull()
    expect(screen.queryByTestId('new-session-form')).toBeNull()
    expect(screen.queryByTestId('composer-input')).toBeNull()

    await openMenu(user)
    expect(await screen.findByTestId('project-remove')).toBeInTheDocument()
    expect(screen.queryByRole('menuitem', { name: 'Rename project' })).toBeNull()
    expect(screen.queryByRole('menuitem', { name: 'Move to trash' })).toBeNull()
  })

  it('the notice has its own remove button that opens the same confirmation', async () => {
    const user = userEvent.setup()
    mount([missing], 'gone')
    await user.click(screen.getByTestId('project-missing-remove'))
    expect(await screen.findByTestId('project-remove-dialog')).toHaveTextContent('Remove “gone” from the list?')
  })
})

describe('ProjectPage: project under the workspace root', () => {
  it('is not marked as linked and keeps “Move to trash”', async () => {
    const user = userEvent.setup()
    mount([project({ name: 'plain' })], 'plain')
    expect(screen.queryByTestId('project-linked')).toBeNull()
    expect(screen.getByTestId('project-new-session')).toBeInTheDocument()
    await openMenu(user)
    expect(await screen.findByRole('menuitem', { name: 'Move to trash' })).toBeInTheDocument()
    expect(screen.queryByTestId('project-remove')).toBeNull()
  })
})
