// 文件预览：文本（高亮 / 大文件不高亮 / 截断提示）、Markdown（渲染、源码、相对链接和图片）、图片（png / svg / 太大 / 出错）、二进制。
import { useState } from 'react'
import { configure, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import type { FsEntry, FsReadParams } from '@/protocol'
import { nodeEntry, project } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { serveFs, type FakeFile } from '@/test/fakeFs'
import { installIntersectionObserver, type FakeIo } from '@/test/intersection'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'

// 数据是异步读的（渲染整页、shiki 都不快）；机器忙的时候默认的 1 秒等待和 5 秒用例超时不够
configure({ asyncUtilTimeout: 5000 })
vi.setConfig({ testTimeout: 20_000 })

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { FilesPage } from './FilesPage'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc
let io: FakeIo | null = null
let go: (path: string) => void = () => {}

const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52])
const MiB = 1024 * 1024

const MARKDOWN = [
  '# Guide',
  '',
  'Read [setup](guide/setup.md#install), the [readme](../README.md), [outside](../../../x.md), [site](https://example.com/docs) or [top](#top).',
  '',
  '![local pic](img/pic.png)',
  '',
  '![escape](../../../evil.png)',
  '',
  '![remote](https://evil.example/x.png?d=secret)',
  '',
  '```ts',
  'const a = 1',
  '```',
].join('\n')

const FILES: Record<string, FakeFile> = {
  'app/src/main.ts': 'export const answer = 42\nexport const other = 1\n',
  'app/notes.xyz': 'plain notes\nsecond line',
  'app/empty.txt': '',
  'app/data.bin': new Uint8Array([0, 1, 2, 3]),
  'app/blob.dat': new Uint8Array(1234),
  'app/logo.png': PNG,
  'app/logo.svg': '<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script><circle id="from-the-file" r="1"/></svg>',
  'app/docs/index.md': MARKDOWN,
  'app/docs/img/pic.png': PNG,
  'app/docs/guide/setup.md': '# Setup',
  'app/README.md': '# Readme',
}

function Harness({ initial }: { initial: string }) {
  const [path, setPath] = useState(initial)
  go = setPath
  return <FilesPage path={path} />
}

function mount(initial: string) {
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, projects: [project({ name: 'app' })] }) }, order: [NODE], loaded: true })
  return render(<Harness initial={initial} />)
}

const reads = (): FsReadParams[] => rpc.calls.filter((c) => c.method === 'fs.read').map((c) => c.params as FsReadParams)

beforeEach(() => {
  navigate.mockReset()
  navigate.mockImplementation((o: { params: { _splat: string } }) => go(o.params._splat))
  rpc = new FakeRpc()
  serveFs(rpc, FILES)
  setRpc(rpc)
  useSessions.setState({ list: {} })
  usePrefs.setState({ filesShowHidden: false })
})
afterEach(async () => {
  setRpc(null)
  io?.restore()
  io = null
  await i18n.changeLanguage('en')
})

