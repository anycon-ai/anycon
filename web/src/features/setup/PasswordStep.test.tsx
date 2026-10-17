// /setup 配对完成后问要不要设节点口令（ADR-36）：可以跳过，设了就是 node.set_enroll_password。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { PasswordStep } from './PasswordStep'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

beforeEach(() => {
  rpc = new FakeRpc()
  rpc.handlers['node.set_enroll_password'] = () => ({})
  setRpc(rpc)
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn: 'ready' }) }, order: [NODE], loaded: true })
  useUi.setState({ toasts: [] })
})
afterEach(() => setRpc(null))

describe('PasswordStep', () => {
  it('explains what the password is for and can be skipped', async () => {
    const onDone = vi.fn()
    const user = userEvent.setup()
    render(<PasswordStep nodeId={NODE} onDone={onDone} />)
    expect(screen.getByTestId('setup-password')).toHaveTextContent(/any device|任何设备/)
    expect(screen.getByTestId('setup-password')).toHaveTextContent(/optional|可以跳过|later|之后/)
    await user.click(screen.getByTestId('setup-pw-skip'))
    expect(onDone).toHaveBeenCalledTimes(1)
    expect(rpc.calls).toEqual([])
  })

  it('sets the password through node.set_enroll_password, then moves on', async () => {
    const onDone = vi.fn()
    const user = userEvent.setup()
    render(<PasswordStep nodeId={NODE} onDone={onDone} />)
    expect(screen.getByTestId('setup-pw-1')).toHaveAttribute('autocomplete', 'new-password')
    await user.type(screen.getByTestId('setup-pw-1'), 'correct horse')
    await user.type(screen.getByTestId('setup-pw-2'), 'correct horse')
    await user.click(screen.getByTestId('setup-pw-submit'))
    await waitFor(() => expect(onDone).toHaveBeenCalledTimes(1))
    expect(rpc.calls).toEqual([{ nodeId: NODE, method: 'node.set_enroll_password', params: { password: 'correct horse' } }])
    // 口令用完就丢
    expect(screen.getByTestId('setup-pw-1')).toHaveValue('')
  })

  it('refuses a short or mismatched password without calling the node', async () => {
    const onDone = vi.fn()
    const user = userEvent.setup()
    render(<PasswordStep nodeId={NODE} onDone={onDone} />)
    await user.type(screen.getByTestId('setup-pw-1'), 'short')
    await user.type(screen.getByTestId('setup-pw-2'), 'short')
    await user.click(screen.getByTestId('setup-pw-submit'))
    expect(await screen.findByText(/^(Use at least 8 characters\.|至少 8 个字符。)$/)).toBeInTheDocument()
    await user.clear(screen.getByTestId('setup-pw-1'))
    await user.type(screen.getByTestId('setup-pw-1'), 'long enough')
    await user.click(screen.getByTestId('setup-pw-submit'))
    expect(await screen.findByText(/different|不一样/)).toBeInTheDocument()
    expect(rpc.calls).toEqual([])
    expect(onDone).not.toHaveBeenCalled()
  })

  it('waits for the freshly paired node to connect', () => {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, conn: 'noise_handshaking' }) }, order: [NODE], loaded: true })
    render(<PasswordStep nodeId={NODE} onDone={() => {}} />)
    expect(screen.getByTestId('setup-pw-submit')).toBeDisabled()
    expect(screen.getByTestId('setup-password')).toHaveTextContent(/Connecting to the node|正在连接节点/)
  })

  it('stays on the step with an error when the node refuses', async () => {
    rpc.handlers['node.set_enroll_password'] = () => {
      throw rpcError('invalid')
    }
    const onDone = vi.fn()
    const user = userEvent.setup()
    render(<PasswordStep nodeId={NODE} onDone={onDone} />)
    await user.type(screen.getByTestId('setup-pw-1'), 'correct horse')
    await user.type(screen.getByTestId('setup-pw-2'), 'correct horse')
    await user.click(screen.getByTestId('setup-pw-submit'))
    await waitFor(() => expect(useUi.getState().toasts.some((t) => t.kind === 'error')).toBe(true))
    expect(onDone).not.toHaveBeenCalled()
  })
})
