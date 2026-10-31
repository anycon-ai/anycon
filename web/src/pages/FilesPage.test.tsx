// 文件浏览页：项目列表、目录树（懒加载、排序、隐藏文件）、URL 里的路径、面包屑、手机两屏、键盘、错误和离线状态。
// 预览（文本 / Markdown / 图片 / 二进制）见 FilesPage.preview.test.tsx。
import { useState } from 'react'
import { configure, act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Project } from '@/protocol'
import { nodeEntry, project, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { serveFs, type FakeFile } from '@/test/fakeFs'
import { mockRouter } from '@/test/routerMock'
import { RpcCallError } from '@/transport/rpcClient'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'

// 数据是异步读的；机器忙的时候 1 秒的默认等待不够
configure({ asyncUtilTimeout: 5000 })

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { FilesPage } from './FilesPage'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc
let go: (path: string) => void = () => {}

const FILES: Record<string, FakeFile> = {
  'app/README.md': '# Hello',
  'app/src/main.ts': 'export const a = 1\n',
  'app/src/util/math.ts': 'export const add = (a: number, b: number) => a + b\n',
  'app/src/lib.rs': 'fn main() {}\n',
  'app/data.bin': new Uint8Array([0, 1, 2, 3]),
  'app/.env.example': 'A=1',
  'app/.git/HEAD': 'ref: refs/heads/main',
  'app/.anycon/worktrees/fix/src/x.ts': 'export const x = 1\n',
  'other/a.txt': 'a',
}

/** 点了文件 / 目录之后 URL 变了：用一个有状态的外壳模拟「路由把新路径交给页面」。 */
function Harness({ initial }: { initial: string }) {
  const [path, setPath] = useState(initial)
  go = setPath
  return <FilesPage path={path} />
}

function mount(initial: string, over: { projects?: Project[]; conn?: 'ready' | 'peer_offline' } = {}) {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn: over.conn ?? 'ready', projects: over.projects ?? [project({ name: 'app' }), project({ name: 'other' })] }) }, order: [NODE], loaded: true })
  return render(<Harness initial={initial} />)
}

const rows = (): string[] => screen.getAllByRole('treeitem').map((r) => r.textContent ?? '')
const row = (name: string): HTMLElement => screen.getByRole('treeitem', { name })
const lists = (): string[] => rpc.calls.filter((c) => c.method === 'fs.list').map((c) => (c.params as { path: string }).path)

beforeEach(() => {
  navigate.mockReset()
  navigate.mockImplementation((o: { params: { _splat: string } }) => go(o.params._splat))
  rpc = new FakeRpc()
  serveFs(rpc, FILES)
  setRpc(rpc)
  useSessions.setState({ list: {} })
  usePrefs.setState({ filesShowHidden: false })
})
afterEach(() => setRpc(null))

describe('FilesPage: project list', () => {
  it('lists the projects of the node when no path is given, each one links to its files', () => {
    mount('', { projects: [project({ name: 'zeta' }), project({ name: 'app' }), project({ name: 'gone', missing: true })] })
    const links = screen.getAllByTestId('files-project')
    expect(links.map((l) => l.textContent)).toEqual(['app', 'zeta'])
    expect(links[0]).toHaveAttribute('href', '/n/nnnnnnnn/files/app')
    // 目录不在了的项目不能点
    expect(screen.getByTestId('files-project-missing')).toHaveTextContent('gone')
    expect(screen.getByTestId('files-preview-pane')).toHaveTextContent('Choose a project to browse its files')
    expect(rpc.methods()).toEqual([]) // 项目列表来自节点上已同步的 workspace.list，不去读目录
  })

  it('says so when the node has no projects', () => {
    mount('', { projects: [] })
    expect(screen.getAllByText('This node has no projects yet.').length).toBeGreaterThan(0)
  })
})