describe('text preview', () => {
  it('shows the file with line numbers and highlights it by extension', async () => {
    mount('app/src/main.ts')
    const code = await screen.findByTestId('file-code')
    expect(code).toHaveTextContent('export const answer = 42')
    expect(screen.getByTestId('file-gutter').textContent).toBe('1\n2')
    expect(screen.getByTestId('file-name')).toHaveTextContent('main.ts')
    expect(screen.getByTestId('file-size')).toHaveTextContent('48 B')
    await waitFor(() => expect(screen.getByTestId('file-code')).toHaveAttribute('data-highlighted', 'true'), { timeout: 10_000 })
    expect(screen.getByTestId('file-code').querySelector('pre.shiki')).not.toBeNull()
    expect(screen.getByTestId('file-code')).toHaveTextContent('export const answer = 42')
    // 文本读 1 MiB 为止
    expect(reads()).toEqual([{ path: 'app/src/main.ts', max_bytes: MiB }])
  }, 20_000)

  it('shows unknown file types as plain text, without highlighting', async () => {
    mount('app/notes.xyz')
    const code = await screen.findByTestId('file-code')
    expect(code).toHaveTextContent('plain notes')
    await new Promise((r) => setTimeout(r, 50))
    expect(code).toHaveAttribute('data-highlighted', 'false')
  })

  it('says when a file is empty', async () => {
    mount('app/empty.txt')
    expect(await screen.findByTestId('files-empty-file')).toHaveTextContent('This file is empty')
  })

  it('shows only the first 1 MiB of a bigger file and says so', async () => {
    const line = `${'x'.repeat(99)}\n`
    serveFs(rpc, { 'app/big.log': line.repeat(15_000) }) // 1.5 MB
    mount('app/big.log')
    const notice = await screen.findByTestId('files-truncated')
    expect(notice).toHaveTextContent('Showing only the first 1.0 MiB of 1.4 MiB.')
    expect(screen.getByTestId('file-code')).toBeInTheDocument()
    expect(reads()[0]).toEqual({ path: 'app/big.log', max_bytes: MiB })
  })

  it('does not show the truncation notice for a file that was read completely', async () => {
    mount('app/src/main.ts')
    await screen.findByTestId('file-code')
    expect(screen.queryByTestId('files-truncated')).toBeNull()
  })

  it('skips highlighting for a large file, to keep the page responsive, and says so', async () => {
    serveFs(rpc, { 'app/huge.ts': 'const a = 1\n'.repeat(25_000) }) // 300 KB
    mount('app/huge.ts')
    const code = await screen.findByTestId('file-code')
    expect(await screen.findByTestId('files-no-highlight')).toHaveTextContent('Syntax highlighting is off for large files.')
    await new Promise((r) => setTimeout(r, 50))
    expect(code).toHaveAttribute('data-highlighted', 'false')
    expect(screen.getByTestId('file-gutter').textContent?.split('\n')).toHaveLength(25_000)
  })
})

describe('binary files', () => {
  it('says it is a binary file and how many bytes', async () => {
    mount('app/data.bin')
    expect(await screen.findByTestId('files-binary')).toHaveTextContent('Binary file, 4 bytes')
    expect(screen.queryByTestId('file-code')).toBeNull()
  })

  it('groups the digits of a bigger number, and follows the language', async () => {
    mount('app/blob.dat')
    expect(await screen.findByTestId('files-binary')).toHaveTextContent('Binary file, 1,234 bytes')
    expect(screen.getByTestId('file-size')).toHaveTextContent('1.2 KiB')
    await i18n.changeLanguage('zh')
    expect(screen.getByTestId('files-binary')).toHaveTextContent('二进制文件，1,234 字节')
  })
})

