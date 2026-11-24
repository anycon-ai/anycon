// Bot 问题复用「提问」队列：可点选、自由输入、按通知跨筛选定位。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTwinDaemon, twinQuestion } from '@/test/fakeTwinDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useEnroll } from '@/stores/enroll'
import { useTodos } from '@/features/todos/store'
import { useTwin } from '@/features/twin/store'
import { useInflight } from '@/features/session/inflight'
import { syncTwin } from '@/features/twin/service'
import { inboxNowCountFromStores } from '@/features/inbox/model'
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/inbox' }))
import { InboxPage } from './InboxPage'
const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const info = { capabilities: { twin: true, todo: true } } as unknown as NodeInfo
let rpc: FakeRpc
let daemon: FakeTwinDaemon
beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeTwinDaemon(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info }), [B]: nodeEntry({ id: B, name: 'beta' }) }, order: [A, B], loaded: true })
  usePrefs.setState({ sidebarNodes: null, sidebarSpace: null })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useEnroll.setState({ items: {}, resolved: {} })
  useInflight.setState({ ids: new Set(), handled: new Set() })
  useTwin.getState().reset()
  useTodos.setState({ nodes: {}, outbox: [] })
  useTodos.getState().setCache(A, [todoItem({ id: 't1', title: 'Fix login', handler: 'twin' })])
})
it('counts open questions, presents them as Question, and answers options inline', async () => {
  daemon.questions = [twinQuestion(), twinQuestion({ id: 'answered', status: 'answered' }), twinQuestion({ id: 'void', status: 'void' })]
  await syncTwin(A)
  expect(inboxNowCountFromStores()).toBe(1)
  render(<InboxPage />)
  expect(screen.getByTestId('inbox-row')).toHaveTextContent('Question')
  expect(screen.getByTestId('inbox-row')).toHaveTextContent('Fix login')
  const button = await screen.findByRole('button', { name: 'Web' })
  await waitFor(() => expect(button).toBeEnabled())
  await userEvent.click(button)
  await waitFor(() => expect(inboxNowCountFromStores()).toBe(0))
  expect(rpc.calls.find((c) => c.method === 'twin.answer')?.params).toEqual({ id: 'q1', answer: 'Web' })
})
it('a notification selects a question outside the node filter, and preserves free input', async () => {
  daemon.questions = [twinQuestion()]
  await syncTwin(A)
  usePrefs.setState({ sidebarNodes: [B] })
  const done = vi.fn()
  render(<InboxPage locate={{ node: 'aaaaaaaa', todo: 't1' }} onLocateDone={done} />)
  await screen.findByTestId('inbox-outside-note')
  expect(done).toHaveBeenCalled()
  expect(inboxNowCountFromStores()).toBe(0)
  const area = screen.getByRole('textbox', { name: 'Answer' })
  await userEvent.type(area, 'Use the mobile project')
  const send = screen.getByRole('button', { name: 'Answer' })
  await waitFor(() => expect(send).toBeEnabled())
  await userEvent.click(send)
  await waitFor(() => expect(rpc.calls.find((c) => c.method === 'twin.answer')?.params).toEqual({ id: 'q1', answer: 'Use the mobile project' }))
})
it('an unsupported node contributes no question or answer entry', async () => {
  daemon.questions = [twinQuestion()]
  await syncTwin(A)
  act(() => useNodes.getState().patch(A, { info: { ...info, capabilities: { ...info.capabilities, twin: false } } }))
  render(<InboxPage />)
  expect(inboxNowCountFromStores()).toBe(0)
  expect(screen.queryByTestId('twin-question')).not.toBeInTheDocument()
})
