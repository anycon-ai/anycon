// 「记一项」的入口和输入条（spec/web.md § 14.5）：第一行标题、其余正文；发送后留着条并清空；撤销（离线也行）；校验提示；
// 多个节点时的缺省；命令面板的快速记；会话页的引用预填。
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
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
import { Markdown } from '@/components/Markdown'
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
    await waitFor(() => expect(usePrefs.getState().lastTodoNode).toBe(B))
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
  it('quoted text: the title is the first line without Markdown markers; the body says where it came from, quotes the text and links back with a relative path', () => {
    const out = captureFromQuote('fix the flaky test\nit fails on CI', '/n/a/s/b?seq=4')
    const [title, ...rest] = out.split('\n')
    expect(title).toBe('fix the flaky test')
    const body = rest.join('\n')
    expect(body).toContain('Quoted from a session message:')
    expect(body).toContain('> fix the flaky test\n> it fails on CI')
    expect(body).toContain('[Open the message](/n/a/s/b?seq=4)')
    expect([...captureFromQuote('w'.repeat(300)).split('\n')[0]!].length).toBeLessThanOrEqual(121)
  })

  it('hostile text: titles never start with a heading marker or a fence, injected links are neutralised, markup stays quoted', () => {
    expect(captureFromQuote('## Summary\nbody').split('\n')[0]).toBe('Summary')
    expect(captureFromQuote('```ts\nconst x = 1\n```').split('\n')[0]).toBe('const x = 1')
    expect(captureFromQuote('- [ ] **bold** item').split('\n')[0]).toBe('[ ] bold item')
    const evil = captureFromQuote('[Approve here](https://evil.example/x) <script>alert(1)</script>', '/n/a/s/b?seq=1')
    expect(evil.split('\n')[0]).toBe('Approve here (https://evil.example/x) <script>alert(1)</script>')
    const body = evil.split('\n').slice(1).join('\n')
    expect(body.replaceAll('\u200b', '')).toContain('> \\[Approve here\\](https://evil.example/x) \\<script>alert(1)\\</script>')
    expect(body).not.toMatch(/(^|\n)\[Approve here\]\(/)
    // 代码块：用比里面任何一串反引号都长的围栏，里面的内容原样
    const code = captureFromQuote('```\n[x](https://evil.example)\n```\n', '/n/a/s/b?seq=1')
    expect(code).toContain('````\n```\n[x](https://evil.example)\n```\n````')
    // 只有记号的文字有个像样的标题
    expect(captureFromQuote('---').split('\n')[0]).not.toBe('')
  })

  it('a quoted excerpt renders with no clickable link except “open the message”: bare URLs, www hosts, email addresses and escaped link syntax are all inert', () => {
    const quote = 'see https://evil.example/x and www.evil.example or mail a@evil.example, also \\[a\\](http://x.example) and [b](https://y.example); _https://evil.example/u foo_https://evil.example/v _www.evil.example/w'
    const body = captureFromQuote(quote, messageLink('a'.repeat(32), 'sess_1', 3)).split('\n').slice(1).join('\n')
    const { container } = render(<Markdown text={body} />)
    const links = [...container.querySelectorAll('a')]
    expect(links.map((a) => a.getAttribute('href'))).toEqual([`/n/${'a'.repeat(32)}/s/sess_1?seq=3`])
    // 原文还在、能读（零宽空格不影响显示）
    expect(container.textContent?.replaceAll('\u200b', '')).toContain('https://evil.example/x')
  })

  it('the back link uses the full node id and encodes both ids, so nothing can break out of the Markdown link', () => {
    expect(messageLink('node id', 'a)b](c', 5)).toBe('/n/node%20id/s/a)b%5D(c?seq=5')
    const body = captureFromQuote('x', messageLink('n', 'a b)', 5))
    expect(body).toContain('(/n/n/s/a%20b)?seq=5)')
  })

  it('the message menu pre-fills the bar and does not add anything by itself', () => {
    openCaptureFromMessage('check the migration', 'aaaaaaaa', 'sess_1', 7)
    const { open, text } = useCapture.getState()
    expect(open).toBe(true)
    expect(text).toContain(`](${messageLink('aaaaaaaa', 'sess_1', 7)})`)
    expect(messageLink('aaaaaaaa', 'sess_1', 7)).toBe('/n/aaaaaaaa/s/sess_1?seq=7')
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

describe('sending is guarded', () => {
  it('two quick Enters create one item, and the box is read-only while the write is in flight', async () => {
    const user = userEvent.setup()
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture())
    const input = screen.getByTestId('capture-input')
    await user.type(input, 'only once')
    await user.keyboard('{Enter}{Enter}')
    await waitFor(() => expect(added()).toHaveLength(1))
    await flush()
    expect(added()).toHaveLength(1)
  })

  it('text typed right after sending is a new draft: it is not wiped by the previous send finishing', async () => {
    const user = userEvent.setup()
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture())
    await user.type(screen.getByTestId('capture-input'), 'first{Enter}')
    await waitFor(() => expect(screen.getByTestId('capture-input')).not.toHaveAttribute('readonly'))
    await user.type(screen.getByTestId('capture-input'), 'second')
    await flush()
    expect(screen.getByTestId('capture-input')).toHaveValue('second')
  })

  it('a failed write keeps the text, the project and the due time, and shows the reason; sending again then creates exactly one item', async () => {
    const user = userEvent.setup()
    vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const append = vi.spyOn(db, 'appendOutbox').mockRejectedValueOnce(new DOMException('quota', 'QuotaExceededError'))
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture())
    await user.type(screen.getByTestId('capture-input'), 'keep me')
    await user.type(screen.getByTestId('capture-project'), 'api')
    await user.click(screen.getByTestId('capture-send'))
    expect(await screen.findByTestId('capture-error')).toHaveTextContent(/storage|device/i)
    expect(screen.getByTestId('capture-input')).toHaveValue('keep me')
    expect(screen.getByTestId('capture-project')).toHaveValue('api')
    expect(added()).toHaveLength(0)
    await user.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect(added()).toHaveLength(1))
    expect(append).toHaveBeenCalledTimes(2)
    expect(screen.getByTestId('capture-input')).toHaveValue('')
  })

  it('a tab in the first line becomes a space instead of an error', async () => {
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture({ text: 'one\ttwo' }))
    await userEvent.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect(added()).toHaveLength(1))
    expect(added()[0]!.title).toBe('one two')
  })
})

