// 后台任务的界面（spec/web.md § 4.6 的 `task`）：workflow 按阶段分组、每个 agent 一行；没有进展的快照显示成「已停止」；
// 输入框上方的 chip 和弹层。
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it } from 'vitest'
import i18n from '@/i18n'
import type { Event } from '@/protocol'
import { evs, resetSeq } from '@/test/factories'
import { allTasks } from '../tasks'
import { buildView, type TaskSnapshot } from '../reducer'
import { BackgroundTasksChip, TaskLiveContext, TaskPanel } from './Tasks'

beforeEach(async () => {
  resetSeq()
  await i18n.changeLanguage('en')
})

type TaskEvent = Extract<Event, { type: 'task' }>
const workflow = (over: Partial<TaskEvent> = {}): TaskEvent => ({
  type: 'task',
  id: 'task-1',
  kind: 'workflow',
  title: 'review',
  status: 'running',
  usage: { total_tokens: 900, tool_calls: 7, duration_ms: 4000 },
  last_tool: 'Grep',
  summary: 'Reviewing the findings',
  phases: [
    { index: 0, title: 'Scan' },
    { index: 1, title: 'Review' },
  ],
  agents: [
    { index: 0, label: 'scanner', state: 'done', phase_index: 0, tokens: 480, tool_calls: 4, duration_ms: 3000, last_tool: 'Read' },
    { index: 1, label: 'reviewer-a', state: 'running', phase_index: 1, tokens: 300, tool_calls: 2, last_tool: 'Grep' },
    { index: 2, label: 'reviewer-b', state: 'queued', phase_index: 1, attempt: 2 },
  ],
  ...over,
})
const snapshot = (e: TaskEvent): TaskSnapshot => allTasks(buildView(evs([e])).blocks)[0]!
const renderPanel = (e: TaskEvent, live = true) =>
  render(
    <TaskLiveContext.Provider value={live}>
      <TaskPanel task={snapshot(e)} />
    </TaskLiveContext.Provider>,
  )

describe('TaskPanel: workflow', () => {
  it('shows the status, name, usage, last tool and summary in one headline, and the agents grouped by phase', () => {
    renderPanel(workflow())
    const task = screen.getByTestId('task')
    expect(task).toHaveAttribute('data-kind', 'workflow')
    expect(task).toHaveAttribute('data-status', 'running')
    expect(screen.getByTestId('task-status')).toHaveTextContent('Running')
    expect(screen.getByTestId('task-title')).toHaveTextContent('review')
    expect(screen.getByTestId('task-meta')).toHaveTextContent('1/3 agents done · 900 tokens · 7 tool calls · 4s')
    expect(screen.getByTestId('task-last-tool')).toHaveTextContent('Grep')
    expect(screen.getByTestId('task-summary')).toHaveTextContent('Reviewing the findings')

    // 跑着的任务默认展开：阶段标题 + 每个 agent 一行
    const phases = screen.getAllByTestId('task-phase')
    expect(phases.map((p) => within(p).getByRole('heading').textContent)).toEqual(['Scan', 'Review'])
    const scanner = within(phases[0]!).getByTestId('task-agent')
    expect(scanner).toHaveAttribute('data-state', 'done')
    expect(scanner).toHaveTextContent('scanner')
    expect(scanner).toHaveTextContent('480 tokens · 4 tool calls · 3s')
    expect(scanner).toHaveTextContent('Read')
    const [a, b] = within(phases[1]!).getAllByTestId('task-agent')
    expect(a).toHaveAttribute('data-state', 'running')
    expect(a).toHaveTextContent('reviewer-a')
    expect(b).toHaveAttribute('data-state', 'queued')
    // 重试过的标第几次；第一次尝试不标
    expect(within(b!).getByTestId('task-agent-attempt')).toHaveTextContent('attempt 2')
    expect(within(a!).queryByTestId('task-agent-attempt')).toBeNull()
  })

  it('each agent’s state is readable without color: an icon with the state as its name', () => {
    renderPanel(workflow())
    const [scanner, a, b] = screen.getAllByTestId('task-agent')
    expect(within(scanner!).getByRole('img')).toHaveAccessibleName('Done')
    expect(within(a!).getByRole('img')).toHaveAccessibleName('Running')
    expect(within(b!).getByRole('img')).toHaveAccessibleName('Queued')
  })

  it('shows a failed agent’s error and a cached result', () => {
    renderPanel(
      workflow({
        status: 'failed',
        agents: [
          { index: 0, label: 'scanner', state: 'done', phase_index: 0, cached: true },
          { index: 1, label: 'reviewer-a', state: 'failed', phase_index: 1, error: 'Tool Grep failed: exit 2' },
        ],
      }),
    )
    const [cached, failed] = screen.getAllByTestId('task-agent')
    expect(within(cached!).getByTestId('task-agent-cached')).toHaveTextContent('cached')
    expect(failed).toHaveAttribute('data-state', 'failed')
    expect(within(failed!).getByTestId('task-agent-error')).toHaveTextContent('Tool Grep failed: exit 2')
    // 失败的任务默认展开，让人看到原因
    expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'failed')
    expect(screen.getByTestId('task-agents')).toBeInTheDocument()
  })

  it('a finished workflow folds into its headline; clicking it shows the agents, and the choice sticks', async () => {
    const user = userEvent.setup()
    renderPanel(workflow({ status: 'completed', summary: 'Review finished: no issues found' }))
    expect(screen.getByTestId('task-status')).toHaveTextContent('Completed')
    expect(screen.getByTestId('task-summary')).toHaveTextContent('no issues found')
    expect(screen.queryByTestId('task-agents')).toBeNull()
    const head = screen.getByRole('button', { expanded: false })
    await user.click(head)
    expect(screen.getAllByTestId('task-agent')).toHaveLength(3)
    await user.click(screen.getByRole('button', { expanded: true }))
    expect(screen.queryByTestId('task-agents')).toBeNull()
  })

  it('lists agents that have no phase (or one that is not listed) after the phases, without a title', () => {
    renderPanel(workflow({ phases: [{ index: 0, title: 'Scan' }], agents: [{ index: 0, label: 'a', state: 'running', phase_index: 0 }, { index: 1, label: 'b', state: 'running' }, { index: 2, label: 'c', state: 'running', phase_index: 7 }] }))
    const phases = screen.getAllByTestId('task-phase')
    expect(phases).toHaveLength(2)
    expect(within(phases[1]!).queryByRole('heading')).toBeNull()
    expect(within(phases[1]!).getAllByTestId('task-agent').map((x) => x.dataset.index)).toEqual(['1', '2'])
  })

  it('renders without usage, phases or agents', () => {
    renderPanel({ type: 'task', id: 't', kind: 'workflow', title: '', status: 'running' })
    expect(screen.getByTestId('task-title')).toHaveTextContent('Background task')
    expect(screen.queryByTestId('task-meta')).toBeNull()
    expect(screen.queryByTestId('task-agents')).toBeNull()
    expect(screen.queryByRole('button')).toBeNull()
  })
})

