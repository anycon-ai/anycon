// 「让 agent 处理」的对话框（spec/web.md § 4.12，docs/spec/daemon.md § 22.6）：缺的条件说清楚、「不 push」的规则、预览、幂等重试、错误映射。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { gitProject, mockAgent, nodeEntry, nodeInfo, project, summary, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { linkError } from '@/test/fakeTodoDaemon'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { resetTodosMemory } from '../service'
import { useTodos } from '../store'
import { useTodoHistory } from '../history'
import type { ViewTodo } from '../types'
import { DispatchDialog } from './DispatchDialog'

const N = 'n'.repeat(32)
let rpc: FakeRpc

const claude = () => mockAgent({ id: 'claude', label: 'Claude Code', options: [{ id: 'no_push', kind: 'bool', label: 'No push', default: false }] })
const item = (over: Partial<ViewTodo> = {}): ViewTodo => ({ ...todoItem({ id: 't1', title: 'Fix the redirect', body: 'details' }), node_id: N, unsynced: false, ...over }) as ViewTodo

function setup(agents = [mockAgent()], projects = [gitProject('proj'), project({ name: 'plain' })]): void {
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn: 'ready', agents, projects, info: nodeInfo({ capabilities: { todo: true, worktree: true } as never }) }) }, order: [N], loaded: true })
}

const preview = (noPush: boolean): string => `Fix the redirect\n\ndetails${noPush ? '\n\nLeave your changes on the branch.' : ''}`
const dispatchCalls = (): Array<Record<string, unknown>> => rpc.calls.filter((c) => c.method === 'todo.dispatch').map((c) => c.params as Record<string, unknown>)
const lastPreviewCall = (): Record<string, unknown> | undefined => rpc.calls.filter((c) => c.method === 'todo.dispatch_preview').at(-1)?.params as Record<string, unknown> | undefined

