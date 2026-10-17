// startTransport 把明文 notify 信封接到「页面在后台时的系统通知」上（spec/web.md § 7）。
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ClientSink } from '@/transport/client'

const h = vi.hoisted(() => ({
  sink: null as ClientSink | null,
  show: vi.fn(async () => true),
}))

vi.mock('@/transport/instance', () => ({
  getClient: () => ({
    setSink: (s: ClientSink) => void (h.sink = s),
    start: async () => undefined,
    onVisible: async () => undefined,
    stop: () => undefined,
  }),
  rpc: () => ({ request: vi.fn(), isReady: () => false }),
}))
vi.mock('@/features/sync/systemNotify', () => ({ showSystemNotification: h.show }))

import { startTransport, stopTransport } from './bootstrap'

beforeEach(() => {
  h.sink = null
  h.show.mockReset()
  h.show.mockResolvedValue(true)
})

describe('startTransport — notify envelopes', () => {
  it('hands each notify envelope to the system notification with the node it came from', async () => {
    await startTransport('acct-notify-1')
    expect(h.sink?.notify).toBeTypeOf('function')
    h.sink!.notify!('n'.repeat(32), { category: 'permission_requested', session_id: 'sess_1' })
    expect(h.show).toHaveBeenCalledExactlyOnceWith('n'.repeat(32), { category: 'permission_requested', session_id: 'sess_1' })
    stopTransport()
  })

  it('a failure while showing is logged, not thrown into the transport', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    h.show.mockRejectedValue(new Error('boom'))
    await startTransport('acct-notify-2')
    expect(() => h.sink!.notify!('n'.repeat(32), { category: 'error', session_id: null })).not.toThrow()
    await vi.waitFor(() => expect(warn).toHaveBeenCalled())
    stopTransport()
  })
})
