// 项目页的「待处理」筛选（web.md § 4.6.1）：带个数，只列标了的会话。
import { fireEvent, render, screen, within } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import { mockAgent, nodeEntry, project, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'
import { useSessions, type SessionItem } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { ProjectPage } from './ProjectPage'

const NODE = 'n'.repeat(32)
const item = (over: Parameters<typeof summary>[0]): SessionItem => ({ ...summary({ project: 'app', ...over }), node_id: NODE, cached_seq: 0 })

beforeEach(async () => {
  await i18n.changeLanguage('en')
  navigate.mockReset()
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, projects: [project({ name: 'app' })], agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  const items = [
    item({ id: 'a', title: 'Marked and done', follow_up: true, status: 'idle' }),
    item({ id: 'b', title: 'Marked and running', follow_up: true, status: 'running' }),
    item({ id: 'c', title: 'Plain', status: 'idle' }),
    item({ id: 'd', title: 'Marked but archived', follow_up: true, archived: true }),
  ]
  useSessions.setState({ list: Object.fromEntries(items.map((i) => [i.id, i])), pending: {}, open: {} })
})

describe('ProjectPage: follow-up filter', () => {
  it('counts the marked sessions (not the archived ones) and lists only those', () => {
    render(<ProjectPage project="app" />)
    const tab = screen.getByTestId('project-filter-follow-up')
    expect(tab).toHaveTextContent('Follow up (2)')
    fireEvent.click(tab)
    const rows = screen.getAllByTestId('session-list-item').map((r) => r.textContent)
    expect(rows).toHaveLength(2)
    expect(rows.join(' ')).toContain('Marked and done')
    expect(rows.join(' ')).toContain('Marked and running')
    expect(screen.queryByText('Plain')).toBeNull()
  })

  it('shows the plain label when nothing is marked', () => {
    useSessions.setState({ list: { c: item({ id: 'c', title: 'Plain' }) } })
    render(<ProjectPage project="app" />)
    expect(within(screen.getByTestId('project-filter-follow-up')).getByText('Follow up')).toBeInTheDocument()
    expect(screen.getByTestId('project-filter-follow-up')).not.toHaveTextContent('(')
  })
})
