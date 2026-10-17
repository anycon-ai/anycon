// 账号设置 → 邮箱（spec/web.md § 4.9）：绑定、更换、解绑和各个失败分支。通过 ProfilePage 渲染，走真实的 TanStack Query。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setUnauthorizedHandler } from '@/api/client'
import { ProfilePage } from '@/pages/settings/ProfilePage'
import { errorBody, installFakeApi, type ApiHandler } from '@/test/fakeApi'
import { useUi } from '@/stores/ui'

interface Hub {
  email: string | null
  methods: string[]
}
let hub: Hub
let unauthorized: ReturnType<typeof vi.fn<() => void>>

const me = () => ({ id: 'u1', display_name: 'Ada', is_admin: false, email: hub.email, login_methods: hub.methods })

function mount(over: Record<string, ApiHandler> = {}, loginMethods = ['token', 'email']) {
  const api = installFakeApi({
    'GET /api/config': () => ({ mode: 'single', protocol_version: 1, version: '0.0.0', login_methods: loginMethods }),
    'GET /api/me': () => me(),
    'POST /api/me/email': () => undefined,
    'POST /api/me/email/verify': (c) => {
      hub.email = (c.body as { email: string }).email
      if (!hub.methods.includes('email')) hub.methods = [...hub.methods, 'email']
      return undefined
    },
    'DELETE /api/me/email': () => {
      hub.email = null
      hub.methods = hub.methods.filter((m) => m !== 'email')
      return undefined
    },
    ...over,
  })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <ProfilePage />
    </QueryClientProvider>,
  )
  return api
}

const toasts = () => useUi.getState().toasts.map((t) => t.text)

beforeEach(() => {
  hub = { email: null, methods: ['token'] }
  useUi.setState({ toasts: [] })
  unauthorized = vi.fn<() => void>()
  setUnauthorizedHandler(unauthorized)
})
afterEach(() => {
  setUnauthorizedHandler(null)
  vi.unstubAllGlobals()
})

describe('email section — visibility', () => {
  it('without SMTP (no "email" login method) the email is read-only: shown if the account has one, no buttons', async () => {
    hub.email = 'old@example.com'
    mount({}, ['token'])
    expect(await screen.findByText('old@example.com')).toBeInTheDocument()
    expect(screen.queryByTestId('email-section')).toBeNull()
    expect(screen.queryByTestId('email-change')).toBeNull()
  })

  it('with SMTP and no email: a "Link an email" button and a hint', async () => {
    mount()
    expect(await screen.findByTestId('email-link')).toBeInTheDocument()
    expect(screen.getByTestId('email-section')).toHaveTextContent(/sign in with a one-time code|一次性验证码登录/)
  })

  it('with SMTP and an email: shows it with Change and Unlink', async () => {
    hub.email = 'me@example.com'
    hub.methods = ['token', 'email']
    mount()
    expect(await screen.findByTestId('email-current')).toHaveTextContent('me@example.com')
    expect(screen.getByTestId('email-change')).toBeEnabled()
    expect(screen.getByTestId('email-unlink')).toBeEnabled()
  })
})

