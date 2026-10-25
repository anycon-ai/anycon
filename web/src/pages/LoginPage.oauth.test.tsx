// 登录页的 OAuth 部分（spec/web.md § 4.1）：按 /api/config.login_methods 出 GitHub / Google 按钮，是整页导航的链接；
// 不在 hub 的公开地址上时禁用并说明；`?oauth_error=` 显示成行内错误；`multi` 的邮箱文案是「登录或创建账号」。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { useUi } from '@/stores/ui'
import { errorBody, installFakeApi, type ApiHandler } from '@/test/fakeApi'

const h = vi.hoisted(() => ({ replace: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => ({ ...(await orig<object>()), useRouter: () => ({ history: { replace: h.replace } }) }))

import { LoginPage } from './LoginPage'

const OTHER_ORIGIN = 'https://hub.example.com'

function mount(config: Record<string, unknown>, props: Parameters<typeof LoginPage>[0] = {}, extra: Record<string, ApiHandler> = {}) {
  const api = installFakeApi({
    'GET /api/config': () => ({ mode: 'single', protocol_version: 1, version: '0.0.0', login_methods: ['token'], ...config }),
    'GET /api/me': () => ({ status: 401, body: errorBody('unauthorized') }),
    ...extra,
  })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <LoginPage {...props} />
    </QueryClientProvider>,
  )
  return api
}

beforeEach(() => {
  h.replace.mockReset()
  sessionStorage.clear()
  useUi.setState({ toasts: [] })
})
afterEach(() => vi.unstubAllGlobals())

describe('LoginPage — OAuth buttons', () => {
  it('shows a button per configured provider, in the hub’s order, as plain links to /auth/oauth/<p>/start', async () => {
    mount({ mode: 'multi', public_url: location.origin, login_methods: ['github', 'google', 'email'] })
    const box = await screen.findByTestId('oauth-buttons')
    const links = within(box).getAllByRole('link')
    expect(links.map((a) => a.getAttribute('href'))).toEqual(['/auth/oauth/github/start', '/auth/oauth/google/start'])
    expect(links[0]).toHaveTextContent(/GitHub/)
    expect(links[1]).toHaveTextContent(/Google/)
  })

  it('shows only the providers the hub has configured', async () => {
    mount({ mode: 'multi', public_url: location.origin, login_methods: ['google', 'email'] })
    expect(await screen.findByTestId('oauth-google')).toBeInTheDocument()
    expect(screen.queryByTestId('oauth-github')).toBeNull()
  })

  it('no OAuth button when no provider is configured', async () => {
    mount({ login_methods: ['token', 'email'] })
    expect(await screen.findByTestId('login-email-input')).toBeInTheDocument()
    expect(screen.queryByTestId('oauth-buttons')).toBeNull()
  })

  it('carries the post-login redirect as next=', async () => {
    mount({ public_url: location.origin, login_methods: ['github', 'token'] }, { redirect: '/n/abc?tab=agent' })
    const href = (await screen.findByTestId('oauth-github')).getAttribute('href')!
    expect(new URL(href, 'http://h').searchParams.get('next')).toBe('/n/abc?tab=agent')
  })

  it('does not trust a redirect that is not a same-origin path', async () => {
    mount({ public_url: location.origin, login_methods: ['github'] }, { redirect: '//evil.example/x' })
    expect((await screen.findByTestId('oauth-github')).getAttribute('href')).toBe('/auth/oauth/github/start')
  })

  it('on the wrong origin the buttons are disabled, not links, and say which address to use', async () => {
    mount({ mode: 'multi', public_url: OTHER_ORIGIN, login_methods: ['github', 'google'] })
    const github = await screen.findByTestId('oauth-github')
    expect(github).toBeDisabled()
    expect(github.tagName).toBe('BUTTON')
    expect(screen.queryAllByRole('link')).toHaveLength(0)
    expect(github.parentElement).toHaveAttribute('title', expect.stringContaining(OTHER_ORIGIN))
    expect(screen.getByTestId('oauth-google')).toBeDisabled()
    expect(screen.getByTestId('oauth-origin-note')).toHaveTextContent(OTHER_ORIGIN)
  })

  it('without a public_url there is nothing to compare with, so the buttons work', async () => {
    mount({ login_methods: ['github'] })
    expect(await screen.findByTestId('oauth-github')).not.toBeDisabled()
  })
})

describe('LoginPage — token and email next to OAuth', () => {
  it('order: OAuth buttons, then email, then the token link folded away', async () => {
    mount({ public_url: location.origin, login_methods: ['github', 'email', 'token'] })
    const email = await screen.findByTestId('login-email-input')
    const oauth = screen.getByTestId('oauth-buttons')
    expect(oauth.compareDocumentPosition(email) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    const token = screen.getByTestId('login-use-token')
    expect(email.compareDocumentPosition(token) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    expect(screen.queryByTestId('login-token-input')).toBeNull()
  })

  it('token is folded behind the link even when OAuth is the only other method', async () => {
    const user = userEvent.setup()
    mount({ public_url: location.origin, login_methods: ['github', 'token'] })
    await user.click(await screen.findByTestId('login-use-token'))
    expect(screen.getByTestId('login-token-input')).toBeInTheDocument()
  })

  it('multi never offers a token form, even if a stale response lists it nowhere', async () => {
    mount({ mode: 'multi', public_url: location.origin, login_methods: ['github', 'email'] })
    expect(await screen.findByTestId('login-email-input')).toBeInTheDocument()
    expect(screen.queryByTestId('login-use-token')).toBeNull()
    expect(screen.queryByTestId('login-token-input')).toBeNull()
  })
})

describe('LoginPage — ?oauth_error', () => {
  it.each([
    ['cancelled', 'github', /GitHub sign-in was cancelled/],
    ['provider_error', 'google', /Google could not complete/],
    ['invalid_state', 'github', /expired|another browser/],
    ['registration_closed', 'github', /invite-only/],
    ['invite_invalid', 'github', /invitation link is not valid/],
    ['invite_expired', 'github', /expired/],
    ['invite_used', 'github', /already been used/],
    ['identity_taken', 'google', /already linked to another account/],
    ['already_linked', 'google', /different Google account/],
    ['not_signed_in', 'github', /signed in to link/],
    ['server_error', 'github', /hub hit an error while finishing the GitHub sign-in/],
  ] as const)('%s shows its own message', async (code, provider, text) => {
    mount({ mode: 'multi', public_url: location.origin, login_methods: ['github', 'google'] }, { oauthError: code, provider })
    const alert = await screen.findByTestId('oauth-error')
    expect(alert).toHaveAttribute('role', 'alert')
    expect(alert).toHaveTextContent(text)
  })

  it('an unknown code (or a missing provider) falls back to a generic message', async () => {
    mount({ login_methods: ['github'], public_url: location.origin }, { oauthError: 'something_new' })
    const alert = await screen.findByTestId('oauth-error')
    expect(alert).toHaveTextContent(/did not work/)
    expect(alert).not.toHaveTextContent(/undefined|something_new/)
  })

  it('no error box without the parameter', async () => {
    mount({ login_methods: ['github'], public_url: location.origin })
    await screen.findByTestId('oauth-buttons')
    expect(screen.queryByTestId('oauth-error')).toBeNull()
  })
})

describe('LoginPage — multi copy', () => {
  it('says "or create an account" and the email hint explains the account is created on first sign-in', async () => {
    mount({ mode: 'multi', login_methods: ['email'] })
    expect(await screen.findByText(/create an account/i)).toBeInTheDocument()
    expect(screen.getByText(/one is created when you enter it/)).toBeInTheDocument()
    expect(screen.queryByText(/Link your email in Account settings first/)).toBeNull()
  })

  it('single keeps its old copy ("link your email first")', async () => {
    mount({ mode: 'single', login_methods: ['email', 'token'] })
    expect(await screen.findByText(/Link your email in Account settings first/)).toBeInTheDocument()
  })

  it('the code step in multi says entering the code creates the account for new users', async () => {
    const user = userEvent.setup()
    mount({ mode: 'multi', login_methods: ['email'] }, {}, { 'POST /auth/email/send': () => undefined })
    await user.type(await screen.findByTestId('login-email-input'), 'new@example.com')
    await user.click(screen.getByTestId('login-email-send'))
    expect(await screen.findByTestId('login-email-sent')).toHaveTextContent(/creates your account if you are new/)
    expect(screen.getByTestId('login-code-submit')).toHaveTextContent(/Continue/)
  })
})

describe('LoginPage — invites (single)', () => {
  it('a stored invite code goes along on the OAuth start links', async () => {
    sessionStorage.setItem('invite', 'inv_123')
    mount({ public_url: location.origin, login_methods: ['github', 'google', 'token'] })
    const github = await screen.findByTestId('oauth-github')
    expect(new URL(github.getAttribute('href')!, 'http://h').searchParams.get('invite')).toBe('inv_123')
    expect(screen.getByTestId('login-invite-accept')).toBeInTheDocument()
  })

  it('multi ignores a stale invite code: no invitation panel, no invite= parameter', async () => {
    sessionStorage.setItem('invite', 'inv_123')
    mount({ mode: 'multi', public_url: location.origin, login_methods: ['github', 'email'] })
    const github = await screen.findByTestId('oauth-github')
    expect(github.getAttribute('href')).toBe('/auth/oauth/github/start')
    expect(screen.queryByTestId('login-invite-accept')).toBeNull()
  })
})

describe('LoginPage — after deleting an account', () => {
  it('shows the "account deleted" toast once', async () => {
    sessionStorage.setItem('anycon.account_deleted', '1')
    mount({ mode: 'multi', login_methods: ['email'] })
    await screen.findByTestId('login-email-input')
    expect(useUi.getState().toasts.map((t) => `${t.kind}:${t.text}`)).toEqual(['success:Your account has been deleted.'])
    expect(sessionStorage.getItem('anycon.account_deleted')).toBeNull()
  })
})
