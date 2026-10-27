// 后台任务 chip（spec/web.md § 4.6 输入区）：agent 进程里还有后台任务（后台 subagent、后台命令、workflow）在跑时，输入框上方有一个
// 「N 个后台任务在运行」的 chip，点开列出；主 turn 结束后会话是 idle 的，弹层里说明结束后 agent 会继续处理。
// 没有后台任务（含旧 daemon 不发该字段）时不显示。
import { act, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { SessionStatus, SessionSummary } from '@/protocol'
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
// 预览入口读 hub 的 /api/config（react-query）；这个测试没有 QueryClientProvider，也不涉及预览：当作 hub 没配预览
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
// 不真的去 daemon 拉会话：事件直接放进 store
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
let rpc: FakeRpc

function seed(over: Partial<SessionSummary> = {}): void {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()] }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, [ev({ type: 'user_message', message_id: 'm1', content: 'go', by_device: 'me' }), ev({ type: 'turn_start', turn_id: 1, message_id: 'm1' }), ev({ type: 'turn_done', turn_id: 1, reason: 'completed' })])
  // watching: false 时状态取摘要里的 status（见 SessionPage 的 status 推导）
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: false })
}
const chip = () => screen.queryByTestId('background-tasks-chip')

beforeEach(async () => {
  resetSeq()
  navigate.mockReset()
  await i18n.changeLanguage('en')
  rpc = new FakeRpc()
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  useGit.setState({ bySession: {} })
  usePrefs.setState({ infoPanelOpen: false })
  resetDrafts()
})
afterEach(() => setRpc(null))

describe('SessionPage: background tasks chip', () => {
  it('shows the count when the session is idle and background tasks are still running, and explains in the list that the agent will continue', async () => {
    const user = userEvent.setup()
    seed({ status: 'idle', background_tasks: 2 })
    render(<SessionPage sessionId={SID} />)
    const el = await screen.findByTestId('background-tasks-chip')
    expect(el).toHaveTextContent('2 background tasks running')
    expect(screen.getByTestId('background-tasks')).toHaveAttribute('data-count', '2')
    // 状态点不因后台任务而变
    expect(screen.getByTestId('session-status')).toHaveAttribute('data-status', 'idle')
    await user.click(el)
    const list = await screen.findByTestId('background-tasks-list')
    // 事件里认不出是哪些任务：只有一行数
    expect(within(list).getByTestId('background-task-other')).toHaveTextContent('2 background tasks')
    expect(list).toHaveTextContent('The agent will pick up their results when they finish.')
  })

  it('uses the singular form for one task', async () => {
    seed({ status: 'idle', background_tasks: 1 })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('background-tasks-chip')).toHaveTextContent('1 background task running')
  })

  it('is written in Chinese when the UI language is Chinese', async () => {
    await i18n.changeLanguage('zh')
    seed({ status: 'idle', background_tasks: 3 })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('background-tasks-chip')).toHaveTextContent('3 个后台任务在运行')
  })

  it('is not shown without background tasks: 0, or a daemon that does not send the field', async () => {
    seed({ status: 'idle', background_tasks: 0 })
    const view = render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-status')
    expect(chip()).toBeNull()
    view.unmount()

    seed({ status: 'idle' })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-status')
    expect(chip()).toBeNull()
  })

  it.each<SessionStatus>(['running', 'starting', 'waiting_permission', 'queued'])('is also shown while the session is %s: tasks run next to the main turn', async (status) => {
    seed({ status, background_tasks: 2 })
    render(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('background-tasks-chip')).toHaveTextContent('2 background tasks running')
  })

  it('follows the session: appears when the main turn ends with tasks left, updates the count, disappears when they finish', async () => {
    seed({ status: 'running', background_tasks: 0 })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('session-status')
    expect(chip()).toBeNull()

    const change = (status: SessionSummary['status'], background_tasks?: number) =>
      act(() => {
        useSessions.getState().applyChanged(NODE, { session_id: SID, title: 'hello', status, archived: false, pending_permissions: 0, background_tasks, updated_at_ms: 9, last_seq: 0 })
      })
    change('idle', 2)
    expect(await screen.findByTestId('background-tasks-chip')).toHaveTextContent('2 background tasks')
    change('idle', 1)
    expect(screen.getByTestId('background-tasks-chip')).toHaveTextContent('1 background task running')
    // agent 自己再跑一轮：chip 还在
    change('running', 1)
    expect(chip()).not.toBeNull()
    change('idle', 0)
    expect(chip()).toBeNull()
    // 通知里没有该字段（旧 daemon）按 0 处理
    change('idle', 1)
    expect(chip()).not.toBeNull()
    change('idle', undefined)
    expect(chip()).toBeNull()
  })
})
