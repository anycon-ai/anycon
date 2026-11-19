// ⌘K 里的「标记为待处理」：只在会话页出现，标记和取消用同一项（web.md § 4.6.1）。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import { nodeEntry, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
const params = vi.hoisted(() => ({ node: 'nnnnnnnn', session: 'sess_a' }) as Record<string, string>)
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, params))
vi.mock('@/api/queries', () => ({ useDevices: () => ({ data: undefined, dataUpdatedAt: 0 }) }))

import { CommandPalette } from './CommandPalette'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

function seed(over: Parameters<typeof summary>[0] = {}): void {
  useSessions.setState({ list: { sess_a: { ...summary({ id: 'sess_a', title: 'Analyse jira', ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
}

beforeEach(async () => {
  params.session = 'sess_a'
  await i18n.changeLanguage('en')
  navigate.mockReset()
  rpc = new FakeRpc()
  rpc.handlers['session.set_marks'] = () => ({})
  setRpc(rpc)
  usePrefs.setState({ sidebarNodes: null })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE }) }, order: [NODE], loaded: true })
})
afterEach(() => setRpc(null))

describe('command palette — follow-up', () => {
  it('marks the session you are on, and closes the palette', async () => {
    const user = userEvent.setup()
    seed()
    const onOpenChange = vi.fn()
    render(<CommandPalette open onOpenChange={onOpenChange} />)
    await user.click(screen.getByRole('button', { name: /Mark for follow-up/ }))
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'session.set_marks').map((c) => c.params)).toEqual([{ session_id: 'sess_a', follow_up: true }]))
    expect(onOpenChange).toHaveBeenCalledWith(false)
  })

  it('offers to remove the mark when it is already there', async () => {
    const user = userEvent.setup()
    seed({ follow_up: true })
    render(<CommandPalette open onOpenChange={() => {}} />)
    await user.click(screen.getByRole('button', { name: /Remove follow-up mark/ }))
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'session.set_marks').map((c) => c.params)).toEqual([{ session_id: 'sess_a', follow_up: false }]))
  })

  it('is not offered off a session page, for an archived session or while the node is offline', () => {
    seed()
    delete params.session
    const { unmount } = render(<CommandPalette open onOpenChange={() => {}} />)
    expect(screen.queryByRole('button', { name: /follow-up/i })).toBeNull()
    unmount()
    params.session = 'sess_a'
    seed({ archived: true })
    const second = render(<CommandPalette open onOpenChange={() => {}} />)
    expect(screen.queryByRole('button', { name: /follow-up/i })).toBeNull()
    second.unmount()
    seed()
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn: 'peer_offline' }) } })
    render(<CommandPalette open onOpenChange={() => {}} />)
    expect(screen.queryByRole('button', { name: /follow-up/i })).toBeNull()
  })
})