describe('FilesPage: tree', () => {
  it('shows the project folder with folders first, sorted by name, and hides dotfiles by default', async () => {
    mount('app')
    await screen.findByRole('treeitem', { name: 'src' })
    expect(rows()).toEqual(['app', 'src', 'data.bin', 'README.md'])
    expect(lists()).toEqual(['app']) // 子目录还没展开，不读
    expect(screen.getByTestId('files-path')).toHaveTextContent('app')
  })

  it('reads a sub-folder only the first time it is opened', async () => {
    const user = userEvent.setup()
    mount('app')
    await user.click(await screen.findByRole('treeitem', { name: 'src' }))
    await screen.findByRole('treeitem', { name: 'util' })
    expect(rows()).toEqual(['app', 'src', 'util', 'lib.rs', 'main.ts', 'data.bin', 'README.md'])
    expect(row('src')).toHaveAttribute('aria-expanded', 'true')
    expect(row('util')).toHaveAttribute('aria-expanded', 'false')
    expect(lists()).toEqual(['app', 'app/src'])
    await user.click(row('src'))
    expect(rows()).toEqual(['app', 'src', 'data.bin', 'README.md'])
    await user.click(row('src'))
    expect(rows()).toContain('main.ts')
    expect(lists()).toEqual(['app', 'app/src']) // 收起再展开不重读
  })

  it('opens a file through the URL when it is clicked, and a folder only toggles', async () => {
    const user = userEvent.setup()
    mount('app')
    await user.click(await screen.findByRole('treeitem', { name: 'README.md' }))
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: 'app/README.md' } })
    expect(await screen.findByTestId('file-preview')).toHaveAttribute('data-path', 'app/README.md')
    expect(row('README.md')).toHaveAttribute('aria-selected', 'true')
    navigate.mockClear()
    await user.click(row('src'))
    expect(navigate).not.toHaveBeenCalled()
  })

  it('opening a nested URL expands every folder above the file and selects it', async () => {
    mount('app/src/util/math.ts')
    await screen.findByTestId('file-preview')
    await screen.findByRole('treeitem', { name: 'math.ts' })
    expect(rows()).toEqual(['app', 'src', 'util', 'math.ts', 'lib.rs', 'main.ts', 'data.bin', 'README.md'])
    expect(row('math.ts')).toHaveAttribute('aria-selected', 'true')
    expect(row('src')).toHaveAttribute('aria-expanded', 'true')
    expect(row('util')).toHaveAttribute('aria-expanded', 'true')
    expect(row('src')).toHaveAttribute('aria-selected', 'false')
    expect(screen.getByTestId('file-name')).toHaveTextContent('math.ts')
  })

  it('shows what is inside a folder on the right when the URL points at one', async () => {
    mount('app/src')
    const dir = await screen.findByTestId('files-dir')
    expect(within(dir).getAllByTestId('files-dir-link').map((l) => l.textContent)).toEqual(['util', 'lib.rs', 'main.ts'])
    expect(within(dir).getAllByTestId('files-dir-link')[2]).toHaveAttribute('href', '/n/nnnnnnnn/files/app/src/main.ts')
    expect(within(dir).getByText('13 B')).toBeInTheDocument() // lib.rs
    expect(row('src')).toHaveAttribute('aria-selected', 'true')
    expect(row('src')).toHaveAttribute('aria-expanded', 'true')
    expect(screen.queryByTestId('file-preview')).toBeNull()
  })

  it('says a folder is empty', async () => {
    serveFs(rpc, FILES, { emptyDirs: ['app/empty'] })
    mount('app/empty')
    expect(await screen.findByTestId('files-empty-dir')).toHaveTextContent('This folder is empty')
    expect(await screen.findByTestId('files-row-empty')).toBeInTheDocument()
  })
})

