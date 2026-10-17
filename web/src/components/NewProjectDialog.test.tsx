import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { BrowseEntry, Project, WorkspaceBrowseParams, WorkspaceBrowseResult, WorkspaceLinkParams } from '@/protocol'
import { nodeEntry, project } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { NewProjectDialog, type NewProjectMode } from './NewProjectDialog'

const NODE = 'n'.repeat(32)
const HOME = '/home/me'
const dir = (name: string, over: Partial<BrowseEntry> = {}): BrowseEntry => ({ name, path: `${HOME}/${name}`, git: false, linkable: true, ...over })

let rpc: FakeRpc
let links: WorkspaceLinkParams[]

function browseHome(entries: BrowseEntry[]): void {
  rpc.handlers['workspace.browse'] = (params) => {
    const p = params as WorkspaceBrowseParams
    const path = p.path ?? HOME
    if (path !== HOME) throw rpcError('not_found')
    return { path: HOME, parent: null, roots: [HOME], linkable: false, project: null, entries } satisfies WorkspaceBrowseResult
  }
}

beforeEach(() => {
  navigate.mockReset()
  rpc = new FakeRpc()
  links = []
  rpc.handlers['workspace.list'] = () => ({ root: '/workspace', projects: [] })
  rpc.handlers['workspace.link'] = (params) => {
    const p = params as WorkspaceLinkParams
    links.push(p)
    return project({ name: p.name ?? 'x', path: p.path, kind: 'linked' }) satisfies Project
  }
  browseHome([dir('code'), dir('.dotdir', { git: true }), dir('taken', { linkable: false, project: 'taken-project' })])
  setRpc(rpc)
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, projects: [project({ name: 'app' })] }) }, order: [NODE], loaded: true })
})
afterEach(() => setRpc(null))

function mount(initialMode: NewProjectMode = 'existing') {
  const onOpenChange = vi.fn()
  render(<NewProjectDialog nodeId={NODE} open onOpenChange={onOpenChange} initialMode={initialMode} />)
  return { onOpenChange }
}

