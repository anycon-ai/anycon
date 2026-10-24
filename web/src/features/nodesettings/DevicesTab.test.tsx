import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { DevicesTab } from './DevicesTab'

// 配额用完的说明里有一个去「配额」页的链接（`Link`）
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const NODE = 'n'.repeat(32)
let rpc: FakeRpc
let pairParams: unknown[]

beforeEach(() => {
  rpc = new FakeRpc()
  pairParams = []
  rpc.handlers['node.devices'] = () => ({ devices: [] })
  rpc.handlers['node.pair_begin'] = (p) => {
    pairParams.push(p)
    const long = (p as { long?: boolean }).long
    const code = long ? 'ABCD-2345-WXYZ-6789' : 'ABCD-2345'
    return { code, expires_at_ms: Date.now() + 300_000, link: `https://hub.example/pair#code=${code}` }
  }
  setRpc(rpc)
  useNodes.getState().hydrate([{ node_id: NODE, name: 'mac', public_key: new Uint8Array(32), paired_at: 1, last_seen: 1 }])
  useNodes.getState().setConn(NODE, 'ready')
})
afterEach(() => setRpc(null))

describe('DevicesTab — add device', () => {
  it('asks for a 16-character code by default (QR / link need no typing), and the toggle switches to a short 8-character one', async () => {
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    await waitFor(() => expect(screen.getByTestId('pair-begin-code')).toHaveTextContent('ABCD-2345-WXYZ-6789'))
    expect(pairParams).toEqual([{ long: true }])
    // 链接里带的是同一个长码（fragment）
    expect(screen.getByTestId('pair-begin-dialog')).toHaveTextContent('#code=ABCD-2345-WXYZ-6789')

    await user.click(screen.getByRole('switch', { name: /Long code|长码/ }))
    await waitFor(() => expect(screen.getByTestId('pair-begin-code')).toHaveTextContent(/^ABCD-2345$/))
    expect(pairParams).toEqual([{ long: true }, { long: false }])

    await user.click(screen.getByRole('switch', { name: /Long code|长码/ }))
    await waitFor(() => expect(screen.getByTestId('pair-begin-code')).toHaveTextContent('ABCD-2345-WXYZ-6789'))
    expect(pairParams).toHaveLength(3)
  })
})

describe('DevicesTab — the hub refuses node.pair_begin', () => {
  it('quota: the dialog stays open, names the limits and links to Settings → Quota (not "the node is busy")', async () => {
    rpc.handlers['node.pair_begin'] = () => {
      throw rpcError('busy', undefined, 'quota')
    }
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    const note = await screen.findByTestId('pair-begin-refused')
    expect(note).toHaveAttribute('role', 'alert')
    expect(note).toHaveTextContent(/limit on devices, nodes or daily pairing attempts/)
    expect(note).not.toHaveTextContent(/busy with a turn/)
    expect(screen.getByTestId('quota-link')).toHaveAttribute('href', '/settings/quota')
    expect(screen.getByTestId('pair-begin-dialog')).toBeInTheDocument()
    expect(screen.queryByTestId('pair-begin-code')).toBeNull()
  })

  it('rate_limited: says to wait, with no quota link; opening the dialog again tries again', async () => {
    let refuse = true
    rpc.handlers['node.pair_begin'] = () => {
      if (refuse) throw rpcError('busy', undefined, 'rate_limited')
      return { code: 'ABCD-2345-WXYZ-6789', expires_at_ms: Date.now() + 300_000, link: 'https://hub.example/pair#code=ABCD-2345-WXYZ-6789' }
    }
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    expect(await screen.findByTestId('pair-begin-refused')).toHaveTextContent(/Too many attempts/)
    expect(screen.queryByTestId('quota-link')).toBeNull()
    // 关掉重开：旧的拒绝说明不留着，重新请求
    refuse = false
    await user.keyboard('{Escape}')
    await user.click(screen.getByTestId('device-add'))
    await waitFor(() => expect(screen.getByTestId('pair-begin-code')).toHaveTextContent('ABCD-2345-WXYZ-6789'))
    expect(screen.queryByTestId('pair-begin-refused')).toBeNull()
  })

  it('other errors (here: the node cannot reach the hub) still close the dialog and show a toast', async () => {
    rpc.handlers['node.pair_begin'] = () => {
      throw rpcError('unsupported')
    }
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    await waitFor(() => expect(screen.queryByTestId('pair-begin-dialog')).toBeNull())
    expect(screen.queryByTestId('pair-begin-refused')).toBeNull()
  })
})

