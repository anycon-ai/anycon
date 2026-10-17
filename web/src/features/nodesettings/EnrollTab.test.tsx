// 节点设置的「接入」标签（spec/web.md § 4.8）：口令的各个操作、批准接入开关、待批准列表、最近活动。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import type { AuditEntry, NodeEnrollStatusResult } from '@/protocol'
import { enrollInfo, nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { EnrollTab, recentEnrollActivity } from './EnrollTab'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc
let status: NodeEnrollStatusResult
let audit: AuditEntry[]

const pw = (over: Partial<NodeEnrollStatusResult['password']>): NodeEnrollStatusResult['password'] => ({ set: true, state: 'enabled', failures: 0, set_at_ms: Date.now() - 3600_000, set_by: 'phone', ...over })

beforeEach(() => {
  status = { approval: true, password: pw({}) }
  audit = []
  rpc = new FakeRpc()
  rpc.handlers['node.enroll_status'] = () => status
  rpc.handlers['node.enroll_list'] = () => ({ enrolls: [] })
  rpc.handlers['node.audit'] = () => ({ entries: audit })
  rpc.handlers['node.set_enroll_password'] = () => ({})
  rpc.handlers['node.set_enroll_enabled'] = () => ({})
  setRpc(rpc)
  useNodes.setState({
    byId: { [NODE]: nodeEntry({ id: NODE, name: 'mac-mini', devices: [{ device_id: 'phone', fingerprint: 'ab', kind: 'web', name: 'iPhone', online: true, paired_at_ms: 1 }] }) },
    order: [NODE],
    loaded: true,
  })
  useHub.setState({ snapshot: { ...useHub.getState().snapshot, deviceId: 'me' } })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: false, securityTick: {} })
  useUi.setState({ toasts: [] })
})
afterEach(() => setRpc(null))

const call = (method: string) => rpc.calls.filter((c) => c.method === method)
const state = () => screen.getByTestId('enroll-password-state')

