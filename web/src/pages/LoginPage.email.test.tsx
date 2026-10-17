// 登录页按 /api/config.login_methods 决定显示什么（spec/web.md § 4.1，ADR-37）。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { errorBody, installFakeApi, type ApiHandler } from '@/test/fakeApi'

const h = vi.hoisted(() => ({ replace: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => ({ ...(await orig<object>()), useRouter: () => ({ history: { replace: h.replace } }) }))

import { LoginPage } from './LoginPage'

const base = { mode: 'single', protocol_version: 1, version: '0.0.0' }
let loggedIn = false

function mount(methods: string[], extra: Record<string, ApiHandler> = {}) {
  const api = installFakeApi({
    'GET /api/config': () => ({ ...base, login_methods: methods }),
    'GET /api/me': () => (loggedIn ? { id: 'u1', display_name: 'U', is_admin: false, login_methods: ['email'], email: 'me@example.com' } : { status: 401, body: errorBody('unauthorized') }),
    'POST /auth/email/send': () => undefined,
    'POST /auth/email/verify': () => {
      loggedIn = true
      return undefined
    },
    'POST /auth/token': () => {
      loggedIn = true
      return undefined
    },
    ...extra,
  })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <LoginPage />
    </QueryClientProvider>,
  )
  return api
}

beforeEach(() => {
  loggedIn = false
  h.replace.mockReset()
  sessionStorage.clear()
})
afterEach(() => vi.unstubAllGlobals())

describe('LoginPage — which methods show', () => {
  it('token only (no SMTP): the token form is right there, no email box (unchanged)', async () => {
    mount(['token'])
    expect(await screen.findByTestId('login-token-input')).toBeInTheDocument()
    expect(screen.queryByTestId('login-email-input')).toBeNull()
    expect(screen.queryByTestId('login-use-token')).toBeNull()
  })

  it('token + email: the email box comes first and the token form is folded behind "Use an access token instead"', async () => {
    const user = userEvent.setup()
    mount(['token', 'email'])
    expect(await screen.findByTestId('login-email-input')).toBeInTheDocument()
    expect(screen.queryByTestId('login-token-input')).toBeNull()
    await user.click(screen.getByTestId('login-use-token'))
    expect(screen.getByTestId('login-token-input')).toBeInTheDocument()
    expect(screen.queryByTestId('login-use-token')).toBeNull()
    expect(screen.getByTestId('login-email-input')).toBeInTheDocument()
  })

  it('email only: just the email box', async () => {
    mount(['email'])
    expect(await screen.findByTestId('login-email-input')).toBeInTheDocument()
    expect(screen.queryByTestId('login-token-input')).toBeNull()
    expect(screen.queryByTestId('login-use-token')).toBeNull()
  })

  it('no method at all says so', async () => {
    mount([])
    expect(await screen.findByText(/No sign-in method is enabled|没有启用任何登录方式/)).toBeInTheDocument()
  })
})

describe('LoginPage — signing in with an email code', () => {
  it('email → code → auto-submit → refreshes /api/me and leaves the login page', async () => {
    const user = userEvent.setup()
    const api = mount(['email'])
    await user.type(await screen.findByTestId('login-email-input'), 'me@example.com')
    await user.click(screen.getByTestId('login-email-send'))
    await user.type(await screen.findByTestId('login-code-input'), '482915')
    await waitFor(() => expect(h.replace).toHaveBeenCalledWith('/'))
    expect(api.callsTo('POST /auth/email/verify')[0]!.body).toEqual({ email: 'me@example.com', code: '482915' })
  })

  it('a wrong code keeps the user on the page', async () => {
    const user = userEvent.setup()
    mount(['email'], { 'POST /auth/email/verify': () => ({ status: 401, body: errorBody('invalid_code') }) })
    await user.type(await screen.findByTestId('login-email-input'), 'me@example.com')
    await user.click(screen.getByTestId('login-email-send'))
    await user.type(await screen.findByTestId('login-code-input'), '000000')
    expect(await screen.findByText(/wrong or has expired|验证码不对或已过期/)).toBeInTheDocument()
    expect(h.replace).not.toHaveBeenCalled()
  })

  it('the token path still works behind the fold', async () => {
    const user = userEvent.setup()
    const api = mount(['token', 'email'])
    await user.click(await screen.findByTestId('login-use-token'))
    await user.type(screen.getByTestId('login-token-input'), 'anycon_secret')
    await user.click(screen.getByTestId('login-submit'))
    await waitFor(() => expect(h.replace).toHaveBeenCalledWith('/'))
    expect(api.callsTo('POST /auth/token')[0]!.body).toEqual({ token: 'anycon_secret' })
  })
})
