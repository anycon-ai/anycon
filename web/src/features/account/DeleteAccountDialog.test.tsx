// 删账号（spec/hub.md § 2.3 的 `DELETE /api/me`，spec/web.md § 4.9）：确认框、`reauth_required` → 重新登录、`bootstrap_admin`、成功后清本地数据再去 /login。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { errorBody, installFakeApi, type ApiHandler } from '@/test/fakeApi'
import { useUi } from '@/stores/ui'
import { mockRouter } from '@/test/routerMock'

const h = vi.hoisted(() => ({ navigate: vi.fn(), order: [] as string[], signOut: vi.fn(), resetClientState: vi.fn(), wipeLocalData: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}))
vi.mock('@/app/session', () => ({ signOut: h.signOut, resetClientState: h.resetClientState, wipeLocalData: h.wipeLocalData }))
// 页面里的邮箱块不是这里要测的
vi.mock('@/features/auth/EmailSection', () => ({ EmailSection: () => null }))

import { ProfilePage } from '@/pages/settings/ProfilePage'
import { DeleteAccountDialog } from './DeleteAccountDialog'

const FLAG = 'anycon.account_deleted'

function client() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } })
}

function mountDialog(del: ApiHandler, onDeleted = vi.fn()) {
  const api = installFakeApi({ 'DELETE /api/me': del })
  render(
    <QueryClientProvider client={client()}>
      <DeleteAccountDialog open onOpenChange={() => {}} onDeleted={onDeleted} />
    </QueryClientProvider>,
  )
  return { api, onDeleted }
}

beforeEach(() => {
  h.navigate.mockReset()
  h.order.length = 0
  h.signOut.mockReset().mockResolvedValue(undefined)
  h.resetClientState.mockReset().mockResolvedValue(undefined)
  h.wipeLocalData.mockReset().mockImplementation(async () => {
    h.order.push('wipe')
    sessionStorage.clear() // 真的 wipe 会清空 sessionStorage：标记必须在它之后写
  })
  sessionStorage.clear()
  useUi.setState({ toasts: [] })
})
afterEach(() => vi.unstubAllGlobals())

describe('DeleteAccountDialog — what it tells you', () => {
  it('says devices are removed from this hub, session history stays on your nodes, and nodes must be paired again', async () => {
    mountDialog(() => undefined)
    const dialog = await screen.findByTestId('delete-account-dialog')
    expect(dialog).toHaveTextContent(/devices, pairings, sign-in methods and notification settings are removed from this hub/)
    expect(dialog).toHaveTextContent(/Session history lives on your nodes and is not deleted/)
    expect(dialog).toHaveTextContent(/pair it again/)
  })
})

describe('DeleteAccountDialog — success', () => {
  it('DELETE /api/me, then clears this browser’s local data, then marks it for the login page and leaves', async () => {
    const user = userEvent.setup()
    const { api, onDeleted } = mountDialog(() => undefined)
    onDeleted.mockImplementation(() => h.order.push(`leave:${sessionStorage.getItem(FLAG)}`))
    await user.click(screen.getByTestId('delete-account-confirm'))
    await waitFor(() => expect(onDeleted).toHaveBeenCalledTimes(1))
    expect(api.callsTo('DELETE /api/me')).toHaveLength(1)
    expect(api.calls[0]!.headers['X-Anycon']).toBe('1')
    expect(h.order).toEqual(['wipe', 'leave:1'])
    // 账号已经没了：不去撤销设备 / 节点，也不再调退出登录
    expect(h.signOut).not.toHaveBeenCalled()
    expect(api.calls.map((c) => `${c.method} ${c.path}`)).toEqual(['DELETE /api/me'])
  })

  it('by default leaves by loading /login (a full page load, so memory state is dropped)', async () => {
    const user = userEvent.setup()
    const replace = vi.fn()
    vi.stubGlobal('location', { ...window.location, replace })
    installFakeApi({ 'DELETE /api/me': () => undefined })
    render(
      <QueryClientProvider client={client()}>
        <DeleteAccountDialog open onOpenChange={() => {}} />
      </QueryClientProvider>,
    )
    await user.click(screen.getByTestId('delete-account-confirm'))
    await waitFor(() => expect(replace).toHaveBeenCalledWith('/login'))
  })
})

