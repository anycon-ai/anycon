// 账号菜单的「退出登录」：走 `signOut()`（先退订本浏览器的推送再结束会话），成功才清内存并回登录页；失败要提示、不假装退出。
import type { ReactNode } from 'react'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockRouter } from '@/test/routerMock'
import { useUi } from '@/stores/ui'

const navigate = vi.hoisted(() => vi.fn())
const session = vi.hoisted(() => ({ signOut: vi.fn(), resetClientState: vi.fn(async () => undefined) }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, {}))
vi.mock('@/api/queries', () => ({ useMe: () => ({ data: undefined }), useDevices: () => ({ data: undefined }), logout: vi.fn() }))
vi.mock('@/app/session', () => ({ signOut: session.signOut, resetClientState: session.resetClientState, signOutAndRemoveDevice: vi.fn() }))

import { Sidebar } from './Sidebar'

function wrap(ui: ReactNode) {
  return render(<QueryClientProvider client={new QueryClient()}>{ui}</QueryClientProvider>)
}

beforeEach(() => {
  navigate.mockReset()
  session.signOut.mockReset()
  session.resetClientState.mockClear()
  useUi.setState({ toasts: [] })
})

async function openSignOut() {
  const user = userEvent.setup()
  wrap(<Sidebar nodeId={undefined} />)
  await user.click(screen.getByTestId('account-menu'))
  await user.click(await screen.findByTestId('logout'))
  await user.click(await screen.findByTestId('logout-confirm'))
}

describe('account menu — sign out', () => {
  it('signs out through signOut() (push first, then the session), clears memory and goes to the login page', async () => {
    session.signOut.mockResolvedValue(undefined)
    await openSignOut()
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: '/login', search: {} }))
    expect(session.signOut).toHaveBeenCalledTimes(1)
    expect(session.resetClientState).toHaveBeenCalledTimes(1)
  })

  it('a failed sign-out says you are still signed in, and does not clear anything', async () => {
    session.signOut.mockRejectedValue(new Error('hub down'))
    await openSignOut()
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.kind)).toContain('error'))
    expect(session.resetClientState).not.toHaveBeenCalled()
    expect(navigate).not.toHaveBeenCalledWith({ to: '/login', search: {} })
  })
})
