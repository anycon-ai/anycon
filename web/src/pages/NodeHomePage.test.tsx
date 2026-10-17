import { render, screen, within } from '@testing-library/react'
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

import { NodeHomePage } from './NodeHomePage'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

function mount(projects: Project[], conn: 'ready' | 'peer_offline' = 'ready') {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn, projects, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  return render(<NodeHomePage />)
}

beforeEach(() => {
  navigate.mockReset()
  rpc = new FakeRpc()
  rpc.handlers['workspace.browse'] = () => ({ path: '/home/me', parent: null, roots: ['/home/me'], linkable: false, project: null, entries: [{ name: 'code', path: '/home/me/code', git: true, linkable: true }] })
  setRpc(rpc)
  useSessions.setState({ list: {} })
})
afterEach(() => setRpc(null))

describe('NodeHomePage: empty state', () => {
  it('has three buttons — new project, clone, open an existing folder — and the last one opens the picker directly', async () => {
    const user = userEvent.setup()
    mount([])
    expect(screen.getByTestId('project-new-empty')).toHaveTextContent('New project')
    expect(screen.getByTestId('project-clone-empty')).toHaveTextContent('Clone a repository')
    expect(screen.getByTestId('project-open-empty')).toHaveTextContent('Open an existing folder')
    expect(screen.queryByTestId('project-new-dialog')).toBeNull()

    await user.click(screen.getByTestId('project-open-empty'))
    const dialog = await screen.findByTestId('project-new-dialog')
    expect(within(dialog).getByTestId('project-new-mode-existing')).toHaveAttribute('data-state', 'on')
    expect(await within(dialog).findByTestId('folder-picker')).toBeInTheDocument()
    expect(rpc.methods()).toContain('workspace.browse')
  })

  it('the other two buttons open the dialog on their own way', async () => {
    const user = userEvent.setup()
    mount([])
    await user.click(screen.getByTestId('project-clone-empty'))
    expect(within(await screen.findByTestId('project-new-dialog')).getByTestId('project-clone-url')).toBeInTheDocument()
    await user.keyboard('{Escape}')
    await user.click(screen.getByTestId('project-new-empty'))
    expect(within(await screen.findByTestId('project-new-dialog')).getByTestId('project-new-name')).toBeInTheDocument()
  })

  it('reopens on the way of the button that was clicked, not the one used last', async () => {
    const user = userEvent.setup()
    mount([])
    await user.click(screen.getByTestId('project-open-empty'))
    await screen.findByTestId('folder-picker')
    await user.keyboard('{Escape}')
    await user.click(screen.getByTestId('project-new-empty'))
    expect(within(await screen.findByTestId('project-new-dialog')).getByTestId('project-new-mode-empty')).toHaveAttribute('data-state', 'on')
  })

  it('shows the buttons disabled while the node is offline', () => {
    mount([], 'peer_offline')
    expect(screen.getByTestId('project-open-empty')).toBeDisabled()
    expect(screen.getByTestId('project-clone-empty')).toBeDisabled()
  })

  it('still shows the picker as unavailable (with the reason) when the node has no link_roots', async () => {
    rpc.handlers['workspace.browse'] = () => {
      throw rpcError('forbidden')
    }
    const user = userEvent.setup()
    mount([])
    await user.click(screen.getByTestId('project-open-empty'))
    expect(await screen.findByTestId('folder-picker-unavailable')).toHaveTextContent('workspace.link_roots')
  })
})

describe('NodeHomePage: project cards', () => {
  it('marks linked projects with their path, flags missing folders, and leaves ordinary projects alone', () => {
    mount([
      project({ name: 'plain', modified_at_ms: Date.now() }),
      project({ name: 'ext', path: '/home/me/code/ext', kind: 'linked', modified_at_ms: Date.now() - 1000 }),
      project({ name: 'gone', path: '/mnt/gone', kind: 'linked', missing: true, modified_at_ms: 0 }),
    ])
    const card = (n: string): HTMLElement => screen.getAllByTestId('project-card').find((c) => c.getAttribute('data-project') === n)!

    expect(within(card('plain')).queryByTestId('project-linked')).toBeNull()
    expect(within(card('plain')).queryByTestId('project-path')).toBeNull()

    expect(within(card('ext')).getByTestId('project-linked')).toHaveTextContent('Linked')
    expect(within(card('ext')).getByTestId('project-path')).toHaveTextContent('/home/me/code/ext')
    expect(within(card('ext')).queryByTestId('project-missing')).toBeNull()

    expect(within(card('gone')).getByTestId('project-linked')).toBeInTheDocument()
    expect(within(card('gone')).getByTestId('project-missing')).toHaveTextContent('Folder not found')
    expect(card('gone')).toHaveAttribute('data-missing', 'true')
    // 仍然能点进去（项目页里有「从列表移除」）
    expect(card('gone')).toHaveAttribute('href', '/n/nnnnnnnn/p/gone')
  })
})
