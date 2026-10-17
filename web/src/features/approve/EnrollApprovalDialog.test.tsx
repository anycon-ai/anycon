// 批准对话框（spec/web.md § 4.3.2）：批准显示码、拒绝、别的设备先处理、过期。
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enrollInfo, nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { enrollKey, useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { EnrollApprovalDialog } from './EnrollApprovalDialog'

const NODE = 'n'.repeat(32)
const ENROLL = 'e'.repeat(32)
let rpc: FakeRpc

beforeEach(() => {
  rpc = new FakeRpc()
  rpc.handlers['node.enroll_list'] = () => ({ enrolls: [] })
  setRpc(rpc)
  useNodes.setState({
    byId: { [NODE]: nodeEntry({ id: NODE, name: 'mac-mini', devices: [{ device_id: 'phone', fingerprint: 'ab', kind: 'web', name: 'iPhone', online: true, paired_at_ms: 1 }] }) },
    order: [NODE],
    loaded: true,
  })
  useHub.setState({ snapshot: { ...useHub.getState().snapshot, deviceId: 'me' } })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: false, securityTick: {} })
})
afterEach(() => {
  setRpc(null)
  vi.useRealTimers()
})

function openRequest(over: Parameters<typeof enrollInfo>[0] = {}) {
  const info = enrollInfo({ enroll_id: ENROLL, ...over })
  useEnroll.getState().upsert(NODE, info)
  useEnroll.getState().openDialog(NODE, ENROLL)
  return render(<EnrollApprovalDialog />)
}

describe('approval dialog — pending', () => {
  it('shows the node, the unverified hint and the warning to approve only the device in front of you', () => {
    openRequest()
    expect(screen.getByTestId('approve-dialog')).toHaveTextContent('mac-mini')
    expect(screen.getByTestId('approve-hint-name')).toHaveTextContent('Chrome · Windows')
    expect(screen.getByTestId('approve-hint-meta')).toHaveTextContent(/not verified|未经验证/)
    expect(screen.getByTestId('approve-pending')).toHaveTextContent(/Only approve the device right in front of you|只批准你眼前这台设备/)
    expect(screen.getByTestId('approve-approve')).toBeEnabled()
    expect(screen.getByTestId('approve-deny')).toBeEnabled()
  })

  it('cleans the self-reported name: control and bidi characters out, 64 characters at most; none → "unknown device"', () => {
    const evil = `Chrome\u{202e}evil\u{0}${'x'.repeat(100)}`
    const { unmount } = openRequest({ hint: { name: evil, kind: 'web', platform: 'browser' } })
    const text = screen.getByTestId('approve-hint-name').textContent!
    expect(text.includes('\u{202e}')).toBe(false)
    expect(text.includes('\u{0}')).toBe(false)
    expect([...text].length).toBeLessThanOrEqual(64)
    unmount()
    useEnroll.setState({ items: {}, resolved: {}, dialog: null })
    openRequest({ hint: null })
    expect(screen.getByTestId('approve-hint-name')).toHaveTextContent(/Unknown device|未知设备/)
  })
})

describe('approval dialog — approve', () => {
  it('approving shows the big 6-digit code with a countdown; the code is not kept in the store and is gone once the dialog is closed', async () => {
    rpc.handlers['node.enroll_approve'] = () => ({ code: '482913', expires_at_ms: Date.now() + 299_000 })
    const user = userEvent.setup()
    openRequest()
    await user.click(screen.getByTestId('approve-approve'))
    expect(await screen.findByTestId('approve-code')).toHaveTextContent('482 913')
    expect(rpc.calls.find((c) => c.method === 'node.enroll_approve')!.params).toEqual({ enroll_id: ENROLL })
    expect(screen.getByTestId('approve-code-countdown')).toHaveTextContent(/4:5\d|5:00/)
    expect(screen.getByTestId('approve-approved')).toHaveTextContent(/Do not tell it to anyone|不要告诉任何人/)
    // 码只在组件状态里
    expect(JSON.stringify(useEnroll.getState())).not.toContain('482913')

    // 关掉再打开：看不到码了（不能再批准一次——daemon 那边已经批准过）
    act(() => useEnroll.getState().closeDialog())
    await waitFor(() => expect(screen.queryByTestId('approve-dialog')).toBeNull())
    expect(document.body.textContent).not.toContain('482')
    act(() => useEnroll.getState().openDialog(NODE, ENROLL))
    expect(screen.queryByTestId('approve-code')).toBeNull()
  })

  it('shows "done" when the daemon reports the new device joined', async () => {
    rpc.handlers['node.enroll_approve'] = () => ({ code: '482913', expires_at_ms: Date.now() + 299_000 })
    const user = userEvent.setup()
    openRequest()
    await user.click(screen.getByTestId('approve-approve'))
    await screen.findByTestId('approve-code')
    // daemon 也会把「approved（by 我）」发给我：不能因此把码收起来
    act(() => useEnroll.getState().resolve(NODE, ENROLL, 'approved', 'me'))
    expect(screen.getByTestId('approve-code')).toBeInTheDocument()
    act(() => useEnroll.getState().resolve(NODE, ENROLL, 'completed', null))
    expect(screen.queryByTestId('approve-code')).toBeNull()
    expect(screen.getByTestId('approve-resolved')).toHaveTextContent(/Done|已完成/)
  })

  it('shows "expired" once the code runs out', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    rpc.handlers['node.enroll_approve'] = () => ({ code: '482913', expires_at_ms: Date.now() + 30_000 })
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    openRequest()
    await user.click(screen.getByTestId('approve-approve'))
    await screen.findByTestId('approve-code')
    await act(async () => {
      await vi.advanceTimersByTimeAsync(31_000)
    })
    expect(screen.queryByTestId('approve-code')).toBeNull()
    expect(screen.getByTestId('approve-resolved')).toHaveTextContent(/expired|过期/)
  })

  it('"already_resolved": another device got there first — collapses to one line and re-reads the pending list', async () => {
    rpc.handlers['node.enroll_approve'] = () => {
      throw rpcError('already_resolved')
    }
    const user = userEvent.setup()
    openRequest()
    await user.click(screen.getByTestId('approve-approve'))
    expect(await screen.findByTestId('approve-resolved')).toHaveTextContent(/Another device|另一台设备/)
    await waitFor(() => expect(rpc.methods()).toContain('node.enroll_list'))
    expect(screen.queryByTestId('approve-approve')).toBeNull()
  })

  it('a request that is gone (not_found) says so', async () => {
    rpc.handlers['node.enroll_approve'] = () => {
      throw rpcError('not_found')
    }
    const user = userEvent.setup()
    openRequest()
    await user.click(screen.getByTestId('approve-approve'))
    expect(await screen.findByTestId('approve-resolved')).toHaveTextContent(/no longer exists|已经不存在/)
  })
})