describe('email section — linking', () => {
  it('email → code → verify (auto-submit on the 6th digit) → linked, shown, success toast', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('email-link'))
    await user.type(screen.getByTestId('email-input'), '  Me@Example.com ')
    await user.click(screen.getByTestId('email-send'))
    expect(api.callsTo('POST /api/me/email')[0]!.body).toEqual({ email: 'me@example.com' })
    expect(await screen.findByTestId('email-sent')).toHaveTextContent('me@example.com')

    const code = screen.getByTestId('email-code')
    expect(code).toHaveAttribute('autocomplete', 'one-time-code')
    await user.type(code, '12345')
    expect(api.callsTo('POST /api/me/email/verify')).toHaveLength(0)
    await user.type(code, '6')
    await waitFor(() => expect(api.callsTo('POST /api/me/email/verify')).toHaveLength(1))
    expect(api.callsTo('POST /api/me/email/verify')[0]!.body).toEqual({ email: 'me@example.com', code: '123456' })

    expect(await screen.findByTestId('email-current')).toHaveTextContent('me@example.com')
    expect(toasts().join(' ')).toMatch(/sign in with it on any device|任何设备上用这个邮箱登录/)
  })

  it('an address that someone else already linked: "already linked to another account", stays on the form', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/me/email': () => ({ status: 409, body: errorBody('email_taken') }) })
    await user.click(await screen.findByTestId('email-link'))
    await user.type(screen.getByTestId('email-input'), 'taken@example.com')
    await user.click(screen.getByTestId('email-send'))
    expect(await screen.findByText(/already linked to another account|已经被别的账号绑定了/)).toBeInTheDocument()
    expect(screen.queryByTestId('email-code')).toBeNull()
  })

  it('a wrong code says so, empties the box, and is not treated as a signed-out session even when the hub answers 401', async () => {
    const user = userEvent.setup()
    const api = mount({ 'POST /api/me/email/verify': () => ({ status: 401, body: errorBody('invalid_code') }) })
    await user.click(await screen.findByTestId('email-link'))
    await user.type(screen.getByTestId('email-input'), 'me@example.com')
    await user.click(screen.getByTestId('email-send'))
    await user.type(await screen.findByTestId('email-code'), '999999')
    expect(await screen.findByText(/wrong or has expired|验证码不对或已过期/)).toBeInTheDocument()
    expect(screen.getByTestId('email-code')).toHaveValue('')
    expect(unauthorized).not.toHaveBeenCalled()
    expect(hub.email).toBeNull()
    expect(api.callsTo('POST /api/me/email/verify')).toHaveLength(1)
  })

  it('a real signed-out 401 on verify still sends the user to the login page', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/me/email/verify': () => ({ status: 401, body: errorBody('unauthorized') }) })
    await user.click(await screen.findByTestId('email-link'))
    await user.type(screen.getByTestId('email-input'), 'me@example.com')
    await user.click(screen.getByTestId('email-send'))
    await user.type(await screen.findByTestId('email-code'), '123456')
    await waitFor(() => expect(unauthorized).toHaveBeenCalled())
  })

  it('a rate limit on sending shows how long to wait', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/me/email': () => ({ status: 429, body: errorBody('rate_limited'), headers: { 'Retry-After': '37' } }) })
    await user.click(await screen.findByTestId('email-link'))
    await user.type(screen.getByTestId('email-input'), 'me@example.com')
    await user.click(screen.getByTestId('email-send'))
    expect(await screen.findByText(/37/)).toBeInTheDocument()
  })

  it('Change: the same flow with the new address; the old address stays until the code is verified', async () => {
    hub.email = 'old@example.com'
    hub.methods = ['token', 'email']
    const user = userEvent.setup()
    mount()
    await user.click(await screen.findByTestId('email-change'))
    await user.type(screen.getByTestId('email-input'), 'old@example.com')
    expect(screen.getByTestId('email-send')).toBeDisabled() // 还是现在这个邮箱
    await user.clear(screen.getByTestId('email-input'))
    await user.type(screen.getByTestId('email-input'), 'new@example.com')
    await user.click(screen.getByTestId('email-send'))
    expect(await screen.findByTestId('email-sent')).toHaveTextContent('new@example.com')
    expect(hub.email).toBe('old@example.com')
    await user.type(screen.getByTestId('email-code'), '654321')
    await waitFor(() => expect(hub.email).toBe('new@example.com'))
    expect(await screen.findByTestId('email-current')).toHaveTextContent('new@example.com')
  })

  it('the resend countdown follows the hub’s resend_after_secs', async () => {
    const user = userEvent.setup()
    mount({ 'POST /api/me/email': () => ({ ok: true, resend_after_secs: 20 }) })
    await user.click(await screen.findByTestId('email-link'))
    await user.type(screen.getByTestId('email-input'), 'me@example.com')
    await user.click(screen.getByTestId('email-send'))
    expect(await screen.findByTestId('email-resend')).toHaveAttribute('data-seconds', '20')
  })

  it('Cancel leaves the form without calling the hub again', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('email-link'))
    await user.click(screen.getByTestId('email-cancel'))
    expect(screen.getByTestId('email-link')).toBeInTheDocument()
    expect(api.callsTo('POST /api/me/email')).toHaveLength(0)
  })
})

describe('email section — unlinking', () => {
  beforeEach(() => {
    hub.email = 'me@example.com'
  })

  it('asks for confirmation, then unlinks', async () => {
    hub.methods = ['token', 'email']
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('email-unlink'))
    const dialog = await screen.findByRole('dialog')
    expect(dialog).toHaveTextContent('me@example.com')
    expect(api.callsTo('DELETE /api/me/email')).toHaveLength(0)
    await user.click(within(dialog).getByTestId('email-unlink-confirm'))
    await waitFor(() => expect(api.callsTo('DELETE /api/me/email')).toHaveLength(1))
    expect(await screen.findByTestId('email-link')).toBeInTheDocument()
    expect(toasts().join(' ')).toMatch(/unlinked|已解绑/)
  })

  it('is disabled with an explanation when email is the account’s only sign-in method', async () => {
    hub.methods = ['email']
    mount()
    expect(await screen.findByTestId('email-unlink')).toBeDisabled()
    expect(screen.getByTestId('email-last-method')).toBeInTheDocument()
    expect(screen.getByTestId('email-change')).toBeEnabled() // 换绑不影响
  })

  it('a 409 last_login_method from the hub is explained (and the email stays)', async () => {
    hub.methods = ['token', 'email']
    const user = userEvent.setup()
    mount({ 'DELETE /api/me/email': () => ({ status: 409, body: errorBody('last_login_method') }) })
    await user.click(await screen.findByTestId('email-unlink'))
    await user.click(within(await screen.findByRole('dialog')).getByTestId('email-unlink-confirm'))
    await waitFor(() => expect(toasts().join(' ')).toMatch(/only sign-in method|唯一的登录方式/))
    expect(screen.getByTestId('email-current')).toHaveTextContent('me@example.com')
  })
})