describe('image preview', () => {
  it('fetches the raw bytes as base64 and shows them as a blob: image', async () => {
    const create = vi.spyOn(URL, 'createObjectURL')
    mount('app/logo.png')
    const img = await screen.findByTestId('file-image')
    expect(img.getAttribute('src')).toMatch(/^blob:/)
    expect(img).toHaveAttribute('alt', 'logo.png')
    expect(reads()).toEqual([{ path: 'app/logo.png', max_bytes: 10 * MiB, encoding: 'base64' }])
    const blob = create.mock.calls[0]![0] as Blob
    expect(blob.type).toBe('image/png')
    expect(blob.size).toBe(PNG.length)
    expect(screen.getByTestId('file-preview')).toHaveAttribute('data-kind', 'image')
  })

  it('releases the blob URL when the file is closed', async () => {
    const revoke = vi.spyOn(URL, 'revokeObjectURL')
    const { unmount } = mount('app/logo.png')
    const src = (await screen.findByTestId('file-image')).getAttribute('src')
    unmount()
    expect(revoke).toHaveBeenCalledWith(src)
  })

  it('shows an SVG through <img> with the svg MIME type and never puts it into the DOM', async () => {
    const create = vi.spyOn(URL, 'createObjectURL')
    mount('app/logo.svg')
    const img = await screen.findByTestId('file-image')
    expect(img.tagName).toBe('IMG')
    expect(img.getAttribute('src')).toMatch(/^blob:/)
    expect((create.mock.calls[0]![0] as Blob).type).toBe('image/svg+xml')
    expect(reads()[0]).toMatchObject({ path: 'app/logo.svg', encoding: 'base64' })
    expect(document.querySelector('script')).toBeNull()
    expect(document.querySelector('#from-the-file')).toBeNull()
    expect(screen.queryByTestId('file-code')).toBeNull()
  })

  it('does not fetch an image the folder listing already shows to be over the limit, and says it is too large', async () => {
    serveFs(rpc, { 'app/huge.png': new Uint8Array(11 * MiB) })
    mount('app/huge.png')
    const msg = await screen.findByTestId('files-image-too-large')
    expect(msg).toHaveTextContent('This image is too large to preview (11 MiB). The limit is 10 MiB.')
    expect(reads()).toEqual([])
    expect(screen.queryByTestId('file-image')).toBeNull()
  })

  it('also says so when only the read comes back truncated (the listing was out of date)', async () => {
    // 列表里的大小是旧的（100 字节），读回来才发现文件有 11 MiB，只给了前一段
    const list = rpc.handlers['fs.list']!
    rpc.handlers['fs.list'] = async (p, n) => ({ entries: ((await list(p, n)) as { entries: FsEntry[] }).entries.map((e) => (e.name === 'logo.png' ? { ...e, bytes: 100 } : e)) })
    rpc.handlers['fs.read'] = () => ({ text: '', binary: true, truncated: true, bytes: 11 * MiB, data_base64: 'iVBORw==' })
    mount('app/logo.png')
    expect(await screen.findByTestId('files-image-too-large')).toHaveTextContent('too large to preview (11 MiB)')
    expect(reads()).toHaveLength(1)
    expect(screen.queryByTestId('file-image')).toBeNull()
  })

  it('shows a read error with a retry that loads the image', async () => {
    const user = userEvent.setup()
    const ok = rpc.handlers['fs.read']!
    rpc.handlers['fs.read'] = () => {
      throw new Error('boom')
    }
    mount('app/logo.png')
    expect(await screen.findByTestId('files-error')).toHaveTextContent('boom')
    rpc.handlers['fs.read'] = ok
    await user.click(screen.getByTestId('files-retry'))
    expect(await screen.findByTestId('file-image')).toBeInTheDocument()
  })

  it('says the image cannot be displayed when the browser cannot decode it', async () => {
    mount('app/logo.png')
    fireEvent.error(await screen.findByTestId('file-image'))
    expect(await screen.findByTestId('files-image-failed')).toHaveTextContent('This image could not be displayed.')
  })

  it('tells the user to update the node when its daemon does not know base64 reads', async () => {
    rpc.handlers['fs.read'] = () => ({ text: '', binary: true, truncated: false, bytes: 16 })
    mount('app/logo.png')
    expect(await screen.findByTestId('files-error')).toHaveTextContent('The node is too old to send images. Update the anycon daemon on it.')
  })
})