describe('approval dialog — deny and other devices', () => {
  it('denying tells the daemon and shows a one-line result', async () => {
    rpc.handlers['node.enroll_deny'] = () => ({})
    const user = userEvent.setup()
    openRequest()
    await user.click(screen.getByTestId('approve-deny'))
    expect(await screen.findByTestId('approve-resolved')).toHaveTextContent(/You denied|你已拒绝/)
    expect(rpc.calls.find((c) => c.method === 'node.enroll_deny')!.params).toEqual({ enroll_id: ENROLL })
  })

  it('another device approved first (node.enroll_resolved): collapses to "Approved by iPhone"', () => {
    openRequest()
    act(() => useEnroll.getState().resolve(NODE, ENROLL, 'approved', 'phone'))
    expect(screen.getByTestId('approve-resolved')).toHaveTextContent(/Approved by iPhone|已由 iPhone 批准/)
    expect(screen.queryByTestId('approve-approve')).toBeNull()
  })

  it('...or denied it', () => {
    openRequest()
    act(() => useEnroll.getState().resolve(NODE, ENROLL, 'denied', 'phone'))
    expect(screen.getByTestId('approve-resolved')).toHaveTextContent(/Denied by iPhone|已由 iPhone 拒绝/)
  })

  it('an unknown device that handled it is "another device"', () => {
    openRequest()
    act(() => useEnroll.getState().resolve(NODE, ENROLL, 'approved', 'someone-else'))
    expect(screen.getByTestId('approve-resolved')).toHaveTextContent(/another device|另一台设备/)
  })

  it('expired / failed results from the daemon', () => {
    openRequest()
    act(() => useEnroll.getState().resolve(NODE, ENROLL, 'expired', null))
    expect(screen.getByTestId('approve-resolved')).toHaveTextContent(/expired|过期/)
  })

  it('a request that was dropped by a fresh node.enroll_list shows as gone', () => {
    openRequest()
    act(() => useEnroll.getState().replaceNode(NODE, []))
    expect(screen.getByTestId('approve-resolved')).toHaveTextContent(/no longer exists|已经不存在/)
  })

  it('points at the next waiting request when there are several', async () => {
    const user = userEvent.setup()
    useEnroll.getState().upsert(NODE, enrollInfo({ enroll_id: 'second', hint: { name: 'iPad', kind: 'web', platform: 'browser' } }))
    openRequest()
    expect(screen.getByTestId('approve-next')).toHaveTextContent(/1/)
    await user.click(screen.getByTestId('approve-next'))
    expect(useEnroll.getState().dialog).toEqual({ nodeId: NODE, enrollId: 'second' })
    expect(await screen.findByText('iPad')).toBeInTheDocument()
  })
})

describe('approval dialog — scan requests', () => {
  it('can only be denied here or approved by scanning the QR code', async () => {
    const user = userEvent.setup()
    openRequest({ enroll_id: ENROLL, method: 'scan', approval: undefined })
    expect(screen.queryByTestId('approve-approve')).toBeNull()
    expect(screen.getByTestId('approve-pending')).toHaveAttribute('data-method', 'scan')
    await user.click(screen.getByTestId('approve-scan'))
    expect(useEnroll.getState().scannerOpen).toBe(true)
    expect(useEnroll.getState().dialog).toBeNull()
  })
})

describe('keys', () => {
  it('requests of different nodes with the same enroll id do not collide', () => {
    expect(enrollKey(NODE, ENROLL)).not.toBe(enrollKey('x'.repeat(32), ENROLL))
  })
})
