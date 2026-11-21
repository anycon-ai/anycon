// 派发对话框里的规则（docs/spec/daemon.md § 23.5，spec/web.md § 4.12）：会注入的规则文字（纯文字）和条数，换项目重新取（带 `project`），
// 所选 agent 收不到规则时直说，节点不支持规则 / 项目还没定 / 节点离线时不显示、不请求。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { gitProject, mockAgent, nodeEntry, nodeInfo, project, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
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
const withPrompt = () => mockAgent({ id: 'claude', label: 'Claude Code', capabilities: { resume: true, interrupt: true, interject: false, thinking_stream: true, usage: true, structured_output: false, env: true, rewind: false, system_prompt: true, attachments: [] } as never, options: [{ id: 'no_push', kind: 'bool', label: 'No push', default: false }] })
const withoutPrompt = () => mockAgent({ id: 'acp', label: 'Some ACP agent', capabilities: { resume: true, interrupt: true, interject: false, thinking_stream: true, usage: true, structured_output: false, env: false, rewind: false, system_prompt: false, attachments: [] } as never, options: [] })
const item = (over: Partial<ViewTodo> = {}): ViewTodo => ({ ...todoItem({ id: 't1', title: 'Fix the redirect', body: 'details' }), node_id: N, unsynced: false, ...over }) as ViewTodo
const previewCalls = (): Array<Record<string, unknown>> => rpc.calls.filter((c) => c.method === 'todo.dispatch_preview').map((c) => c.params as Record<string, unknown>)

const RULES = 'Standing rules from the user. The user wrote or confirmed each of them; follow them in this work:\n- Write the failing test first.\n- Ask before adding a dependency.\n  Explain <b>why</b>.'

function setup(agents = [withPrompt()], rules = true, conn: 'ready' | 'peer_offline' = 'ready'): void {
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'box', conn, agents, projects: [gitProject('proj'), project({ name: 'other' })], info: nodeInfo({ capabilities: { todo: true, worktree: true, rules } as never }) }) }, order: [N], loaded: true })
}

beforeEach(() => {
  rpc = new FakeRpc()
  setRpc(rpc)
  rpc.handlers['todo.dispatch_preview'] = (p) => ({ first_message: 'Fix the redirect', rules_text: (p as { project?: string }).project === 'other' ? 'Standing rules from the user. The user wrote or confirmed each of them; follow them in this work:\n- Only for the other project.' : RULES })
  resetTodosMemory()
  useTodos.setState({ nodes: { [N]: { items: { t1: todoItem({ id: 't1' }) }, hydrated: true, synced: true, syncing: false, closed: { expanded: false, loading: false, complete: false, hasMore: false, failed: false } } } })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useTodoHistory.getState().reset()
  useUi.setState({ toasts: [] })
  setup()
})
afterEach(() => setRpc(null))

describe('rules in the dispatch dialog', () => {
  it('shows the rules that will be added to the system prompt, as plain text, with the count', async () => {
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    const text = await screen.findByTestId('dispatch-rules-text')
    expect(text.textContent).toBe(RULES)
    expect(text.querySelector('b')).toBeNull()
    expect(screen.getByTestId('dispatch-rules-count')).toHaveTextContent('2')
    expect(previewCalls()[0]).toMatchObject({ id: 't1', project: 'proj' })
  })

  it('asks again with the new project when the project is changed in the dialog', async () => {
    const user = userEvent.setup()
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    await screen.findByTestId('dispatch-rules-text')
    await user.selectOptions(screen.getByTestId('dispatch-project'), 'other')
    await waitFor(() => expect(screen.getByTestId('dispatch-rules-text')).toHaveTextContent('Only for the other project.'))
    expect(previewCalls().at(-1)).toMatchObject({ project: 'other' })
    expect(screen.getByTestId('dispatch-rules-count')).toHaveTextContent('1')
  })

  it('an item with no project shows nothing until one is picked, then asks for that project', async () => {
    const user = userEvent.setup()
    render(<DispatchDialog item={item()} open onOpenChange={vi.fn()} />)
    expect(screen.queryByTestId('dispatch-rules')).toBeNull()
    expect(previewCalls()).toEqual([])
    await user.selectOptions(screen.getByTestId('dispatch-project'), 'other')
    await screen.findByTestId('dispatch-rules-text')
    expect(previewCalls()[0]).toMatchObject({ project: 'other' })
  })

  it('says there are no active rules when the node has none', async () => {
    rpc.handlers['todo.dispatch_preview'] = () => ({ first_message: 'x', rules_text: '' })
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(await screen.findByTestId('dispatch-rules-none')).toBeInTheDocument()
    expect(screen.queryByTestId('dispatch-rules-text')).toBeNull()
  })

  it('for an agent that cannot take a system prompt it says the agent does not receive rules, and does not show them as if it would', async () => {
    setup([withoutPrompt()])
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    const note = await screen.findByTestId('dispatch-rules-unsupported')
    expect(note).toHaveTextContent('Some ACP agent does not receive rules')
    expect(screen.queryByTestId('dispatch-rules-text')).toBeNull()
  })

  it('shows a failure of the preview instead of an empty box', async () => {
    rpc.handlers['todo.dispatch_preview'] = () => {
      throw new Error('boom')
    }
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    expect(await screen.findByTestId('dispatch-rules-error')).toBeInTheDocument()
  })

  it('is not there on a node without rules support, and nothing is requested', async () => {
    setup([withPrompt()], false)
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20))
    })
    expect(screen.queryByTestId('dispatch-rules')).toBeNull()
    expect(previewCalls()).toEqual([])
  })

  it('offline: no request is made', async () => {
    setup([withPrompt()], true, 'peer_offline')
    render(<DispatchDialog item={item({ project: 'proj' })} open onOpenChange={vi.fn()} />)
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20))
    })
    expect(previewCalls()).toEqual([])
  })
})