describe('TaskPanel: not live', () => {
  it('shows a snapshot stuck on running as stopped, and its unfinished agents with it', () => {
    renderPanel(workflow(), false)
    expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'stopped')
    expect(screen.getByTestId('task-status')).toHaveTextContent('Stopped')
    // 不是「运行中」了，默认收起
    expect(screen.queryByTestId('task-agents')).toBeNull()
  })

  it('unfinished agents are shown as stopped when the task is opened', async () => {
    const user = userEvent.setup()
    renderPanel(workflow(), false)
    await user.click(screen.getByRole('button'))
    expect(screen.getAllByTestId('task-agent').map((a) => a.dataset.state)).toEqual(['done', 'stopped', 'stopped'])
  })

  it('does not change a snapshot that already finished', () => {
    renderPanel(workflow({ status: 'completed' }), false)
    expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'completed')
  })

  it('a cancelled task (the daemon closes the ones left when the agent process goes away) shows as cancelled with its agents stopped', async () => {
    const user = userEvent.setup()
    renderPanel(workflow({ status: 'cancelled' }), true)
    expect(screen.getByTestId('task-status')).toHaveTextContent('Cancelled')
    await user.click(screen.getByRole('button'))
    expect(screen.getAllByTestId('task-agent').map((a) => a.dataset.state)).toEqual(['done', 'stopped', 'stopped'])
  })
})

describe('TaskPanel: restart', () => {
  it('a finished sub-agent that gets new input goes back to running, and a task nobody touched opens again', () => {
    const child = (status: TaskEvent['status']): TaskEvent => ({ type: 'task', id: 'child', kind: 'subagent', title: 'pong', status, agents: [{ index: 0, label: 'pong', state: status === 'running' ? 'running' : 'done' }] })
    const { rerender } = render(<TaskPanel task={snapshot(child('completed'))} />)
    expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'completed')
    expect(screen.queryByTestId('task-agents')).toBeNull()
    rerender(<TaskPanel task={snapshot(child('running'))} />)
    expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'running')
    expect(screen.getByTestId('task-status')).toHaveTextContent('Running')
    expect(screen.getByTestId('task-agent')).toHaveAttribute('data-state', 'running')
    rerender(<TaskPanel task={snapshot(child('completed'))} />)
    expect(screen.getByTestId('task')).toHaveAttribute('data-status', 'completed')
  })
})

