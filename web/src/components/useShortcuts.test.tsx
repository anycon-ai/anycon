// ⌘⇧A / ⌘⇧D（允许 / 拒绝最近一条待审批）收到 `not_found` 时的提示，要和卡片上的按钮一致：
// 带 `data.reason = project_dir_changed` 才说「目录变了」，别的 not_found 只说「请求已不存在」，都不算失败、不重试。
import { fireEvent, render } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ev, mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))

import { useShortcuts } from './useShortcuts'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

function Harness() {
  useShortcuts()
  return <div data-request="r1" />
}

function seed(): HTMLElement {
  const detail = { tool: { tool: 'Bash', category: 'shell' as const, input: { command: 'ls' }, detail: { command: 'ls' } } }
  const options = [
    { id: 'allow', label: 'Allow', kind: 'allow_once' as const },
    { id: 'deny', label: 'Deny', kind: 'reject_once' as const },
  ]
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, [ev({ type: 'permission', request_id: 'r1', kind: 'tool', title: 'Run', detail, options })])
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
  useSessions.getState().addPending(NODE, { session_id: SID, request_id: 'r1', kind: 'tool', title: 'Run', detail, options, created_at_ms: 1 })
  const { container } = render(<Harness />)
  const card = container.querySelector('[data-request="r1"]') as HTMLElement
  // jsdom 没有布局：让这张卡片「在视野里」，快捷键才会直接答复而不是先滚动到它
  card.getBoundingClientRect = () => ({ top: 10, bottom: 50, left: 0, right: 100, width: 100, height: 40, x: 0, y: 10, toJSON: () => ({}) })
  return card
}

const press = (key: 'a' | 'd'): void => {
  fireEvent.keyDown(document.body, { key, ctrlKey: true, shiftKey: true })
}
const waitToasts = async (): Promise<void> => {
  await vi.waitFor(() => expect(useUi.getState().toasts.length).toBeGreaterThan(0))
}

beforeEach(() => {
  resetSeq()
  rpc = new FakeRpc()
  setRpc(rpc)
  useUi.setState({ toasts: [] })
})
afterEach(() => setRpc(null))

describe('useShortcuts: ⌘⇧A / ⌘⇧D answering a permission', () => {
  it('data.reason = project_dir_changed: an info toast that the request was denied because the folder changed, not an error; the request leaves the pending list', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('not_found', undefined, 'project_dir_changed')
    }
    seed()
    press('a')
    await waitToasts()
    expect(rpc.calls.filter((c) => c.method === 'permission.respond')).toHaveLength(1)
    expect(rpc.calls[0]!.params).toMatchObject({ request_id: 'r1', option_id: 'allow' })
    expect(useUi.getState().toasts).toEqual([expect.objectContaining({ kind: 'info', text: 'This request was denied because the project folder changed, and the turn was stopped.' })])
    expect(useSessions.getState().pending).toEqual({})
  })

  it('any other not_found: an info toast that the request no longer exists — nothing about the folder', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('not_found')
    }
    seed()
    press('d')
    await waitToasts()
    expect(rpc.calls[0]!.params).toMatchObject({ request_id: 'r1', option_id: 'deny' })
    const toasts = useUi.getState().toasts
    expect(toasts).toEqual([expect.objectContaining({ kind: 'info', text: 'This request no longer exists, so it can’t be answered.' })])
    expect(toasts[0]!.text).not.toMatch(/folder|stopped/i)
    expect(useSessions.getState().pending).toEqual({})
  })

  it('any other failure is still an error toast', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('internal')
    }
    seed()
    press('a')
    await waitToasts()
    expect(useUi.getState().toasts.map((t) => t.kind)).toEqual(['error'])
  })
})
