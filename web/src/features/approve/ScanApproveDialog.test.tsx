// 应用内扫码批准（spec/web.md § 4.3.2）：只认 anycon-enroll:1:、节点必须是配对过的、认出来立刻停摄像头、确认后才批准。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { base32Encode } from '@/lib/base32'
import { buildEnrollQr } from '@/lib/enrollQr'
import { enrollInfo, nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'

const cam = vi.hoisted(() => ({
  stop: vi.fn(),
  openCamera: vi.fn(),
  read: vi.fn(),
  createQrReader: vi.fn(),
}))
vi.mock('./scanner', () => {
  class CameraError extends Error {
    constructor(readonly code: 'denied' | 'unavailable') {
      super(code)
    }
  }
  return { CameraError, openCamera: cam.openCamera, createQrReader: cam.createQrReader, cameraAvailable: async () => true }
})

import { CameraError } from './scanner'
import { ScanApproveDialog } from './ScanApproveDialog'

const NODE = 'd'.repeat(32)
const ENROLL = 'e'.repeat(32)
const SECRET = Uint8Array.from({ length: 32 }, (_, i) => i + 1)
const SECRET_HEX = Array.from(SECRET, (b) => b.toString(16).padStart(2, '0')).join('')
const QR = buildEnrollQr(NODE, ENROLL, SECRET)
let rpc: FakeRpc

/** 摄像头每一帧读到的内容（按顺序），读完后一直是 null。 */
function frames(...texts: Array<string | null>) {
  const queue = [...texts]
  cam.read.mockImplementation(async () => queue.shift() ?? null)
}

beforeEach(() => {
  cam.stop.mockReset()
  cam.openCamera.mockReset().mockResolvedValue(cam.stop)
  cam.read.mockReset().mockResolvedValue(null)
  cam.createQrReader.mockReset().mockResolvedValue({ read: cam.read })
  rpc = new FakeRpc()
  rpc.handlers['node.enroll_list'] = () => ({
    enrolls: [enrollInfo({ enroll_id: ENROLL, method: 'scan', approval: undefined, hint: { name: 'Pixel 9', kind: 'web', platform: 'browser' } })],
  })
  setRpc(rpc)
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'mac-mini' }) }, order: [NODE], loaded: true })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: true, securityTick: {} })
})
afterEach(() => setRpc(null))