describe('FilesPage: hidden files', () => {
  it('shows them with the toggle, and remembers the choice', async () => {
    const user = userEvent.setup()
    mount('app')
    await screen.findByRole('treeitem', { name: 'src' })
    expect(screen.getByTestId('files-hidden-toggle')).toHaveAttribute('aria-pressed', 'false')
    await user.click(screen.getByTestId('files-hidden-toggle'))
    expect(rows()).toEqual(['app', '.anycon', '.git', 'src', '.env.example', 'data.bin', 'README.md'])
    expect(screen.getByTestId('files-hidden-toggle')).toHaveAttribute('aria-pressed', 'true')
    expect(usePrefs.getState().filesShowHidden).toBe(true)
    await user.click(screen.getByTestId('files-hidden-toggle'))
    expect(rows()).toEqual(['app', 'src', 'data.bin', 'README.md'])
  })

  it('keeps the hidden folders on the way to the selected path visible (a worktree lives in .anycon/worktrees)', async () => {
    mount('app/.anycon/worktrees/fix/src/x.ts')
    await screen.findByRole('treeitem', { name: 'x.ts' })
    expect(rows()).toEqual(['app', '.anycon', 'worktrees', 'fix', 'src', 'x.ts', 'src', 'data.bin', 'README.md'])
    expect(rows()).not.toContain('.git')
    const path = screen.getByTestId('files-path')
    expect(within(path).getAllByTestId('files-crumb').map((c) => c.textContent)).toEqual(['app', '.anycon', 'worktrees', 'fix', 'src'])
    expect(within(path).getByTestId('files-crumb-current')).toHaveTextContent('x.ts')
  })

  it('also hides dotfiles in the folder listing on the right', async () => {
    const user = userEvent.setup()
    mount('app')
    const dir = await screen.findByTestId('files-dir')
    expect(within(dir).getAllByTestId('files-dir-link').map((l) => l.textContent)).toEqual(['src', 'data.bin', 'README.md'])
    await user.click(screen.getByTestId('files-hidden-toggle'))
    expect(within(screen.getByTestId('files-dir')).getAllByTestId('files-dir-link').map((l) => l.textContent)).toEqual(['.anycon', '.git', 'src', '.env.example', 'data.bin', 'README.md'])
  })
})

describe('FilesPage: breadcrumbs', () => {
  it('every level up to the current one is a link to that folder, the last one is plain text', async () => {
    mount('app/src/util/math.ts')
    await screen.findByTestId('file-preview')
    const nav = screen.getByRole('navigation', { name: 'Path' })
    const links = within(nav).getAllByTestId('files-crumb')
    expect(links.map((l) => [l.textContent, l.getAttribute('href')])).toEqual([
      ['app', '/n/nnnnnnnn/files/app'],
      ['src', '/n/nnnnnnnn/files/app/src'],
      ['util', '/n/nnnnnnnn/files/app/src/util'],
    ])
    expect(within(nav).getByTestId('files-crumb-current')).toHaveTextContent('math.ts')
    expect(within(nav).getByTestId('files-crumb-current').closest('a')).toBeNull()
  })

  it('percent-encodes names in the links and the header crumbs lead back to the node and to the project list', async () => {
    serveFs(rpc, { 'app/my dir/a b#1.txt': 'x' })
    mount('app/my dir/a b#1.txt')
    await screen.findByTestId('file-preview')
    expect(screen.getAllByTestId('files-crumb')[1]).toHaveAttribute('href', '/n/nnnnnnnn/files/app/my%20dir')
    const header = screen.getByRole('navigation', { name: 'Breadcrumb' })
    expect(within(header).getByRole('link', { name: 'test-node' })).toHaveAttribute('href', '/n/nnnnnnnn')
    expect(within(header).getByRole('link', { name: 'Files' })).toHaveAttribute('href', '/n/nnnnnnnn/files/')
  })
})

describe('FilesPage: scratch session', () => {
  it('opens `.scratch/<id>` as its own tree root, labelled with the session title', async () => {
    serveFs(rpc, { '.scratch/sess_1/notes.md': '# notes', '.scratch/sess_1/sub/a.txt': 'a' })
    useSessions.setState({ list: { sess_1: { ...summary({ id: 'sess_1', title: 'Fix the login bug' }), node_id: NODE, cached_seq: 0 } } })
    mount('.scratch/sess_1/notes.md')
    await screen.findByRole('treeitem', { name: 'notes.md' })
    expect(rows()).toEqual(['Fix the login bug', 'sub', 'notes.md'])
    expect(lists()).toContain('.scratch/sess_1')
    expect(screen.getByTestId('files-path')).toHaveTextContent('Fix the login bug')
    expect(screen.getByTestId('files-path')).not.toHaveTextContent('.scratch')
  })
})

describe('FilesPage: refresh', () => {
  it('reloads the lists and the open file', async () => {
    const user = userEvent.setup()
    const files: Record<string, FakeFile> = { ...FILES }
    serveFs(rpc, files)
    mount('app/README.md')
    await screen.findByRole('heading', { name: 'Hello' })
    expect(rows()).not.toContain('NEW.md')
    files['app/NEW.md'] = 'new'
    files['app/README.md'] = '# Changed'
    await user.click(screen.getByTestId('files-refresh'))
    expect(await screen.findByRole('treeitem', { name: 'NEW.md' })).toBeInTheDocument()
    await screen.findByRole('heading', { name: 'Changed' })
  })
})

