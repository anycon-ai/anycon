// 会话页里的图片附件（ADR-35，spec/web.md § 4.6）：能力判断、选图 → 发送 → 乐观气泡 → 回显、
// 发送失败（附件已过期）后的编辑 / 删除、重试这一轮带上原来的图、导出。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AgentInfo, AttachmentRef, Event, NodeInfo, SessionSendParams } from '@/protocol'
import { resetDb } from '@/db'
import { resetBlobCache } from '@/features/attachments/cache'
import { resetDownloads } from '@/features/attachments/download'
import { setImageCodec, type ImageCodec } from '@/features/attachments/normalize'
import { blobId, installBlobDaemon } from '@/test/blobDaemon'
import { ev, mockAgent, nodeEntry, nodeInfo, resetSeq, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { installIntersectionObserver, type FakeIo } from '@/test/intersection'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts, useDrafts } from '@/stores/drafts'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
const download = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn', session: 'sess_a' }))
// 预览入口读 hub 的 /api/config（react-query）；这个测试没有 QueryClientProvider，也不涉及预览：当作 hub 没配预览
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => null }))
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))
vi.mock('@/features/session/export', async (orig) => ({ ...(await orig<object>()), downloadText: download }))

import { SessionPage } from './SessionPage'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'
const PNG_HEAD = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
let rpc: FakeRpc
let io: FakeIo

const codec: ImageCodec = { decode: async () => ({ width: 40, height: 30, close: () => {} }), encode: async () => null }
const png = (name = 'shot.png', size = 1500): File => {
  const bytes = new Uint8Array(size)
  bytes.set(PNG_HEAD)
  return new File([bytes], name, { type: 'image/png' })
}
const IMAGE_CAPS = { env: false, interject: false, interrupt: true, resume: true, rewind: false, structured_output: false, thinking_stream: true, usage: true, attachments: ['image/png', 'image/jpeg'] }
const blobInfo = (blobs: boolean): NodeInfo => nodeInfo({ capabilities: { audit: true, blobs, fs: true, search: false, workspace: true }, worktree: null })

function seed(opts: { blobs?: boolean; agent?: Partial<AgentInfo>; events?: Event[]; conn?: 'ready' | 'peer_offline' } = {}): void {
  rpc.handlers['session.send'] = () => ({ queued: false })
  const agent = mockAgent({ capabilities: IMAGE_CAPS, ...opts.agent })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [agent], info: blobInfo(opts.blobs ?? true), conn: opts.conn ?? 'ready' }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, (opts.events ?? []).map((e) => ev(e)))
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
}
const push = (...events: Event[]): void => {
  act(() => {
    useSessions.getState().ingest(SID, events.map((e) => ev(e)))
  })
}
const sends = () => rpc.calls.filter((c) => c.method === 'session.send').map((c) => c.params as SessionSendParams)
const attachments = () => screen.queryAllByTestId('composer-attachment')

async function addImage(name = 'shot.png'): Promise<void> {
  await userEvent.upload(screen.getByTestId('composer-file-input'), png(name))
  await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
}

