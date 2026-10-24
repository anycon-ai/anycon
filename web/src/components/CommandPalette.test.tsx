// ⌘K：除了跳到会话 / 项目 / 节点，还能直接新建项目、克隆仓库、关联已有目录（不用先去节点首页）。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNewProject } from '@/stores/newProject'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { CommandPalette } from './CommandPalette'

const NODE = 'n'.repeat(32)

beforeEach(() => {
  navigate.mockReset()
  useNewProject.setState({ request: null })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

describe('command palette — new project', () => {
  it.each([
    ['New project', 'empty'],
    ['Clone a repository', 'clone'],
    ['Open an existing folder', 'existing'],
  ])('“%s” opens the new-project dialog on that way, and closes the palette', async (label, mode) => {
    const user = userEvent.setup()
    const onOpenChange = vi.fn()
    render(<CommandPalette open onOpenChange={onOpenChange} />)
    await user.click(screen.getByRole('button', { name: label }))
    expect(useNewProject.getState().request).toEqual({ nodeId: NODE, mode, then: 'project', pickNode: true })
    expect(onOpenChange).toHaveBeenCalledWith(false)
  })
})