describe('FilesPage: errors', () => {
  it('explains that the node refuses the path, and retries', async () => {
    const user = userEvent.setup()
    serveFs(rpc, FILES, { forbidden: ['app'] })
    mount('app')
    const err = await screen.findByTestId('files-root-error')
    expect(err).toHaveTextContent('Could not open this path')
    expect(err).toHaveTextContent('The node does not allow access to this path.')
    serveFs(rpc, FILES)
    await user.click(screen.getByTestId('files-retry'))
    expect(await screen.findByRole('treeitem', { name: 'src' })).toBeInTheDocument()
    expect(screen.queryByTestId('files-root-error')).toBeNull()
  })

  it('explains that the path does not exist', async () => {
    mount('gone/x.ts')
    expect(await screen.findByTestId('files-root-error')).toHaveTextContent('This path does not exist on the node.')
  })

  it('shows the node as offline instead of an error, and loads by itself once it is back', async () => {
    rpc.ready = false
    mount('app', { conn: 'peer_offline' })
    const off = await screen.findByTestId('files-offline')
    expect(off).toHaveTextContent('The node is offline')
    expect(screen.queryByTestId('files-root-error')).toBeNull()
    rpc.ready = true
    await act(async () => useNodes.getState().setConn(NODE, 'ready'))
    expect(await screen.findByRole('treeitem', { name: 'src' })).toBeInTheDocument()
    expect(screen.queryByTestId('files-offline')).toBeNull()
  })

  it('a folder that cannot be listed gets an inline error with a retry, the rest of the tree stays', async () => {
    const user = userEvent.setup()
    serveFs(rpc, FILES, { forbidden: ['app/src'] })
    mount('app')
    await user.click(await screen.findByRole('treeitem', { name: 'src' }))
    const err = await screen.findByTestId('files-row-error')
    expect(err).toHaveTextContent('The node does not allow access to this path.')
    expect(rows()).toEqual(['app', 'src', 'data.bin', 'README.md'])
    serveFs(rpc, FILES)
    await user.click(screen.getByTestId('files-row-retry'))
    expect(await screen.findByRole('treeitem', { name: 'main.ts' })).toBeInTheDocument()
    expect(screen.queryByTestId('files-row-error')).toBeNull()
  })

  it('a timeout while listing a folder is reported as such', async () => {
    const user = userEvent.setup()
    serveFs(rpc, FILES, { failWith: (p) => (p === 'app/src' ? new RpcCallError('timeout', 'timed out') : undefined) })
    mount('app')
    await user.click(await screen.findByRole('treeitem', { name: 'src' }))
    expect(await screen.findByTestId('files-row-error')).toHaveTextContent('The node did not answer in time.')
  })

  it('retries a folder that failed because the link dropped, once the node is back', async () => {
    const user = userEvent.setup()
    let down = true
    serveFs(rpc, FILES, { failWith: (p) => (down && p === 'app/src' ? new RpcCallError('undeliverable', 'peer offline') : undefined) })
    mount('app')
    await user.click(await screen.findByRole('treeitem', { name: 'src' }))
    expect(await screen.findByTestId('files-row-error')).toHaveTextContent('The node is offline.')
    down = false
    await act(async () => useNodes.getState().setConn(NODE, 'peer_offline'))
    await act(async () => useNodes.getState().setConn(NODE, 'ready'))
    expect(await screen.findByRole('treeitem', { name: 'main.ts' })).toBeInTheDocument()
  })

  it('a path whose parent folder cannot be read shows the reason in the preview', async () => {
    serveFs(rpc, FILES, { forbidden: ['app/src/util'] })
    mount('app/src/util/math.ts')
    const err = await screen.findByTestId('files-error')
    expect(err).toHaveTextContent('The node does not allow access to this path.')
  })

  it('opening a file that was deleted shows not-found with a retry', async () => {
    mount('app/src/vanished.ts')
    const err = await screen.findByTestId('files-error')
    expect(err).toHaveTextContent('This path does not exist on the node. It may have been moved or deleted.')
    expect(screen.getByTestId('files-retry')).toBeInTheDocument()
  })
})