describe('DevicesTab — access code page (ADR-36)', () => {
  let enrollCodes: number
  beforeEach(() => {
    enrollCodes = 0
    rpc.handlers['node.enroll_code'] = () => {
      enrollCodes += 1
      return { code: enrollCodes === 1 ? '482913' : '777000', expires_at_ms: Date.now() + 299_000 }
    }
  })

  it('only makes a code when the "Access code" page is picked, shows it big with a 5-minute countdown and how to use it', async () => {
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    await waitFor(() => expect(screen.getByTestId('pair-begin-code')).toBeInTheDocument())
    expect(enrollCodes).toBe(0) // 新的码会顶掉旧的：不点进来就不生成
    await user.click(screen.getByTestId('add-page-code'))
    expect(await screen.findByTestId('enroll-code')).toHaveTextContent('482 913')
    expect(screen.getByTestId('enroll-code-countdown')).toHaveTextContent(/4:5\d|5:00/)
    expect(screen.getByTestId('enroll-code-page')).toHaveTextContent(/I have an access code|我有接入码/)
    expect(rpc.calls.filter((c) => c.method === 'node.enroll_code')[0]!.params).toEqual({})
  })

  it('switching between the pages does not make a new code; "make a new code" does (the old one is replaced)', async () => {
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    await user.click(await screen.findByTestId('add-page-code'))
    await screen.findByTestId('enroll-code')
    await user.click(screen.getByTestId('add-page-pair'))
    await user.click(screen.getByTestId('add-page-code'))
    expect(screen.getByTestId('enroll-code')).toHaveTextContent('482 913')
    expect(enrollCodes).toBe(1)
    await user.click(screen.getByTestId('enroll-code-regenerate'))
    await waitFor(() => expect(screen.getByTestId('enroll-code')).toHaveTextContent('777 000'))
    expect(enrollCodes).toBe(2)
  })

  it('the short-code page still works next to it (pair_begin is called when the dialog opens)', async () => {
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    await waitFor(() => expect(pairParams).toEqual([{ long: true }]))
  })

  it('says so when approving new devices is turned off on the node (forbidden)', async () => {
    rpc.handlers['node.enroll_code'] = () => {
      throw rpcError('forbidden')
    }
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    await user.click(await screen.findByTestId('add-page-code'))
    expect(await screen.findByTestId('enroll-code-error')).toHaveTextContent(/turned off|关闭了批准接入/)
  })

  it('an expired code is replaced by "expired" and can be regenerated', async () => {
    rpc.handlers['node.enroll_code'] = () => ({ code: '482913', expires_at_ms: Date.now() + 1_200 })
    const user = userEvent.setup()
    render(<DevicesTab nodeId={NODE} />)
    await user.click(screen.getByTestId('device-add'))
    await user.click(await screen.findByTestId('add-page-code'))
    await screen.findByTestId('enroll-code')
    await waitFor(() => expect(screen.getByTestId('enroll-code-expired')).toBeInTheDocument(), { timeout: 4000 })
    expect(screen.getByTestId('enroll-code-regenerate')).toBeEnabled()
  })

  it('"Scan to approve" is only offered when there is a camera, and opens the scanner', async () => {
    const user = userEvent.setup()
    const { unmount } = render(<DevicesTab nodeId={NODE} />)
    expect(screen.queryByTestId('device-scan-approve')).toBeNull() // jsdom: 没有 mediaDevices
    unmount()
    Object.defineProperty(navigator, 'mediaDevices', { configurable: true, value: { getUserMedia: () => {}, enumerateDevices: async () => [{ kind: 'videoinput' }] } })
    try {
      render(<DevicesTab nodeId={NODE} />)
      await user.click(await screen.findByTestId('device-scan-approve'))
      expect(useEnroll.getState().scannerOpen).toBe(true)
      act(() => useEnroll.getState().closeScanner())
    } finally {
      delete (navigator as { mediaDevices?: unknown }).mediaDevices
    }
  })
})
