import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AttachmentRef, BlobGetParams } from '@/protocol'
import { resetDb } from '@/db'
import { bytesToBase64 } from '@/lib/base64'
import { installIntersectionObserver, type FakeIo } from '@/test/intersection'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { readCached, resetBlobCache, writeCached } from '../cache'
import { resetDownloads } from '../download'
import { MessageAttachment, MessageAttachments, thumbBox } from './MessageAttachments'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc
let io: FakeIo

const ref = (n: number, over: Partial<AttachmentRef> = {}): AttachmentRef => ({ blob_id: `c${n.toString(16).padStart(31, '0')}`, name: `pic${n}.png`, mime: 'image/png', bytes: 600, width: 400, height: 200, ...over })
const content = (n: number): Uint8Array => new Uint8Array(600).fill(n)
const readyCount = (): number => screen.getAllByTestId('message-attachment').filter((b) => b.getAttribute('data-state') === 'ready').length

/** 假 daemon 的 blob.get：每张图 600 字节，分页大小 `page`。 */
function serve(opts: { page?: number; delayMs?: number } = {}) {
  const stats = { now: 0, peak: 0, calls: [] as string[] }
  rpc.handlers['blob.get'] = async (p) => {
    const params = p as BlobGetParams
    const n = parseInt(params.blob_id.slice(1), 16)
    stats.calls.push(`${n}@${params.offset ?? 0}`)
    stats.now += 1
    stats.peak = Math.max(stats.peak, stats.now)
    if (opts.delayMs) await new Promise((r) => setTimeout(r, opts.delayMs))
    stats.now -= 1
    const all = content(n)
    const offset = params.offset ?? 0
    const end = Math.min(all.length, offset + (opts.page ?? 512 * 1024))
    return { blob: ref(n), offset, data: bytesToBase64(all.subarray(offset, end)), eof: end >= all.length }
  }
  return stats
}