describe('FilesPage: phone layout', () => {
  const realMatchMedia = window.matchMedia
  beforeEach(() => {
    window.matchMedia = ((query: string) => ({ matches: !/min-width/.test(query), media: query, onchange: null, addEventListener: () => {}, removeEventListener: () => {}, addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false })) as unknown as typeof window.matchMedia
  })
  afterEach(() => {
    window.matchMedia = realMatchMedia
  })

  it('shows the tree on one screen and the preview, with a way back, on the other', async () => {
    const user = userEvent.setup()
    mount('app')
    await screen.findByRole('treeitem', { name: 'src' })
    expect(screen.getByTestId('files-page')).toHaveAttribute('data-screen', 'tree')
    expect(screen.queryByTestId('files-preview-pane')).toBeNull()
    // 展开一个文件夹，再点它里面的文件
    await user.click(row('src'))
    await user.click(await screen.findByRole('treeitem', { name: 'main.ts' }))
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: 'app/src/main.ts' } })
    expect(await screen.findByTestId('file-preview')).toHaveAttribute('data-path', 'app/src/main.ts')
    expect(screen.getByTestId('files-page')).toHaveAttribute('data-screen', 'preview')
    expect(screen.queryByTestId('files-tree')).toBeNull()
    // 返回：回到文件所在的文件夹，树还是展开的样子
    navigate.mockClear()
    await user.click(screen.getByTestId('header-back'))
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: 'app/src' } })
    await waitFor(() => expect(screen.getByTestId('files-page')).toHaveAttribute('data-screen', 'tree'))
    expect(rows()).toEqual(['app', 'src', 'util', 'lib.rs', 'main.ts', 'data.bin', 'README.md'])
    expect(row('src')).toHaveAttribute('aria-expanded', 'true')
  })

  it('lists the projects on the first screen', () => {
    mount('')
    expect(screen.getByTestId('files-page')).toHaveAttribute('data-screen', 'tree')
    expect(screen.getAllByTestId('files-project')).toHaveLength(2)
    expect(screen.queryByTestId('files-preview-pane')).toBeNull()
  })
})

describe('FilesPage: keyboard', () => {
  it('moves with the arrow keys, expands and collapses with left / right, opens a file with Enter', async () => {
    const user = userEvent.setup()
    mount('app')
    await screen.findByRole('treeitem', { name: 'src' })
    row('app').focus()
    await user.keyboard('{ArrowDown}')
    expect(row('src')).toHaveFocus()
    await user.keyboard('{ArrowRight}')
    await screen.findByRole('treeitem', { name: 'main.ts' })
    expect(row('src')).toHaveAttribute('aria-expanded', 'true')
    expect(row('src')).toHaveFocus()
    await user.keyboard('{ArrowRight}') // 已展开：进到第一个孩子
    expect(row('util')).toHaveFocus()
    await user.keyboard('{ArrowDown}{ArrowDown}')
    expect(row('main.ts')).toHaveFocus()
    await user.keyboard('{ArrowLeft}') // 文件：回到父目录
    expect(row('src')).toHaveFocus()
    await user.keyboard('{ArrowLeft}')
    expect(row('src')).toHaveAttribute('aria-expanded', 'false')
    await user.keyboard('{End}')
    expect(row('README.md')).toHaveFocus()
    await user.keyboard('{Enter}')
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: 'app/README.md' } })
    await user.keyboard('{Home}')
    expect(row('app')).toHaveFocus()
  })

  it('only one row is in the tab order', async () => {
    mount('app/src/main.ts')
    await screen.findByRole('treeitem', { name: 'main.ts' })
    const stops = screen.getAllByRole('treeitem').filter((r) => r.tabIndex === 0)
    expect(stops.map((r) => r.textContent)).toEqual(['main.ts'])
  })
})

describe('FilesPage: node switch', () => {
  it('does not render for a node that is not paired', () => {
    useNodes.setState({ byId: {}, order: [], loaded: true })
    const { container } = render(<Harness initial="app" />)
    expect(container).toBeEmptyDOMElement()
    expect(rpc.methods()).toEqual([])
  })
})
