// 在新标签页里经目录预览打开文件（ADR-62）：空白页被浏览器拦了时的退路、目录 / 路径的拆分。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { resetDb } from '@/db'
import type { PreviewOpenParams } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { installFakeApi } from '@/test/fakeApi'
import { FakeRpc } from '@/test/fakeRpc'
import { LABEL, NODE_KEY, openResult, PREVIEW_NODE, PREVIEW_TEMPLATE, previewNodeInfo, seedDeviceToken } from '@/test/preview'
import { useNodes } from '@/stores/nodes'
import { setRpc } from '@/transport/instance'
import { openDirPreview } from './openDir'
import { clearTickets } from './ticket'

let rpc: FakeRpc

beforeEach(async () => {
  await resetDb()
  clearTickets()
  await seedDeviceToken()
  rpc = new FakeRpc()
  setRpc(rpc)
  rpc.handlers['preview.open'] = () => openResult({ port: undefined, dir: '.scratch', session_id: undefined })
  installFakeApi({ 'POST /api/preview/tokens': () => ({ token: 'hub-token', origin: `https://${LABEL}.preview.test`, expires_at_ms: Date.now() + 3_600_000 }) })
  useNodes.setState({ byId: { [PREVIEW_NODE]: nodeEntry({ id: PREVIEW_NODE, publicKey: NODE_KEY, info: previewNodeInfo() }) }, order: [PREVIEW_NODE], loaded: true })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

describe('openDirPreview', () => {
  it('when the blank tab is blocked, waits for the ticket and opens the preview with noopener', async () => {
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const onError = vi.fn()
    await openDirPreview({ nodeId: PREVIEW_NODE, fsPath: '.scratch/sess_1/out/r.html', template: PREVIEW_TEMPLATE, onError })
    expect(open).toHaveBeenNthCalledWith(1, '', '_blank')
    expect(open).toHaveBeenCalledTimes(2)
    const [url, target, features] = open.mock.calls[1]!
    expect(String(url)).toMatch(new RegExp(`^https://${LABEL}\\.preview\\.test/__anycon/enter#`))
    expect([target, features]).toEqual(['_blank', 'noopener,noreferrer'])
    expect(rpc.calls.find((c) => c.method === 'preview.open')!.params).toEqual({ dir: '.scratch' } satisfies PreviewOpenParams)
    expect(onError).not.toHaveBeenCalled()
  })

  it('reports a failure through onError and opens nothing', async () => {
    rpc.handlers['preview.open'] = () => {
      throw new Error('refused')
    }
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const onError = vi.fn()
    await openDirPreview({ nodeId: PREVIEW_NODE, fsPath: 'web/a.html', template: PREVIEW_TEMPLATE, onError })
    expect(onError).toHaveBeenCalledTimes(1)
    expect(open).toHaveBeenCalledTimes(1) // 只有那次被拦的空白页
  })

  it('does nothing for an empty path', async () => {
    const open = vi.spyOn(window, 'open')
    await openDirPreview({ nodeId: PREVIEW_NODE, fsPath: '', template: PREVIEW_TEMPLATE, onError: vi.fn() })
    expect(open).not.toHaveBeenCalled()
    expect(rpc.calls).toHaveLength(0)
  })
})