describe('DeleteAccountDialog — refusals', () => {
  it('403 reauth_required: explains, keeps all local data, and offers "Sign in again"', async () => {
    const user = userEvent.setup()
    const { onDeleted } = mountDialog(() => ({ status: 403, body: errorBody('reauth_required') }))
    await user.click(screen.getByTestId('delete-account-confirm'))
    expect(await screen.findByTestId('delete-account-reauth')).toHaveTextContent(/recent sign-in/)
    expect(h.wipeLocalData).not.toHaveBeenCalled()
    expect(onDeleted).not.toHaveBeenCalled()
    expect(screen.queryByTestId('delete-account-confirm')).toBeNull()
  })

  it('"Sign in again" signs out, then goes to /login with a way back to the profile page', async () => {
    const user = userEvent.setup()
    mountDialog(() => ({ status: 403, body: errorBody('reauth_required') }))
    await user.click(screen.getByTestId('delete-account-confirm'))
    await user.click(await screen.findByTestId('delete-account-signin-again'))
    await waitFor(() => expect(h.navigate).toHaveBeenCalledWith({ to: '/login', search: { redirect: '/settings' } }))
    expect(h.signOut).toHaveBeenCalledTimes(1)
    expect(h.resetClientState).toHaveBeenCalledTimes(1)
    expect(h.wipeLocalData).not.toHaveBeenCalled() // 只是重新登录：设备密钥留着
  })

  it('"Sign in again" that fails to sign out says so and stays put', async () => {
    const user = userEvent.setup()
    h.signOut.mockRejectedValueOnce(new Error('boom'))
    mountDialog(() => ({ status: 403, body: errorBody('reauth_required') }))
    await user.click(screen.getByTestId('delete-account-confirm'))
    await user.click(await screen.findByTestId('delete-account-signin-again'))
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.kind)).toEqual(['error']))
    expect(h.navigate).not.toHaveBeenCalled()
  })

  it('409 bootstrap_admin: says this account cannot be deleted here, with nothing to retry', async () => {
    const user = userEvent.setup()
    const { onDeleted } = mountDialog(() => ({ status: 409, body: errorBody('bootstrap_admin') }))
    await user.click(screen.getByTestId('delete-account-confirm'))
    expect(await screen.findByTestId('delete-account-bootstrap')).toHaveTextContent(/administrator account created when the hub was set up/)
    expect(screen.queryByTestId('delete-account-confirm')).toBeNull()
    expect(h.wipeLocalData).not.toHaveBeenCalled()
    expect(onDeleted).not.toHaveBeenCalled()
  })

  it('any other failure is shown inline, keeps local data, and can be retried', async () => {
    const user = userEvent.setup()
    let fail = true
    const { onDeleted } = mountDialog(() => (fail ? { status: 500, body: errorBody('internal') } : undefined))
    await user.click(screen.getByTestId('delete-account-confirm'))
    expect(await screen.findByTestId('delete-account-error')).toHaveTextContent('The hub had an error.')
    expect(h.wipeLocalData).not.toHaveBeenCalled()
    fail = false
    await user.click(screen.getByTestId('delete-account-confirm'))
    await waitFor(() => expect(onDeleted).toHaveBeenCalledTimes(1))
    expect(screen.queryByTestId('delete-account-error')).toBeNull()
  })
})

describe('Profile page', () => {
  it('has a danger section whose button opens the confirmation', async () => {
    const user = userEvent.setup()
    installFakeApi({
      'GET /api/config': () => ({ mode: 'multi', protocol_version: 1, version: '0.0.0', login_methods: ['email'] }),
      'GET /api/me': () => ({ id: 'u1', display_name: 'Ada', is_admin: false, email: 'ada@example.com', login_methods: ['email'] }),
    })
    render(
      <QueryClientProvider client={client()}>
        <ProfilePage />
      </QueryClientProvider>,
    )
    expect(screen.queryByTestId('delete-account-dialog')).toBeNull()
    await user.click(await screen.findByTestId('delete-account'))
    expect(await screen.findByTestId('delete-account-dialog')).toBeInTheDocument()
    expect(screen.getByTestId('danger-zone')).toBeInTheDocument()
  })
})
