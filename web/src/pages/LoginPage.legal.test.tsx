// 登录页底部的服务条款 / 隐私政策链接（spec/web.md § 4.1）：只在 /api/config 带了 terms_url / privacy_url 时出现，
// 配了哪个提哪个；新标签页打开，不带 opener 和 referrer。
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import { errorBody, installFakeApi } from '@/test/fakeApi'

vi.mock('@tanstack/react-router', async (orig) => ({ ...(await orig<object>()), useRouter: () => ({ history: { replace: vi.fn() } }) }))

import { LoginPage } from './LoginPage'

const TERMS = 'https://example.com/terms'
const PRIVACY = 'https://example.com/privacy'

function mount(config: Record<string, unknown>) {
  installFakeApi({
    'GET /api/config': () => ({ mode: 'multi', protocol_version: 1, version: '0.0.0', login_methods: ['email'], ...config }),
    'GET /api/me': () => ({ status: 401, body: errorBody('unauthorized') }),
  })
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <QueryClientProvider client={queryClient}>
      <LoginPage />
    </QueryClientProvider>,
  )
}

beforeEach(async () => {
  sessionStorage.clear()
  await i18n.changeLanguage('en')
})
afterEach(async () => {
  vi.unstubAllGlobals()
  await i18n.changeLanguage('en')
})

describe('LoginPage — terms and privacy links', () => {
  it('shows nothing when the hub has neither URL (the self-hosted default)', async () => {
    mount({})
    expect(await screen.findByTestId('login-email-input')).toBeInTheDocument()
    expect(screen.queryByTestId('login-legal')).toBeNull()
  })

  it('links both documents, opening them in a new tab without opener or referrer', async () => {
    mount({ terms_url: TERMS, privacy_url: PRIVACY })
    const notice = await screen.findByTestId('login-legal')
    expect(notice).toHaveTextContent('By continuing, you agree to the Terms of Service and acknowledge the Privacy Policy.')
    const terms = within(notice).getByRole('link', { name: 'Terms of Service' })
    const privacy = within(notice).getByRole('link', { name: 'Privacy Policy' })
    expect(terms).toHaveAttribute('href', TERMS)
    expect(privacy).toHaveAttribute('href', PRIVACY)
    for (const a of [terms, privacy]) {
      expect(a).toHaveAttribute('target', '_blank')
      expect(a).toHaveAttribute('rel', 'noopener noreferrer')
    }
  })

  it('mentions only the document that is configured', async () => {
    mount({ mode: 'single', login_methods: ['token'], privacy_url: PRIVACY })
    const notice = await screen.findByTestId('login-legal')
    expect(notice).toHaveTextContent('By continuing, you acknowledge the Privacy Policy.')
    expect(within(notice).getAllByRole('link').map((a) => a.getAttribute('href'))).toEqual([PRIVACY])
  })

  it('only the terms', async () => {
    mount({ terms_url: TERMS })
    const notice = await screen.findByTestId('login-legal')
    expect(notice).toHaveTextContent('By continuing, you agree to the Terms of Service.')
    expect(within(notice).getAllByRole('link').map((a) => a.getAttribute('href'))).toEqual([TERMS])
  })

  it('is translated', async () => {
    await i18n.changeLanguage('zh')
    mount({ terms_url: TERMS, privacy_url: PRIVACY })
    const notice = await screen.findByTestId('login-legal')
    expect(notice).toHaveTextContent('继续即表示你同意服务条款，并已阅读隐私政策。')
    expect(within(notice).getByRole('link', { name: '服务条款' })).toHaveAttribute('href', TERMS)
    expect(within(notice).getByRole('link', { name: '隐私政策' })).toHaveAttribute('href', PRIVACY)
  })
})