describe('TaskPanel: subagent and shell', () => {
  it('is a single headline with status, tokens, last tool and summary, and nothing to expand', () => {
    renderPanel({ type: 'task', id: 's', kind: 'subagent', title: 'Explore the repo', status: 'running', usage: { total_tokens: 14_275, tool_calls: 12 }, last_tool: 'Read', summary: 'Reading src/' })
    expect(screen.getByTestId('task-title')).toHaveTextContent('Explore the repo')
    expect(screen.getByTestId('task-meta')).toHaveTextContent('14k tokens · 12 tool calls')
    expect(screen.getByTestId('task-last-tool')).toHaveTextContent('Read')
    expect(screen.getByTestId('task-summary')).toHaveTextContent('Reading src/')
    expect(screen.queryByRole('button')).toBeNull()
  })

  it('shows a failed shell task in the danger color with its word', () => {
    renderPanel({ type: 'task', id: 's', kind: 'shell', title: 'npm test', status: 'failed', summary: 'exit 1' })
    expect(screen.getByTestId('task-status')).toHaveTextContent('Failed')
    expect(screen.getByTestId('task')).toHaveAttribute('data-kind', 'shell')
  })

  it('is written in Chinese when the UI language is Chinese', async () => {
    await i18n.changeLanguage('zh')
    renderPanel(workflow({ summary: null }))
    expect(screen.getByTestId('task-status')).toHaveTextContent('运行中')
    expect(screen.getByTestId('task-meta')).toHaveTextContent('1/3 个 agent 完成 · 900 token · 7 次调用 · 4s')
    const queued = screen.getAllByTestId('task-agent')[2]!
    expect(queued).toHaveTextContent('第 2 次尝试')
    expect(within(queued).getByRole('img')).toHaveAccessibleName('排队中')
  })
})

describe('BackgroundTasksChip', () => {
  const running = (...ids: string[]): TaskSnapshot[] => ids.map((id) => snapshot({ type: 'task', id, kind: 'subagent', title: `task ${id}`, status: 'running', usage: { total_tokens: 100 }, summary: `doing ${id}` }))

  it('says how many are running and lists them when opened', async () => {
    const user = userEvent.setup()
    render(<BackgroundTasksChip tasks={running('a', 'b')} count={2} idle={false} />)
    const chip = screen.getByTestId('background-tasks-chip')
    expect(chip).toHaveTextContent('2 background tasks running')
    expect(chip).toHaveAttribute('data-count', '2')
    expect(screen.queryByTestId('background-tasks-list')).toBeNull()
    await user.click(chip)
    const list = await screen.findByTestId('background-tasks-list')
    const rows = within(list).getAllByTestId('background-task')
    expect(rows).toHaveLength(2)
    expect(rows[0]).toHaveTextContent('task a')
    expect(rows[0]).toHaveTextContent('Running')
    expect(rows[0]).toHaveTextContent('100 tokens')
    expect(rows[0]).toHaveTextContent('doing a')
    // 会话还在跑的时候没有「agent 会接着处理」的说明
    expect(list).not.toHaveTextContent('pick up')
  })

  it('uses the singular for one, and adds a line for tasks the history does not show', async () => {
    const user = userEvent.setup()
    const { rerender } = render(<BackgroundTasksChip tasks={running('a')} count={1} idle={false} />)
    expect(screen.getByTestId('background-tasks-chip')).toHaveTextContent('1 background task running')
    rerender(<BackgroundTasksChip tasks={running('a')} count={3} idle={false} />)
    await user.click(screen.getByTestId('background-tasks-chip'))
    expect(await screen.findByTestId('background-task-other')).toHaveTextContent('2 more background tasks')
  })

  it('with none recognized, the list is just the count; idle sessions get the note that the agent will continue', async () => {
    const user = userEvent.setup()
    render(<BackgroundTasksChip tasks={[]} count={2} idle />)
    await user.click(screen.getByTestId('background-tasks-chip'))
    const list = await screen.findByTestId('background-tasks-list')
    expect(within(list).queryAllByTestId('background-task')).toHaveLength(0)
    expect(within(list).getByTestId('background-task-other')).toHaveTextContent('2 background tasks')
    expect(list).toHaveTextContent('The agent will pick up their results when they finish.')
  })
})
