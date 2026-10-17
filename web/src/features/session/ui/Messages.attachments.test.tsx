// 消息气泡里的图片：已确认的用户消息（user block）、还没确认的（发送中 / 未发送 / 失败）。
import { act, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AttachmentRef } from '@/protocol'
import { resetDb } from '@/db'
import { resetBlobCache, writeCached } from '@/features/attachments/cache'
import { resetDownloads } from '@/features/attachments/download'
import { installIntersectionObserver, type FakeIo } from '@/test/intersection'
import { evs, resetSeq } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import type { PendingMessage } from '@/stores/drafts'
import { buildView } from '../reducer'
import { MessageList, type MessageListProps } from './MessageList'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc
let io: FakeIo

const att = (n: number, over: Partial<AttachmentRef> = {}): AttachmentRef => ({ blob_id: `d${n.toString(16).padStart(31, '0')}`, name: `img${n}.png`, mime: 'image/png', bytes: 100, width: 40, height: 30, ...over })

function props(over: Partial<MessageListProps> = {}): MessageListProps {
  return {
    nodeId: NODE,
    blocks: [],
    pending: [],
    hasMoreOlder: false,
    loadingOlder: false,
    onLoadOlder: () => {},
    deviceLabel: (id) => id,
    respondingIds: new Set(),
    onRespond: () => {},
    onRetryPending: () => {},
    onEditPending: () => {},
    onDeletePending: () => {},
    modelLabel: (id) => id,
    modeLabel: (id) => id,
    ...over,
  }
}

beforeEach(async () => {
  resetSeq()
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

describe('user messages with images', () => {
  it('shows the thumbnails above the bubble text, loaded from the image cache without asking the node', async () => {
    await writeCached(att(1).blob_id, new Uint8Array(100).fill(1), 'image/png')
    await writeCached(att(2).blob_id, new Uint8Array(100).fill(2), 'image/png')
    const view = buildView(evs([{ type: 'user_message', message_id: 'm1', content: 'compare these', by_device: 'me', attachments: [att(1), att(2)] }]))
    render(<MessageList {...props({ blocks: view.blocks })} />)
    act(() => io.intersect())
    const bubble = screen.getByTestId('message-user')
    expect(await within(bubble).findByRole('img', { name: 'img1.png' })).toBeInTheDocument()
    expect(await within(bubble).findByRole('img', { name: 'img2.png' })).toBeInTheDocument()
    expect(within(bubble).getAllByTestId('message-attachment')).toHaveLength(2)
    expect(bubble).toHaveTextContent('compare these')
    // 缩略图网格在文字气泡前面
    const grid = within(bubble).getByTestId('message-attachments')
    expect(grid.compareDocumentPosition(within(bubble).getByText('compare these')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    expect(rpc.calls).toEqual([])
  })

  it('an image-only message has no empty text bubble', () => {
    const view = buildView(evs([{ type: 'user_message', message_id: 'm1', content: '', by_device: 'me', attachments: [att(1)] }]))
    render(<MessageList {...props({ blocks: view.blocks })} />)
    const bubble = screen.getByTestId('message-user')
    expect(within(bubble).getAllByTestId('message-attachment')).toHaveLength(1)
    expect(bubble.querySelector('p')).toBeNull()
  })

  it('a text-only message looks as before (no attachment grid)', () => {
    const view = buildView(evs([{ type: 'user_message', message_id: 'm1', content: 'hi', by_device: 'me' }]))
    render(<MessageList {...props({ blocks: view.blocks })} />)
    expect(screen.queryByTestId('message-attachments')).toBeNull()
    expect(screen.getByTestId('message-user')).toHaveTextContent('hi')
  })
})

describe('messages that have not been confirmed yet', () => {
  const pending = (over: Partial<PendingMessage> = {}): PendingMessage => ({ messageId: 'p1', content: 'sending…', attachments: [att(1)], createdAt: 1, state: 'sending', ...over })

  it('the optimistic bubble shows the thumbnails of the images being sent', async () => {
    await writeCached(att(1).blob_id, new Uint8Array(100).fill(1), 'image/png')
    render(<MessageList {...props({ pending: [pending()] })} />)
    act(() => io.intersect())
    const bubble = screen.getByTestId('message-user')
    expect(bubble).toHaveAttribute('data-state', 'sending')
    expect(await within(bubble).findByRole('img', { name: 'img1.png' })).toBeInTheDocument()
  })

  it('a failed message whose attachment expired says so and offers edit / retry / delete', async () => {
    const onEdit = vi.fn()
    render(<MessageList {...props({ pending: [pending({ state: 'failed', error: 'blob_expired' })], onEditPending: onEdit })} />)
    const err = screen.getByTestId('message-unsent-error')
    expect(err).toHaveTextContent('no longer on the node')
    expect(err).toHaveAttribute('data-reason', 'blob_expired')
    await userEvent.click(screen.getByRole('button', { name: 'Edit' }))
    expect(onEdit).toHaveBeenCalledWith('p1')
  })

  it('an image-only pending message has no empty text bubble', () => {
    render(<MessageList {...props({ pending: [pending({ content: '' })] })} />)
    expect(screen.getByTestId('message-user').querySelector('p')).toBeNull()
  })
})