beforeEach(async () => {
  resetSeq()
  navigate.mockReset()
  download.mockReset()
  await resetDb()
  resetBlobCache()
  resetDownloads()
  resetDrafts()
  rpc = new FakeRpc()
  setRpc(rpc)
  setImageCodec(codec)
  io = installIntersectionObserver()
  useUi.setState({ toasts: [] })
  usePrefs.setState({ enterBehavior: 'send', infoPanelOpen: false })
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => {
  setRpc(null)
  setImageCodec(null)
  io.restore()
})

describe('SessionPage: when the attach menu is offered', () => {
  it('node supports blobs and the agent takes images: the plus menu offers “Add files or photos”, enabled', async () => {
    seed()
    render(<SessionPage sessionId={SID} />)
    expect(screen.getByTestId('composer-plus')).toBeEnabled()
    await userEvent.click(screen.getByTestId('composer-plus'))
    const item = await screen.findByTestId('composer-attach')
    expect(item).toHaveTextContent('Add files or photos')
    expect(item).not.toHaveAttribute('data-disabled')
    expect((screen.getByTestId('composer-file-input') as HTMLInputElement).accept).toBe('image/png,image/jpeg,image/*')
  })

  it('an agent that takes any file (“*/*”): the plus menu is offered and the file picker takes any type', async () => {
    seed({ agent: { capabilities: { ...IMAGE_CAPS, attachments: ['*/*'] } } })
    render(<SessionPage sessionId={SID} />)
    await userEvent.click(screen.getByTestId('composer-plus'))
    expect(await screen.findByTestId('composer-attach')).toHaveTextContent('Add files or photos')
    expect(screen.getByTestId('composer-file-input')).not.toHaveAttribute('accept')
  })

  it('node without the blobs capability: not shown', () => {
    seed({ blobs: false })
    render(<SessionPage sessionId={SID} />)
    expect(screen.queryByTestId('composer-plus')).toBeNull()
    expect(screen.queryByTestId('composer-file-input')).toBeNull()
  })

  it('an agent that takes no attachments (none listed, or only formats the node does not take): not shown', () => {
    seed({ agent: { capabilities: { ...IMAGE_CAPS, attachments: undefined } } })
    const { unmount } = render(<SessionPage sessionId={SID} />)
    expect(screen.queryByTestId('composer-plus')).toBeNull()
    unmount()
    seed({ agent: { capabilities: { ...IMAGE_CAPS, attachments: ['application/pdf', 'image/bmp'] } } })
    render(<SessionPage sessionId={SID} />)
    expect(screen.queryByTestId('composer-plus')).toBeNull()
  })

  it('node offline: the menu item is shown but disabled, with the reason', async () => {
    seed({ conn: 'peer_offline' })
    render(<SessionPage sessionId={SID} />)
    await userEvent.click(screen.getByTestId('composer-plus'))
    const item = await screen.findByTestId('composer-attach')
    expect(item).toHaveAttribute('data-disabled')
    expect(item).toHaveTextContent("The node is offline. Files can't be attached right now.")
    expect(screen.getByTestId('composer-file-input')).toBeDisabled()
  })
})

describe('SessionPage: sending images', () => {
  it('uploads the picked image, sends its reference with the text, shows it in the pending bubble from local data, and swaps it for the echoed message', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    seed()
    render(<SessionPage sessionId={SID} />)
    await addImage('shot.png')
    await userEvent.type(screen.getByTestId('composer-input'), 'what is this?{Enter}')

    await waitFor(() => expect(sends()).toHaveLength(1))
    const sent = sends()[0]!
    expect(sent).toMatchObject({ session_id: SID, content: 'what is this?', attachments: [{ blob_id: blobId(1), name: 'shot.png', mime: 'image/png', bytes: 1500, width: 40, height: 30 }] })
    // 整张图都到了 daemon
    expect([...d.uploads.get(blobId(1))!.chunks.values()].reduce((n, c) => n + c.length, 0)).toBe(1500)
    expect(attachments()).toEqual([])

    // 乐观气泡：有缩略图，数据来自本地缓存，不去拉
    act(() => io.intersect())
    const pendingBubble = await screen.findByTestId('message-user')
    expect(pendingBubble).toHaveAttribute('data-state', 'accepted')
    expect(await within(pendingBubble).findByRole('img', { name: 'shot.png' })).toBeInTheDocument()
    expect(rpc.methods()).not.toContain('blob.get')

    // 事件流里出现回显：乐观气泡换成确认过的消息，缩略图还在
    push({ type: 'user_message', message_id: sent.message_id, content: 'what is this?', by_device: 'me', attachments: sent.attachments })
    act(() => io.intersect())
    await waitFor(() => expect(screen.getAllByTestId('message-user')).toHaveLength(1))
    expect(screen.getByTestId('message-user')).not.toHaveAttribute('data-state')
    expect(await within(screen.getByTestId('message-user')).findByRole('img', { name: 'shot.png' })).toBeInTheDocument()
  })

  it('an image-only message is sent with empty content', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    seed()
    render(<SessionPage sessionId={SID} />)
    await addImage()
    await userEvent.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(sends()).toHaveLength(1))
    expect(sends()[0]).toMatchObject({ content: '', attachments: [{ name: 'shot.png' }] })
  })

  it('while the node is unreachable the message stays local as “not sent”, with its images, and goes out once the link is back', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    seed()
    render(<SessionPage sessionId={SID} />)
    await addImage()
    rpc.ready = false
    await userEvent.type(screen.getByTestId('composer-input'), 'later{Enter}')
    const bubble = await screen.findByTestId('message-unsent-badge')
    expect(bubble).toBeInTheDocument()
    expect(useDrafts.getState().byKey[SID]!.unsent[0]).toMatchObject({ content: 'later', state: 'unsent', attachments: [{ name: 'shot.png' }] })

    rpc.ready = true
    await userEvent.click(screen.getByTestId('message-retry'))
    // FakeRpc 把离线时的那次尝试也记下了：第一次没送出去，第二次是重发
    await waitFor(() => expect(sends()).toHaveLength(2))
    expect(sends().map((s) => s.message_id)).toEqual([sends()[0]!.message_id, sends()[0]!.message_id])
    expect(sends()[1]).toMatchObject({ content: 'later', attachments: [{ name: 'shot.png' }] })
    await waitFor(() => expect(useDrafts.getState().byKey[SID]!.unsent).toEqual([]))
  })
})