beforeEach(async () => {
  await resetDb()
  resetBlobCache()
  resetDownloads()
  rpc = new FakeRpc()
  setRpc(rpc)
  io = installIntersectionObserver()
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => {
  setRpc(null)
  io.restore()
})

describe('thumbBox', () => {
  it('fits the recorded size into a 200 px box without upscaling, so the layout does not jump when the image arrives', () => {
    expect(thumbBox(1280, 800)).toEqual({ w: 200, h: 125 })
    expect(thumbBox(800, 1600)).toEqual({ w: 100, h: 200 })
    expect(thumbBox(100, 50)).toEqual({ w: 100, h: 64 })
    expect(thumbBox(10, 10)).toEqual({ w: 64, h: 64 })
  })
  it('falls back to a square when the node could not read the size', () => {
    expect(thumbBox(undefined, undefined)).toEqual({ w: 160, h: 160 })
    expect(thumbBox(null, 20)).toEqual({ w: 160, h: 160 })
    expect(thumbBox(0, 0)).toEqual({ w: 160, h: 160 })
  })
})

describe('history thumbnails', () => {
  it('holds the place using the recorded width and height and fetches nothing until the thumbnail is near the viewport', () => {
    serve()
    render(<MessageAttachments nodeId={NODE} attachments={[ref(1, { width: 1280, height: 800 })]} />)
    const box = screen.getByTestId('message-attachment')
    expect(box).toHaveAttribute('data-state', 'idle')
    expect(box.style.width).toBe('200px')
    expect(box.style.aspectRatio).toBe('200 / 125')
    expect(screen.queryByRole('img', { name: 'pic1.png' })).toBeNull()
    expect(rpc.calls).toEqual([])
  })

  it('pulls the image with blob.get once it scrolls into view, shows it, and writes the cache', async () => {
    const stats = serve({ page: 250 })
    render(<MessageAttachments nodeId={NODE} attachments={[ref(1)]} />)
    act(() => io.intersect())
    const img = await screen.findByRole('img', { name: 'pic1.png' })
    expect(img).toHaveAttribute('src', expect.stringMatching(/^blob:/))
    expect(img).toHaveAttribute('loading', 'lazy')
    expect(screen.getByTestId('message-attachment')).toHaveAttribute('data-state', 'ready')
    expect(stats.calls).toEqual(['1@0', '1@250', '1@500'])
    expect(Array.from((await readCached(ref(1).blob_id))!.bytes)).toEqual(Array.from(content(1)))
  })

  it('uses the image cache when it has the image (no request to the node), e.g. one this browser uploaded itself', async () => {
    await writeCached(ref(1).blob_id, content(1), 'image/png')
    render(<MessageAttachments nodeId={NODE} attachments={[ref(1)]} />)
    act(() => io.intersect())
    await screen.findByRole('img', { name: 'pic1.png' })
    expect(rpc.calls).toEqual([])
  })

  it('only fetches the thumbnails that came into view', async () => {
    const stats = serve()
    render(<MessageAttachments nodeId={NODE} attachments={[ref(1), ref(2), ref(3)]} />)
    const boxes = screen.getAllByTestId('message-attachment')
    act(() => io.intersect(boxes[1]!))
    await screen.findByRole('img', { name: 'pic2.png' })
    expect(stats.calls).toEqual(['2@0'])
    expect(boxes[0]).toHaveAttribute('data-state', 'idle')
    expect(boxes[2]).toHaveAttribute('data-state', 'idle')
  })

  it('pulls at most 2 images at a time from one node', async () => {
    const stats = serve({ delayMs: 15 })
    render(<MessageAttachments nodeId={NODE} attachments={[ref(1), ref(2), ref(3), ref(4), ref(5)]} />)
    act(() => io.intersect())
    await waitFor(() => expect(readyCount()).toBe(5))
    expect(stats.peak).toBe(2)
  })

  it('shows “Image unavailable” when the image cannot be pulled, and retry tries again', async () => {
    rpc.handlers['blob.get'] = () => {
      throw rpcError('not_found', undefined, 'blob')
    }
    render(<MessageAttachments nodeId={NODE} attachments={[ref(1)]} />)
    act(() => io.intersect())
    const err = await screen.findByTestId('message-attachment-error')
    expect(err).toHaveTextContent('Image unavailable')
    expect(screen.getByTestId('message-attachment')).toHaveAttribute('data-state', 'error')

    serve()
    await userEvent.click(within(err).getByRole('button', { name: 'Retry' }))
    await screen.findByRole('img', { name: 'pic1.png' })
    expect(screen.queryByTestId('message-attachment-error')).toBeNull()
  })

  it('shows the images straight away when the browser has no IntersectionObserver', async () => {
    io.restore()
    vi.stubGlobal('IntersectionObserver', undefined)
    serve()
    render(<MessageAttachments nodeId={NODE} attachments={[ref(1)]} />)
    await screen.findByRole('img', { name: 'pic1.png' })
  })

  it('revokes the object URL when the thumbnail goes away', async () => {
    await writeCached(ref(1).blob_id, content(1), 'image/png')
    const created = vi.spyOn(URL, 'createObjectURL')
    const revoked = vi.spyOn(URL, 'revokeObjectURL')
    const { unmount } = render(<MessageAttachments nodeId={NODE} attachments={[ref(1)]} />)
    act(() => io.intersect())
    await screen.findByRole('img', { name: 'pic1.png' })
    const url = created.mock.results.at(-1)!.value as string
    expect(revoked).not.toHaveBeenCalledWith(url)
    unmount()
    expect(revoked).toHaveBeenCalledWith(url)
  })

  it('cancels a fetch that is still queued when the thumbnail unmounts before it starts', async () => {
    const stats = serve({ delayMs: 150 })
    const { rerender } = render(<MessageAttachments nodeId={NODE} attachments={[ref(1), ref(2), ref(3)]} />)
    act(() => io.intersect())
    await waitFor(() => expect(stats.calls).toHaveLength(2)) // 1、2 在拉，3 排队
    rerender(<MessageAttachments nodeId={NODE} attachments={[ref(1), ref(2)]} />)
    await waitFor(() => expect(readyCount()).toBe(2))
    expect(stats.calls.sort()).toEqual(['1@0', '2@0'])
  })
})

describe('image viewer', () => {
  async function open() {
    await writeCached(ref(1).blob_id, content(1), 'image/png')
    render(<MessageAttachment nodeId={NODE} att={ref(1, { name: 'screenshot.png', bytes: 123_456, width: 1280, height: 800 })} />)
    act(() => io.intersect())
    await userEvent.click(await screen.findByRole('button', { name: 'Open image screenshot.png' }))
    return screen.findByTestId('attachment-viewer')
  }

  it('opens a dialog with the big image, the file name, the size and a download link', async () => {
    const viewer = await open()
    expect(within(viewer).getByRole('heading', { name: 'screenshot.png' })).toBeInTheDocument()
    expect(within(viewer).getByTestId('attachment-viewer-meta')).toHaveTextContent('1280×800 · 121 KiB')
    expect(within(viewer).getByTestId('attachment-viewer-image')).toHaveAttribute('src', expect.stringMatching(/^blob:/))
    const link = within(viewer).getByTestId('attachment-download')
    expect(link).toHaveAttribute('download', 'screenshot.png')
    expect(link).toHaveAttribute('href', within(viewer).getByTestId('attachment-viewer-image').getAttribute('src'))
  })

  it('names the download after what the file really is: main name + the extension of its type, whatever the recorded name says', async () => {
    const open2 = async (att: AttachmentRef) => {
      await writeCached(att.blob_id, content(1), att.mime)
      const view = render(<MessageAttachment nodeId={NODE} att={att} />)
      act(() => io.intersect())
      await userEvent.click(await screen.findByRole('button', { name: `Open image ${att.name}` }))
      const link = within(await screen.findByTestId('attachment-viewer')).getByTestId('attachment-download')
      const name = link.getAttribute('download')
      view.unmount()
      return name
    }
    // 内容是 JPEG，名字却叫 x.html / x.exe：存盘不能带着那个扩展名
    expect(await open2(ref(1, { name: 'x.html', mime: 'image/jpeg' }))).toBe('x.jpg')
    expect(await open2(ref(2, { name: 'invoice_gnp.exe', mime: 'image/png' }))).toBe('invoice_gnp.png')
    // 没有扩展名就补上；对得上的原样保留（大小写也不动）
    expect(await open2(ref(3, { name: 'photo', mime: 'image/webp' }))).toBe('photo.webp')
    expect(await open2(ref(4, { name: 'IMG_0042.JPEG', mime: 'image/jpeg' }))).toBe('IMG_0042.JPEG')
  })

  it('closes on Escape and with the close button', async () => {
    await open()
    fireEvent.keyDown(screen.getByTestId('attachment-viewer'), { key: 'Escape' })
    await waitFor(() => expect(screen.queryByTestId('attachment-viewer')).toBeNull())
    await userEvent.click(screen.getByRole('button', { name: 'Open image screenshot.png' }))
    await userEvent.click(within(await screen.findByTestId('attachment-viewer')).getByRole('button', { name: 'Close' }))
    await waitFor(() => expect(screen.queryByTestId('attachment-viewer')).toBeNull())
  })
})

describe('files (anything that is not a png / jpeg / gif / webp image)', () => {
  const file = (n: number, over: Partial<AttachmentRef> = {}): AttachmentRef => ({ blob_id: `c${n.toString(16).padStart(31, '0')}`, name: `doc${n}.pdf`, mime: 'application/pdf', bytes: 600, ...over })
  /** 截下 `<a download>` 的点击（jsdom 不会真的下载），记下名字和 href 对应的 Blob。 */
  function captureDownloads() {
    const blobs = new Map<string, Blob>()
    const created = vi.spyOn(URL, 'createObjectURL').mockImplementation((b: Blob | MediaSource) => {
      const url = `blob:test/${blobs.size + 1}`
      blobs.set(url, b as Blob)
      return url
    })
    const clicks: { name: string; blob: Blob | undefined }[] = []
    vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function (this: HTMLAnchorElement) {
      clicks.push({ name: this.download, blob: blobs.get(this.getAttribute('href') ?? '') })
    })
    return { created, clicks }
  }

  it('shows a file chip with the name and the size, and fetches nothing when it scrolls into view', () => {
    serve()
    render(<MessageAttachments nodeId={NODE} attachments={[file(1, { bytes: 123_456 })]} />)
    const chip = screen.getByTestId('message-attachment')
    expect(chip).toHaveAttribute('data-kind', 'file')
    expect(chip).toHaveTextContent('doc1.pdf')
    expect(chip).toHaveTextContent('121 KiB')
    act(() => io.intersect())
    expect(rpc.calls).toEqual([])
    expect(screen.queryByRole('img')).toBeNull()
  })

  it('clicking downloads it: the bytes are pulled with blob.get (and cached), then saved under the recorded name as application/octet-stream', async () => {
    const stats = serve({ page: 250 })
    const { clicks } = captureDownloads()
    render(<MessageAttachments nodeId={NODE} attachments={[file(1, { name: 'report.final.pdf' })]} />)
    await userEvent.click(screen.getByRole('button', { name: 'Download report.final.pdf' }))
    await waitFor(() => expect(clicks).toHaveLength(1))
    expect(stats.calls).toEqual(['1@0', '1@250', '1@500'])
    expect(clicks[0]!.name).toBe('report.final.pdf')
    expect(clicks[0]!.blob!.type).toBe('application/octet-stream')
    expect(new Uint8Array(await clicks[0]!.blob!.arrayBuffer())).toEqual(content(1))
    expect(await readCached(file(1).blob_id)).not.toBeNull()
    expect(screen.getByTestId('message-attachment')).toHaveAttribute('data-state', 'idle')
  })

  it('never shows HTML or SVG inline, and never hands them to the browser with their own type', async () => {
    const atts = [file(1, { name: 'page.html', mime: 'text/html' }), file(2, { name: 'logo.svg', mime: 'image/svg+xml' })]
    for (const a of atts) await writeCached(a.blob_id, content(1), a.mime)
    const { created, clicks } = captureDownloads()
    render(<MessageAttachments nodeId={NODE} attachments={atts} />)
    act(() => io.intersect())
    expect(screen.queryByRole('img')).toBeNull()
    expect(created).not.toHaveBeenCalled()
    expect(screen.getAllByTestId('message-attachment').map((c) => c.getAttribute('data-kind'))).toEqual(['file', 'file'])
    await userEvent.click(screen.getByRole('button', { name: 'Download page.html' }))
    await userEvent.click(screen.getByRole('button', { name: 'Download logo.svg' }))
    await waitFor(() => expect(clicks).toHaveLength(2))
    expect(clicks.map((c) => [c.name, c.blob!.type])).toEqual([
      ['page.html', 'application/octet-stream'],
      ['logo.svg', 'application/octet-stream'],
    ])
  })

  it('a failed download says so on the chip, and clicking again tries again', async () => {
    rpc.handlers['blob.get'] = () => {
      throw rpcError('not_found', undefined, 'blob')
    }
    const { clicks } = captureDownloads()
    render(<MessageAttachments nodeId={NODE} attachments={[file(1)]} />)
    await userEvent.click(screen.getByRole('button', { name: 'Download doc1.pdf' }))
    expect(await screen.findByTestId('message-attachment-error')).toHaveTextContent("Couldn't download. Click to try again.")
    expect(screen.getByTestId('message-attachment')).toHaveAttribute('data-state', 'error')
    serve()
    await userEvent.click(screen.getByRole('button', { name: 'Download doc1.pdf' }))
    await waitFor(() => expect(clicks).toHaveLength(1))
    expect(screen.queryByTestId('message-attachment-error')).toBeNull()
  })

  it('an image the node recorded under one of the four raster types still shows inline, next to the files', async () => {
    await writeCached(ref(1).blob_id, content(1), 'image/png')
    render(<MessageAttachments nodeId={NODE} attachments={[ref(1), file(2)]} />)
    act(() => io.intersect())
    await screen.findByRole('img', { name: 'pic1.png' })
    expect(screen.getAllByTestId('message-attachment').map((c) => c.getAttribute('data-kind'))).toEqual(['image', 'file'])
  })
})
