// 输入区的图片附件（ADR-35，spec/web.md § 4.6）：「＋」菜单 / 粘贴 / 拖放 → 缩略图条 → 发送。
// jsdom 没有 createImageBitmap / canvas：规整用的编解码注入假的，上传走假 daemon。
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AttachmentRef } from '@/protocol'
import { resetDb } from '@/db'
import { resetBlobCache, writeCached } from '@/features/attachments/cache'
import type { ImageCodec } from '@/features/attachments/normalize'
import { blobId, installBlobDaemon } from '@/test/blobDaemon'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { resetDrafts, useDrafts } from '@/stores/drafts'
import { usePrefs } from '@/stores/prefs'
import { useUi } from '@/stores/ui'
import { Composer, type ComposerProps } from './Composer'

const NODE = 'n'.repeat(32)
const KEY = 's1'
const PNG_HEAD = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
let rpc: FakeRpc

const png = (name = 'shot.png', size = 2000): File => {
  const bytes = new Uint8Array(size)
  bytes.set(PNG_HEAD)
  return new File([bytes], name, { type: 'image/png' })
}
const codec: ImageCodec = {
  decode: async () => ({ width: 40, height: 30, close: () => {} }),
  encode: async () => null,
}

type AttachProp = NonNullable<ComposerProps['attachments']>
const attach = (over: Partial<AttachProp> = {}): AttachProp => ({ nodeId: NODE, accept: ['image/png', 'image/jpeg'], test: { codec, upload: { retryDelayMs: 0 } }, ...over })

function setup(props: Partial<ComposerProps> = {}) {
  const onSend = vi.fn()
  const view = render(<Composer draftKey={KEY} onSend={onSend} attachments={attach()} {...props} />)
  return { onSend, ...view }
}

/** 打开输入框左边的「＋」菜单（Radix 菜单要用 userEvent 的指针事件）。 */
const openPlus = async (): Promise<void> => {
  await userEvent.click(screen.getByTestId('composer-plus'))
}

const thumbs = () => screen.queryAllByTestId('composer-attachment')
const toastTexts = () => useUi.getState().toasts.map((t) => t.text)

