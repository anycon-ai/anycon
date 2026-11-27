// 会话消息里的本地文件链接和图片（web.md § 4.6「本地文件链接」，ADR-62）。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { Markdown } from '@/components/Markdown'
import { resetDb } from '@/db'
import { clearTickets } from '@/features/preview/ticket'
import type { FsReadParams, PreviewLaunch, PreviewOpenParams } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { installFakeApi } from '@/test/fakeApi'
import { serveFs } from '@/test/fakeFs'
import { FakeRpc } from '@/test/fakeRpc'
import { installIntersectionObserver, type FakeIo } from '@/test/intersection'
import { LABEL, NODE_KEY, openResult, PREVIEW_TEMPLATE, previewNodeInfo, seedDeviceToken } from '@/test/preview'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { useSessionLocalFiles } from './localFilesContext'
import { SessionFilesProvider, useSessionFiles, type SessionFilesOptions } from './LocalFiles'

const NODE = 'b'.repeat(32)
const PROJECTS = [{ name: 'hive', path: '/Users/me/anycon/hive' }]
let rpc: FakeRpc
let io: FakeIo | null = null

/** 消息组件那一侧：从 context 里拿本地文件处理，交给 Markdown。 */
function Message({ text }: { text: string }) {
  return <Markdown text={text} local={useSessionLocalFiles()} />
}

function Session({ text, ...over }: { text: string } & Partial<SessionFilesOptions>) {
  const local = useSessionFiles({ nodeId: NODE, nodeParam: 'nnnnnnnn', sessionId: 'sess_a', dirTemplate: PREVIEW_TEMPLATE, workDir: 'hive', projects: PROJECTS, primaryRoot: '/Users/me/anycon', ...over })
  return (
    <SessionFilesProvider value={local}>
      <Message text={text} />
    </SessionFilesProvider>
  )
}

function fakeTab() {
  return { opener: {} as unknown, document: { title: '', body: { textContent: '' } }, location: { replace: vi.fn() }, close: vi.fn() }
}

/** 启动参数（URL fragment）里的 JSON。 */
function launchOf(url: string): PreviewLaunch {
  const b64 = new URL(url).hash.slice(1).replace(/-/g, '+').replace(/_/g, '/')
  const bin = atob(b64 + '='.repeat((4 - (b64.length % 4)) % 4))
  return JSON.parse(new TextDecoder().decode(Uint8Array.from(bin, (c) => c.charCodeAt(0)))) as PreviewLaunch
}

beforeEach(async () => {
  await resetDb()
  clearTickets()
  await seedDeviceToken()
  rpc = new FakeRpc()
  setRpc(rpc)
  useUi.setState({ toasts: [] })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, publicKey: NODE_KEY, info: previewNodeInfo() }) }, order: [NODE], loaded: true })
})
afterEach(() => {
  setRpc(null)
  io?.restore()
  io = null
  vi.restoreAllMocks()
})