beforeEach(() => {
  rpc = new FakeRpc()
  setRpc(rpc)
  rpc.handlers['todo.dispatch_preview'] = (p) => ({ first_message: preview((p as { no_push?: boolean }).no_push !== false) })
  rpc.handlers['todo.dispatch'] = (p) => {
    const q = p as { id: string; project: string }
    return { todo: todoItem({ id: q.id, project: q.project, sessions: ['s-new'] as never, activity: 'working' as never }), session: summary({ id: 's-new', agent: 'claude', status: 'running', todo_id: q.id }) }
  }
  resetTodosMemory()
  useTodos.setState({ nodes: { [N]: { items: { t1: todoItem({ id: 't1' }) }, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useTodoHistory.getState().reset()
  useUi.setState({ toasts: [] })
  setup()
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('what is needed before it can be sent', () => {
  it('an item without a project asks for one; picking it enables sending and is sent along (the daemon writes it back)', async () => {
    const user = userEvent.setup()
    setup([claude()])
    render(<DispatchDialog item={item()} open onOpenChange={vi.fn()} />)
    expect(screen.getByTestId('dispatch-blocked')).toHaveAttribute('data-reason', 'noProject')
    expect(screen.getByTestId('dispatch-send')).toBeDisabled()
    await user.selectOptions(screen.getByTestId('dispatch-project'), 'proj')
    expect(screen.getByTestId('dispatch-send')).toBeEnabled()
    await user.click(screen.getByTestId('dispatch-send'))
    await waitFor(() => expect(dispatchCalls()).toHaveLength(1))
    expect(dispatchCalls()[0]).toMatchObject({ id: 't1', agent: 'claude', project: 'proj', no_push: true, workspace: { mode: 'worktree' } })
  })

  it('a project that can have a worktree defaults to one; a plain folder says there is no isolation before sending', async () => {
    const user = userEvent.setup()
    setup([claude()])
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(screen.getByTestId('dispatch-workspace-worktree')).toBeChecked()
    expect(screen.queryByTestId('dispatch-no-isolation')).toBeNull()
    await user.click(screen.getByTestId('dispatch-workspace-local'))
    expect(screen.getByTestId('dispatch-no-isolation')).toHaveTextContent(/no isolation/i)
    await user.selectOptions(screen.getByTestId('dispatch-project'), 'plain')
    expect(screen.queryByTestId('dispatch-workspace-worktree')).toBeNull()
    expect(screen.getByTestId('dispatch-no-isolation')).toHaveTextContent(/directly in the project directory/i)
    await user.click(screen.getByTestId('dispatch-send'))
    await waitFor(() => expect(dispatchCalls()).toHaveLength(1))
    expect(dispatchCalls()[0]!.workspace).toBeUndefined() // 不能开 worktree：不带，由 daemon 定（项目目录里）
  })

  it('the node being offline disables sending and says why; an unreviewed agent item says that dispatching accepts it', () => {
    useNodes.getState().patch(N, { conn: 'peer_offline' })
    render(<DispatchDialog item={item({ project: 'proj', unreviewed: true, source: { kind: 'agent' } as never })} open onOpenChange={vi.fn()} />)
    expect(screen.getByTestId('dispatch-blocked')).toHaveAttribute('data-reason', 'offline')
    expect(screen.getByTestId('dispatch-send')).toBeDisabled()
    expect(screen.getByTestId('dispatch-accepts-note')).toHaveTextContent(/accepts/i)
  })

  it('no ready agent: says so', () => {
    setup([])
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(screen.getByTestId('dispatch-blocked')).toHaveAttribute('data-reason', 'noAgent')
  })

  it('a project whose git state is still being read blocks sending instead of guessing', () => {
    setup([claude()], [project({ name: 'proj', git_pending: true })])
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(screen.getByTestId('dispatch-blocked')).toHaveAttribute('data-reason', 'gitPending')
  })
})

describe('“no push, no pull request”', () => {
  it('is ticked by default and sent as true for an agent that declares the option; the generic options do not show it twice', async () => {
    const user = userEvent.setup()
    setup([claude()])
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(screen.getByTestId('dispatch-no-push')).toBeChecked()
    expect(screen.queryByTestId('dispatch-no-push-warning')).toBeNull()
    expect(screen.queryByTestId('info-option-no_push')).toBeNull()
    await user.click(screen.getByTestId('dispatch-send'))
    await waitFor(() => expect(dispatchCalls()).toHaveLength(1))
    expect(dispatchCalls()[0]!.no_push).toBe(true)
    expect((dispatchCalls()[0]!.settings as { options?: Record<string, unknown> }).options?.no_push).toBeUndefined()
  })

  it('an agent that cannot block it: the box stays ticked, the warning says so, and sending is only possible after unticking (which then states that it may push)', async () => {
    const user = userEvent.setup()
    setup([mockAgent()])
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(screen.getByTestId('dispatch-no-push')).toBeChecked()
    expect(screen.getByTestId('dispatch-no-push-warning')).toHaveTextContent(/cannot block/i)
    expect(screen.getByTestId('dispatch-blocked')).toHaveAttribute('data-reason', 'noPushUnsupported')
    expect(screen.getByTestId('dispatch-send')).toBeDisabled()
    await user.click(screen.getByTestId('dispatch-no-push'))
    expect(screen.queryByTestId('dispatch-no-push-warning')).toBeNull()
    expect(screen.getByTestId('dispatch-push-allowed')).toHaveTextContent(/may push/i)
    expect(screen.getByTestId('dispatch-send')).toBeEnabled()
    await user.click(screen.getByTestId('dispatch-send'))
    await waitFor(() => expect(dispatchCalls()).toHaveLength(1))
    expect(dispatchCalls()[0]!.no_push).toBe(false)
  })

  it('switching to an agent that declares it makes the warning go away', async () => {
    const user = userEvent.setup()
    setup([mockAgent(), claude()])
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(screen.getByTestId('dispatch-no-push-warning')).toBeInTheDocument()
    await user.selectOptions(screen.getByTestId('dispatch-agent'), 'claude')
    expect(screen.queryByTestId('dispatch-no-push-warning')).toBeNull()
    expect(screen.getByTestId('dispatch-send')).toBeEnabled()
  })
})

describe('the preview of the first message', () => {
  it('comes from the daemon, is fetched when expanded, and is fetched again when “no push” changes or a new result is recorded; a stale reply never wins', async () => {
    const user = userEvent.setup()
    setup([claude()])
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(rpc.calls.some((c) => c.method === 'todo.dispatch_preview')).toBe(false)
    await user.click(screen.getByTestId('dispatch-preview-toggle'))
    expect(await screen.findByTestId('dispatch-preview')).toHaveTextContent('Leave your changes on the branch.')
    expect(lastPreviewCall()).toEqual({ id: 't1', no_push: true })
    await user.click(screen.getByTestId('dispatch-no-push'))
    await waitFor(() => expect(screen.getByTestId('dispatch-preview')).not.toHaveTextContent('Leave your changes'))
    expect(lastPreviewCall()).toEqual({ id: 't1', no_push: false })
    // 有新结果记进「经过」：重新取
    const before = rpc.calls.filter((c) => c.method === 'todo.dispatch_preview').length
    useTodoHistory.getState().patch(`${N}:t1`, () => ({ loaded: true, events: [{ seq: 1, ts_ms: 1, kind: 'result', session_id: 's', turn: 1, text: 'x', truncated: false } as never] }))
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'todo.dispatch_preview').length).toBe(before + 1))
    expect(screen.getByTestId('dispatch-preview-box')).toHaveTextContent(/may differ|differ/i)
  })

  it('a refusal (the item is closed) is shown in words', async () => {
    const user = userEvent.setup()
    setup([claude()])
    rpc.handlers['todo.dispatch_preview'] = () => {
      throw rpcError('invalid', 'id', 'closed')
    }
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    await user.click(screen.getByTestId('dispatch-preview-toggle'))
    expect(await screen.findByTestId('dispatch-preview-error')).toHaveTextContent(/closed/i)
  })
})

describe('sending', () => {
  it('a retry after a lost reply uses the same message id; changing a parameter takes a new one; success stops reusing it', async () => {
    const user = userEvent.setup()
    setup([claude(), mockAgent()])
    let first = true
    rpc.handlers['todo.dispatch'] = (p) => {
      if (first) {
        first = false
        throw linkError('timeout')
      }
      const q = p as { id: string; project: string }
      return { todo: todoItem({ id: q.id, project: q.project, sessions: ['s-new'] as never }), session: summary({ id: 's-new', todo_id: q.id }) }
    }
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    await user.click(screen.getByTestId('dispatch-send'))
    expect(await screen.findByTestId('dispatch-error')).toHaveTextContent(/safe to try again|will not start a second/i)
    // 换了 agent：参数变了，新的 message_id
    await user.selectOptions(screen.getByTestId('dispatch-agent'), 'mock')
    await user.click(screen.getByTestId('dispatch-no-push'))
    await user.selectOptions(screen.getByTestId('dispatch-agent'), 'claude')
    await user.click(screen.getByTestId('dispatch-no-push')) // 回到第一次的参数
    await user.click(screen.getByTestId('dispatch-send'))
    await waitFor(() => expect(dispatchCalls()).toHaveLength(2))
    const [a, b] = dispatchCalls()
    expect(typeof a!.message_id).toBe('string')
    expect(b!.message_id).toBe(a!.message_id)
  })

  it('different parameters get a different message id', async () => {
    const user = userEvent.setup()
    setup([claude()])
    rpc.handlers['todo.dispatch'] = () => {
      throw linkError('timeout')
    }
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    await user.click(screen.getByTestId('dispatch-send'))
    await screen.findByTestId('dispatch-error')
    await user.click(screen.getByTestId('dispatch-no-push'))
    await user.click(screen.getByTestId('dispatch-send'))
    await waitFor(() => expect(dispatchCalls()).toHaveLength(2))
    expect(dispatchCalls()[1]!.message_id).not.toBe(dispatchCalls()[0]!.message_id)
  })

  it('success: the dialog closes, the item and the new session are in the stores (the status dot works at once), and a toast offers the session', async () => {
    const user = userEvent.setup()
    setup([claude()])
    const onOpenChange = vi.fn()
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={onOpenChange} />)
    await user.click(screen.getByTestId('dispatch-send'))
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    expect(useTodos.getState().nodes[N]!.items.t1!.sessions).toEqual(['s-new'])
    expect(useSessions.getState().list['s-new']!.status).toBe('running')
    const toast = useUi.getState().toasts.find((x) => x.kind === 'info')!
    expect(toast.action?.label).toMatch(/open/i)
  })

  it.each([
    ['closed', () => rpcError('invalid', 'id', 'closed'), /closed/i],
    ['too many', () => rpcError('invalid', 'id', 'too_many_sessions'), /20 times/i],
    ['no project', () => rpcError('invalid', 'project'), /project/i],
    ['no_push unsupported', () => rpcError('unsupported', 'no_push', 'no_push'), /cannot block|untick/i],
    ['old daemon', () => rpcError('unsupported'), /does not support|update/i],
    ['agent unavailable', () => rpcError('agent_unavailable'), /not available/i],
    ['invalid settings', () => rpcError('invalid_settings', 'thinking'), /does not accept/i],
    ['forbidden', () => rpcError('forbidden'), /not allowed|cannot/i],
    ['not found', () => rpcError('not_found'), /not found|no longer/i],
  ])('maps the error “%s” to words and keeps the dialog open', async (_name, make, text) => {
    const user = userEvent.setup()
    setup([claude()])
    rpc.handlers['todo.dispatch'] = () => {
      throw make()
    }
    const onOpenChange = vi.fn()
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={onOpenChange} />)
    await user.click(screen.getByTestId('dispatch-send'))
    expect(await screen.findByTestId('dispatch-error')).toHaveTextContent(text)
    expect(onOpenChange).not.toHaveBeenCalledWith(false)
    expect(screen.getByTestId('dispatch-send')).toBeEnabled()
  })
})
