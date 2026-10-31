// 节点设置 →「工作区」：workspace 根（ADR-45）的列表和状态、回收站条目来自哪个根。
import { render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { TrashEntry, WorkspaceRootInfo } from '@/protocol'
import { nodeEntry, nodeInfo, project } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { WorkspaceTab } from './WorkspaceTab'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

const NODE = 'n'.repeat(32)
const MAIN = '/home/me/anycon'
const EXT = '/mnt/ext/projects'
const GONE = '/mnt/unplugged'
let rpc: FakeRpc
let trash: TrashEntry[]

function node(roots: WorkspaceRootInfo[] | undefined, over: { trash?: TrashEntry[]; workspaceRoot?: string } = {}): void {
  useNodes.setState({
    byId: { [NODE]: nodeEntry({ id: NODE, info: nodeInfo({ workspace_roots: roots, workspace_root: MAIN }), workspaceRoot: over.workspaceRoot ?? MAIN, projects: [project({ name: 'a' }), project({ name: 'b' })], trash: over.trash ?? [] }) },
    order: [NODE],
    loaded: true,
  })
}
const entry = (id: string, root?: string): TrashEntry => ({ id: `${id}-20261031T120000Z`, original_name: id, trashed_at_ms: Date.now() - 60_000, bytes: 1024, root })

beforeEach(() => {
  rpc = new FakeRpc()
  trash = []
  rpc.handlers['workspace.trash_list'] = () => ({ entries: trash })
  rpc.handlers['worktree.list'] = () => ({ worktrees: [] })
  rpc.handlers['workspace.worktrees'] = () => ({ worktrees: [] })
  setRpc(rpc)
})
afterEach(() => setRpc(null))

describe('node settings: the Workspace tab and its roots', () => {
  it('shows one root the way it always did', () => {
    node([{ path: MAIN, primary: true, available: true }])
    render(<WorkspaceTab nodeId={NODE} />)
    const rows = screen.getAllByTestId('ws-root')
    expect(rows).toHaveLength(1)
    expect(rows[0]).toHaveTextContent('Workspace root')
    expect(rows[0]).toHaveTextContent(MAIN)
    expect(screen.queryByTestId('ws-root-unavailable')).toBeNull()
  })

  it('falls back to the single root of a node that predates workspace_roots', () => {
    node(undefined)
    render(<WorkspaceTab nodeId={NODE} />)
    expect(screen.getAllByTestId('ws-root')).toHaveLength(1)
    expect(screen.getByTestId('ws-root')).toHaveTextContent(MAIN)
  })

  it('lists every root, marks the primary one and the one that is not available', () => {
    node([
      { path: MAIN, primary: true, available: true },
      { path: EXT, primary: false, available: true },
      { path: GONE, primary: false, available: false },
    ])
    render(<WorkspaceTab nodeId={NODE} />)
    const rows = screen.getAllByTestId('ws-root')
    expect(rows.map((r) => r.textContent)).toEqual([
      `Workspace root (default)${MAIN}`,
      `Extra workspace root${EXT}`,
      `Extra workspace root${GONE}Not available`,
    ])
    expect(within(rows[2]!).getByTestId('ws-root-unavailable')).toHaveAttribute('title', expect.stringMatching(/Restart the node/))
    expect(within(rows[1]!).queryByTestId('ws-root-unavailable')).toBeNull()
  })

  it('says which root each trash entry came from, but only when there is more than one root', async () => {
    trash = [entry('old-a', MAIN), entry('old-b', EXT)]
    node(
      [
        { path: MAIN, primary: true, available: true },
        { path: EXT, primary: false, available: true },
      ],
      { trash },
    )
    render(<WorkspaceTab nodeId={NODE} />)
    const origins = await screen.findAllByTestId('trash-root')
    expect(origins.map((o) => o.textContent)).toEqual([`From ${MAIN}`, `From ${EXT}`])
  })

  it('does not repeat the root on the trash entries of a single-root node', async () => {
    trash = [entry('old-a', MAIN)]
    node([{ path: MAIN, primary: true, available: true }], { trash })
    render(<WorkspaceTab nodeId={NODE} />)
    expect(await screen.findByText('old-a')).toBeInTheDocument()
    expect(screen.queryByTestId('trash-root')).toBeNull()
  })
})
