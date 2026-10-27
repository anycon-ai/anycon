// 会话页上的 `task` / `notice` / `activity` / 上下文占用（spec/web.md § 4.6）：
// 任务进度画在启动它的工具行下面；没有在跑的会话里停在「运行中」的快照显示成「已停止」，也不算后台任务；
// 临时状态只在会话还在跑时显示；信息面板有上下文占用。
import { act, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Event, SessionSummary } from '@/protocol'
import i18n from '@/i18n'
import { ev, mockAgent, nodeEntry, resetSeq, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts } from '@/stores/drafts'
import { useGit } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'

const workflowCall: Event = { type: 'tool_call', id: 'tool-1', name: 'Workflow', category: 'subagent', title: 'Workflow: review', input: { workflow: 'review' } }
const workflowTask = (status: 'running' | 'completed' = 'running'): Event => ({
  type: 'task',
  id: 'task-1',
  kind: 'workflow',
  tool_id: 'tool-1',
  title: 'review',
  status,
  usage: { total_tokens: 900, tool_calls: 7 },
  phases: [{ index: 0, title: 'Scan' }],
  agents: [{ index: 0, label: 'scanner', state: status === 'running' ? 'running' : 'done', phase_index: 0 }],
})

function seed(events: Event[], over: Partial<SessionSummary> = {}): void {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, [ev({ type: 'user_message', message_id: 'm1', content: 'go', by_device: 'me' }), ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }), ...events.map((e) => ev(e))])
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: false })
}

beforeEach(async () => {
  resetSeq()
  navigate.mockReset()
  await i18n.changeLanguage('en')
  setRpc(new FakeRpc())
  useUi.setState({ toasts: [] })
  useGit.setState({ bySession: {} })
  usePrefs.setState({ infoPanelOpen: false })
  resetDrafts()
})
afterEach(() => setRpc(null))

describe('SessionPage: tasks', () => {
  it('draws the workflow under its tool line and lists it in the background tasks chip while the session runs', async () => {
    const user = userEvent.setup()
    seed([workflowCall, { type: 'tool_result', id: 'tool-1', output: 'started', is_error: false, detail: { background: true } }, workflowTask()], { status: 'running' })
    render(<SessionPage sessionId={SID} />)
    const task = await screen.findByTestId('task')
    expect(task).toHaveAttribute('data-status', 'running')
    expect(within(task).getByTestId('task-title')).toHaveTextContent('review')
    expect(within(task).getByTestId('task-agent')).toHaveTextContent('scanner')

    const chip = screen.getByTestId('background-tasks-chip')
    expect(chip).toHaveTextContent('1 background task running')
    await user.click(chip)
    const rows = within(await screen.findByTestId('background-tasks-list')).getAllByTestId('background-task')
    expect(rows).toHaveLength(1)
    expect(rows[0]).toHaveTextContent('review')
    expect(rows[0]).toHaveTextContent('900 tokens · 7 tool calls')
  })

  it('a snapshot left on running in an idle session with no background tasks shows as stopped and is not counted', async () => {
    seed([workflowCall, workflowTask(), { type: 'turn_done', turn_id: 1, reason: 'completed' }], { status: 'idle', background_tasks: 0 })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('task')).toHaveAttribute('data-status', 'stopped')
    expect(screen.getByTestId('task-status')).toHaveTextContent('Stopped')
    expect(screen.queryByTestId('background-tasks-chip')).toBeNull()
  })

  it('the same snapshot counts as running while the daemon still reports background tasks', async () => {
    seed([workflowCall, workflowTask(), { type: 'turn_done', turn_id: 1, reason: 'completed' }], { status: 'idle', background_tasks: 1 })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('task')).toHaveAttribute('data-status', 'running')
    expect(screen.getByTestId('background-tasks-chip')).toHaveTextContent('1 background task running')
  })

  it('a finished workflow is not a running background task', async () => {
    seed([workflowCall, workflowTask('completed'), { type: 'turn_done', turn_id: 1, reason: 'completed' }], { status: 'running' })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('task')).toHaveAttribute('data-status', 'completed')
    expect(screen.queryByTestId('background-tasks-chip')).toBeNull()
  })

  it('updates in place as new snapshots arrive', async () => {
    seed([workflowCall, workflowTask()], { status: 'running' })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('task')).toHaveAttribute('data-status', 'running')
    act(() => {
      useSessions.getState().ingest(SID, [ev(workflowTask('completed'))])
    })
    expect(screen.getAllByTestId('task')).toHaveLength(1)
    expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'completed')
    expect(screen.queryByTestId('background-tasks-chip')).toBeNull()
  })
})

describe('SessionPage: activity and notices', () => {
  const retrying: Event = { type: 'activity', state: 'retrying', attempt: 2, max_attempts: 10, delay_ms: 5000, reason: '529 overloaded_error' }

  it('shows the activity at the bottom while the session runs, and clears it when text arrives', async () => {
    seed([retrying], { status: 'running' })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('activity')).toHaveTextContent('Retrying (2/10)')
    act(() => {
      useSessions.getState().ingest(SID, [ev({ type: 'text_delta', text: 'hello' })])
    })
    expect(screen.queryByTestId('activity')).toBeNull()
  })

  it.each<SessionSummary['status']>(['idle', 'queued', 'ended', 'error', 'interrupted'])('does not show a stale activity when the session is %s', async (status) => {
    seed([retrying], { status })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-status')
    expect(screen.queryByTestId('activity')).toBeNull()
  })

  it('shows notices in the stream with the agent’s model names', async () => {
    seed([{ type: 'notice', code: 'model_fallback', level: 'warning', message: 'overloaded', model_from: 'mock-large', model_to: 'mock-small' }], { status: 'idle' })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('notice')).toHaveTextContent('Model switched from Mock Large to Mock Small')
  })
})

describe('SessionPage: context usage', () => {
  it('shows the latest context usage in the info panel', async () => {
    usePrefs.setState({ infoPanelOpen: true })
    seed([{ type: 'turn_done', turn_id: 1, reason: 'completed', usage: { duration_ms: 1, context_window: 200_000, context_tokens: 12_000 } }], { status: 'idle' })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('info-context-value')).toHaveTextContent('6% · 12k / 200k')
  })
})
