import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { BrowseEntry, WorkspaceBrowseParams, WorkspaceBrowseResult } from '@/protocol'
import i18n from '@/i18n'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { FolderPicker } from './FolderPicker'

const NODE = 'n'.repeat(32)
const HOME = '/home/me'

const entry = (parent: string, name: string, over: Partial<BrowseEntry> = {}): BrowseEntry => ({ name, path: `${parent}/${name}`, git: false, linkable: true, ...over })

/** 一棵假的目录树：路径 → 子目录。隐藏目录（名字以 `.` 开头）只在 show_hidden 时列出。 */
const TREE: Record<string, BrowseEntry[]> = {
  [HOME]: [
    entry(HOME, '.config'),
    entry(HOME, 'code'),
    entry(HOME, 'notes'),
    entry(HOME, 'workspace', { linkable: false, project: undefined }), // 包含 anycon 工作区：不能关联，但能进入
  ],
  [`${HOME}/code`]: [entry(`${HOME}/code`, 'app', { git: true }), entry(`${HOME}/code`, 'old-app', { project: 'old', linkable: false }), entry(`${HOME}/code`, '.cache')],
  [`${HOME}/code/app`]: [],
  [`${HOME}/notes`]: [],
  [`${HOME}/.config`]: [entry(`${HOME}/.config`, 'tool')],
  [`${HOME}/workspace`]: [entry(`${HOME}/workspace`, 'proj')],
}

let rpc: FakeRpc
let browseCalls: WorkspaceBrowseParams[]

function install(opts: { roots?: string[]; onBrowse?: (p: WorkspaceBrowseParams) => WorkspaceBrowseResult | void; truncated?: boolean } = {}): void {
  const roots = opts.roots ?? [HOME]
  rpc.handlers['workspace.browse'] = (params) => {
    const p = params as WorkspaceBrowseParams
    browseCalls.push(p)
    const custom = opts.onBrowse?.(p)
    if (custom) return custom
    const path = p.path ?? roots[0]!
    const root = roots.find((r) => path === r || path.startsWith(`${r}/`))
    if (!root) throw rpcError('forbidden')
    const entries = TREE[path]
    if (!entries) throw rpcError('not_found')
    return {
      path,
      parent: path === root ? null : path.slice(0, path.lastIndexOf('/')),
      roots,
      linkable: path !== HOME && !path.endsWith('/workspace'),
      project: path.endsWith('/old-app') ? 'old' : null,
      entries: entries.filter((e) => p.show_hidden || !e.name.startsWith('.')),
      truncated: opts.truncated,
    } satisfies WorkspaceBrowseResult
  }
}

beforeEach(() => {
  rpc = new FakeRpc()
  browseCalls = []
  setRpc(rpc)
  install()
})
afterEach(() => setRpc(null))

const names = (): string[] => screen.queryAllByTestId('folder-entry').map((e) => e.getAttribute('data-name') ?? '')
const row = (name: string): HTMLElement => screen.getAllByTestId('folder-entry').find((e) => e.getAttribute('data-name') === name)!

