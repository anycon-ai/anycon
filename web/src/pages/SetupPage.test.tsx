// /setup 的第 3 步：配对完成后先问要不要设节点口令（可跳过），再进「完成」（spec/web.md § 4.2）。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'

const h = vi.hoisted(() => ({ navigate: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}))
vi.mock('@/features/pairing/useLinkCode', () => ({ useLinkCode: () => '' }))
vi.mock('@/features/setup/PasswordStep', () => ({
  PasswordStep: ({ nodeId, onDone }: { nodeId: string; onDone: () => void }) => (
    <div data-testid="password-step" data-node={nodeId}>
      <button type="button" onClick={onDone} data-testid="password-step-done" />
    </div>
  ),
}))
vi.mock('@/features/setup/NotificationsStep', () => ({
  NotificationsStep: ({ onDone }: { onDone: () => void }) => (
    <div data-testid="notify-step">
      <button type="button" onClick={onDone} data-testid="notify-step-done" />
    </div>
  ),
}))
vi.mock('@/components/PairForm', () => ({
  PairForm: ({ onPaired }: { onPaired: (id: string) => void }) => (
    <button type="button" data-testid="fake-pair" onClick={() => onPaired('n'.repeat(32))}>
      pair
    </button>
  ),
}))

import { SetupPage } from './SetupPage'

const NODE = 'n'.repeat(32)

beforeEach(() => {
  h.navigate.mockReset()
  useNodes.setState({ byId: {}, order: [], loaded: true })
})

describe('SetupPage — after pairing', () => {
  it('asks about a node password for the node that was just paired, and only then shows "all set"', async () => {
    const user = userEvent.setup()
    render(<SetupPage />)
    await user.click(screen.getByTestId('setup-next')) // 第 1 步 → 配对
    await user.click(screen.getByTestId('fake-pair'))
    expect(screen.getByTestId('password-step')).toHaveAttribute('data-node', NODE)
    expect(screen.queryByTestId('setup-finish')).toBeNull()

    await user.click(screen.getByTestId('password-step-done')) // 设了或跳过，一样往下走
    expect(screen.queryByTestId('password-step')).toBeNull()
    // 然后问要不要开通知（开了或跳过，一样往下走），之后才是「完成」
    expect(screen.getByTestId('notify-step')).toBeInTheDocument()
    expect(screen.queryByTestId('setup-finish')).toBeNull()
    await user.click(screen.getByTestId('notify-step-done'))
    expect(screen.queryByTestId('notify-step')).toBeNull()
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE }) }, order: [NODE], loaded: true })
    await user.click(screen.getByTestId('setup-finish'))
    expect(h.navigate).toHaveBeenCalledWith({ to: '/n/$node', params: { node: 'nnnnnnnn' } })
  })

  it('goes straight to "all set" when the user skips pairing (nothing to put a password on)', async () => {
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE }) }, order: [NODE], loaded: true })
    const user = userEvent.setup()
    render(<SetupPage />)
    await user.click(screen.getByTestId('setup-next'))
    await user.click(screen.getByRole('button', { name: /Skip|跳过/ }))
    expect(screen.queryByTestId('password-step')).toBeNull()
    expect(screen.queryByTestId('notify-step')).toBeNull() // 没配对新节点，没有可问的
    expect(screen.getByTestId('setup-finish')).toBeInTheDocument()
  })
})

describe('SetupPage — install step', () => {
  it('picks the PowerShell installer on Windows', () => {
    const ua = vi
      .spyOn(navigator, 'userAgent', 'get')
      .mockReturnValue('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36')
    try {
      render(<SetupPage />)
      const commands = screen.getByTestId('setup-commands')
      expect(commands).toHaveTextContent('install.ps1 | iex')
      expect(commands).not.toHaveTextContent('install.sh')
    } finally {
      ua.mockRestore()
    }
  })
})
