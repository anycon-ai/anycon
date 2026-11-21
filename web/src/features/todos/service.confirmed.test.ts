// 直接发给节点的请求（派发、`todo.get`）的回执是请求处理那一刻的快照：它发出之后收到过这一项的 `todo.changed`，
// 回执不能把通知带来的 `activity` 盖回旧的；没有通知时，只动 `activity` 的回执照样生效（`updated_ms` 不变也一样）。
// 也检查退出登录 / 忘记节点时「经过」和「再改改」的草稿一起清掉。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, summary, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { dispatchTodo } from './dispatch'
import { historyKey, useTodoHistory } from './history'
import { useReworkDrafts } from './reworkDraft'
import { applyConfirmedTodo, forgetNodeTodos, handleTodoChanged, resetTodosMemory, todoTickNow } from './service'
import { useTodos } from './store'

const N = 'n'.repeat(32)
const info = { capabilities: { todo: true } } as unknown as NodeInfo
let rpc: FakeRpc

const cached = () => useTodos.getState().nodes[N]?.items.t1
const args = { id: 't1', agent: 'mock', project: 'proj', settings: {}, noPush: true, messageId: 'm1' }

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  setRpc(rpc)
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, info, conn: 'ready' }) }, order: [N], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  resetTodosMemory()
  useTodos.getState().mergeItems(N, [todoItem({ id: 't1', updated_ms: 5_000 })])
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('a reply older than a todo.changed that arrived while it was in flight', () => {
  it('dispatch: the notification’s activity (review) is not overwritten by the reply’s (working), although updated_ms is the same', async () => {
    rpc.handlers['todo.dispatch'] = () => {
      // 守护进程释放锁之后、回执到达之前：这一轮已经结束，通知先到
      handleTodoChanged(N, { id: 't1', todo: todoItem({ id: 't1', updated_ms: 6_000, activity: 'review' as never, sessions: ['s1'] as never }) })
      return { todo: todoItem({ id: 't1', updated_ms: 6_000, activity: 'working' as never, sessions: ['s1'] as never }), session: summary({ id: 's1', todo_id: 't1' }) }
    }
    await dispatchTodo(N, args)
    expect(cached()!.activity).toBe('review')
    // 会话还是并进了列表
    expect(useSessions.getState().list.s1).toBeDefined()
  })

  it('a reply that is strictly newer than the cache (a later content change) still applies', async () => {
    rpc.handlers['todo.dispatch'] = () => {
      handleTodoChanged(N, { id: 't1', todo: todoItem({ id: 't1', updated_ms: 6_000, activity: 'review' as never }) })
      return { todo: todoItem({ id: 't1', title: 'renamed', updated_ms: 7_000, activity: 'working' as never }), session: summary({ id: 's1', todo_id: 't1' }) }
    }
    await dispatchTodo(N, args)
    expect(cached()!.title).toBe('renamed')
  })

  it('no notification in between: an activity-only reply (same updated_ms) is applied', async () => {
    useTodos.getState().mergeItems(N, [todoItem({ id: 't1', updated_ms: 6_000 })])
    rpc.handlers['todo.dispatch'] = () => ({ todo: todoItem({ id: 't1', updated_ms: 6_000, activity: 'working' as never }), session: summary({ id: 's1', todo_id: 't1' }) })
    await dispatchTodo(N, args)
    expect(cached()!.activity).toBe('working')
  })

  it('a notification for another item, or one that arrived before the request was sent, does not hold the reply back', async () => {
    handleTodoChanged(N, { id: 't1', todo: todoItem({ id: 't1', updated_ms: 6_000 }) })
    rpc.handlers['todo.dispatch'] = () => {
      handleTodoChanged(N, { id: 't2', todo: todoItem({ id: 't2', updated_ms: 6_000 }) })
      return { todo: todoItem({ id: 't1', updated_ms: 6_000, activity: 'working' as never }), session: summary({ id: 's1', todo_id: 't1' }) }
    }
    await dispatchTodo(N, args)
    expect(cached()!.activity).toBe('working')
  })

  it('applyConfirmedTodo directly (what todo.get uses): the tick taken before sending decides', async () => {
    const sentAt = todoTickNow()
    handleTodoChanged(N, { id: 't1', todo: todoItem({ id: 't1', updated_ms: 6_000, activity: 'review' as never }) })
    await applyConfirmedTodo(N, todoItem({ id: 't1', updated_ms: 6_000, activity: 'working' as never }), sentAt)
    expect(cached()!.activity).toBe('review')
    // 没带序号（旧的调用方式）：和以前一样按 updated_ms 比，相等就换
    await applyConfirmedTodo(N, todoItem({ id: 't1', updated_ms: 6_000, activity: 'working' as never }))
    expect(cached()!.activity).toBe('working')
  })
})

describe('what is cleared on sign-out and when a node is forgotten', () => {
  const seed = (): void => {
    useTodoHistory.getState().patch(historyKey(N, 't1'), () => ({ loaded: true, events: [{ seq: 1, ts_ms: 1, kind: 'result', session_id: 's', turn: 1, text: 'secret result', truncated: false } as never] }))
    useTodoHistory.getState().patch(historyKey('o'.repeat(32), 't9'), () => ({ loaded: true, events: [] }))
    useReworkDrafts.getState().set(N, 't1', 'half a thought')
    useReworkDrafts.getState().set('o'.repeat(32), 't9', 'other node')
  }

  it('forgetting a node drops that node’s history and rework drafts and nobody else’s', async () => {
    seed()
    await forgetNodeTodos(N)
    expect(useTodoHistory.getState().byKey[historyKey(N, 't1')]).toBeUndefined()
    expect(useTodoHistory.getState().byKey[historyKey('o'.repeat(32), 't9')]).toBeDefined()
    expect(useReworkDrafts.getState().byKey).toEqual({ [`${'o'.repeat(32)}:t9`]: 'other node' })
  })

  it('signing out (the in-memory reset) drops all history and all rework drafts', () => {
    seed()
    resetTodosMemory()
    expect(useTodoHistory.getState().byKey).toEqual({})
    expect(useReworkDrafts.getState().byKey).toEqual({})
  })
})