describe('in-app QR approval', () => {
  it('opens the rear camera, recognises the enrollment QR, stops the camera, and asks for confirmation with the unverified hint', async () => {
    frames(QR)
    render(<ScanApproveDialog />)
    expect(cam.openCamera).toHaveBeenCalledTimes(1)
    expect(await screen.findByTestId('scan-confirm', {}, { timeout: 3000 })).toBeInTheDocument()
    expect(cam.stop).toHaveBeenCalledTimes(1) // 认出来立刻停
    expect(screen.getByTestId('scan-confirm')).toHaveTextContent('mac-mini')
    expect(screen.getByTestId('scan-hint-name')).toHaveTextContent('Pixel 9')
    expect(screen.getByTestId('scan-confirm')).toHaveTextContent(/not verified|未经验证/)
    expect(screen.getByTestId('scan-confirm')).toHaveTextContent(/right in front of you|眼前/)
    expect(rpc.methods()).toEqual(['node.enroll_list']) // 确认之前什么都没批准
  })

  it('approving sends the QR secret (hex) to the node, once the user confirms', async () => {
    rpc.handlers['node.enroll_approve'] = () => ({})
    frames(QR)
    const user = userEvent.setup()
    render(<ScanApproveDialog />)
    await user.click(await screen.findByTestId('scan-approve', {}, { timeout: 3000 }))
    expect(await screen.findByTestId('scan-done')).toHaveTextContent(/Approved|已批准/)
    const call = rpc.calls.find((c) => c.method === 'node.enroll_approve')!
    expect(call.nodeId).toBe(NODE)
    expect(call.params).toEqual({ enroll_id: ENROLL, secret: SECRET_HEX })
  })

  it('cancelling on the confirmation page approves nothing', async () => {
    frames(QR)
    const user = userEvent.setup()
    render(<ScanApproveDialog />)
    await user.click(await screen.findByTestId('scan-cancel', {}, { timeout: 3000 }))
    expect(useEnroll.getState().scannerOpen).toBe(false)
    expect(rpc.methods()).not.toContain('node.enroll_approve')
  })

  it('keeps looking after something that is not an anycon QR code, and says so', async () => {
    frames('https://example.com/pair#code=ABCD-2345', null, QR)
    render(<ScanApproveDialog />)
    const notice = await screen.findByTestId('scan-notice', {}, { timeout: 3000 })
    expect(notice).toHaveAttribute('data-kind', 'notAnycon')
    expect(notice).toHaveTextContent(/not an anycon|不是 anycon/)
    // 没停摄像头，接着扫到真的就往下走
    expect(await screen.findByTestId('scan-confirm', {}, { timeout: 3000 })).toBeInTheDocument()
  })

  it('a malformed anycon code (bad secret length) is treated as "not an anycon QR code" too', async () => {
    frames(`anycon-enroll:1:${NODE}:${ENROLL}:${base32Encode(new Uint8Array(16))}`)
    render(<ScanApproveDialog />)
    expect(await screen.findByTestId('scan-notice', {}, { timeout: 3000 })).toHaveAttribute('data-kind', 'notAnycon')
    expect(cam.stop).not.toHaveBeenCalled()
    expect(rpc.calls).toEqual([])
  })

  it('a node this browser is not paired with cannot be approved from here, and nothing is sent', async () => {
    const stranger = 'f'.repeat(32)
    frames(buildEnrollQr(stranger, ENROLL, SECRET))
    render(<ScanApproveDialog />)
    const notice = await screen.findByTestId('scan-notice', {}, { timeout: 3000 })
    expect(notice).toHaveAttribute('data-kind', 'unpaired')
    expect(notice).toHaveTextContent('ffffffff')
    expect(notice).toHaveTextContent(/not paired|没有和/)
    expect(cam.stop).not.toHaveBeenCalled()
    expect(rpc.calls).toEqual([])
  })

  it('a request the node no longer has (expired) is reported, with no approve button', async () => {
    rpc.handlers['node.enroll_list'] = () => ({ enrolls: [] })
    frames(QR)
    render(<ScanApproveDialog />)
    expect(await screen.findByTestId('scan-missing', {}, { timeout: 3000 })).toHaveTextContent(/expired|过期/)
    expect(screen.queryByTestId('scan-approve')).toBeNull()
  })

  // 二维码对应的请求必须是「扫码请求且在等批准」。扫到「请求批准」的二维码若也能点「批准」，daemon 会忽略密钥、
  // 返回 6 位码，这里又不显示，那个请求就白白作废了。
  it('a QR code that belongs to a "request approval" request is refused: no confirm page, nothing is approved', async () => {
    rpc.handlers['node.enroll_list'] = () => ({ enrolls: [enrollInfo({ enroll_id: ENROLL, method: 'code', approval: true, status: 'wait_approval' })] })
    rpc.handlers['node.enroll_approve'] = () => ({ code: '447178', expires_at_ms: Date.now() + 300_000 })
    frames(QR)
    const user = userEvent.setup()
    render(<ScanApproveDialog />)
    const refused = await screen.findByTestId('scan-not-scan', {}, { timeout: 3000 })
    expect(refused).toHaveTextContent(/not a scan request|不是扫码请求/)
    expect(screen.queryByTestId('scan-confirm')).toBeNull()
    expect(screen.queryByTestId('scan-approve')).toBeNull()
    expect(cam.stop).toHaveBeenCalledTimes(1)
    expect(rpc.methods()).toEqual(['node.enroll_list']) // 没有批准，请求还在
    // 可以重新扫别的二维码
    frames(QR)
    await user.click(within(refused).getByRole('button', { name: /Scan again|重新扫描/ }))
    await waitFor(() => expect(cam.openCamera).toHaveBeenCalledTimes(2))
    expect(rpc.methods()).not.toContain('node.enroll_approve')
  })

  it.each([
    ['already approved (ready)', { method: 'scan' as const, approval: undefined, status: 'ready' as const }],
    ['no hello yet', { method: null, approval: undefined, status: 'waiting_hello' as const, hint: null }],
    ['a scan request with the wrong status', { method: 'scan' as const, approval: undefined, status: 'waiting_hello' as const }],
  ])('a request that is not waiting for approval (%s) cannot be approved either', async (_name, over) => {
    rpc.handlers['node.enroll_list'] = () => ({ enrolls: [enrollInfo({ enroll_id: ENROLL, ...over })] })
    frames(QR)
    render(<ScanApproveDialog />)
    expect(await screen.findByTestId('scan-not-waiting', {}, { timeout: 3000 })).toHaveTextContent(/not waiting|不在等批准/)
    expect(screen.queryByTestId('scan-approve')).toBeNull()
    expect(rpc.methods()).toEqual(['node.enroll_list'])
  })

  it('"already_resolved" and an unreachable node are explained', async () => {
    rpc.handlers['node.enroll_approve'] = () => {
      throw rpcError('already_resolved')
    }
    frames(QR)
    const user = userEvent.setup()
    const { unmount } = render(<ScanApproveDialog />)
    await user.click(await screen.findByTestId('scan-approve', {}, { timeout: 3000 }))
    expect(await screen.findByTestId('scan-failed')).toHaveTextContent(/Another device|另一台设备/)
    unmount()

    rpc.ready = false
    useEnroll.setState({ scannerOpen: true })
    frames(QR)
    render(<ScanApproveDialog />)
    expect(await screen.findByTestId('scan-failed', {}, { timeout: 3000 })).toHaveTextContent(/cannot be reached|连不上/)
  })

  it('camera permission denied: explains how to allow it (and the other way), and can retry', async () => {
    cam.openCamera.mockRejectedValueOnce(new CameraError('denied'))
    const user = userEvent.setup()
    render(<ScanApproveDialog />)
    const denied = await screen.findByTestId('scan-denied')
    expect(denied).toHaveTextContent(/blocked|拒绝/)
    expect(denied).toHaveTextContent(/browser settings|浏览器的网站设置/)
    expect(denied).toHaveTextContent(/Approve on a paired device|在已配对设备上批准/)
    await user.click(screen.getByTestId('scan-retry'))
    await waitFor(() => expect(cam.openCamera).toHaveBeenCalledTimes(2))
    await waitFor(() => expect(screen.getByTestId('scan-session')).toHaveAttribute('data-view', 'scanning'))
  })

  it('no camera at all', async () => {
    cam.openCamera.mockRejectedValueOnce(new CameraError('unavailable'))
    render(<ScanApproveDialog />)
    expect(await screen.findByTestId('scan-unavailable')).toBeInTheDocument()
  })

  it('closing the dialog turns the camera off', async () => {
    render(<ScanApproveDialog />)
    await waitFor(() => expect(screen.getByTestId('scan-session')).toHaveAttribute('data-view', 'scanning'))
    act(() => useEnroll.getState().closeScanner())
    await waitFor(() => expect(cam.stop).toHaveBeenCalled())
    expect(screen.queryByTestId('scan-session')).toBeNull()
  })
})