beforeEach(async () => {
  await resetDb()
  resetBlobCache()
  resetDrafts()
  useUi.setState({ toasts: [] })
  usePrefs.setState({ enterBehavior: 'send' })
  rpc = new FakeRpc()
  setRpc(rpc)
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => setRpc(null))

describe('Composer attachments: capability', () => {
  it('shows no plus menu, file input or drop handling without the attachments prop (node without blobs, or an agent that takes no images)', async () => {
    const onSend = vi.fn()
    render(<Composer draftKey={KEY} onSend={onSend} />)
    expect(screen.queryByTestId('composer-plus')).toBeNull()
    expect(screen.queryByTestId('composer-attach')).toBeNull()
    expect(screen.queryByTestId('composer-file-input')).toBeNull()
    fireEvent.paste(screen.getByTestId('composer-input'), { clipboardData: { files: [png()], items: [], getData: () => '' } })
    fireEvent.drop(screen.getByTestId('composer-dropzone'), { dataTransfer: { types: ['Files'], files: [png()] } })
    expect(thumbs()).toEqual([])
    expect(rpc.calls).toEqual([])
  })

  it('shows the plus menu with “Add files or photos” and a hidden multi-file input that accepts what the agent takes plus image/*', async () => {
    setup({ attachments: attach({ accept: ['image/png', 'image/webp'] }) })
    expect(screen.getByTestId('composer-plus')).toBeEnabled()
    expect(screen.queryByTestId('composer-attach')).toBeNull() // 菜单没打开时不渲染
    await openPlus()
    const item = await screen.findByTestId('composer-attach')
    expect(item).toHaveTextContent('Add files or photos')
    expect(item).not.toHaveAttribute('data-disabled')
    const input = screen.getByTestId('composer-file-input') as HTMLInputElement
    expect(input.type).toBe('file')
    expect(input.multiple).toBe(true)
    expect(input.accept).toBe('image/png,image/webp,image/*')
  })

  it('“Add files or photos” in the plus menu opens the file picker', async () => {
    setup()
    const click = vi.spyOn(HTMLInputElement.prototype, 'click').mockImplementation(() => {})
    await openPlus()
    await userEvent.click(await screen.findByTestId('composer-attach'))
    expect(click).toHaveBeenCalledTimes(1)
  })

  it('“Add files or photos” is disabled with an explanation while the node is offline, and pasting or dropping explains instead of uploading', async () => {
    setup({ attachments: attach({ disabledReason: 'The node is offline.' }) })
    // 「＋」本身不禁用（还能打开看到原因）；菜单项禁用，原因写在它下面
    expect(screen.getByTestId('composer-plus')).toBeEnabled()
    await openPlus()
    const item = await screen.findByTestId('composer-attach')
    expect(item).toHaveAttribute('data-disabled')
    expect(item).toHaveAttribute('aria-disabled', 'true')
    expect(item).toHaveTextContent('The node is offline.')
    expect(screen.getByTestId('composer-file-input')).toBeDisabled()
    fireEvent.paste(screen.getByTestId('composer-input'), { clipboardData: { files: [png()], items: [], getData: () => '' } })
    expect(toastTexts()).toEqual(['The node is offline.'])
    expect(thumbs()).toEqual([])
    expect(rpc.calls).toEqual([])
  })
})

describe('Composer attachments: picking, uploading, sending', () => {
  it('a picked image appears as an uploading thumbnail with progress, the send button stays disabled until it is ready, then an image-only message can be sent', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((r) => (release = r))
    installBlobDaemon(rpc, { chunkSize: 1024, onChunk: () => gate })
    const { onSend } = setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('shot.png', 2500))

    const thumb = await screen.findByTestId('composer-attachment')
    expect(thumb).toHaveAttribute('data-state', 'uploading')
    expect(within(thumb).getByRole('progressbar')).toBeInTheDocument()
    expect(within(thumb).getByRole('img', { name: 'shot.png' })).toHaveAttribute('src', expect.stringMatching(/^blob:/))
    expect(screen.getByTestId('composer-send')).toBeDisabled()
    expect(screen.getByTestId('composer-send')).toHaveAttribute('title', 'Wait for the images to finish uploading.')
    // 上传中，即使有文字也不能发
    await userEvent.type(screen.getByTestId('composer-input'), 'look{Enter}')
    expect(onSend).not.toHaveBeenCalled()
    await userEvent.clear(screen.getByTestId('composer-input'))

    release()
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(screen.getByTestId('composer-send')).toBeEnabled() // 没有文字，只有图片
    await userEvent.click(screen.getByTestId('composer-send'))
    expect(onSend).toHaveBeenCalledTimes(1)
    const [text, refs] = onSend.mock.calls[0] as [string, AttachmentRef[]]
    expect(text).toBe('')
    expect(refs).toMatchObject([{ blob_id: blobId(1), name: 'shot.png', mime: 'image/png', bytes: 2500, width: 40, height: 30 }])
    expect(thumbs()).toEqual([])
    expect(useDrafts.getState().byKey[KEY]?.attachments).toEqual([])
  })

  it('sends text and several images together, in the order they were added', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    const { onSend } = setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), [png('a.png'), png('b.png'), png('c.png')])
    await waitFor(() => expect(thumbs().map((t) => t.getAttribute('data-state'))).toEqual(['ready', 'ready', 'ready']))
    await userEvent.type(screen.getByTestId('composer-input'), 'what are these?{Enter}')
    const [text, refs] = onSend.mock.calls[0] as [string, AttachmentRef[]]
    expect(text).toBe('what are these?')
    expect(refs.map((r) => r.name)).toEqual(['a.png', 'b.png', 'c.png'])
  })

  it('keeps the send button disabled when a block reason is set, even with a finished image', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    setup({ blockedReason: 'Node is offline' })
    await userEvent.upload(screen.getByTestId('composer-file-input'), png())
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(screen.getByTestId('composer-send')).toBeDisabled()
  })

  it('removing a finished image discards it on the node and takes it out of the draft', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('a.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    await userEvent.click(screen.getByTestId('composer-attachment-remove'))
    expect(thumbs()).toEqual([])
    expect(d.discarded).toEqual([blobId(1)])
    expect(screen.getByTestId('composer-send')).toBeDisabled()
  })

  it('a failed upload shows an error thumbnail with a reason, blocks sending, and the retry button uploads it again', async () => {
    let fail = true
    installBlobDaemon(rpc, {
      chunkSize: 1024,
      onChunk: () => {
        if (fail) throw new RpcCallError('timeout', 'put_chunk: timed out')
      },
    })
    const { onSend } = setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('a.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'error'))
    expect(screen.getByTestId('composer-attachment-error')).toHaveTextContent("Couldn't upload a.png")
    expect(screen.getByTestId('composer-attachment-error')).toHaveTextContent('did not answer in time')
    await userEvent.type(screen.getByTestId('composer-input'), 'hello')
    expect(screen.getByTestId('composer-send')).toBeDisabled()
    expect(screen.getByTestId('composer-send')).toHaveAttribute('title', 'Retry or remove the images that failed to upload first.')

    fail = false
    await userEvent.click(screen.getByTestId('composer-attachment-retry'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(screen.queryByTestId('composer-attachment-error')).toBeNull()
    await userEvent.click(screen.getByTestId('composer-send'))
    expect(onSend).toHaveBeenCalledWith('hello', [expect.objectContaining({ name: 'a.png' })])
  })

  it('takes at most 10 images and tells the user about the rest', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), Array.from({ length: 11 }, (_, i) => png(`${i}.png`)))
    await waitFor(() => expect(thumbs()).toHaveLength(10))
    expect(toastTexts()).toEqual(['You can attach up to 10 images to one message.'])
  })

  it('ignores files that are not images, with a hint', async () => {
    installBlobDaemon(rpc)
    setup()
    // input 的 accept 在真实浏览器里会过滤；拖放不会，所以走拖放
    fireEvent.drop(screen.getByTestId('composer-dropzone'), { dataTransfer: { types: ['Files'], files: [new File(['x'], 'notes.txt', { type: 'text/plain' })] } })
    expect(thumbs()).toEqual([])
    expect(toastTexts()).toEqual(['Only images can be attached. Skipped notes.txt.'])
  })

  it('tells the user when an image cannot be decoded and leaves no placeholder', async () => {
    installBlobDaemon(rpc)
    const bad: ImageCodec = { decode: () => Promise.reject(new Error('bad')), encode: async () => null }
    setup({ attachments: attach({ test: { codec: bad, upload: { retryDelayMs: 0 } } }) })
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('bad.png'))
    await waitFor(() => expect(toastTexts()).toEqual(["Couldn't read bad.png as an image."]))
    expect(thumbs()).toEqual([])
  })
})

