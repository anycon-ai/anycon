// 验收页按地址解析：说清每一种「看不到」的原因；已经关闭的项（刚通过的、刷新后再打开的）不在没关闭的清单缓存里，要向节点要一次。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { linkError } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { resetTodosMemory } from '../service'
import { useTodos } from '../store'
import { TodoReviewPane } from './TodoReviewPane'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const N = 'n'.repeat(32)
let rpc: FakeRpc

const hydrated = () => useTodos.setState({ nodes: { [N]: { items: {}, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } })

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  rpc.handlers['todo.history'] = () => ({ events: [], has_more: false })
  rpc.handlers['session.diff'] = () => ({ from_sha: 'a'.repeat(40), files: [] })
  setRpc(rpc)
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', info: { capabilities: { todo: true } } as never }) }, order: [N], loaded: true })
  resetTodosMemory()
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('TodoReviewPane', () => {
  it('a closed item that is not in the open list is asked for from the node and then shown', async () => {
    rpc.handlers['todo.get'] = () => todoItem({ id: 't1', title: 'Approved earlier', state: 'done' })
    hydrated()
    render(<TodoReviewPane node="nnnnnnnn" todo="t1" />)
    expect(await screen.findByTestId('review-title')).toHaveTextContent('Approved earlier')
    expect(screen.getByTestId('review-closed')).toHaveAttribute('data-state', 'done')
    expect(rpc.calls.filter((c) => c.method === 'todo.get')).toHaveLength(1)
  })

  it('an item the node does not have is said to be gone', async () => {
    rpc.handlers['todo.get'] = () => {
      throw rpcError('not_found')
    }
    hydrated()
    render(<TodoReviewPane node="nnnnnnnn" todo="nope" />)
    expect(await screen.findByTestId('review-gone')).toBeInTheDocument()
  })

  it('an unknown node and a node without to-dos are explained, with no actions', async () => {
    hydrated()
    const first = render(<TodoReviewPane node="zzzzzzzz" todo="t1" />)
    expect(screen.getByTestId('review-unknown-node')).toBeInTheDocument()
    first.unmount()
    useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', info: { capabilities: { todo: false } } as never }) } })
    render(<TodoReviewPane node="nnnnnnnn" todo="t1" />)
    await waitFor(() => expect(screen.getByTestId('review-unsupported-node')).toBeInTheDocument())
    expect(screen.queryByTestId('review-approve')).toBeNull()
  })
})

describe('only “not found” means gone', () => {
  it('a link failure or timeout says the node could not be reached and offers to try again; trying again loads the item', async () => {
    const user = userEvent.setup()
    let n = 0
    rpc.handlers['todo.get'] = () => {
      if (n++ === 0) throw linkError('timeout')
      return todoItem({ id: 't1', title: 'Approved earlier', state: 'done' })
    }
    hydrated()
    render(<TodoReviewPane node="nnnnnnnn" todo="t1" />)
    expect(await screen.findByTestId('review-unreachable')).toBeInTheDocument()
    expect(screen.queryByTestId('review-gone')).toBeNull()
    await user.click(screen.getByTestId('review-retry'))
    expect(await screen.findByTestId('review-title')).toHaveTextContent('Approved earlier')
    expect(rpc.calls.filter((c) => c.method === 'todo.get')).toHaveLength(2)
  })

  it('another error from the node is “could not load” with a retry, not “gone”', async () => {
    rpc.handlers['todo.get'] = () => {
      throw rpcError('internal')
    }
    hydrated()
    render(<TodoReviewPane node="nnnnnnnn" todo="t1" />)
    expect(await screen.findByTestId('review-load-failed')).toBeInTheDocument()
    expect(screen.getByTestId('review-retry')).toBeInTheDocument()
    expect(screen.queryByTestId('review-gone')).toBeNull()
  })

  it('offline with the item not cached says so instead of spinning forever, and asks nothing', async () => {
    useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'peer_offline', info: { capabilities: { todo: true } } as never }) } })
    hydrated()
    render(<TodoReviewPane node="nnnnnnnn" todo="t1" />)
    expect(await screen.findByTestId('review-offline')).toBeInTheDocument()
    expect(screen.queryByTestId('review-loading')).toBeNull()
    expect(rpc.calls.some((c) => c.method === 'todo.get')).toBe(false)
  })
})