describe('SessionPage: attachments the node no longer has', () => {
  async function sendAndFail(): Promise<void> {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    seed()
    rpc.handlers['session.send'] = () => {
      throw rpcError('not_found', undefined, 'blob')
    }
    render(<SessionPage sessionId={SID} />)
    await addImage('old.png')
    await userEvent.type(screen.getByTestId('composer-input'), 'see this{Enter}')
    await screen.findByTestId('message-unsent-error')
  }

  it('the failed bubble says the attachment expired, and keeps the images in view', async () => {
    await sendAndFail()
    const err = screen.getByTestId('message-unsent-error')
    expect(err).toHaveTextContent('no longer on the node')
    expect(err).toHaveAttribute('data-reason', 'blob_expired')
    expect(useUi.getState().toasts.map((t) => t.text)).toEqual(['An attachment is no longer on the node. Edit the message and add the attachments again.'])
    act(() => io.intersect())
    expect(await within(screen.getByTestId('message-user')).findByRole('img', { name: 'old.png' })).toBeInTheDocument()
  })

  it('Edit puts the text back and uploads the images again from the local copy', async () => {
    await sendAndFail()
    rpc.handlers['session.send'] = () => ({ queued: false })
    await userEvent.click(screen.getByRole('button', { name: 'Edit' }))
    expect(screen.getByTestId('composer-input')).toHaveValue('see this')
    // 换了新的 blob（第一个已经过期），字节来自本地缓存
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-name', 'old.png')
    expect(useDrafts.getState().byKey[SID]!.attachments).toMatchObject([{ blob_id: blobId(2), name: 'old.png' }])
    expect(useDrafts.getState().byKey[SID]!.unsent).toEqual([])
    await userEvent.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(sends()).toHaveLength(2))
    expect(sends()[1]).toMatchObject({ content: 'see this', attachments: [{ blob_id: blobId(2) }] })
  })

  it('Delete throws the message away and releases its images on the node', async () => {
    await sendAndFail()
    const before = rpc.calls.length
    await userEvent.click(screen.getByRole('button', { name: 'Delete' }))
    expect(useDrafts.getState().byKey[SID]!.unsent).toEqual([])
    const after = rpc.calls.slice(before)
    expect(after).toMatchObject([{ method: 'blob.discard', params: { blob_id: blobId(1) } }])
  })
})