describe('Composer attachments: viewing a thumbnail', () => {
  it('clicking a thumbnail opens the shared image viewer with that image, its name and size, and a download link', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('shot.png', 2500))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    const thumb = screen.getByTestId('composer-attachment')
    const previewUrl = within(thumb).getByRole('img', { name: 'shot.png' }).getAttribute('src')
    expect(previewUrl).toMatch(/^blob:/)
    expect(screen.queryByTestId('attachment-viewer')).toBeNull()

    await userEvent.click(within(thumb).getByTestId('composer-attachment-open'))
    const viewer = await screen.findByTestId('attachment-viewer')
    expect(within(viewer).getByTestId('attachment-viewer-image')).toHaveAttribute('src', previewUrl)
    expect(within(viewer).getByRole('heading', { name: 'shot.png' })).toBeInTheDocument()
    expect(within(viewer).getByTestId('attachment-viewer-meta')).toHaveTextContent('40×30')
    expect(within(viewer).getByTestId('attachment-download')).toHaveAttribute('href', previewUrl)

    // Esc 关闭，缩略图还在，没被当成移除
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(screen.queryByTestId('attachment-viewer')).toBeNull())
    expect(thumbs()).toHaveLength(1)
  })

  it('a thumbnail can be opened while it is still uploading', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((r) => (release = r))
    installBlobDaemon(rpc, { chunkSize: 1024, onChunk: () => gate })
    setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('slow.png', 2500))
    const thumb = await screen.findByTestId('composer-attachment')
    expect(thumb).toHaveAttribute('data-state', 'uploading')
    await userEvent.click(within(thumb).getByTestId('composer-attachment-open'))
    expect(within(await screen.findByTestId('attachment-viewer')).getByTestId('attachment-viewer-image')).toHaveAttribute('src', expect.stringMatching(/^blob:/))
    await userEvent.keyboard('{Escape}')
    release()
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
  })
})

