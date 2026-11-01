// ⌘K：除了跳到会话 / 项目 / 节点，还能直接新建项目、克隆仓库、关联已有目录（不用先去节点首页）；
// 输入够长时对勾选的在线节点做会话内容的全文搜索（`session.search`），命中单独一组，选中后打开会话并定位到那条消息。
import { act, fireEvent, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNewProject } from '@/stores/newProject'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'

const navigate = vi.hoisted(() => vi.fn())
const devices = vi.hoisted(() => ({ data: undefined as unknown, dataUpdatedAt: 0 }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))
vi.mock('@/api/queries', () => ({ useDevices: () => devices }))

import { CommandPalette } from './CommandPalette'

const NODE = 'n'.repeat(32)

beforeEach(() => {
  navigate.mockReset()
  devices.data = undefined
  usePrefs.setState({ sidebarNodes: null })
  useNewProject.setState({ request: null })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
})

describe('command palette — new project', () => {
  it.each([
    ['New project', 'empty'],
    ['Clone a repository', 'clone'],
    ['Open an existing folder', 'existing'],
  ])('“%s” opens the new-project dialog on that way, and closes the palette', async (label, mode) => {
    const user = userEvent.setup()
    const onOpenChange = vi.fn()
    render(<CommandPalette open onOpenChange={onOpenChange} />)
    await user.click(screen.getByRole('button', { name: label }))
    expect(useNewProject.getState().request).toEqual({ nodeId: NODE, mode, then: 'project', pickNode: true })
    expect(onOpenChange).toHaveBeenCalledWith(false)
  })
})

// ---- 会话内容搜索 ----

const NODE_B = 'b'.repeat(32)

interface Deferred<T> {
  promise: Promise<T>
  resolve: (v: T) => void
  reject: (e: unknown) => void
}
function deferred<T>(): Deferred<T> {
  const d = {} as Deferred<T>
  d.promise = new Promise<T>((resolve, reject) => {
    d.resolve = resolve
    d.reject = reject
  })
  return d
}

function hit(session_id: string, seq: number, snippet: string, over: Record<string, unknown> = {}) {
  return { session_id, seq, title: `title of ${session_id}`, snippet, ts_ms: Date.now() - 2 * 3_600_000, ...over }
}

let rpc: FakeRpc
const searches = () => rpc.calls.filter((c) => c.method === 'session.search')

// 假时间下不用 userEvent（它的异步包装靠 setTimeout 排空微任务，假时间下永远等不到）：输入、按键、点击都用同步的 fireEvent
const input = (): HTMLElement => screen.getByRole('textbox')
const typeQuery = (value: string): void => void fireEvent.change(input(), { target: { value } })
const press = (key: string, init: KeyboardEventInit = {}): void => void fireEvent.keyDown(input(), { key, ...init })

/** 走过 `ms` 毫秒（防抖、超时），并让已经完成的请求把结果写进界面。 */
async function settle(ms = 0): Promise<void> {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms)
  })
}

function setupTwoNodes(): void {
  useNodes.setState({
    byId: { [NODE]: nodeEntry({ id: NODE, name: 'laptop' }), [NODE_B]: nodeEntry({ id: NODE_B, name: 'desktop' }) },
    order: [NODE, NODE_B],
    loaded: true,
  })
}

