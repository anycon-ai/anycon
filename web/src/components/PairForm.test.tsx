import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'

const pair = vi.fn()
vi.mock('@/transport/instance', () => ({
  getClient: () => ({ pair, deviceName: 'Chrome · macOS' }),
}))

import { PairForm } from './PairForm'

beforeEach(() => {
  pair.mockReset()
  pair.mockResolvedValue({ nodeId: 'n'.repeat(32), publicKey: new Uint8Array(32) })
})

describe('PairForm', () => {
  it('formats 8- and 16-character codes while typing and enables submit for both', async () => {
    const user = userEvent.setup()
    render(<PairForm onPaired={() => {}} />)
    const input = screen.getByTestId('pair-code-input')
    await user.type(input, '7k3pq2mx')
    expect(input).toHaveValue('7K3P-Q2MX')
    expect(screen.getByTestId('pair-submit')).toBeEnabled()
    await user.type(input, 'abcd')
    expect(input).toHaveValue('7K3P-Q2MX-ABCD')
    expect(screen.getByTestId('pair-submit')).toBeDisabled() // 12 位不合法
    await user.type(input, 'efghjk')
    expect(input).toHaveValue('7K3P-Q2MX-ABCD-EFGH') // 最多 16 位
    expect(screen.getByTestId('pair-submit')).toBeEnabled()
  })

  it('accepts a whole pairing link pasted into the box and takes the code from its #code= fragment', async () => {
    const user = userEvent.setup()
    render(<PairForm onPaired={() => {}} />)
    const input = screen.getByTestId('pair-code-input')
    await user.click(input)
    await user.paste('https://hub.example.com/pair#code=7k3p-q2mx-abcd-efgh')
    expect(input).toHaveValue('7K3P-Q2MX-ABCD-EFGH')
    expect(screen.getByTestId('pair-submit')).toBeEnabled()
  })

  it('pairs with the typed code on submit', async () => {
    const user = userEvent.setup()
    const onPaired = vi.fn()
    render(<PairForm onPaired={onPaired} />)
    await user.type(screen.getByTestId('pair-code-input'), '7k3pq2mx')
    await user.click(screen.getByTestId('pair-submit'))
    await waitFor(() => expect(onPaired).toHaveBeenCalled())
    expect(pair).toHaveBeenCalledWith('7K3PQ2MX', expect.any(String), expect.any(Function))
  })

  it('shows the "deriving keys" step while the (slow) key derivation runs', async () => {
    pair.mockImplementation((_code: string, _name: string, report: (stage: string) => void) => {
      report('connecting')
      report('deriving')
      return new Promise(() => {}) // 一直停在派生密钥这一步
    })
    const user = userEvent.setup()
    render(<PairForm onPaired={() => {}} />)
    await user.type(screen.getByTestId('pair-code-input'), '7k3pq2mx')
    await user.click(screen.getByTestId('pair-submit'))
    const progress = await screen.findByTestId('pair-progress')
    await waitFor(() => expect(progress).toHaveAttribute('data-phase', 'deriving'))
    expect(progress).toHaveTextContent(/正在派生密钥|Deriving keys/)
  })

  it('a code that came from a link is never submitted automatically: it waits for an explicit confirmation', async () => {
    const user = userEvent.setup()
    const onPaired = vi.fn()
    render(<PairForm initialCode="7K3P-Q2MX-ABCD-EFGH" fromLink onPaired={onPaired} />)
    expect(screen.getByTestId('pair-link-confirm')).toBeInTheDocument()
    expect(screen.getByTestId('pair-link-code')).toHaveTextContent('7K3P-Q2MX-ABCD-EFGH')
    // 给异步副作用一点时间：什么都不该发生
    await new Promise((r) => setTimeout(r, 50))
    expect(pair).not.toHaveBeenCalled()
    await user.click(screen.getByTestId('pair-link-confirm-submit'))
    await waitFor(() => expect(onPaired).toHaveBeenCalled())
    expect(pair).toHaveBeenCalledWith('7K3PQ2MXABCDEFGH', expect.any(String), expect.any(Function))
  })

  it('cancelling the confirmation falls back to the empty manual form', async () => {
    const user = userEvent.setup()
    render(<PairForm initialCode="7K3P-Q2MX" fromLink onPaired={() => {}} />)
    await user.click(screen.getByTestId('pair-link-cancel'))
    expect(screen.queryByTestId('pair-link-confirm')).toBeNull()
    expect(screen.getByTestId('pair-code-input')).toHaveValue('')
    expect(pair).not.toHaveBeenCalled()
  })

  it('shows an explicit error when the stored key of that node changed', async () => {
    const { PairError } = await import('@/transport/client')
    pair.mockRejectedValue(new PairError('key_changed'))
    const user = userEvent.setup()
    render(<PairForm onPaired={() => {}} />)
    await user.type(screen.getByTestId('pair-code-input'), '7k3pq2mx')
    await user.click(screen.getByTestId('pair-submit'))
    expect(await screen.findByText(/先在设置里移除它|Remove it in settings first/)).toBeInTheDocument()
  })
})
