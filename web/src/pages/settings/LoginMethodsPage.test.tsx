// /settings/login：已关联的账号（关联 / 解除）、token（只在 single）、最后一种登录方式不能移除、`?linked=` / `?oauth_error=` 的处理。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { useUi } from '@/stores/ui'
import { errorBody, installFakeApi, type ApiHandler } from '@/test/fakeApi'
import { mockRouter } from '@/test/routerMock'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, {}))

import { LoginMethodsPage } from './LoginMethodsPage'

interface World {
  config: Record<string, unknown>
  methods: string[]
  identities: Array<{ provider: string; created_at_ms: number }>
  tokens: Array<{ id: string; label: string; created_at_ms: number; last_used_ms?: number }>
}
let w: World

const toasts = () => useUi.getState().toasts.map((t) => `${t.kind}:${t.text}`)

function mount(props: Parameters<typeof LoginMethodsPage>[0] = {}, extra: Record<string, ApiHandler> = {}) {
  const api = installFakeApi({
    'GET /api/config': () => w.config,
    'GET /api/me': () => ({ id: 'u1', display_name: 'Ada', is_admin: false, login_methods: w.methods }),
    'GET /api/me/identities': () => w.identities,
    'DELETE /api/me/identities/*': (c) => {
      const p = c.path.split('/').pop()
      w.identities = w.identities.filter((i) => i.provider !== p)
      w.methods = w.methods.filter((m) => m !== p)
      return undefined
    },
    'GET /api/me/tokens': () => w.tokens,
    'DELETE /api/me/tokens/*': (c) => {
      const id = c.path.split('/').pop()
      w.tokens = w.tokens.filter((t) => t.id !== id)
      return undefined
    },
    ...extra,
  })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <LoginMethodsPage {...props} />
    </QueryClientProvider>,
  )
  return api
}

beforeEach(() => {
  navigate.mockReset()
  useUi.setState({ toasts: [] })
  w = {
    config: { mode: 'multi', public_url: location.origin, protocol_version: 1, version: '0.0.0', login_methods: ['github', 'google', 'email'] },
    methods: ['github', 'email'],
    identities: [{ provider: 'github', created_at_ms: 1_700_000_000_000 }],
    tokens: [],
  }
})
afterEach(() => vi.unstubAllGlobals())

describe('linked accounts', () => {
  it('lists the configured providers: linked ones offer Unlink, the others a Link that navigates to /auth/oauth/<p>/start?link=1', async () => {
    mount()
    expect(await screen.findByTestId('identity-unlink-github')).toBeEnabled()
    expect(screen.getByTestId('identity-github')).toHaveAttribute('data-linked', 'true')
    expect(screen.getByTestId('identity-google')).toHaveAttribute('data-linked', 'false')
    expect(screen.getByTestId('identity-link-google')).toHaveAttribute('href', '/auth/oauth/google/start?link=1')
  })

  it('does not list providers the hub has not configured', async () => {
    w.config = { ...w.config, login_methods: ['github', 'email'] }
    mount()
    await screen.findByTestId('identity-github')
    expect(screen.queryByTestId('identity-google')).toBeNull()
  })

  it('no linked-accounts section at all when no provider is configured', async () => {
    w.config = { ...w.config, login_methods: ['email'] }
    mount()
    expect(await screen.findByTestId('login-email-only')).toBeInTheDocument()
    expect(screen.queryByTestId('linked-accounts')).toBeNull()
  })

  it('Unlink calls DELETE /api/me/identities/<p>, then shows the provider as not linked and says so', async () => {
    const user = userEvent.setup()
    const api = mount()
    await user.click(await screen.findByTestId('identity-unlink-github'))
    await waitFor(() => expect(screen.getByTestId('identity-github')).toHaveAttribute('data-linked', 'false'))
    expect(api.callsTo('DELETE /api/me/identities/github')).toHaveLength(1)
    expect(api.calls.find((c) => c.method === 'DELETE')!.headers['X-Anycon']).toBe('1')
    expect(toasts()).toEqual(['success:GitHub unlinked.'])
  })

  it('a provider that is the account’s only sign-in method cannot be unlinked: disabled, with the reason', async () => {
    w.methods = ['github']
    mount()
    const unlink = await screen.findByTestId('identity-unlink-github')
    await waitFor(() => expect(screen.queryByTestId('identity-last-github')).not.toBeNull())
    expect(unlink).toBeDisabled()
    expect(screen.getByTestId('identity-last-github')).toHaveTextContent(/only way to sign in/)
  })

  it('stays disabled until /api/me says what the other methods are (never guess)', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((r) => (release = r))
    mount({}, { 'GET /api/me': async () => { await gate; return { id: 'u1', display_name: 'Ada', is_admin: false, login_methods: w.methods } } })
    expect(await screen.findByTestId('identity-unlink-github')).toBeDisabled()
    release()
    await waitFor(() => expect(screen.getByTestId('identity-unlink-github')).toBeEnabled())
  })

  it('a refused unlink (409 last_login_method) is explained, and the link stays', async () => {
    const user = userEvent.setup()
    mount({}, { 'DELETE /api/me/identities/*': () => ({ status: 409, body: errorBody('last_login_method') }) })
    await user.click(await screen.findByTestId('identity-unlink-github'))
    await waitFor(() => expect(toasts()).toEqual(['error:That is your only sign-in method, so it cannot be removed.']))
    expect(screen.getByTestId('identity-github')).toHaveAttribute('data-linked', 'true')
  })

  it('on the wrong origin Link is disabled with an explanation (like the login page)', async () => {
    w.config = { ...w.config, public_url: 'https://hub.example.com' }
    mount()
    const link = await screen.findByTestId('identity-link-google')
    expect(link).toBeDisabled()
    expect(link.parentElement).toHaveAttribute('title', expect.stringContaining('https://hub.example.com'))
    // 解除不经过 OAuth，不受 origin 限制
    expect(screen.getByTestId('identity-unlink-github')).toBeEnabled()
  })
})

