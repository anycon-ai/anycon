// 会话页 / 权限卡片上的「来自清单项」：向节点要那一项（`todo.get`）、读不到时什么都不画；节点确认的内容并进缓存时带发出时的通知序号。
import { render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { linkError } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { handleTodoChanged, resetTodosMemory } from '../service'
import { useTodos } from '../store'
import { TodoOriginLink } from './TodoOrigin'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const N = 'n'.repeat(32)
let rpc: FakeRpc
const empty = () => useTodos.setState({ nodes: { [N]: { items: {}, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } })

beforeEach(() => {
  rpc = new FakeRpc()
  setRpc(rpc)
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', info: { capabilities: { todo: true } } as never }) }, order: [N], loaded: true })
  resetTodosMemory()
  empty()
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('TodoOriginLink with a label', () => {
  // 在途的 `todo.get` 按（节点、项）合并在模块里：每个场景用自己的 id，免得拿到上一个场景悬着的请求
  const link = (id = 't1') => render(<TodoOriginLink nodeId={N} todoId={id} label="From to-do:" testId="permission-todo" />)

  it('writes the label and the link together once the item is known', async () => {
    rpc.handlers['todo.get'] = () => todoItem({ id: 't1', title: 'The item' })
    link('t1')
    expect(await screen.findByTestId('permission-todo')).toHaveTextContent('From to-do:The item')
  })

  it('draws nothing at all (no bare label) while loading, when unreachable, when the node errors, and when offline', async () => {
    rpc.handlers['todo.get'] = () => new Promise(() => undefined)
    const loading = link('loading')
    expect(screen.queryByTestId('permission-todo')).toBeNull()
    loading.unmount()

    rpc.handlers['todo.get'] = () => {
      throw linkError('timeout')
    }
    const unreachable = link('unreachable')
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'todo.get' && (c.params as { id: string }).id === 'unreachable')).toHaveLength(1))
    await new Promise((r) => setTimeout(r, 20))
    expect(screen.queryByTestId('permission-todo')).toBeNull()
    expect(document.body.textContent).not.toContain('From to-do:')
    unreachable.unmount()

    rpc.handlers['todo.get'] = () => {
      throw rpcError('internal')
    }
    const failing = link('failing')
    await waitFor(() => expect(rpc.calls.filter((c) => (c.params as { id?: string }).id === 'failing')).toHaveLength(1))
    await new Promise((r) => setTimeout(r, 20))
    expect(document.body.textContent).not.toContain('From to-do:')
    failing.unmount()

    useNodes.getState().patch(N, { conn: 'peer_offline' })
    link('offline')
    expect(document.body.textContent).not.toContain('From to-do:')
  })

  it('an item the node says is gone is written as deleted, with the label', async () => {
    rpc.handlers['todo.get'] = () => {
      throw rpcError('not_found')
    }
    link('gone')
    expect(await screen.findByTestId('permission-todo')).toHaveTextContent(/From to-do:.*deleted/i)
  })
})

describe('a todo.get reply that is older than a todo.changed that arrived while it was in flight', () => {
  it('does not overwrite the notification’s activity (the reply is applied with the tick taken before sending)', async () => {
    rpc.handlers['todo.get'] = () => {
      handleTodoChanged(N, { id: 'tick', todo: todoItem({ id: 'tick', title: 'The item', updated_ms: 6_000, activity: 'review' as never }) })
      return todoItem({ id: 'tick', title: 'The item', updated_ms: 6_000, activity: 'working' as never })
    }
    render(<TodoOriginLink nodeId={N} todoId="tick" />)
    await waitFor(() => expect(rpc.calls.some((c) => c.method === 'todo.get')).toBe(true))
    await waitFor(() => expect(screen.getByTestId('session-todo-link')).toBeInTheDocument())
    // 回执晚到：缓存里仍然是通知带来的 review
    await new Promise((r) => setTimeout(r, 20))
    expect(useTodos.getState().nodes[N]!.items.tick!.activity).toBe('review')
  })
})