describe('FolderPicker', () => {
  it('starts at the first root (browse without a path) and lists sub-folders, hiding dot-folders', async () => {
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    expect(await screen.findByTestId('folder-list')).toBeInTheDocument()
    expect(browseCalls).toEqual([{ show_hidden: false }])
    expect(names()).toEqual(['code', 'notes', 'workspace'])
    // 路径框显示 daemon 返回的 canonical 路径
    expect(screen.getByTestId('folder-path')).toHaveValue(HOME)
  })

  it('opens a folder on click, shows breadcrumbs, and goes up / jumps back via parent and crumbs', async () => {
    const user = userEvent.setup()
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    await screen.findByTestId('folder-list')
    // 在 root：不能再往上
    expect(screen.getByTestId('folder-up')).toBeDisabled()

    await user.click(within(row('code')).getByTestId('folder-enter'))
    await waitFor(() => expect(names()).toEqual(['app', 'old-app']))
    expect(browseCalls.at(-1)).toEqual({ path: `${HOME}/code`, show_hidden: false })
    expect(screen.getByTestId('folder-path')).toHaveValue(`${HOME}/code`)
    expect(within(screen.getByTestId('folder-crumbs')).getByText('code')).toHaveAttribute('aria-current', 'page')

    await user.click(within(row('app')).getByTestId('folder-enter'))
    await waitFor(() => expect(screen.getByTestId('folder-empty')).toBeInTheDocument())
    expect(screen.getAllByTestId('folder-crumb').map((c) => c.textContent)).toEqual(['me', 'code'])

    expect(screen.getByTestId('folder-up')).toBeEnabled()
    await user.click(screen.getByTestId('folder-up'))
    await waitFor(() => expect(names()).toEqual(['app', 'old-app']))
    expect(browseCalls.at(-1)?.path).toBe(`${HOME}/code`)

    await user.click(screen.getByTestId('folder-up'))
    await waitFor(() => expect(names()).toEqual(['code', 'notes', 'workspace']))
    expect(screen.getByTestId('folder-up')).toBeDisabled()

    // 点面包屑里的上层
    await user.click(within(row('code')).getByTestId('folder-enter'))
    await waitFor(() => expect(names()).toEqual(['app', 'old-app']))
    await user.click(within(row('app')).getByTestId('folder-enter'))
    await waitFor(() => expect(screen.getByTestId('folder-empty')).toBeInTheDocument())
    await user.click(screen.getAllByTestId('folder-crumb')[0]!)
    await waitFor(() => expect(names()).toEqual(['code', 'notes', 'workspace']))
  })

  it('marks git folders, greys out folders that are already projects, and lets the user open the ones that cannot be linked', async () => {
    const user = userEvent.setup()
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    await screen.findByTestId('folder-list')
    // 不能关联但能进入：workspace 没有「选择」按钮，有「不能关联」标记
    const ws = row('workspace')
    expect(ws).toHaveAttribute('data-linkable', 'false')
    expect(within(ws).queryByTestId('folder-select')).toBeNull()
    expect(within(ws).getByTestId('folder-unlinkable')).toBeInTheDocument()
    await user.click(within(ws).getByTestId('folder-enter'))
    await waitFor(() => expect(names()).toEqual(['proj']))

    await user.click(screen.getByTestId('folder-up'))
    await waitFor(() => expect(names()).toContain('code'))
    await user.click(within(row('code')).getByTestId('folder-enter'))
    await waitFor(() => expect(names()).toEqual(['app', 'old-app']))
    expect(within(row('app')).getByTestId('folder-git')).toHaveTextContent('git')
    // 已是项目：灰掉、写明项目名、不能选
    const old = row('old-app')
    expect(old).toHaveAttribute('data-project', 'old')
    expect(within(old).getByTestId('folder-project')).toHaveTextContent('Project: old')
    expect(within(old).getByTestId('folder-enter').className).toContain('opacity-60')
    expect(within(old).queryByTestId('folder-select')).toBeNull()
  })

  it('selects a linkable folder, and the current folder only when it is linkable', async () => {
    const onSelect = vi.fn()
    const user = userEvent.setup()
    const { rerender } = render(<FolderPicker nodeId={NODE} selected={null} onSelect={onSelect} />)
    await screen.findByTestId('folder-list')
    // root（HOME）本身不能关联
    expect(screen.getByTestId('folder-select-current')).toBeDisabled()

    await user.click(within(row('notes')).getByTestId('folder-select'))
    expect(onSelect).toHaveBeenLastCalledWith(`${HOME}/notes`)
    rerender(<FolderPicker nodeId={NODE} selected={`${HOME}/notes`} onSelect={onSelect} />)
    expect(within(row('notes')).getByTestId('folder-select')).toHaveAttribute('aria-pressed', 'true')
    expect(row('notes')).toHaveAttribute('data-selected', 'true')
    expect(row('code')).toHaveAttribute('data-selected', 'false')

    await user.click(within(row('code')).getByTestId('folder-enter'))
    await waitFor(() => expect(names()).toContain('app'))
    expect(screen.getByTestId('folder-select-current')).toBeEnabled()
    await user.click(screen.getByTestId('folder-select-current'))
    expect(onSelect).toHaveBeenLastCalledWith(`${HOME}/code`)
  })

  it('re-lists with show_hidden when the switch is turned on, staying in the same folder', async () => {
    const user = userEvent.setup()
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    await screen.findByTestId('folder-list')
    await user.click(screen.getByRole('switch', { name: 'Show hidden folders' }))
    await waitFor(() => expect(names()).toEqual(['.config', 'code', 'notes', 'workspace']))
    expect(browseCalls.at(-1)).toEqual({ path: HOME, show_hidden: true })
    // 进入子目录时保持开关
    await user.click(within(row('code')).getByTestId('folder-enter'))
    await waitFor(() => expect(names()).toEqual(['app', 'old-app', '.cache']))
    expect(browseCalls.at(-1)).toEqual({ path: `${HOME}/code`, show_hidden: true })
  })

  it('goes to a pasted path, also with Enter, without submitting a surrounding form', async () => {
    const onSubmit = vi.fn((e: { preventDefault: () => void }) => e.preventDefault())
    const user = userEvent.setup()
    render(
      <form onSubmit={onSubmit}>
        <FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />
      </form>,
    )
    await screen.findByTestId('folder-list')
    const input = screen.getByTestId('folder-path')
    await user.clear(input)
    await user.click(input)
    await user.paste(`${HOME}/code`)
    await user.click(screen.getByTestId('folder-go'))
    await waitFor(() => expect(names()).toEqual(['app', 'old-app']))
    expect(browseCalls.at(-1)).toEqual({ path: `${HOME}/code`, show_hidden: false })

    await user.clear(input)
    await user.type(input, '~/notes{Enter}')
    await waitFor(() => expect(browseCalls.at(-1)?.path).toBe('~/notes'))
    expect(onSubmit).not.toHaveBeenCalled()
  })

  it('keeps the current folder and says why when a typed path is refused (outside the roots / missing)', async () => {
    const user = userEvent.setup()
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    await screen.findByTestId('folder-list')
    const input = screen.getByTestId('folder-path')
    await user.clear(input)
    await user.type(input, '/etc{Enter}')
    expect(await screen.findByTestId('folder-error')).toHaveTextContent('outside the folders this node allows')
    expect(names()).toEqual(['code', 'notes', 'workspace']) // 仍在原来的目录
    expect(input).toHaveValue('/etc') // 输入框留着，方便改

    await user.clear(input)
    await user.type(input, `${HOME}/nope{Enter}`)
    expect(await screen.findByTestId('folder-error')).toHaveTextContent('does not exist')

    // 成功之后错误消失
    await user.clear(input)
    await user.type(input, `${HOME}/notes{Enter}`)
    await waitFor(() => expect(screen.queryByTestId('folder-error')).toBeNull())
  })

  it('shows the feature as unavailable, with the reason, when browse is forbidden from the start (link_roots empty)', async () => {
    rpc.handlers['workspace.browse'] = () => {
      throw rpcError('forbidden')
    }
    const onUnavailable = vi.fn()
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} onUnavailable={onUnavailable} />)
    const card = await screen.findByTestId('folder-picker-unavailable')
    expect(card).toHaveTextContent('turned off on this node')
    expect(card).toHaveTextContent('workspace.link_roots')
    expect(onUnavailable).toHaveBeenCalledTimes(1)
    expect(screen.queryByTestId('folder-list')).toBeNull()
    expect(screen.queryByTestId('folder-path')).toBeNull()
  })

  it('offers a retry when the first listing fails for another reason', async () => {
    let fail = true
    install({
      onBrowse: () => {
        if (fail) throw rpcError('internal')
      },
    })
    const user = userEvent.setup()
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    const err = await screen.findByTestId('folder-picker-error')
    expect(err).toHaveTextContent('Something went wrong on the node')
    expect(screen.queryByTestId('folder-picker-unavailable')).toBeNull()
    fail = false
    await user.click(within(err).getByRole('button', { name: 'Try again' }))
    expect(await screen.findByTestId('folder-list')).toBeInTheDocument()
  })

  it('switches between several roots', async () => {
    install({ roots: [HOME, '/srv'] })
    TREE['/srv'] = [entry('/srv', 'www')]
    const user = userEvent.setup()
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    await screen.findByTestId('folder-list')
    const select = screen.getByTestId('folder-roots') as HTMLSelectElement
    expect(select.value).toBe(HOME)
    await user.selectOptions(select, '/srv')
    await waitFor(() => expect(names()).toEqual(['www']))
    expect(browseCalls.at(-1)).toEqual({ path: '/srv', show_hidden: false })
    expect(screen.getByTestId('folder-up')).toBeDisabled()
    delete TREE['/srv']
  })

  it('has no root switcher with a single root', async () => {
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    await screen.findByTestId('folder-list')
    expect(screen.queryByTestId('folder-roots')).toBeNull()
  })

  it('tells the user when the listing was cut off at the daemon’s limit', async () => {
    install({ truncated: true })
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    expect(await screen.findByTestId('folder-truncated')).toHaveTextContent('first 1000')
  })

  it('gives every row a tap target of at least 44px so it works on a phone', async () => {
    render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
    await screen.findByTestId('folder-list')
    for (const b of screen.getAllByTestId('folder-enter')) expect(b.className).toContain('min-h-11')
    // 列表自己滚动，不把对话框撑爆
    expect(screen.getByTestId('folder-list').className).toContain('overflow-y-auto')
  })

  describe('blocked (why a folder cannot be linked)', () => {
    const BLOCKED: Array<[NonNullable<BrowseEntry['blocked']>, RegExp]> = [
      ['hidden', /Hidden folder: it can only be linked if it is listed explicitly in workspace\.link_roots/],
      ['sensitive', /System or credentials folder, or one that contains them: it cannot be linked/],
      ['workspace', /Inside the anycon workspace, or containing it/],
      ['data_dir', /anycon’s data folder/],
      ['filesystem_root', /The filesystem root cannot be linked/],
    ]
    const withBlocked = (entries: BrowseEntry[], current: Partial<WorkspaceBrowseResult> = {}): void =>
      install({
        onBrowse: (p) =>
          p.path
            ? undefined
            : { path: HOME, parent: null, roots: [HOME], linkable: false, project: null, entries, ...current },
      })

    it.each(BLOCKED)('shows the reason for a %s folder, marks it with data-blocked, and gives it no Select button — but it can still be opened', async (code, text) => {
      withBlocked([entry(HOME, 'x', { linkable: false, blocked: code }), entry(HOME, 'ok')])
      const user = userEvent.setup()
      render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
      await screen.findByTestId('folder-list')
      const blocked = row('x')
      expect(blocked).toHaveAttribute('data-blocked', code)
      expect(blocked).toHaveAttribute('data-linkable', 'false')
      expect(within(blocked).getByTestId('folder-blocked-reason')).toHaveTextContent(text)
      expect(within(blocked).queryByTestId('folder-select')).toBeNull()
      expect(within(blocked).queryByTestId('folder-unlinkable')).toBeNull() // 有具体原因就不再显示笼统的「不能关联」
      expect(within(blocked).getByTestId('folder-enter')).toBeEnabled()
      // 没有原因的行：data-blocked 为空
      expect(row('ok')).toHaveAttribute('data-blocked', '')
      expect(within(row('ok')).queryByTestId('folder-blocked-reason')).toBeNull()
      await user.click(within(blocked).getByTestId('folder-enter'))
      await waitFor(() => expect(browseCalls.at(-1)?.path).toBe(`${HOME}/x`))
    })

    it('shows the reason for the current folder, disables “Select this folder”, and exposes data-blocked on folder-current', async () => {
      withBlocked([], { path: HOME, linkable: false, blocked: 'hidden' })
      render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
      await screen.findByTestId('folder-list')
      const current = screen.getByTestId('folder-current')
      expect(current).toHaveAttribute('data-blocked', 'hidden')
      expect(screen.getByTestId('folder-current-blocked')).toHaveTextContent('Hidden folder')
      expect(screen.getByTestId('folder-select-current')).toBeDisabled()
      expect(screen.getByTestId('folder-select-current')).toHaveAttribute('title', expect.stringContaining('Hidden folder'))
    })

    it('has an empty data-blocked on a linkable current folder, and no reason line for a folder that is already a project', async () => {
      withBlocked([entry(HOME, 'proj', { linkable: false, project: 'proj' })], { linkable: true })
      render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
      await screen.findByTestId('folder-list')
      expect(screen.getByTestId('folder-current')).toHaveAttribute('data-blocked', '')
      expect(screen.queryByTestId('folder-current-blocked')).toBeNull()
      expect(row('proj')).toHaveAttribute('data-blocked', '')
      expect(row('proj')).toHaveAttribute('data-project', 'proj')
      expect(within(row('proj')).queryByTestId('folder-blocked-reason')).toBeNull()
    })

    it('falls back to a generic reason for a blocked code this web app does not know yet', async () => {
      withBlocked([entry(HOME, 'x', { linkable: false, blocked: 'something_new' as never })])
      render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
      await screen.findByTestId('folder-list')
      expect(within(row('x')).getByTestId('folder-blocked-reason')).toHaveTextContent('This folder cannot be linked')
    })

    it.each([
      ['sensitive', '.ssh', /“\.ssh” cannot be opened because it is a protected system or credentials folder/],
      ['data_dir', '.anycon', /“\.anycon” cannot be opened because it is anycon’s own data folder/],
    ] as const)('says a %s folder cannot be opened because it is protected (not a generic failure), and offers no “open” on that row afterwards', async (code, name, message) => {
      withBlocked([entry(HOME, name, { linkable: false, blocked: code }), entry(HOME, 'other', { linkable: false, blocked: 'hidden' }), entry(HOME, 'ok')])
      const user = userEvent.setup()
      render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
      await screen.findByTestId('folder-list')
      // 协议里没有「能不能进入」：第一次点之前还是可以点的
      expect(within(row(name)).getByTestId('folder-enter')).toBeEnabled()
      rpc.handlers['workspace.browse'] = () => {
        throw rpcError('forbidden')
      }
      await user.click(within(row(name)).getByTestId('folder-enter'))
      const err = await screen.findByTestId('folder-error')
      expect(err).toHaveTextContent(message)
      expect(err).toHaveAttribute('data-kind', 'protected')
      expect(err).toHaveAttribute('role', 'status') // 预期内的拒绝：不是 alert，不是红色
      expect(err.className).not.toContain('text-danger')
      expect(err).not.toHaveTextContent('Something went wrong')
      expect(names()).toEqual([name, 'other', 'ok']) // 仍在原来的目录
      // 被拒之后这一行不再提供「打开」，也没有箭头；其它行不受影响
      const refused = row(name)
      expect(refused).toHaveAttribute('data-refused', 'true')
      expect(within(refused).getByTestId('folder-enter')).toBeDisabled()
      expect(refused.querySelector('svg.lucide-chevron-right')).toBeNull()
      expect(row('ok')).toHaveAttribute('data-refused', 'false')
      expect(within(row('ok')).getByTestId('folder-enter')).toBeEnabled()
      expect(row('ok').querySelector('svg.lucide-chevron-right')).not.toBeNull() // 其它行的箭头还在（上面那条不是空转）
      const calls = browseCalls.length
      await user.click(within(refused).getByTestId('folder-enter'))
      expect(browseCalls.length).toBe(calls) // 不会再发一次请求
    })

    it('keeps hidden and workspace rows openable even if the node refuses them, and shows the usual error for those', async () => {
      withBlocked([entry(HOME, 'h', { linkable: false, blocked: 'hidden' }), entry(HOME, 'w', { linkable: false, blocked: 'workspace' })])
      const user = userEvent.setup()
      render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
      await screen.findByTestId('folder-list')
      rpc.handlers['workspace.browse'] = () => {
        throw rpcError('forbidden')
      }
      await user.click(within(row('h')).getByTestId('folder-enter'))
      const err = await screen.findByTestId('folder-error')
      expect(err).toHaveAttribute('data-kind', 'error')
      expect(err).toHaveAttribute('role', 'alert')
      expect(row('h')).toHaveAttribute('data-refused', 'false')
      expect(within(row('h')).getByTestId('folder-enter')).toBeEnabled()
      expect(within(row('w')).getByTestId('folder-enter')).toBeEnabled()
    })

    it('treats a typed path to a protected folder with the generic refusal (there is no row to explain)', async () => {
      withBlocked([entry(HOME, '.ssh', { linkable: false, blocked: 'sensitive' })])
      const user = userEvent.setup()
      render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
      await screen.findByTestId('folder-list')
      rpc.handlers['workspace.browse'] = () => {
        throw rpcError('forbidden')
      }
      await user.clear(screen.getByTestId('folder-path'))
      await user.type(screen.getByTestId('folder-path'), '/etc{Enter}')
      const err = await screen.findByTestId('folder-error')
      expect(err).toHaveAttribute('data-kind', 'error')
      expect(err).toHaveTextContent('system or credentials folder')
    })

    it('has Chinese copy for the protected-folder refusal', async () => {
      await i18n.changeLanguage('zh')
      try {
        withBlocked([entry(HOME, '.ssh', { linkable: false, blocked: 'sensitive' }), entry(HOME, '.anycon', { linkable: false, blocked: 'data_dir' })])
        const user = userEvent.setup()
        render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
        await screen.findByTestId('folder-list')
        rpc.handlers['workspace.browse'] = () => {
          throw rpcError('forbidden')
        }
        await user.click(within(row('.ssh')).getByTestId('folder-enter'))
        expect(await screen.findByTestId('folder-error')).toHaveTextContent('「.ssh」不能打开：它是受保护的系统或凭证目录。')
        await user.click(within(row('.anycon')).getByTestId('folder-enter'))
        await waitFor(() => expect(screen.getByTestId('folder-error')).toHaveTextContent('「.anycon」不能打开：它是 anycon 自己的数据目录。'))
      } finally {
        await i18n.changeLanguage('en')
      }
    })

    it('has Chinese copy for every reason', async () => {
      await i18n.changeLanguage('zh')
      try {
        withBlocked(BLOCKED.map(([code]) => entry(HOME, code, { linkable: false, blocked: code })))
        render(<FolderPicker nodeId={NODE} selected={null} onSelect={() => {}} />)
        await screen.findByTestId('folder-list')
        expect(within(row('hidden')).getByTestId('folder-blocked-reason')).toHaveTextContent('隐藏目录：必须在 workspace.link_roots 里显式列出才能关联')
        expect(within(row('sensitive')).getByTestId('folder-blocked-reason')).toHaveTextContent('系统或凭证目录，或者包含它们：不能关联')
        expect(within(row('workspace')).getByTestId('folder-blocked-reason')).toHaveTextContent('工作区')
        expect(within(row('data_dir')).getByTestId('folder-blocked-reason')).toHaveTextContent('数据目录')
        expect(within(row('filesystem_root')).getByTestId('folder-blocked-reason')).toHaveTextContent('文件系统根目录')
      } finally {
        await i18n.changeLanguage('en')
      }
    })
  })
})
