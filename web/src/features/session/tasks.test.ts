import { beforeEach, describe, expect, it } from 'vitest'
import type { TaskAgent } from '@/protocol'
import { evs, resetSeq } from '@/test/factories'
import { buildView, type TaskSnapshot } from './reducer'
import { agentCounts, agentDisplayState, allTasks, backgroundTaskCount, groupAgents, runningTasks, taskDisplayStatus, tasksLive } from './tasks'

beforeEach(() => resetSeq())

const agent = (index: number, over: Partial<TaskAgent> = {}): TaskAgent => ({ index, label: `agent-${index}`, state: 'running', ...over })
const snap = (status: TaskSnapshot['status']): Pick<TaskSnapshot, 'status'> => ({ status })

describe('tasksLive', () => {
  it('is true while the main agent is in a turn, or the daemon reports background tasks', () => {
    for (const s of ['running', 'starting', 'waiting_permission'] as const) expect(tasksLive(s, 0)).toBe(true)
    expect(tasksLive('idle', 2)).toBe(true)
    expect(tasksLive('ended', 1)).toBe(true)
  })
  it('is false when the session is not running and there are no background tasks', () => {
    for (const s of ['idle', 'ended', 'error', 'interrupted', 'queued'] as const) expect(tasksLive(s, 0)).toBe(false)
  })
})

describe('taskDisplayStatus', () => {
  it('shows a snapshot stuck on running as stopped when nothing can still be running it', () => {
    expect(taskDisplayStatus(snap('running'), true)).toBe('running')
    expect(taskDisplayStatus(snap('running'), false)).toBe('stopped')
  })
  it('leaves terminal snapshots alone', () => {
    for (const s of ['completed', 'failed', 'cancelled'] as const) {
      expect(taskDisplayStatus(snap(s), true)).toBe(s)
      expect(taskDisplayStatus(snap(s), false)).toBe(s)
    }
  })
})

describe('agentDisplayState', () => {
  it('shows queued / running agents as stopped once the task is not running any more', () => {
    expect(agentDisplayState({ state: 'running' }, 'running')).toBe('running')
    expect(agentDisplayState({ state: 'queued' }, 'running')).toBe('queued')
    for (const task of ['stopped', 'cancelled', 'failed', 'completed'] as const) {
      expect(agentDisplayState({ state: 'running' }, task)).toBe('stopped')
      expect(agentDisplayState({ state: 'queued' }, task)).toBe('stopped')
      expect(agentDisplayState({ state: 'done' }, task)).toBe('done')
      expect(agentDisplayState({ state: 'failed' }, task)).toBe('failed')
      expect(agentDisplayState({ state: 'skipped' }, task)).toBe('skipped')
    }
  })
})

describe('allTasks / runningTasks', () => {
  const view = () =>
    buildView(
      evs([
        { type: 'tool_call', id: 't1', name: 'Workflow', category: 'subagent', title: 'wf', input: {} },
        { type: 'task', id: 'a', kind: 'workflow', tool_id: 't1', title: 'A', status: 'running' },
        { type: 'task', id: 'b', kind: 'shell', tool_id: 'gone', title: 'B', status: 'running' },
        { type: 'task', id: 'c', kind: 'subagent', tool_id: 't1', title: 'C', status: 'completed' },
        { type: 'task', id: 'b', kind: 'shell', tool_id: 'gone', title: 'B', status: 'failed' },
      ]),
    )
  it('lists the latest snapshot of every task in order of first appearance, attached or not', () => {
    expect(allTasks(view().blocks).map((t) => [t.id, t.status])).toEqual([
      ['a', 'running'],
      ['c', 'completed'],
      ['b', 'failed'],
    ])
  })
  it('lists the running ones only when something can still be running them', () => {
    expect(runningTasks(view().blocks, true).map((t) => t.id)).toEqual(['a'])
    expect(runningTasks(view().blocks, false)).toEqual([])
  })
})

describe('a task that restarts', () => {
  it('counts as running again after a completed snapshot, and leaves the list again when it completes anew', () => {
    const call = { type: 'tool_call', id: 'call_1', name: 'spawnAgent', category: 'subagent', title: 'Start sub-agent', input: {} } as const
    const child = (status: TaskSnapshot['status']) => ({ type: 'task', id: 'child', kind: 'subagent', tool_id: 'call_1', title: 'pong', status }) as const
    const running = (...events: Parameters<typeof evs>[0]) => runningTasks(buildView(evs([call, ...events])).blocks, true).map((t) => t.id)
    expect(running(child('running'), child('completed'))).toEqual([])
    expect(running(child('running'), child('completed'), child('running'))).toEqual(['child'])
    expect(running(child('running'), child('completed'), child('running'), child('completed'))).toEqual([])
  })
})

describe('backgroundTaskCount', () => {
  it('takes the larger of what the history shows and what the daemon reports', () => {
    expect(backgroundTaskCount(1, 0)).toBe(1)
    expect(backgroundTaskCount(0, 3)).toBe(3)
    expect(backgroundTaskCount(2, 2)).toBe(2)
    expect(backgroundTaskCount(0, 0)).toBe(0)
  })
})

describe('groupAgents', () => {
  const phases = [
    { index: 1, title: 'Review' },
    { index: 0, title: 'Scan' },
  ]
  it('groups by phase in phase order and agents in index order', () => {
    const groups = groupAgents(phases, [agent(2, { phase_index: 1 }), agent(0, { phase_index: 0 }), agent(1, { phase_index: 1 })])
    expect(groups.map((g) => [g.phase?.title, g.agents.map((a) => a.index)])).toEqual([
      ['Scan', [0]],
      ['Review', [1, 2]],
    ])
  })
  it('keeps a phase that has no agents yet, and puts agents that match no phase in a last untitled group', () => {
    const groups = groupAgents(phases, [agent(0, { phase_index: 0 }), agent(1), agent(2, { phase_index: 9 })])
    expect(groups.map((g) => [g.phase?.title, g.agents.map((a) => a.index)])).toEqual([
      ['Scan', [0]],
      ['Review', []],
      [undefined, [1, 2]],
    ])
  })
  it('is one untitled group without phases, and nothing without agents or phases', () => {
    expect(groupAgents([], [agent(1), agent(0)]).map((g) => [g.phase, g.agents.map((a) => a.index)])).toEqual([[undefined, [0, 1]]])
    expect(groupAgents([], [])).toEqual([])
  })
  it('does not reorder the inputs', () => {
    const agents = [agent(1, { phase_index: 0 }), agent(0, { phase_index: 0 })]
    groupAgents(phases, agents)
    expect(agents.map((a) => a.index)).toEqual([1, 0])
    expect(phases.map((p) => p.index)).toEqual([1, 0])
  })
})

describe('agentCounts', () => {
  it('counts agents by state', () => {
    expect(agentCounts([agent(0, { state: 'done' }), agent(1, { state: 'done' }), agent(2, { state: 'failed' }), agent(3, { state: 'queued' })])).toEqual({ queued: 1, running: 0, done: 2, failed: 1, skipped: 0 })
  })
})
