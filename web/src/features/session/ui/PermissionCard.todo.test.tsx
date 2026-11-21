// 权限卡片上的来源：会话是从清单项派出的，卡片上写明是哪一项（标题当链接，清理成单行）；不是的会话没有这一行（spec/web.md § 4.6、§ 4.11）。
import { render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetTodosMemory } from '@/features/todos/service'
import { useTodos } from '@/features/todos/store'
import { useNodes } from '@/stores/nodes'
import type { PermissionBlock } from '../reducer'
import { TodoOriginLink } from '@/features/todos/ui/TodoOrigin'
import { PermissionCard } from './PermissionCard'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const N = 'n'.repeat(32)
const block: PermissionBlock = {
  type: 'permission',
  key: 'perm-r1',
  seq: 1,
  ts: 1,
  requestId: 'r1',
  kind: 'tool',
  title: 'Run command',
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: 'echo hi' }, cwd: '/work', detail: { command: 'echo hi' } } },
  options: [{ id: 'allow', label: 'Allow', kind: 'allow_once' }],
}
const render1 = (todo = true) => render(<PermissionCard block={block} deviceLabel={() => 'you'} onRespond={() => {}} todoOrigin={todo ? <TodoOriginLink nodeId={N} todoId="t1" /> : undefined} />)

beforeEach(() => {
  setRpc(new FakeRpc())
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', info: { capabilities: { todo: true } } as never }) }, order: [N], loaded: true })
  resetTodosMemory()
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('PermissionCard: the to-do the session came from', () => {
  it('shows the item title (cleaned to one line) as a link to the item', () => {
    useTodos.setState({ nodes: { [N]: { items: { t1: todoItem({ id: 't1', title: 'Fix the\nredirect‮' }) }, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } })
    render1()
    expect(screen.getByTestId('permission-todo')).toHaveTextContent('From to-do:')
    const link = screen.getByTestId('session-todo-link')
    expect(link).toHaveTextContent('Fix the redirect')
    expect(link.textContent).not.toMatch(/[‮\n]/)
    expect(link).toHaveAttribute('href', expect.stringContaining('/todos/nnnnnnnn/t1'))
  })

  it('a session that is not from a to-do has no such line', () => {
    render1(false)
    expect(screen.queryByTestId('permission-todo')).toBeNull()
  })

  it('an item the node says is gone is written as deleted, not as a dead link', async () => {
    const rpc = new FakeRpc()
    rpc.handlers['todo.get'] = () => {
      throw rpcError('not_found')
    }
    setRpc(rpc)
    useTodos.setState({ nodes: { [N]: { items: {}, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } })
    render1()
    expect(await screen.findByTestId('session-todo-deleted')).toBeInTheDocument()
    expect(screen.queryByTestId('session-todo-link')).toBeNull()
  })
})