describe('Composer attachments: paste and drag-and-drop', () => {
  it('pasting an image (no text on the clipboard) attaches it and does not paste anything into the box', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    setup()
    const notPrevented = fireEvent.paste(screen.getByTestId('composer-input'), { clipboardData: { files: [png('pasted.png')], items: [], getData: () => '' } })
    expect(notPrevented).toBe(false) // preventDefault 了
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-name', 'pasted.png')
  })

  it('finds the image in clipboard items when `files` is empty', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    setup()
    const file = png('item.png')
    fireEvent.paste(screen.getByTestId('composer-input'), { clipboardData: { files: [], items: [{ kind: 'file', getAsFile: () => file }, { kind: 'string', getAsFile: () => null }], getData: () => '' } })
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-name', 'item.png'))
  })

  it('pasting text that comes with an image (copied from a document) pastes the text and attaches nothing', () => {
    setup()
    const notPrevented = fireEvent.paste(screen.getByTestId('composer-input'), { clipboardData: { files: [png()], items: [], getData: () => 'some text' } })
    expect(notPrevented).toBe(true)
    expect(thumbs()).toEqual([])
  })

  it('a plain text paste is left alone', () => {
    setup()
    expect(fireEvent.paste(screen.getByTestId('composer-input'), { clipboardData: { files: [], items: [], getData: () => 'x' } })).toBe(true)
  })

  it('highlights the whole composer while files are dragged over it, and attaches them on drop', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    setup()
    const zone = screen.getByTestId('composer-dropzone')
    expect(zone).toHaveAttribute('data-dragging', 'false')
    fireEvent.dragEnter(zone, { dataTransfer: { types: ['Files'] } })
    expect(zone).toHaveAttribute('data-dragging', 'true')
    expect(screen.getByTestId('composer-drop-hint')).toBeInTheDocument()
    // 子元素之间移动会触发 leave + enter，不能闪掉高亮
    fireEvent.dragEnter(screen.getByTestId('composer-input'), { dataTransfer: { types: ['Files'] } })
    fireEvent.dragLeave(zone, { dataTransfer: { types: ['Files'] } })
    expect(zone).toHaveAttribute('data-dragging', 'true')
    const notPrevented = fireEvent.drop(zone, { dataTransfer: { types: ['Files'], files: [png('dropped.png')] } })
    expect(notPrevented).toBe(false)
    expect(zone).toHaveAttribute('data-dragging', 'false')
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-name', 'dropped.png')
  })

  it('dragging away without dropping removes the highlight; dragging text does not highlight', () => {
    setup()
    const zone = screen.getByTestId('composer-dropzone')
    fireEvent.dragEnter(zone, { dataTransfer: { types: ['text/plain'] } })
    expect(zone).toHaveAttribute('data-dragging', 'false')
    fireEvent.dragEnter(zone, { dataTransfer: { types: ['Files'] } })
    expect(zone).toHaveAttribute('data-dragging', 'true')
    fireEvent.dragLeave(zone, { dataTransfer: { types: ['Files'] } })
    expect(zone).toHaveAttribute('data-dragging', 'false')
  })
})