describe('node password', () => {
  it('shows "not set" and invites to set one; the form needs two equal passwords of at least 8 characters', async () => {
    status = { approval: true, password: { set: false, state: 'enabled', failures: 0 } }
    const user = userEvent.setup()
    render(<EnrollTab nodeId={NODE} />)
    await waitFor(() => expect(state()).toHaveTextContent(/Not set|未设置/))
    expect(screen.queryByTestId('enroll-password-clear')).toBeNull()
    await user.click(screen.getByTestId('enroll-password-set'))

    const first = screen.getByTestId('enroll-pw-1')
    const second = screen.getByTestId('enroll-pw-2')
    expect(first).toHaveAttribute('autocomplete', 'new-password')
    expect(first).toHaveAttribute('type', 'password')
    await user.type(first, 'short')
    await user.type(second, 'short')
    await user.click(screen.getByTestId('enroll-pw-submit'))
    expect(await screen.findByText(/^(Use at least 8 characters\.|至少 8 个字符。)$/)).toBeInTheDocument()
    expect(call('node.set_enroll_password')).toHaveLength(0)

    await user.clear(first)
    await user.clear(second)
    await user.type(first, 'long enough 1')
    await user.type(second, 'long enough 2')
    await user.click(screen.getByTestId('enroll-pw-submit'))
    expect(await screen.findByText(/different|不一样/)).toBeInTheDocument()
    expect(call('node.set_enroll_password')).toHaveLength(0)

    await user.clear(second)
    await user.type(second, 'long enough 1')
    status = { approval: true, password: pw({}) }
    await user.click(screen.getByTestId('enroll-pw-submit'))
    await waitFor(() => expect(call('node.set_enroll_password')).toHaveLength(1))
    expect(call('node.set_enroll_password')[0]!.params).toEqual({ password: 'long enough 1' })
    await waitFor(() => expect(screen.queryByTestId('enroll-password-dialog')).toBeNull())
    await waitFor(() => expect(state()).toHaveTextContent(/On|已启用/)) // 保存后重新读了状态
  })

  it('shows the state, who set it and when; "change" uses the same form', async () => {
    const user = userEvent.setup()
    render(<EnrollTab nodeId={NODE} />)
    await waitFor(() => expect(state()).toHaveTextContent(/On|已启用/))
    expect(screen.getByTestId('enroll-password-setter')).toHaveTextContent(/iPhone/)
    expect(screen.getByTestId('enroll-password-set')).toHaveTextContent(/Change|修改/)
    await user.click(screen.getByTestId('enroll-password-set'))
    expect(screen.getByTestId('enroll-password-dialog')).toHaveTextContent(/Change the node password|修改节点口令/)
  })

  it('"set by the node itself" reads as such', async () => {
    status = { approval: true, password: pw({ set_by: 'local' }) }
    render(<EnrollTab nodeId={NODE} />)
    await waitFor(() => expect(screen.getByTestId('enroll-password-setter')).toHaveTextContent(/node’s own machine|节点本机/))
  })

  it('clearing asks first, then sends set_enroll_password without a password', async () => {
    const user = userEvent.setup()
    render(<EnrollTab nodeId={NODE} />)
    await waitFor(() => expect(state()).toHaveTextContent(/On|已启用/))
    await user.click(screen.getByTestId('enroll-password-clear'))
    expect(call('node.set_enroll_password')).toHaveLength(0)
    expect(screen.getByTestId('enroll-clear-dialog')).toHaveTextContent(/already paired keep working|照常可用/)
    status = { approval: true, password: { set: false, state: 'enabled', failures: 0 } }
    await user.click(screen.getByTestId('enroll-clear-confirm'))
    await waitFor(() => expect(call('node.set_enroll_password')).toHaveLength(1))
    expect(call('node.set_enroll_password')[0]!.params).toEqual({})
    await waitFor(() => expect(state()).toHaveTextContent(/Not set|未设置/))
  })

  it('the on/off switch sends set_enroll_enabled { password }', async () => {
    const user = userEvent.setup()
    render(<EnrollTab nodeId={NODE} />)
    await waitFor(() => expect(state()).toHaveTextContent(/On|已启用/))
    status = { approval: true, password: pw({ state: 'disabled' }) }
    await user.click(screen.getByRole('switch', { name: /Allow unlocking with the password|允许用口令解锁/ }))
    await waitFor(() => expect(call('node.set_enroll_enabled')).toHaveLength(1))
    expect(call('node.set_enroll_enabled')[0]!.params).toEqual({ password: false })
    await waitFor(() => expect(state()).toHaveTextContent(/Off|已停用/))
    await user.click(screen.getByRole('switch', { name: /Allow unlocking with the password|允许用口令解锁/ }))
    await waitFor(() => expect(call('node.set_enroll_enabled')).toHaveLength(2))
    expect(call('node.set_enroll_enabled')[1]!.params).toEqual({ password: true })
  })

  it('locked: red explanation, no switch; re-enabling asks for confirmation that spells out the risk', async () => {
    status = { approval: true, password: pw({ state: 'locked', failures: 10 }) }
    const user = userEvent.setup()
    render(<EnrollTab nodeId={NODE} />)
    const locked = await screen.findByTestId('enroll-locked')
    expect(state()).toHaveTextContent(/Locked|已锁定/)
    expect(locked).toHaveTextContent(/10/)
    expect(locked).toHaveTextContent('anycon passwd --enable')
    expect(screen.queryByRole('switch', { name: /Allow unlocking with the password|允许用口令解锁/ })).toBeNull()
    await user.click(screen.getByTestId('enroll-reenable'))
    expect(screen.getByTestId('enroll-reenable-dialog')).toHaveTextContent(/someone is guessing|有人在猜/)
    expect(call('node.set_enroll_enabled')).toHaveLength(0)
    status = { approval: true, password: pw({}) }
    await user.click(screen.getByTestId('enroll-reenable-confirm'))
    await waitFor(() => expect(call('node.set_enroll_enabled')).toHaveLength(1))
    expect(call('node.set_enroll_enabled')[0]!.params).toEqual({ password: true })
    await waitFor(() => expect(screen.queryByTestId('enroll-locked')).toBeNull())
  })

  it('shows the failure count and when the next try is allowed', async () => {
    status = { approval: true, password: pw({ failures: 4, retry_after_ms: 120_000 }) }
    render(<EnrollTab nodeId={NODE} />)
    await waitFor(() => expect(screen.getByTestId('enroll-password-backoff')).toHaveTextContent(/[12]:\d\d/))
    expect(screen.getByTestId('enroll-password')).toHaveTextContent(/4/)
  })

  it('a failed save shows the error and keeps the dialog open', async () => {
    status = { approval: true, password: { set: false, state: 'enabled', failures: 0 } }
    rpc.handlers['node.set_enroll_password'] = () => {
      throw rpcError('invalid')
    }
    const user = userEvent.setup()
    render(<EnrollTab nodeId={NODE} />)
    await user.click(await screen.findByTestId('enroll-password-set'))
    await user.type(screen.getByTestId('enroll-pw-1'), 'long enough')
    await user.type(screen.getByTestId('enroll-pw-2'), 'long enough')
    await user.click(screen.getByTestId('enroll-pw-submit'))
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true))
    expect(screen.getByTestId('enroll-password-dialog')).toBeInTheDocument()
  })

  it('every control is disabled while the node is offline', async () => {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn: 'peer_offline' }) }, order: [NODE], loaded: true })
    render(<EnrollTab nodeId={NODE} />)
    expect(screen.getByTestId('enroll-password-set')).toBeDisabled()
    expect(rpc.calls).toEqual([])
  })
})

describe('approving new devices', () => {
  it('the switch sends set_enroll_enabled { approval }', async () => {
    const user = userEvent.setup()
    render(<EnrollTab nodeId={NODE} />)
    const sw = await screen.findByRole('switch', { name: /Allow approving from a paired device|允许在已配对设备上批准/ })
    await waitFor(() => expect(sw).toBeEnabled())
    expect(sw).toBeChecked()
    status = { approval: false, password: pw({}) }
    await user.click(sw)
    await waitFor(() => expect(call('node.set_enroll_enabled')).toHaveLength(1))
    expect(call('node.set_enroll_enabled')[0]!.params).toEqual({ approval: false })
    await waitFor(() => expect(sw).not.toBeChecked())
  })
})