describe('links to files on the node', () => {
  it('a file in a project (absolute or relative) is an in-app link to Files, not a hub URL', () => {
    render(<Session text={'[notes](/Users/me/anycon/hive/docs/notes.md) and [rel](docs/a%20b.txt) and [dir](/Users/me/anycon/hive/docs)'} />)
    const links = screen.getAllByTestId('md-local-link')
    expect(links.map((a) => a.getAttribute('data-path'))).toEqual(['hive/docs/notes.md', 'hive/docs/a b.txt', 'hive/docs'])
    expect(links[0]).toHaveAttribute('href', '/n/nnnnnnnn/files/hive/docs/notes.md')
    expect(links[0]).not.toHaveAttribute('target')
    expect(links[0]).toHaveAttribute('title', 'Open in Files')
  })

  it('an HTML report opens in a new tab through the folder preview; its href still points at Files', async () => {
    rpc.handlers['preview.open'] = () => openResult({ port: undefined, dir: 'hive', session_id: 'sess_a' })
    installFakeApi({ 'POST /api/preview/tokens': () => ({ token: 'hub-token', origin: `https://${LABEL}.preview.test`, expires_at_ms: Date.now() + 3_600_000 }) })
    const tab = fakeTab()
    const open = vi.spyOn(window, 'open').mockReturnValue(tab as unknown as Window)
    // micromark 把非 ASCII 字符编码成 %XX，和 agent 真写出来的一样
    render(<Session text={'[查看完整报告](/Users/me/anycon/hive/qa/测试报告.html)'} />)
    const link = screen.getByTestId('md-local-web')
    expect(link).toHaveAttribute('href', `/n/nnnnnnnn/files/hive/qa/${encodeURIComponent('测试报告.html')}`)
    expect(link).toHaveAttribute('title', 'Open as a web page in a new tab')
    await userEvent.click(link)
    expect(open).toHaveBeenCalledWith('', '_blank')
    expect(tab.opener).toBeNull()
    await waitFor(() => expect(tab.location.replace).toHaveBeenCalledTimes(1))
    expect(navigate).not.toHaveBeenCalled()
    expect(rpc.calls.find((c) => c.method === 'preview.open')!.params).toEqual({ dir: 'hive', session_id: 'sess_a' } satisfies PreviewOpenParams)
    const url = tab.location.replace.mock.calls[0]![0] as string
    expect(url.startsWith(`https://${LABEL}.preview.test/__anycon/enter#`)).toBe(true)
    const launch = launchOf(url)
    expect(launch.dir).toBe('hive')
    expect(launch.path).toBe(`/qa/${encodeURIComponent('测试报告.html')}`)

    // 第二次点：票据在缓存里，直接同步打开，不再开空白页
    await userEvent.click(link)
    expect(open).toHaveBeenLastCalledWith(expect.stringContaining(`https://${LABEL}.preview.test/__anycon/enter#`), '_blank', 'noopener,noreferrer')
    expect(rpc.calls.filter((c) => c.method === 'preview.open')).toHaveLength(1)
  })

  it('a modified click on an HTML link is left to the browser (Files in a new tab), no preview is opened', async () => {
    const open = vi.spyOn(window, 'open')
    render(<Session text={'[r](/Users/me/anycon/hive/r.html)'} />)
    const link = screen.getByTestId('md-local-web')
    link.addEventListener('click', (e) => e.preventDefault()) // jsdom 不导航
    const user = userEvent.setup()
    await user.keyboard('{Meta>}')
    await user.click(link)
    await user.keyboard('{/Meta}')
    expect(open).not.toHaveBeenCalled()
    expect(rpc.calls.filter((c) => c.method === 'preview.open')).toHaveLength(0)
  })

  it('without folder previews (no hub preview domain, older node) an HTML report goes to Files', () => {
    render(<Session text={'[r](/Users/me/anycon/hive/r.html)'} dirTemplate={null} />)
    expect(screen.queryByTestId('md-local-web')).toBeNull()
    expect(screen.getByTestId('md-local-link')).toHaveAttribute('data-path', 'hive/r.html')
  })

  it('a path outside every project is not a link: it explains why and copies the path', async () => {
    const writeText = vi.fn(async () => {})
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })
    render(<Session text={'[查看完整报告](/Users/me/Downloads/qa/测试报告.html)'} />)
    expect(document.querySelector('a')).toBeNull()
    const outside = screen.getByTestId('md-local-outside')
    expect(outside).toHaveTextContent('查看完整报告')
    expect(outside.getAttribute('title')).toBe("This file isn’t in a project on this node, so anycon can’t open it. Click to copy the path: /Users/me/Downloads/qa/测试报告.html")
    await userEvent.click(outside)
    expect(writeText).toHaveBeenCalledWith('/Users/me/Downloads/qa/测试报告.html')
    await waitFor(() => expect(useUi.getState().toasts.map((x) => x.text)).toContain('Path copied'))
  })

  it('file: URLs and ~ paths are handled the same way; remote links are untouched', () => {
    render(<Session text={'[a](file:///Users/me/anycon/hive/a.txt) [b](file:///etc/hosts) [c](~/x.html) [d](https://example.com/x) [e](#top)'} />)
    expect(screen.getByTestId('md-local-link')).toHaveAttribute('data-path', 'hive/a.txt')
    expect(screen.getAllByTestId('md-local-outside').map((b) => b.getAttribute('data-path'))).toEqual(['/etc/hosts', '~/x.html'])
    const remote = screen.getByRole('link', { name: 'd' })
    expect(remote).toHaveAttribute('href', 'https://example.com/x')
    expect(remote).toHaveAttribute('target', '_blank')
    // `#锚点`：会话里没有可去的地方，只是文字
    expect(screen.queryByRole('link', { name: 'e' })).toBeNull()
    expect(screen.getByText('e').tagName).toBe('SPAN')
  })
})

describe('images on the node', () => {
  it('local images load through fs.read; ones outside the projects show their alt text', async () => {
    io = installIntersectionObserver()
    const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52])
    serveFs(rpc, { 'hive/shots/a.png': PNG })
    render(<Session text={'![shot](shots/a.png)\n\n![far](/Users/me/Desktop/b.png)\n\n![remote](https://evil.example/x.png)'} />)
    expect(screen.getByTestId('md-local-image')).toBeInTheDocument()
    act(() => io!.intersect())
    await waitFor(() => expect(rpc.calls.filter((c) => c.method === 'fs.read').map((c) => (c.params as FsReadParams).path)).toContain('hive/shots/a.png'))
    expect(screen.getByText('far').tagName).toBe('SPAN')
    expect(screen.getByTestId('md-image-placeholder')).toHaveAttribute('data-host', 'evil.example')
  })
})

describe('without a session (todos, rules, the file browser)', () => {
  it('a file: link is never rendered as a clickable href', () => {
    render(<Markdown text={'[x](file:///etc/passwd) [y](C:\\Users\\me\\a.html)'} />)
    expect(document.querySelector('a')).toBeNull()
    expect(screen.getByText('x').tagName).toBe('SPAN')
  })
})