describe('Composer attachments: drafts', () => {
  const ref: AttachmentRef = { blob_id: blobId(5), name: 'kept.png', mime: 'image/png', bytes: 600, width: 20, height: 10 }

  it('restores finished attachments saved with the draft, with the thumbnail taken from the image cache', async () => {
    await writeCached(ref.blob_id, new Uint8Array(600).fill(3), 'image/png')
    useDrafts.setState({ byKey: { [KEY]: { session_id: KEY, text: '', attachments: [ref], unsent: [] } } })
    const { onSend } = setup()
    const thumb = await screen.findByTestId('composer-attachment')
    expect(thumb).toHaveAttribute('data-state', 'ready')
    await waitFor(() => expect(within(thumb).getByRole('img', { name: 'kept.png' })).toHaveAttribute('src', expect.stringMatching(/^blob:/)))
    // 草稿里的图片直接可以发；节点只被问了一次「还在不在」
    await userEvent.click(screen.getByTestId('composer-send'))
    expect(onSend).toHaveBeenCalledWith('', [ref])
    expect(rpc.methods()).toEqual(['blob.get'])
  })

  it('checks that a restored attachment is still on the node before it can be sent', async () => {
    let answer: (v: unknown) => void = () => {}
    rpc.handlers['blob.get'] = () => new Promise((resolve) => (answer = resolve))
    await writeCached(ref.blob_id, new Uint8Array(600).fill(3), 'image/png') // 缩略图从缓存取，不去问节点
    useDrafts.setState({ byKey: { [KEY]: { session_id: KEY, text: '', attachments: [ref], unsent: [] } } })
    const { onSend } = setup()
    await screen.findByTestId('composer-attachment')
    // 还没答复：不能发
    await waitFor(() => expect(rpc.methods()).toEqual(['blob.get']))
    expect(rpc.calls[0]!.params).toEqual({ blob_id: ref.blob_id, offset: 0, max_bytes: 1 })
    expect(screen.getByTestId('composer-send')).toBeDisabled()
    answer({ blob: ref, data: 'AA==', eof: false })
    await waitFor(() => expect(screen.getByTestId('composer-send')).toBeEnabled())
    await userEvent.click(screen.getByTestId('composer-send'))
    expect(onSend).toHaveBeenCalledWith('', [ref])
  })

  it('a restored attachment the node has already dropped is re-uploaded from the local cache instead of failing later at send time', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    rpc.handlers['blob.get'] = () => {
      throw rpcError('not_found', undefined, 'blob')
    }
    await writeCached(ref.blob_id, new Uint8Array(600).fill(3), 'image/png')
    useDrafts.setState({ byKey: { [KEY]: { session_id: KEY, text: '', attachments: [ref], unsent: [] } } })
    const { onSend } = setup()
    await waitFor(() => expect(d.uploads.size).toBe(1))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-name', 'kept.png')
    await userEvent.click(screen.getByTestId('composer-send'))
    expect(onSend).toHaveBeenCalledWith('', [expect.objectContaining({ blob_id: blobId(1), name: 'kept.png' })])
  })

  it('one the cache lacks too is dropped, and the user is told', async () => {
    rpc.handlers['blob.get'] = () => {
      throw rpcError('not_found', undefined, 'blob')
    }
    useDrafts.setState({ byKey: { [KEY]: { session_id: KEY, text: '', attachments: [ref], unsent: [] } } })
    setup()
    await waitFor(() => expect(toastTexts()).toContain('1 image could not be restored. Add it again.'))
    expect(thumbs()).toEqual([])
  })

  it('does not ask a node that is offline (the thumbnails are still restored)', async () => {
    await writeCached(ref.blob_id, new Uint8Array(600).fill(3), 'image/png')
    useDrafts.setState({ byKey: { [KEY]: { session_id: KEY, text: '', attachments: [ref], unsent: [] } } })
    setup({ attachments: attach({ disabledReason: 'The node is offline.' }) })
    expect(await screen.findByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready')
    expect(rpc.calls).toEqual([])
    expect(screen.getByTestId('composer-send')).toBeEnabled()
  })

  it('persists the references of finished uploads with the draft and leaves out the ones still uploading', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024, onChunk: (p) => (p.blob_id === blobId(2) ? new Promise(() => {}) : undefined) })
    setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), [png('done.png'), png('stuck.png')])
    await waitFor(() => expect(thumbs().map((t) => t.getAttribute('data-state')).sort()).toEqual(['ready', 'uploading']))
    const saved = useDrafts.getState().byKey[KEY]?.attachments
    expect(saved).toHaveLength(1)
    const db = await import('@/db')
    await waitFor(async () => expect((await db.getDraft(KEY))?.attachments?.map((r) => r.blob_id)).toEqual(saved!.map((r) => r.blob_id)))
  })

  it('uploads started before the composer unmounted keep going', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024, onChunk: () => new Promise((r) => setTimeout(r, 30)) })
    const first = setup()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('a.png'))
    expect(thumbs()).toHaveLength(1)
    first.unmount()
    render(<Composer draftKey={KEY} onSend={() => {}} attachments={attach()} />)
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
  })
})
