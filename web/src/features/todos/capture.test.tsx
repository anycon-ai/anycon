// 「记一项」的入口和输入条（spec/web.md § 14.5）：第一行标题、其余正文；发送后留着条并清空；撤销（离线也行）；校验提示；
// 多个节点时的缺省；命令面板的快速记；会话页的引用预填。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import * as db from '@/db'
import { CaptureHost } from '@/components/CaptureHost'
import { nodeEntry } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { resetViewport, setViewport } from '@/test/viewport'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useUi } from '@/stores/ui'
import { captureFromQuote, useCapture } from './capture'
import { CaptureForm } from './ui/CaptureForm'
import { messageLink, openCaptureFromMessage } from './ui/MessageTodoMenu'
import { defaultTodoNode, quickAdd } from './quickAdd'
import { resetTodosMemory } from './service'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const C = 'c'.repeat(32)
const info = (todo: boolean): NodeInfo => ({ capabilities: { todo } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeTodoDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 5))
  })
}
const added = () => [...daemon.todos.values()]

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  resetViewport()
  usePrefs.setState({ sidebarNodes: null, lastTodoNode: null })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(true), conn: 'ready' }), [C]: nodeEntry({ id: C, name: 'gamma', info: info(false), conn: 'ready' }) }, order: [A, C], loaded: true })
  resetTodosMemory()
  useUi.setState({ toasts: [] })
  useCapture.setState({ open: false, text: '', focusTick: 0 })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('capture bar and sheet', () => {
  it('desktop: a bar at the top, Esc closes it', async () => {
    const user = userEvent.setup()
    render(<CaptureHost />)
    expect(screen.queryByTestId('capture-bar')).toBeNull()
    act(() => useCapture.getState().openCapture())
    expect(screen.getByTestId('capture-bar')).toBeInTheDocument()
    expect(screen.getByTestId('capture-input')).toHaveFocus()
    await user.keyboard('{Escape}')
    expect(useCapture.getState().open).toBe(false)
  })

  it('phone: a bottom sheet', () => {
    setViewport('phone')
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture())
    expect(screen.getByTestId('capture-sheet')).toBeInTheDocument()
    expect(screen.queryByTestId('capture-bar')).toBeNull()
  })

  it('first line is the title, the rest the body; after sending the bar stays open, empties and offers undo', async () => {
    const user = userEvent.setup()
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture())
    await user.type(screen.getByTestId('capture-input'), 'Buy milk')
    await user.keyboard('{Shift>}{Enter}{/Shift}two litres')
    await user.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect(added()).toHaveLength(1))
    expect(added()[0]).toMatchObject({ title: 'Buy milk', body: 'two litres' })
    expect(useCapture.getState().open).toBe(true)
    expect(screen.getByTestId('capture-input')).toHaveValue('')
    expect(screen.getByTestId('capture-input')).toHaveFocus()
    expect(usePrefs.getState().lastTodoNode).toBe(A)
    const toast = useUi.getState().toasts.find((t) => t.action)!
    await act(async () => toast.action!.run())
    await flush()
    expect(added()).toHaveLength(0)
  })

  it('Enter sends on a keyboard device', async () => {
    const user = userEvent.setup()
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture({ text: '' }))
    await user.type(screen.getByTestId('capture-input'), 'quick{Enter}')
    await waitFor(() => expect(added().map((t) => t.title)).toEqual(['quick']))
  })

  it('optional project and due time are sent along', async () => {
    const user = userEvent.setup()
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture())
    await user.type(screen.getByTestId('capture-input'), 'ship it')
    await user.type(screen.getByTestId('capture-project'), 'api')
    await user.type(screen.getByTestId('capture-due'), '2030-01-02T03:04')
    await user.click(screen.getByTestId('capture-send'))
    await flush()
    expect(added()[0]).toMatchObject({ title: 'ship it', project: 'api' })
    expect(added()[0]!.due_ms).toBe(new Date('2030-01-02T03:04').getTime())
  })

  it('shows what is wrong and does not send: a title over 200 code points', async () => {
    const user = userEvent.setup()
    render(<CaptureForm />)
    await user.click(screen.getByTestId('capture-input'))
    await user.paste('x'.repeat(201))
    await user.click(screen.getByTestId('capture-send'))
    expect(await screen.findByTestId('capture-error')).toBeInTheDocument()
    expect(added()).toHaveLength(0)
    expect(screen.getByTestId('capture-input')).toHaveValue('x'.repeat(201))
  })

  it('works while the node is offline: the item waits in the outbox; undo is queued behind it and the daemon ends without the item', async () => {
    const user = userEvent.setup()
    useNodes.getState().patch(A, { conn: 'peer_offline' })
    rpc.ready = false
    render(<CaptureForm />)
    await user.type(screen.getByTestId('capture-input'), 'offline one')
    await user.click(screen.getByTestId('capture-send'))
    await flush()
    const toast = useUi.getState().toasts.find((t) => t.action)!
    await act(async () => toast.action!.run())
    await flush()
    rpc.ready = true
    useNodes.getState().patch(A, { conn: 'ready' })
    const { replayOutbox } = await import('./service')
    await replayOutbox(A)
    expect(daemon.methods().filter((m) => m !== 'todo.list')).toEqual(['todo.add', 'todo.delete'])
    expect(added()).toHaveLength(0)
  })

  it('with several nodes that support to-dos it says which one it will go to; the choice is remembered', async () => {
    const user = userEvent.setup()
    useNodes.setState({ byId: { ...useNodes.getState().byId, [B]: nodeEntry({ id: B, name: 'beta', info: info(true), conn: 'ready' }) }, order: [A, B, C] })
    render(<CaptureForm />)
    expect(screen.getByTestId('capture-target')).toHaveTextContent('alpha')
    await user.selectOptions(screen.getByTestId('capture-node'), B)
    expect(screen.getByTestId('capture-target')).toHaveTextContent('beta')
    expect(screen.queryByText('gamma')).toBeNull() // 不支持清单的节点不在选项里
    await user.type(screen.getByTestId('capture-input'), 'to beta{Enter}')
    await flush()
    expect(usePrefs.getState().lastTodoNode).toBe(B)
    expect(defaultTodoNode()).toBe(B)
  })

  it('says so when no selected node supports to-dos', () => {
    usePrefs.setState({ sidebarNodes: [C] })
    render(<CaptureForm />)
    expect(screen.getByTestId('capture-unsupported')).toBeInTheDocument()
    expect(screen.queryByTestId('capture-input')).toBeNull()
  })
})