describe('the capture bar and the /capture page (nits)', () => {
  it('Esc closes the desktop bar wherever the focus is, but leaves a dialog’s Esc alone', async () => {
    const user = userEvent.setup()
    render(
      <>
        <button data-testid="elsewhere">elsewhere</button>
        <CaptureHost />
      </>,
    )
    act(() => useCapture.getState().openCapture())
    screen.getByTestId('elsewhere').focus()
    await user.keyboard('{Escape}')
    expect(useCapture.getState().open).toBe(false)
    act(() => useCapture.getState().openCapture())
    const dialog = document.createElement('div')
    dialog.setAttribute('role', 'dialog')
    document.body.append(dialog)
    screen.getByTestId('elsewhere').focus()
    await user.keyboard('{Escape}')
    expect(useCapture.getState().open).toBe(true)
    dialog.remove()
  })

  it('the bar floats over the page instead of pushing it down', () => {
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture())
    expect(screen.getByTestId('capture-bar').className).toMatch(/absolute/)
  })

  it('while the /capture page is showing its own form, openCapture does not open a second one', () => {
    useCapture.getState().setInline(true)
    render(<CaptureHost />)
    act(() => useCapture.getState().openCapture({ text: 'x' }))
    expect(screen.queryByTestId('capture-bar')).toBeNull()
    expect(useCapture.getState().text).toBe('x')
    useCapture.getState().setInline(false)
  })
})

describe('names are cleaned (nit)', () => {
  it('node names in the select and the “will go to” line lose control and bidi characters', () => {
    useNodes.setState({ byId: { ...useNodes.getState().byId, [B]: nodeEntry({ id: B, name: `be${String.fromCharCode(0x202e)}ta${String.fromCharCode(7)}`, info: info(true), conn: 'ready' }) }, order: [A, B, C] })
    render(<CaptureForm />)
    const text = screen.getByTestId('capture-target').textContent ?? ''
    expect(text).not.toContain(String.fromCharCode(0x202e))
    expect(text).not.toContain(String.fromCharCode(7))
  })
})

describe('a quoted excerpt can never become a link, an image or raw HTML (character references and a randomized hostile search)', () => {
  const backLink = messageLink('a'.repeat(32), 'sess_1', 3)
  const renderBody = (quote: string) => {
    const body = captureFromQuote(quote, backLink).split('\n').slice(1).join('\n')
    return render(<Markdown text={body} />).container
  }
  // （代码块自己的界面——复制按钮和它的图标——不算原文里冒出来的元素）
  const FORBIDDEN = 'img, script, iframe, style, object, embed, form, input, video, audio, textarea, select, link, meta, base'
  const onlyBackLink = (container: HTMLElement, quote: string): void => {
    const hrefs = [...container.querySelectorAll('a')].map((a) => a.getAttribute('href'))
    expect(hrefs, `links from ${JSON.stringify(quote)}`).toEqual([backLink])
    expect([...container.querySelectorAll(FORBIDDEN)].map((e) => e.tagName), `raw elements from ${JSON.stringify(quote)}`).toEqual([])
  }

  it.each([
    'http&#115;://evil.example/x',
    '&#104;ttps://evil.example/x',
    'h&#x74;tps://evil.example/x',
    'www&#46;evil.example',
    'user&#64;evil.example',
    'https&colon;//evil.example',
    '&lt;a href="https://evil.example"&gt;x&lt;/a&gt;',
  ])('character reference trick %s stays inert', (quote) => {
    onlyBackLink(renderBody(quote), quote)
  })

  it('400 generated hostile strings (fixed seed): the only link is the back link, no image, no raw element', () => {
    // mulberry32：固定种子，每次跑出同样的 400 个字符串
    let a = 20241120
    const rnd = (): number => {
      a = (a + 0x6d2b79f5) | 0
      let t = Math.imul(a ^ (a >>> 15), 1 | a)
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296
    }
    const alphabet = [...'abcxyzHTPSW0123456789:/.@_&#;\\[]()<>*~` \n-!=|']
    const fragments = ['http', 'https', 'www', 'mailto', 'ftp', '&#', '&#x', '](', '<', '[x]:', '://', 'evil.example', 'a@b.co', '![', '```', '~~~', '> ', '1. ', '# ', '<http://x.test>', '[a]: http://r.test', '[^1]', '\n\n', '    ', '&#104;', '&#x74;', '&#115;', '&#58;', '&#46;', '&#64;', '&colon;', '&commat;', '&period;', '&#x2f;']
    let cases = 0
    for (let i = 0; i < 400; i++) {
      const len = 5 + Math.floor(rnd() * 55)
      let quote = ''
      for (let k = 0; k < len; k++) quote += rnd() < 0.3 ? fragments[Math.floor(rnd() * fragments.length)]! : alphabet[Math.floor(rnd() * alphabet.length)]!
      const container = renderBody(quote)
      onlyBackLink(container, quote)
      cleanup()
      cases++
    }
    expect(cases).toBe(400)
  }, 60_000)
})
