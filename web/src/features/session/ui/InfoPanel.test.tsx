// 信息面板的「Agent」卡片：主 agent 的状态和 token，子 agent 的状态、token、调用数，在工作的个数；不再有选项和改动的文件。
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it } from 'vitest'
import type { Event, SessionStatus } from '@/protocol'
import { evs, resetSeq } from '@/test/factories'
import { buildView } from '../reducer'
import { subagentRows } from '../subagents'
import { InfoPanel, type InfoPanelProps } from './InfoPanel'

beforeEach(() => resetSeq())

const agent = (id: string, parent_id?: string): Event => ({ type: 'tool_call', id, name: 'Agent', category: 'subagent', title: `agent ${id}`, input: {}, ...(parent_id ? { parent_id } : {}) })
const bash = (id: string, parent_id: string): Event => ({ type: 'tool_call', id, name: 'Bash', category: 'shell', title: id, input: {}, parent_id })
const done = (id: string, total_tokens?: number): Event => ({ type: 'tool_result', id, output: 'ok', is_error: false, ...(total_tokens ? { detail: { total_tokens } } : {}) })

function panel(status: SessionStatus, events: Event[], over: Partial<InfoPanelProps> = {}) {
  const view = buildView(evs(events))
  const props: InfoPanelProps = {
    agentLabel: 'Claude Code',
    status,
    subagents: subagentRows(view.blocks, status),
    createdAt: 1,
    usage: { turns: 2, inputTokens: 12_300, outputTokens: 4_500, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: 0.5 },
    pending: [],
    onFocusPending: () => {},
    archived: false,
    onRename: () => {},
    onArchive: () => {},
    onDelete: () => {},
    onExport: () => {},
    onRestart: () => {},
    ...over,
  }
  return render(<InfoPanel {...props} />)
}

describe('InfoPanel agents card', () => {
  it('shows the main agent and each subagent with state, tokens and calls, and counts who is working', () => {
    panel('running', [agent('a'), bash('a1', 'a'), agent('a2', 'a'), done('a2', 900), done('a', 14_275), agent('b'), bash('b1', 'b')])
    const card = screen.getByTestId('info-agents')
    expect(card).toHaveTextContent('2 working')

    const main = screen.getByTestId('info-agent-main')
    expect(main).toHaveTextContent('Claude Code')
    expect(main).toHaveTextContent('12k in / 4.5k out')
    expect(main).toHaveTextContent('Running')

    const rows = screen.getAllByTestId('info-subagent')
    expect(rows.map((r) => r.dataset.state)).toEqual(['done', 'done', 'running'])
    expect(rows[0]).toHaveTextContent('agent a')
    expect(rows[0]).toHaveTextContent('14k tokens · 2 tool calls')
    expect(rows[1]).toHaveTextContent('900 tokens')
    expect(rows[1]).not.toHaveTextContent('tool call')
    expect(within(rows[2]!).getByText('Running')).toBeInTheDocument()
    expect(rows[2]).toHaveTextContent('1 tool call')
    expect(rows[2]).not.toHaveTextContent('tokens')
  })

  it('has no options or changed-files cards any more', () => {
    panel('idle', [{ type: 'file_changed', path: 'src/a.ts', kind: 'created' }])
    expect(screen.queryByTestId('info-files')).toBeNull()
    expect(screen.queryByText('src/a.ts')).toBeNull()
    expect(screen.queryByText('Options')).toBeNull()
    expect(screen.queryByTestId('info-subagent')).toBeNull()
    expect(screen.getByTestId('info-agents')).not.toHaveTextContent('working')
  })

  it('keeps long lists short: running agents and the latest few stay, the rest expand on demand', async () => {
    const events: Event[] = [agent('bg'), { type: 'tool_result', id: 'bg', output: 'launched', is_error: false, detail: { background: true } }]
    for (let i = 0; i < 8; i++) events.push(agent(`s${i}`), done(`s${i}`))
    panel('idle', events)
    expect(screen.getByTestId('info-agents')).toHaveTextContent('1 working')
    const ids = () => screen.getAllByTestId('info-subagent').map((r) => r.textContent)
    // 后台那个还在跑，排在最前面也留着；完成的只留最近 6 个
    expect(ids()).toHaveLength(7)
    expect(ids()[0]).toContain('agent bg')
    expect(ids()[1]).toContain('agent s2')
    await userEvent.setup().click(screen.getByTestId('info-subagents-more'))
    expect(ids()).toHaveLength(9)
    expect(screen.queryByTestId('info-subagents-more')).toBeNull()
  })
})