describe('requests waiting for approval', () => {
  it('lists them with approve / deny (the same card as the dialog); a scan request can only be denied or scanned', async () => {
    rpc.handlers['node.enroll_list'] = () => ({
      enrolls: [enrollInfo({ enroll_id: 'a' }), enrollInfo({ enroll_id: 'b', method: 'scan', approval: undefined, hint: { name: 'Pixel', kind: 'web', platform: 'browser' } }), enrollInfo({ enroll_id: 'c', status: 'ready' })],
    })
    rpc.handlers['node.enroll_approve'] = () => ({ code: '123456', expires_at_ms: Date.now() + 290_000 })
    const user = userEvent.setup()
    render(<EnrollTab nodeId={NODE} />)
    const list = await screen.findByTestId('enroll-pending')
    expect(within(list).getAllByTestId(/approve-(pending|approved)/)).toHaveLength(2) // c 已经批准过，不在待批准里
    await user.click(within(list).getByTestId('approve-approve'))
    expect(await within(list).findByTestId('approve-code')).toHaveTextContent('123 456')
    await user.click(within(list).getByTestId('approve-scan'))
    expect(useEnroll.getState().scannerOpen).toBe(true)
  })

  it('says so when there are none', async () => {
    render(<EnrollTab nodeId={NODE} />)
    expect(await screen.findByTestId('enroll-pending-empty')).toBeInTheDocument()
  })

  it('a push that opens the tab with review=1 pops the approval dialog for the first request, once', async () => {
    rpc.handlers['node.enroll_list'] = () => ({ enrolls: [enrollInfo({ enroll_id: 'a' }), enrollInfo({ enroll_id: 'b' })] })
    const { rerender } = render(<EnrollTab nodeId={NODE} autoReview />)
    await waitFor(() => expect(useEnroll.getState().dialog).toEqual({ nodeId: NODE, enrollId: 'a' }))
    act(() => useEnroll.getState().closeDialog())
    rerender(<EnrollTab nodeId={NODE} autoReview />)
    await new Promise((r) => setTimeout(r, 30))
    expect(useEnroll.getState().dialog).toBeNull()
  })
})

describe('recent activity', () => {
  const entry = (action: string, ts: number, extra: Partial<AuditEntry> = {}): AuditEntry => ({ action, ts_ms: ts, device_id: 'x', device_name: 'iPad', ...extra })

  it('keeps only enroll.* entries, newest first, at most 20', () => {
    const entries = [
      ...Array.from({ length: 25 }, (_, i) => entry('enroll.requested', 1000 + i)),
      entry('session.create', 5000),
      entry('node.pair_begin', 6000),
      entry('enroll.completed', 9000),
    ]
    const out = recentEnrollActivity(entries)
    expect(out).toHaveLength(20)
    expect(out[0]!.action).toBe('enroll.completed')
    expect(out.every((e) => e.action.startsWith('enroll.'))).toBe(true)
    expect(out.map((e) => e.ts_ms)).toEqual([...out.map((e) => e.ts_ms)].sort((a, b) => b - a))
  })

  it('renders them with a readable label, the (self-reported) device name and the method / reason', async () => {
    audit = [
      entry('enroll.completed', Date.now() - 60_000, { detail: { method: 'password' } }),
      entry('enroll.failed', Date.now() - 120_000, { device_name: 'Unknown', detail: { method: 'password', reason: 'wrong_secret', failures: 3 } }),
      entry('session.create', Date.now()),
    ]
    render(<EnrollTab nodeId={NODE} />)
    const list = await screen.findByTestId('enroll-activity-list')
    const rows = within(list).getAllByRole('listitem')
    expect(rows).toHaveLength(2)
    expect(rows[0]).toHaveTextContent(/Device joined|设备已接入/)
    expect(rows[0]).toHaveTextContent('iPad')
    expect(rows[0]).toHaveTextContent('password')
    expect(rows[1]).toHaveTextContent(/Join failed|接入失败/)
    expect(rows[1]).toHaveTextContent('wrong_secret')
  })

  it('has an empty state', async () => {
    render(<EnrollTab nodeId={NODE} />)
    expect(await screen.findByText(/Nothing yet|还没有/)).toBeInTheDocument()
  })
})

describe('keeping the status fresh', () => {
  it('reads the status again when the node reports a security event (someone changed the password elsewhere)', async () => {
    render(<EnrollTab nodeId={NODE} />)
    await waitFor(() => expect(state()).toHaveTextContent(/On|已启用/))
    const before = call('node.enroll_status').length
    status = { approval: true, password: pw({ state: 'locked', failures: 10 }) }
    act(() => useEnroll.getState().bumpSecurity(NODE))
    await waitFor(() => expect(call('node.enroll_status').length).toBeGreaterThan(before))
    await waitFor(() => expect(state()).toHaveTextContent(/Locked|已锁定/))
  })
})