describe('command palette — conversation search', () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] })
    rpc = new FakeRpc()
    setRpc(rpc)
    rpc.handlers['session.search'] = () => ({ hits: [] })
  })
  afterEach(() => {
    setRpc(null)
    vi.useRealTimers()
  })

  it('waits for the typing to pause (300 ms) before searching, and sends the trimmed query', async () => {
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('  refac')
    await settle(299)
    expect(searches()).toHaveLength(0)
    // 继续输入会重新计时
    typeQuery('  refactor  ')
    await settle(299)
    expect(searches()).toHaveLength(0)
    await settle(1)
    expect(searches()).toHaveLength(1)
    expect(searches()[0]).toMatchObject({ nodeId: NODE, params: { query: 'refactor', limit: 20 } })
    // 只多了空白的改动不会重新发
    typeQuery('refactor')
    await settle(1000)
    expect(searches()).toHaveLength(1)
  })

  it('does not search for fewer than 2 characters (after trimming), and says so', async () => {
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery(' a ')
    await settle(1000)
    expect(searches()).toHaveLength(0)
    expect(screen.getByTestId('palette-min-chars')).toBeInTheDocument()
    expect(screen.queryByTestId('palette-content-group')).toBeNull()
    // 一个汉字也是一个字符
    typeQuery('库')
    await settle(1000)
    expect(searches()).toHaveLength(0)
    typeQuery('库表')
    await settle(300)
    expect(searches()).toHaveLength(1)
    expect(screen.queryByTestId('palette-min-chars')).toBeNull()
  })

  it('searches every checked node that is online at once and merges the hits, naming the node when several are checked', async () => {
    setupTwoNodes()
    const laptop = deferred<unknown>()
    const desktop = deferred<unknown>()
    rpc.handlers['session.search'] = (_p, nodeId) => (nodeId === NODE ? laptop.promise : desktop.promise)
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('parser')
    await settle(300)
    // 并发：两个节点的请求都发出去了，谁也没等谁
    expect(searches().map((c) => c.nodeId)).toEqual([NODE, NODE_B])
    expect(screen.getByTestId('palette-searching')).toBeInTheDocument()
    // 后一个节点先回来，先显示
    await act(async () => desktop.resolve({ hits: [hit('sess_b1', 4, 'the <mark>parser</mark> on desktop')] }))
    expect(screen.getAllByTestId('palette-hit')).toHaveLength(1)
    expect(screen.getByTestId('palette-searching')).toBeInTheDocument()
    await act(async () => laptop.resolve({ hits: [hit('sess_a1', 9, 'a <mark>parser</mark> bug'), hit('sess_a2', 2, 'rewrite the <mark>parser</mark>')] }))
    expect(screen.queryByTestId('palette-searching')).toBeNull()
    const rows = screen.getAllByTestId('palette-hit')
    // 按勾选的节点顺序合并：laptop 的在前，desktop 的在后
    expect(rows.map((r) => within(r).getByTestId('palette-hit-node').textContent)).toEqual(['laptop', 'laptop', 'desktop'])
    expect(rows[0]).toHaveTextContent('title of sess_a1')
    expect(rows[1]).toHaveTextContent('title of sess_a2')
    expect(rows[2]).toHaveTextContent('title of sess_b1')
    // 每条有标题、片段（命中词高亮）、时间
    expect(within(rows[0]!).getByTestId('palette-hit-snippet')).toHaveTextContent('a parser bug')
    expect(within(rows[0]!).getByText('parser').tagName).toBe('MARK')
    expect(rows[0]!.querySelector('time')).toHaveTextContent(/ago/)
    expect(screen.getByTestId('palette-content-group')).toHaveTextContent('In conversations')
  })

  it('only searches the nodes that are checked in the sidebar filter, and leaves out the node name when there is just one', async () => {
    setupTwoNodes()
    usePrefs.setState({ sidebarNodes: [NODE_B] })
    rpc.handlers['session.search'] = () => ({ hits: [hit('sess_b1', 4, 'the <mark>parser</mark>')] })
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('parser')
    await settle(300)
    expect(searches().map((c) => c.nodeId)).toEqual([NODE_B])
    expect(screen.getAllByTestId('palette-hit')).toHaveLength(1)
    expect(screen.queryByTestId('palette-hit-node')).toBeNull()
    // 没勾的节点不算「没搜」，不提示
    expect(screen.queryByTestId('palette-skipped')).toBeNull()
  })

  it('skips offline nodes and nodes this browser has not unlocked, and says so at the bottom of the group', async () => {
    setupTwoNodes()
    useNodes.setState({ byId: { ...useNodes.getState().byId, [NODE_B]: nodeEntry({ id: NODE_B, name: 'desktop', conn: 'peer_offline' }) } })
    devices.data = [{ id: 'c'.repeat(32), name: 'nas', kind: 'daemon', platform: 'linux', created_at_ms: 1, online: true, paired_with: [] }]
    rpc.handlers['session.search'] = () => ({ hits: [hit('sess_a1', 9, 'a <mark>parser</mark> bug')] })
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('parser')
    await settle(300)
    expect(searches().map((c) => c.nodeId)).toEqual([NODE])
    expect(screen.getAllByTestId('palette-hit')).toHaveLength(1)
    expect(screen.getByTestId('palette-skipped')).toHaveTextContent('Not searched (offline or not unlocked): desktop and nas.')
    // 提示在这一组的最下面
    const items = within(screen.getByTestId('palette-content-group').parentElement!).getAllByRole('listitem')
    expect(items.at(-1)).toBe(screen.getByTestId('palette-skipped'))
  })

  it('with no node online it makes no request and only shows the hint (no endless spinner)', async () => {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'laptop', conn: 'hub_disconnected' }) } })
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('parser')
    await settle(300)
    expect(searches()).toHaveLength(0)
    expect(screen.queryByTestId('palette-searching')).toBeNull()
    expect(screen.queryByTestId('palette-no-content-match')).toBeNull()
    expect(screen.getByTestId('palette-skipped')).toHaveTextContent('laptop')
  })

  it('an error from one node does not affect the results of the others', async () => {
    setupTwoNodes()
    rpc.handlers['session.search'] = (_p, nodeId) => {
      if (nodeId === NODE) throw rpcError('internal')
      return { hits: [hit('sess_b1', 4, 'the <mark>parser</mark>')] }
    }
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('parser')
    await settle(300)
    expect(screen.getAllByTestId('palette-hit')).toHaveLength(1)
    expect(screen.getByTestId('palette-failed')).toHaveTextContent('Could not search laptop.')
    expect(screen.queryByTestId('palette-searching')).toBeNull()
  })

  it('a node that answers with something that is not a result list counts as failed; malformed hits are dropped', async () => {
    setupTwoNodes()
    rpc.handlers['session.search'] = (_p, nodeId) => (nodeId === NODE ? { nope: true } : { hits: [hit('ok', 1, 'a <mark>parser</mark>'), { session_id: 5 }, null, hit('bad_seq', -1, 'x')] })
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('parser')
    await settle(300)
    expect(screen.getAllByTestId('palette-hit')).toHaveLength(1)
    expect(screen.getByTestId('palette-failed')).toHaveTextContent('laptop')
  })

  it('says so when nothing matches', async () => {
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('zzzz')
    await settle(300)
    expect(screen.getByTestId('palette-no-content-match')).toHaveTextContent('No matches in conversations.')
  })

  it('drops responses to an earlier query: they never show up, whether they arrive before or after the new results', async () => {
    const calls: Array<{ query: string; gate: Deferred<unknown> }> = []
    rpc.handlers['session.search'] = (p) => {
      const gate = deferred<unknown>()
      calls.push({ query: (p as { query: string }).query, gate })
      return gate.promise
    }
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('foo')
    await settle(300)
    expect(calls.map((c) => c.query)).toEqual(['foo'])
    // foo 还没回，又输入了：旧的作废，立刻回到「正在搜索」
    typeQuery('foob')
    await act(async () => calls[0]!.gate.resolve({ hits: [hit('old1', 1, 'a <mark>foo</mark> from the old query')] }))
    expect(screen.queryByText(/old query/)).toBeNull()
    expect(screen.queryByTestId('palette-hit')).toBeNull()
    expect(screen.getByTestId('palette-searching')).toBeInTheDocument()
    await settle(300)
    expect(calls.map((c) => c.query)).toEqual(['foo', 'foob'])
    // 新的先回来、旧的（已经作废的）再回来，也不能盖掉新的
    typeQuery('fooba')
    await settle(300)
    expect(calls.map((c) => c.query)).toEqual(['foo', 'foob', 'fooba'])
    await act(async () => calls[2]!.gate.resolve({ hits: [hit('new1', 5, 'the <mark>fooba</mark>r result')] }))
    await act(async () => calls[1]!.gate.resolve({ hits: [hit('stale1', 3, 'a <mark>foob</mark> from the stale query')] }))
    const rows = screen.getAllByTestId('palette-hit')
    expect(rows).toHaveLength(1)
    expect(rows[0]).toHaveTextContent('title of new1')
    expect(screen.queryByText(/stale query/)).toBeNull()
  })

  it('renders the snippet as plain text: only the literal <mark> highlights, markup in the conversation stays text', async () => {
    rpc.handlers['session.search'] = () => ({
      hits: [
        // daemon 的约定：会话文字里的 < > & 已转义
        hit('sess_1', 1, 'run &lt;script&gt;alert(1)&lt;/script&gt; <mark>needle</mark> &lt;img src=x onerror=alert(2)&gt; &amp; more', { title: 'a <b>bold</b> title' }),
        // 对端不守约定、没转义的也只能是文字
        hit('sess_2', 2, '<script>alert(3)</script><img src=x onerror=alert(4)> <mark>needle</mark> <mark onclick="x">no'),
      ],
    })
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('needle')
    await settle(300)
    const [first, second] = screen.getAllByTestId('palette-hit-snippet')
    expect(first).toHaveTextContent('run <script>alert(1)</script> needle <img src=x onerror=alert(2)> & more')
    expect(second).toHaveTextContent('<script>alert(3)</script><img src=x onerror=alert(4)> needle <mark onclick="x">no')
    // 页面里没有因此多出元素：只有高亮用的 <mark>
    expect(document.querySelector('script')).toBeNull()
    expect(document.querySelector('img')).toBeNull()
    expect(first!.querySelectorAll('mark')).toHaveLength(1)
    expect(first!.querySelector('mark')).toHaveTextContent('needle')
    expect(second!.querySelectorAll('mark')).toHaveLength(1)
    expect(first!.innerHTML).not.toMatch(/<(script|img)/)
    expect(screen.getAllByTestId('palette-hit')[0]).toHaveTextContent('a <b>bold</b> title')
    expect(screen.getAllByTestId('palette-hit')[0]!.querySelector('b')).toBeNull()
  })

  it('opens the session at the hit when one is chosen (click), and closes the palette', async () => {
    useSessions.setState({ list: { sess_1: { ...summary({ id: 'sess_1', title: 'T' }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
    rpc.handlers['session.search'] = () => ({ hits: [hit('sess_1', 42, 'a <mark>needle</mark>')] })
    const onOpenChange = vi.fn()
    render(<CommandPalette open onOpenChange={onOpenChange} />)
    typeQuery('needle')
    await settle(300)
    fireEvent.click(screen.getByTestId('palette-hit'))
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/s/$session', params: { node: 'nnnnnnnn', session: 'sess_1' }, search: { seq: 42 } })
    expect(onOpenChange).toHaveBeenCalledWith(false)
  })

  it('works from the keyboard: ↓ ↑ move through local matches and hits, Enter opens the selected one', async () => {
    setupTwoNodes()
    useSessions.setState({ list: { sess_t: { ...summary({ id: 'sess_t', title: 'needle in a title' }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
    rpc.handlers['session.search'] = (_p, nodeId) => ({ hits: nodeId === NODE ? [hit('h1', 11, 'one <mark>needle</mark>'), hit('h2', 22, 'two <mark>needle</mark>')] : [] })
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('needle')
    await settle(300)
    const active = () => document.getElementById(input().getAttribute('aria-activedescendant')!)!
    // 默认选中第一条：本地的标题匹配
    expect(active()).toHaveTextContent('needle in a title')
    expect(active()).toHaveAttribute('aria-current', 'true')
    press('ArrowDown')
    expect(active()).toHaveTextContent('title of h1')
    press('ArrowDown')
    press('ArrowDown')
    // 到底了，回到第一条
    expect(active()).toHaveTextContent('needle in a title')
    press('ArrowUp')
    expect(active()).toHaveTextContent('title of h2')
    expect(screen.getAllByRole('button').filter((b) => b.getAttribute('aria-current') === 'true')).toHaveLength(1)
    press('Enter')
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/s/$session', params: { node: 'nnnnnnnn', session: 'h2' }, search: { seq: 22 } })
  })

  it('Enter opens the first local match by default, as before; and does nothing while an input method is composing', async () => {
    useSessions.setState({ list: { sess_t: { ...summary({ id: 'sess_t', title: 'needle in a title' }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
    render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('needle')
    press('Enter', { isComposing: true })
    press('ArrowDown', { isComposing: true })
    expect(navigate).not.toHaveBeenCalled()
    press('Enter')
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/s/$session', params: { node: 'nnnnnnnn', session: 'sess_t' } })
  })

  it('starts from a blank query after it has been closed, and does not search while closed', async () => {
    const { rerender } = render(<CommandPalette open onOpenChange={() => {}} />)
    typeQuery('parser')
    rerender(<CommandPalette open={false} onOpenChange={() => {}} />)
    await settle(1000)
    expect(searches()).toHaveLength(0)
    rerender(<CommandPalette open onOpenChange={() => {}} />)
    expect(input()).toHaveValue('')
  })
})