describe('SessionPage: images the node turns a message down for', () => {
  async function sendRejected(reason: string): Promise<void> {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    seed()
    rpc.handlers['session.send'] = () => {
      throw rpcError('invalid', undefined, reason)
    }
    render(<SessionPage sessionId={SID} />)
    await addImage('pic.png')
    await userEvent.type(screen.getByTestId('composer-input'), 'see this{Enter}')
    await screen.findByTestId('message-unsent-error')
  }

  it('blob_in_use: the failed bubble and the toast say the images belong to another session; Edit uploads fresh copies from the local cache', async () => {
    await sendRejected('blob_in_use')
    const err = screen.getByTestId('message-unsent-error')
    expect(err).toHaveAttribute('data-reason', 'blob_in_use')
    expect(err).toHaveTextContent('already belong to another session')
    expect(useUi.getState().toasts.map((t) => t.text)).toEqual([expect.stringContaining('already belong to another session')])

    rpc.handlers['session.send'] = () => ({ queued: false })
    await userEvent.click(screen.getByRole('button', { name: 'Edit' }))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    // 新的 blob（旧的在别的会话里），字节来自本地缓存
    expect(useDrafts.getState().byKey[SID]!.attachments).toMatchObject([{ blob_id: blobId(2), name: 'pic.png' }])
  })

  it('too_large: the failed bubble says the images add up to too much; Edit puts the same images back (no re-upload) so some can be removed', async () => {
    await sendRejected('too_large')
    const err = screen.getByTestId('message-unsent-error')
    expect(err).toHaveAttribute('data-reason', 'attachments_too_large')
    expect(err).toHaveTextContent('add up to more than the node accepts')
    await userEvent.click(screen.getByRole('button', { name: 'Edit' }))
    expect(await screen.findByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready')
    expect(useDrafts.getState().byKey[SID]!.attachments).toMatchObject([{ blob_id: blobId(1) }])
    expect(rpc.calls.filter((c) => c.method === 'blob.put_begin')).toHaveLength(1)
  })
})

describe('SessionPage: retrying a turn and exporting', () => {
  const ref: AttachmentRef = { blob_id: 'e'.repeat(32), name: 'bug.png', mime: 'image/png', bytes: 5000, width: 800, height: 600 }
  const events: Event[] = [
    { type: 'user_message', message_id: 'm1', content: 'why is this broken?', by_device: 'me', attachments: [ref] },
    { type: 'turn_start', turn_id: 1, message_id: 'm1' },
    { type: 'error', code: 'agent_crashed', message: 'crashed', recoverable: true },
  ]

  it('“Retry” on a recoverable error sends the last user message again together with its images', async () => {
    seed({ events })
    render(<SessionPage sessionId={SID} />)
    await userEvent.click(await screen.findByRole('button', { name: 'Retry' }))
    await waitFor(() => expect(sends()).toHaveLength(1))
    expect(sends()[0]).toMatchObject({ content: 'why is this broken?', attachments: [ref] })
  })

  it('the Markdown export lists images as [image: name] and other files as [file: name], in the UI language', async () => {
    seed({ events: [{ type: 'user_message', message_id: 'm1', content: 'why is this broken?', by_device: 'me', attachments: [ref, { blob_id: 'f'.repeat(32), name: 'trace.log', mime: 'text/plain', bytes: 99 }] }] })
    usePrefs.setState({ infoPanelOpen: true })
    const i18n = (await import('@/i18n')).default
    render(<SessionPage sessionId={SID} />)
    await userEvent.click(await screen.findByRole('button', { name: 'Export Markdown' }))
    expect(download).toHaveBeenCalledTimes(1)
    expect(download.mock.calls[0]![1]).toContain('why is this broken?\n\n[image: bug.png]\n\n[file: trace.log]')

    await i18n.changeLanguage('zh')
    try {
      await userEvent.click(await screen.findByRole('button', { name: /导出/ }))
      expect(download.mock.calls[1]![1]).toContain('[图片：bug.png]\n\n[文件：trace.log]')
    } finally {
      await i18n.changeLanguage('en')
    }
  })
})