describe('quickAdd (command palette)', () => {
  it('records the typed text without opening the bar and offers undo', async () => {
    expect(await quickAdd('Call the bank\nabout the card')).toBe(true)
    await flush()
    expect(added()[0]).toMatchObject({ title: 'Call the bank', body: 'about the card' })
    expect(useCapture.getState().open).toBe(false)
    expect(useUi.getState().toasts.some((t) => t.action)).toBe(true)
  })

  it('opens the bar with the text instead when it cannot be recorded (invalid or no node)', async () => {
    expect(await quickAdd('y'.repeat(201))).toBe(false)
    expect(useCapture.getState()).toMatchObject({ open: true, text: 'y'.repeat(201) })
    useCapture.setState({ open: false })
    usePrefs.setState({ sidebarNodes: [C] })
    expect(await quickAdd('hello')).toBe(false)
    expect(useCapture.getState()).toMatchObject({ open: true, text: 'hello' })
    expect(added()).toHaveLength(0)
  })
})

describe('from a session', () => {
  it('quoted text: the first line is the title, the body has the quote and a link back to the message', () => {
    expect(captureFromQuote('fix the flaky test\nit fails on CI', 'https://h/n/a/s/b?seq=4')).toBe('fix the flaky test\nfix the flaky test\nit fails on CI\n\nhttps://h/n/a/s/b?seq=4')
    expect(captureFromQuote('short')).toBe('short')
    const long = 'w'.repeat(300)
    expect([...captureFromQuote(long).split('\n')[0]!].length).toBeLessThanOrEqual(121)
  })

  it('the message menu pre-fills the bar and does not add anything by itself', () => {
    openCaptureFromMessage('check the migration', 'aaaaaaaa', 'sess_1', 7)
    const { open, text } = useCapture.getState()
    expect(open).toBe(true)
    expect(text).toContain(messageLink('aaaaaaaa', 'sess_1', 7))
    expect(text.split('\n')[0]).toBe('check the migration')
    expect(added()).toHaveLength(0)
  })
})

describe('waitFor sanity', () => {
  it('has a defined default node when the last used one is no longer selected', async () => {
    usePrefs.setState({ lastTodoNode: B })
    await waitFor(() => expect(defaultTodoNode()).toBe(A))
  })
})