describe('tokens', () => {
  beforeEach(() => {
    w.config = { ...w.config, mode: 'single', login_methods: ['github', 'token'] }
    w.methods = ['github', 'token']
    w.tokens = [{ id: 't1', label: 'phone', created_at_ms: 1 }]
  })

  it('single shows the tokens section; multi neither shows it nor asks for /api/me/tokens', async () => {
    const api = mount()
    expect(await screen.findByTestId('tokens-section')).toBeInTheDocument()
    expect(await screen.findByText('phone')).toBeInTheDocument()
    expect(api.callsTo('GET /api/me/tokens')).toHaveLength(1)
  })

  it('multi: no tokens section and no request for tokens', async () => {
    w.config = { ...w.config, mode: 'multi', login_methods: ['github', 'email'] }
    const api = mount()
    await screen.findByTestId('linked-accounts')
    expect(screen.queryByTestId('tokens-section')).toBeNull()
    expect(api.callsTo('GET /api/me/tokens')).toHaveLength(0)
  })

  it('the only token can be deleted while another method exists (the rule is /api/me.login_methods, not the token count)', async () => {
    mount()
    await screen.findByText('phone')
    expect(screen.getByRole('button', { name: 'Delete token' })).toBeEnabled()
  })

  it('the last token cannot be deleted when tokens are the account’s only sign-in method', async () => {
    w.methods = ['token']
    mount()
    await screen.findByText('phone')
    await waitFor(() => expect(screen.getByRole('button', { name: 'Delete token' })).toBeDisabled())
    expect(screen.getByText('The last sign-in method cannot be removed.', { selector: 'p' })).toBeInTheDocument()
  })

  it('with tokens as the only method but several tokens, deleting one is fine', async () => {
    w.methods = ['token']
    w.tokens = [
      { id: 't1', label: 'phone', created_at_ms: 1 },
      { id: 't2', label: 'laptop', created_at_ms: 2 },
    ]
    const user = userEvent.setup()
    const api = mount()
    await screen.findByText('laptop')
    const buttons = screen.getAllByRole('button', { name: 'Delete token' })
    expect(buttons).toHaveLength(2)
    expect(buttons[0]).toBeEnabled()
    await user.click(buttons[0]!)
    await waitFor(() => expect(api.callsTo('DELETE /api/me/tokens/t1')).toHaveLength(1))
  })
})

describe('callback query handling', () => {
  it('?linked=<p>: success toast, refreshes the identities, then strips the query', async () => {
    const api = mount({ linked: 'google' })
    await waitFor(() => expect(toasts()).toEqual(['success:Google linked. You can now sign in with it.']))
    expect(navigate).toHaveBeenCalledWith({ to: '/settings/login', search: {}, replace: true })
    expect(navigate).toHaveBeenCalledTimes(1)
    await screen.findByTestId('identity-github')
    expect(api.callsTo('GET /api/me/identities').length).toBeGreaterThanOrEqual(1)
  })

  it('?oauth_error=<code>&provider=<p>: an inline error that stays after the query is gone, dismissible', async () => {
    const user = userEvent.setup()
    mount({ oauthError: 'identity_taken', provider: 'github' })
    const notice = await screen.findByTestId('login-methods-notice')
    expect(notice).toHaveTextContent(/GitHub account is already linked to another account/)
    expect(navigate).toHaveBeenCalledWith({ to: '/settings/login', search: {}, replace: true })
    await user.click(screen.getByRole('button', { name: 'Dismiss' }))
    expect(screen.queryByTestId('login-methods-notice')).toBeNull()
  })

  it('an unknown error code gets the generic message', async () => {
    mount({ oauthError: 'weird', provider: 'google' })
    expect(await screen.findByTestId('login-methods-notice')).toHaveTextContent(/Google did not work|Signing in with Google did not work/)
  })

  it('nothing to handle without the parameters: no toast, no navigation', async () => {
    mount()
    await screen.findByTestId('identity-github')
    expect(toasts()).toEqual([])
    expect(navigate).not.toHaveBeenCalled()
  })
})