describe('NewProjectDialog: existing folder', () => {
  it('opens straight on the picker when asked to, and the other two ways are one click away', async () => {
    mount('existing')
    expect(await screen.findByTestId('folder-picker')).toBeInTheDocument()
    expect(screen.getByTestId('project-new-mode-existing')).toHaveAttribute('data-state', 'on')
    expect(screen.getByTestId('project-new-mode-empty')).toHaveAttribute('data-state', 'off')
    // 没选目录之前不能提交，名称框也不可用
    expect(screen.getByTestId('project-new-submit')).toBeDisabled()
    expect(screen.getByTestId('project-new-submit')).toHaveTextContent('Link folder')
    expect(screen.getByTestId('project-link-name')).toBeDisabled()
    expect(screen.getByTestId('folder-selected')).toHaveTextContent('No folder selected yet')
  })

  it('does not browse the node until the existing-folder way is shown', async () => {
    const user = userEvent.setup()
    mount('empty')
    expect(rpc.methods()).not.toContain('workspace.browse')
    await user.click(screen.getByTestId('project-new-mode-existing'))
    expect(await screen.findByTestId('folder-picker')).toBeInTheDocument()
    expect(rpc.methods()).toContain('workspace.browse')
  })

  it('defaults the name to the folder name, lets the user change it, links, and goes to the project page', async () => {
    const user = userEvent.setup()
    const { onOpenChange } = mount()
    await screen.findByTestId('folder-list')
    const code = screen.getAllByTestId('folder-entry').find((e) => e.getAttribute('data-name') === 'code')!
    await user.click(within(code).getByTestId('folder-select'))
    expect(screen.getByTestId('folder-selected')).toHaveTextContent(`${HOME}/code`)
    const name = screen.getByTestId('project-link-name')
    expect(name).toBeEnabled()
    expect(name).toHaveValue('code')
    expect(screen.getByTestId('project-new-submit')).toBeEnabled()

    await user.clear(name)
    await user.type(name, 'my-code')
    await user.click(screen.getByTestId('project-new-submit'))

    await waitFor(() => expect(links).toEqual([{ path: `${HOME}/code`, name: 'my-code' }]))
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    // 跳到项目页用的是 daemon 返回的 Project.name
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/p/$project', params: { node: 'nnnnnnnn', project: 'my-code' } })
    // 项目列表被刷新（workspace.changed 也会来，这里是不依赖它的保险）
    expect(rpc.methods()).toContain('workspace.list')
  })

  it('does not overwrite a name the user typed when another folder is picked, but follows the folder name otherwise', async () => {
    const user = userEvent.setup()
    browseHome([dir('code'), dir('notes')])
    mount()
    await screen.findByTestId('folder-list')
    const pick = async (n: string): Promise<void> => {
      const r = screen.getAllByTestId('folder-entry').find((e) => e.getAttribute('data-name') === n)!
      await user.click(within(r).getByTestId('folder-select'))
    }
    await pick('code')
    expect(screen.getByTestId('project-link-name')).toHaveValue('code')
    await pick('notes')
    expect(screen.getByTestId('project-link-name')).toHaveValue('notes')
    await user.clear(screen.getByTestId('project-link-name'))
    await user.type(screen.getByTestId('project-link-name'), 'mine')
    await pick('code')
    expect(screen.getByTestId('project-link-name')).toHaveValue('mine')
  })

  it('strips the leading dot of a hidden folder for the default name (a project name cannot start with one)', async () => {
    const user = userEvent.setup()
    mount()
    await screen.findByTestId('folder-list')
    const dot = screen.getAllByTestId('folder-entry').find((e) => e.getAttribute('data-name') === '.dotdir')!
    await user.click(within(dot).getByTestId('folder-select'))
    expect(screen.getByTestId('project-link-name')).toHaveValue('dotdir')
  })

  it('refuses a name that is invalid or already used by a project, before asking the node', async () => {
    const user = userEvent.setup()
    mount()
    await screen.findByTestId('folder-list')
    const code = screen.getAllByTestId('folder-entry').find((e) => e.getAttribute('data-name') === 'code')!
    await user.click(within(code).getByTestId('folder-select'))
    const name = screen.getByTestId('project-link-name')

    await user.clear(name)
    await user.type(name, 'app') // 项目列表里已经有 app
    expect(screen.getByText('A project with this name already exists.')).toBeInTheDocument()
    expect(screen.getByTestId('project-new-submit')).toBeDisabled()

    await user.clear(name)
    await user.type(name, 'a/b')
    expect(screen.getByText(/cannot be empty, contain/)).toBeInTheDocument()
    expect(screen.getByTestId('project-new-submit')).toBeDisabled()

    await user.clear(name)
    await user.type(name, 'fresh')
    expect(screen.queryByText(/cannot be empty, contain/)).toBeNull()
    expect(screen.getByTestId('project-new-submit')).toBeEnabled()
    expect(links).toEqual([])
  })

  describe('local duplicate check (NFC, case-insensitive where the node’s filesystem is)', () => {
    const setNode = (platform: string | undefined, names: string[]): void =>
      useNodes.setState({
        byId: { [NODE]: nodeEntry({ id: NODE, projects: names.map((name) => project({ name })), info: platform ? ({ platform } as never) : undefined }) },
        order: [NODE],
        loaded: true,
      })
    const pickCodeAndName = async (user: ReturnType<typeof userEvent.setup>, name: string): Promise<void> => {
      await screen.findByTestId('folder-list')
      const code = screen.getAllByTestId('folder-entry').find((e) => e.getAttribute('data-name') === 'code')!
      await user.click(within(code).getByTestId('folder-select'))
      const input = screen.getByTestId('project-link-name')
      await user.clear(input)
      await user.type(input, name)
    }

    it('counts the NFC and NFD spellings of a name as the same, on any node', async () => {
      setNode('linux-x86_64', ['Caf\u00e9'])
      const user = userEvent.setup()
      mount()
      await pickCodeAndName(user, 'Cafe\u0301')
      expect(screen.getByText('A project with this name already exists.')).toBeInTheDocument()
      expect(screen.getByTestId('project-new-submit')).toBeDisabled()
    })

    it.each(['macos-aarch64', 'windows-x86_64'])('refuses a name that differs only in case on %s (case-insensitive filesystem)', async (platform) => {
      setNode(platform, ['proj'])
      const user = userEvent.setup()
      mount()
      await pickCodeAndName(user, 'PROJ')
      expect(screen.getByText(/Another project is named “proj”\. Names that differ only in case count as the same name/)).toBeInTheDocument()
      expect(screen.getByTestId('project-new-submit')).toBeDisabled()
    })

    it('only warns about a case-only difference on a Linux node (the filesystem is usually case-sensitive), and still lets the node decide', async () => {
      setNode('linux-x86_64', ['proj'])
      const user = userEvent.setup()
      mount()
      await pickCodeAndName(user, 'PROJ')
      expect(screen.getByText(/On a case-insensitive filesystem/)).toBeInTheDocument()
      expect(screen.getByTestId('project-new-submit')).toBeEnabled()
      await user.click(screen.getByTestId('project-new-submit'))
      await waitFor(() => expect(links).toEqual([{ path: `${HOME}/code`, name: 'PROJ' }]))
    })

    it('rejects a name with a backslash or a control character before asking the node', async () => {
      const user = userEvent.setup()
      mount()
      await pickCodeAndName(user, 'a\\b')
      expect(screen.getByText(/cannot be empty, contain/)).toBeInTheDocument()
      expect(screen.getByTestId('project-new-submit')).toBeDisabled()
    })
  })

  it.each([
    ['forbidden', /does not allow linking that folder/],
    ['exists', /already used by another project, or this folder is already linked/],
    ['invalid', /cannot be linked/],
    ['not_found', /no longer exists/],
  ] as const)('shows a specific message when the node answers %s, and stays open', async (code, message) => {
    rpc.handlers['workspace.link'] = () => {
      throw rpcError(code)
    }
    const user = userEvent.setup()
    const { onOpenChange } = mount()
    await screen.findByTestId('folder-list')
    const code_ = screen.getAllByTestId('folder-entry').find((e) => e.getAttribute('data-name') === 'code')!
    await user.click(within(code_).getByTestId('folder-select'))
    await user.click(screen.getByTestId('project-new-submit'))
    expect(await screen.findByTestId('project-new-error')).toHaveTextContent(message)
    expect(onOpenChange).not.toHaveBeenCalled()
    expect(navigate).not.toHaveBeenCalled()
    expect(screen.getByTestId('project-new-submit')).toBeEnabled() // 可以改名字重试
  })

  it('explains a forbidden link with every reason it can have (range, hidden, system folders, anycon’s own folders)', async () => {
    rpc.handlers['workspace.link'] = () => {
      throw rpcError('forbidden')
    }
    const user = userEvent.setup()
    mount()
    await screen.findByTestId('folder-list')
    const code = screen.getAllByTestId('folder-entry').find((e) => e.getAttribute('data-name') === 'code')!
    await user.click(within(code).getByTestId('folder-select'))
    await user.click(screen.getByTestId('project-new-submit'))
    const err = await screen.findByTestId('project-new-error')
    for (const part of ['workspace.link_roots', 'hidden', 'system or credentials folder', 'workspace or data folders']) expect(err).toHaveTextContent(part)
  })

  it('marks the way as unavailable and explains why when the node has no link_roots', async () => {
    rpc.handlers['workspace.browse'] = () => {
      throw rpcError('forbidden')
    }
    const user = userEvent.setup()
    mount()
    const card = await screen.findByTestId('folder-picker-unavailable')
    expect(card).toHaveTextContent('workspace.link_roots')
    expect(screen.getByTestId('project-new-submit')).toBeDisabled()
    expect(screen.queryByTestId('project-link-name')).toBeNull()
    // 这一项带锁和说明；另外两种方式照常能用
    expect(screen.getByTestId('project-new-mode-existing')).toHaveAttribute('title', 'Turned off on this node')
    await user.click(screen.getByTestId('project-new-mode-empty'))
    await user.type(screen.getByTestId('project-new-name'), 'x')
    expect(screen.getByTestId('project-new-submit')).toBeEnabled()
  })

  it('keeps the empty-project and clone ways working', async () => {
    rpc.handlers['workspace.create'] = () => ({})
    const user = userEvent.setup()
    mount('empty')
    await user.type(screen.getByTestId('project-new-name'), 'fresh')
    await user.click(screen.getByTestId('project-new-submit'))
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/p/$project', params: { node: 'nnnnnnnn', project: 'fresh' } }))
    expect(rpc.calls.find((c) => c.method === 'workspace.create')?.params).toEqual({ name: 'fresh', git_init: true })
  })

  it('opens on the clone way when asked to', () => {
    mount('clone')
    expect(screen.getByTestId('project-clone-url')).toBeInTheDocument()
    expect(screen.getByTestId('project-new-submit')).toHaveTextContent('Clone')
  })
})