describe('Markdown preview', () => {
  it('renders Markdown by default and can switch to the source and back', async () => {
    const user = userEvent.setup()
    mount('app/README.md')
    const md = await screen.findByTestId('files-markdown')
    expect(within(md).getByRole('heading', { name: 'Readme' })).toBeInTheDocument()
    expect(screen.queryByTestId('file-code')).toBeNull()
    await user.click(screen.getByTestId('files-md-source'))
    expect(screen.getByTestId('file-code')).toHaveTextContent('# Readme')
    expect(screen.queryByTestId('files-markdown')).toBeNull()
    await user.click(screen.getByTestId('files-md-rendered'))
    expect(screen.getByRole('heading', { name: 'Readme' })).toBeInTheDocument()
    expect(reads()).toHaveLength(1) // 切换视图不重新读
  })

  it('turns relative links into in-app links to the file browser; external links open in a new tab', async () => {
    mount('app/docs/index.md')
    const md = await screen.findByTestId('files-markdown')
    const setup = within(md).getByRole('link', { name: 'setup' })
    expect(setup).toHaveAttribute('href', '/n/nnnnnnnn/files/app/docs/guide/setup.md')
    expect(setup).not.toHaveAttribute('target')
    expect(within(md).getByRole('link', { name: 'readme' })).toHaveAttribute('href', '/n/nnnnnnnn/files/app/README.md')
    const site = within(md).getByRole('link', { name: 'site' })
    expect(site).toHaveAttribute('href', 'https://example.com/docs')
    expect(site).toHaveAttribute('target', '_blank')
    expect(site).toHaveAttribute('rel', 'noopener noreferrer')
    // 走出项目目录的链接和 `#锚点` 不是链接
    expect(within(md).queryByRole('link', { name: 'outside' })).toBeNull()
    expect(within(md).queryByRole('link', { name: 'top' })).toBeNull()
    expect(within(md).getByText('outside')).toBeInTheDocument()
  })

  it('loads a relative image from the node once it scrolls into view, and never asks the browser for any other address', async () => {
    io = installIntersectionObserver()
    mount('app/docs/index.md')
    const md = await screen.findByTestId('files-markdown')
    const slot = await within(md).findByTestId('md-local-image')
    expect(slot.querySelector('img')).toBeNull()
    expect(reads().map((r) => r.path)).toEqual(['app/docs/index.md']) // 没进视口之前不取
    io.intersect(slot)
    await waitFor(() => expect(slot.querySelector('img')?.getAttribute('src')).toMatch(/^blob:/))
    expect(reads().at(-1)).toEqual({ path: 'app/docs/img/pic.png', max_bytes: 10 * MiB, encoding: 'base64' })
    // 走出项目目录的图片：不取，只显示说明文字
    expect(within(md).getByText('escape')).toBeInTheDocument()
    // 远程图片：和消息里一样先是占位，点击才加载；这里没有点，所以什么请求都没有
    const placeholder = within(md).getByTestId('md-image-placeholder')
    expect(placeholder).toHaveAttribute('data-host', 'evil.example')
    expect(reads().map((r) => r.path).filter((p) => /evil/.test(p))).toEqual([])
    // 页面里所有 <img> 的地址都是 blob:，没有 http(s)、没有相对地址
    for (const img of document.querySelectorAll('img')) expect(img.getAttribute('src')).toMatch(/^blob:/)
  })

  it('highlights code blocks the way messages do', async () => {
    mount('app/docs/index.md')
    const md = await screen.findByTestId('files-markdown')
    expect(within(md).getByTestId('code-block')).toHaveTextContent('const a = 1')
  })

  it('a relative image that is not an image file, or cannot be read, falls back to its text', async () => {
    io = installIntersectionObserver()
    serveFs(rpc, { 'app/docs/a.md': '![gone](missing.png)\n\n![notimg](a.md)' })
    mount('app/docs/a.md')
    const md = await screen.findByTestId('files-markdown')
    expect(await within(md).findByText('Image: notimg')).toBeInTheDocument()
    io.intersect()
    expect(await within(md).findByText('Image: gone')).toBeInTheDocument()
    expect(reads().map((r) => r.path)).toContain('app/docs/missing.png')
  })

  it('opens a linked Markdown file in the browser: the link goes through the URL like a click on the tree', async () => {
    const user = userEvent.setup()
    mount('app/docs/index.md')
    const md = await screen.findByTestId('files-markdown')
    // StubLink 渲染成普通 <a>；真正的路由里点它就是 navigate。这里改路径，等价于跟随链接
    expect(within(md).getByRole('link', { name: 'setup' })).toBeInTheDocument()
    await user.click(await screen.findByRole('treeitem', { name: 'README.md' }))
    expect(navigate).toHaveBeenCalledWith({ to: '/n/$node/files/$', params: { node: 'nnnnnnnn', _splat: 'app/README.md' } })
    expect(await screen.findByRole('heading', { name: 'Readme' })).toBeInTheDocument()
  })

  it('a different file starts again in the default view', async () => {
    const user = userEvent.setup()
    mount('app/README.md')
    await screen.findByTestId('files-markdown')
    await user.click(screen.getByTestId('files-md-source'))
    expect(screen.queryByTestId('files-markdown')).toBeNull()
    go('app/docs/index.md')
    expect(await screen.findByTestId('files-markdown')).toBeInTheDocument()
  })
})
