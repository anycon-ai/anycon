// 导出 Markdown 里的 notice 和后台任务（事件形状和 reducer.test 里的一致）。
import { beforeEach, describe, expect, it } from 'vitest'
import { evs, resetSeq } from '@/test/factories'
import { exportMarkdown } from './export'
import { buildView } from './reducer'

beforeEach(() => resetSeq())

const md = (events: Parameters<typeof evs>[0]): string => exportMarkdown('T', buildView(evs(events)).blocks)

describe('exportMarkdown: notices', () => {
  it('writes each notice as one line with its level, code and the agent’s text', () => {
    const out = md([
      { type: 'text_delta', text: 'hello' },
      { type: 'notice', code: 'model_fallback', level: 'warning', message: 'Switched to mock-small\nbecause of load', model_from: 'mock-large', model_to: 'mock-small' },
      { type: 'notice', code: 'context_compacted', level: 'info', message: 'Context compacted (180k → 20k tokens)' },
    ])
    expect(out).toContain('> **Warning** (model_fallback): Switched to mock-small because of load\n')
    expect(out).toContain('> **Notice** (context_compacted): Context compacted (180k → 20k tokens)\n')
    // 一条提示一行
    expect(out.split('\n').filter((l) => l.includes('(model_fallback)'))).toHaveLength(1)
  })

  it('builds a sentence from the structured fields when the agent gave no text, and falls back to the code', () => {
    const out = md([
      { type: 'notice', code: 'model_fallback', level: 'warning', message: '', model_from: 'a', model_to: 'b' },
      { type: 'notice', code: 'model_fallback', level: 'warning', message: ' ', model_to: 'b' },
      { type: 'notice', code: 'rate_limit', level: 'warning', message: '', resets_at_ms: Date.UTC(2026, 9, 26, 12, 0, 0) },
      { type: 'notice', code: 'deprecation', level: 'warning', message: '' },
    ])
    expect(out).toContain('> **Warning** (model_fallback): Model switched from a to b\n')
    expect(out).toContain('> **Warning** (model_fallback): Model switched to b\n')
    expect(out).toContain('> **Warning** (rate_limit): Usage limit reached, resets 2026-10-26T12:00:00.000Z\n')
    expect(out).toContain('> **Warning** (deprecation): deprecation\n')
  })

  it('keeps a very long notice to one clipped line', () => {
    const out = md([{ type: 'notice', code: 'agent_notice', level: 'info', message: 'x'.repeat(5000) }])
    const line = out.split('\n').find((l) => l.includes('(agent_notice)'))!
    expect(line.length).toBeLessThan(600)
    expect(line.endsWith('…')).toBe(true)
  })

  it('local command output (/context) is a code block, not a quote line', () => {
    const out = md([{ type: 'notice', code: 'command_output', level: 'info', message: 'Context Usage\n  Messages: 3.1k tokens\n' }])
    expect(out).toContain('> **Command output**\n\n```\nContext Usage\n  Messages: 3.1k tokens\n```\n')
  })
})

describe('exportMarkdown: tasks', () => {
  const call = { type: 'tool_call', id: 'toolu_wf', name: 'Workflow', category: 'other', title: 'Run workflow: alpha-beta', input: { script: 'x' } } as const
  const launched = { type: 'tool_result', id: 'toolu_wf', output: 'Workflow launched in background. Task ID: w1', is_error: false } as const

  it('writes each task’s final state under the tool call that started it: title, status, agents done / total, usage', () => {
    const out = md([
      call,
      launched,
      { type: 'task', id: 'w1', kind: 'workflow', tool_id: 'toolu_wf', title: 'alpha-beta', status: 'running', agents: [{ index: 1, label: 'alpha', state: 'running' }] },
      {
        type: 'task',
        id: 'w1',
        kind: 'workflow',
        tool_id: 'toolu_wf',
        title: 'alpha-beta',
        status: 'completed',
        usage: { total_tokens: 27253, tool_calls: 0, duration_ms: 3715 },
        agents: [
          { index: 1, label: 'alpha', state: 'done' },
          { index: 2, label: 'beta', state: 'done' },
        ],
      },
    ])
    // 只写最后一份快照
    expect(out.match(/\*\*Task\*\*/g)).toHaveLength(1)
    expect(out).toContain('> **Task** (workflow) — `alpha-beta`: completed, 2/2 agents done, 27253 tokens, 0 tool calls, 4s\n')
    // 在工具行后面
    expect(out.indexOf('**Tool** — `Run workflow: alpha-beta`')).toBeLessThan(out.indexOf('**Task**'))
  })

  it('failed, cancelled and still-running tasks say so; a task without a loaded tool call is written where it appeared', () => {
    const out = md([
      { type: 'text_delta', text: 'before' },
      { type: 'task', id: 'a', kind: 'subagent', tool_id: 'gone', title: 'Explore the repo', status: 'failed', usage: { total_tokens: 100 } },
      { type: 'text_done' },
      { type: 'task', id: 'b', kind: 'shell', title: '', status: 'cancelled' },
      { type: 'task', id: 'c', kind: 'other', title: 'tail -f log', status: 'running' },
    ])
    expect(out).toContain('> **Task** (subagent) — `Explore the repo`: failed, 100 tokens\n')
    expect(out).toContain('> **Task** (shell) — `Background task`: cancelled\n')
    expect(out).toContain('> **Task** (other) — `tail -f log`: running\n')
    expect(out.indexOf('before')).toBeLessThan(out.indexOf('Explore the repo'))
  })
})
