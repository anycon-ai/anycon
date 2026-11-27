// 「待处理」清空的那一刻（spec/web.md § 4.11）：Logo 的一次性动效只在列表清空 / 进来时本来就空的那一次挂上，
// 下面的今日小结只写本地如实算得出的数字，没有可说的就换回原来的说明。
import { act, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PermissionKind } from '@/protocol'
import { nodeEntry, summary } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { resetViewport } from '@/test/viewport'
import { recordAnswered } from '@/features/inbox/today'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type PendingPermission, type SessionItem } from '@/stores/sessions'

const h = vi.hoisted(() => ({ navigate: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}, { pathname: '/inbox' }))
vi.mock('@/api/queries', async (orig) => ({ ...(await orig<object>()), useDevices: () => ({ data: undefined, dataUpdatedAt: 0 }) }))

import { InboxPage } from './InboxPage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)

const sess = (over: Partial<SessionItem> & { id: string }): SessionItem => ({ ...summary({ title: over.id, project: 'app', status: 'idle', updated_at_ms: Date.now() - 60_000, ...over }), node_id: A, cached_seq: 0, ...over })
const req = (id: string): PendingPermission => ({
  request_id: id,
  session_id: `s-${id}`,
  node_id: A,
  kind: 'tool' as PermissionKind,
  title: `Run ${id}`,
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: 'ls' }, detail: { command: 'ls' } } },
  options: [{ id: 'allow', label: 'Allow once', kind: 'allow_once' }],
  created_at_ms: Date.now() - 120_000,
  expires_at_ms: null,
})
const seed = (o: { sessions?: SessionItem[]; pending?: PendingPermission[] }): void =>
  useSessions.setState({ list: Object.fromEntries((o.sessions ?? []).map((s) => [s.id, s])), pending: Object.fromEntries((o.pending ?? []).map((p) => [p.request_id, p])), open: {} })

const logo = (): SVGElement | null => screen.getByTestId('inbox-empty').querySelector('svg')

beforeEach(() => {
  localStorage.removeItem('anycon.today')
  resetViewport()
  usePrefs.setState({ sidebarNodes: null })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha' }), [B]: nodeEntry({ id: B, name: 'beta' }) }, order: [A, B], loaded: true })
  seed({})
})
afterEach(() => localStorage.removeItem('anycon.today'))

describe('today summary', () => {
  it('falls back to the usual explanation when there is nothing to tell (no “0 sessions”)', () => {
    // 一个昨天的会话、一个今天归档的会话：都不算
    const yesterday = new Date()
    yesterday.setHours(0, 0, 0, 0)
    seed({ sessions: [sess({ id: 'old', updated_at_ms: yesterday.getTime() - 1000 }), sess({ id: 'arch', archived: true, updated_at_ms: Date.now() })] })
    render(<InboxPage />)
    expect(screen.queryByTestId('inbox-today')).toBeNull()
    expect(screen.getByTestId('inbox-empty')).toHaveTextContent('Approvals, plans, questions')
    expect(screen.getByTestId('inbox-empty').textContent).not.toMatch(/\b0\b/)
  })

  it('states the numbers as they are: sessions active today on the picked nodes, answers given in this browser', () => {
    seed({ sessions: [sess({ id: 's1', updated_at_ms: Date.now() - 1000 }), sess({ id: 's2', status: 'running', updated_at_ms: Date.now() - 2000 }), sess({ id: 'b1', node_id: B })] })
    recordAnswered(A)
    recordAnswered(A)
    recordAnswered(B)
    render(<InboxPage />)
    expect(screen.getByTestId('inbox-today')).toHaveTextContent('Today: 3 sessions moved forward · 3 requests answered in this browser')
    expect(screen.getByTestId('inbox-empty')).not.toHaveTextContent('Approvals, plans, questions')
  })

  it('only counts the nodes picked in the node filter, and leaves out a number that is zero', () => {
    usePrefs.setState({ sidebarNodes: [A] })
    seed({ sessions: [sess({ id: 'b1', node_id: B })] })
    recordAnswered(A)
    render(<InboxPage />)
    expect(screen.getByTestId('inbox-today')).toHaveTextContent(/^Today: 1 request answered in this browser$/)
  })
})

describe('the logo moment', () => {
  it('plays when the page opens already empty', () => {
    render(<InboxPage />)
    expect(logo()).toHaveClass('moment-pop')
  })

  it('waits while the page is hidden behind another one (phone), then plays once it is in view', () => {
    const { rerender } = render(<InboxPage active={false} />)
    expect(logo()).not.toHaveClass('moment-pop')
    rerender(<InboxPage active />)
    expect(logo()).toHaveClass('moment-pop')
  })

  it('waits for the nodes to settle: an empty list before the first sync is not the moment', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', synced: false, syncing: true }), [B]: nodeEntry({ id: B, name: 'beta' }) } })
    render(<InboxPage />)
    expect(logo()).not.toHaveClass('moment-pop')
    act(() => useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha' }), [B]: nodeEntry({ id: B, name: 'beta' }) } }))
    expect(logo()).toHaveClass('moment-pop')
  })

  it('plays again when the list goes from something to nothing, and not on plain re-renders', () => {
    seed({ pending: [req('r1')] })
    render(<InboxPage />)
    expect(screen.queryByTestId('inbox-empty')).toBeNull()
    act(() => useSessions.getState().resolvePending(A, 'r1'))
    const el = logo()
    expect(el).toHaveClass('moment-pop')
    // 重渲染：还是同一个元素（没有重新挂载，动画不会重播）
    act(() => useSessions.setState((s) => ({ list: { ...s.list, x: sess({ id: 'x' }) } })))
    expect(logo()).toBe(el)
  })
})
