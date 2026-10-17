// 登录页的邮箱两步（spec/web.md § 4.1）：发码提示、倒计时、输满 6 位自动提交、错码。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setUnauthorizedHandler } from '@/api/client'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { EmailLogin } from './EmailLogin'

let signedIn: ReturnType<typeof vi.fn<() => Promise<void>>>
let unauthorized: ReturnType<typeof vi.fn<() => void>>

beforeEach(() => {
  signedIn = vi.fn(async () => undefined)
  unauthorized = vi.fn<() => void>()
  setUnauthorizedHandler(unauthorized)
})
afterEach(() => {
  setUnauthorizedHandler(null)
  vi.useRealTimers()
  vi.unstubAllGlobals()
})

async function sendCodeTo(user: ReturnType<typeof userEvent.setup>, email = 'me@example.com') {
  await user.type(screen.getByTestId('login-email-input'), email)
  await user.click(screen.getByTestId('login-email-send'))
  await screen.findByTestId('login-code-input')
}

describe('EmailLogin — step 1', () => {
  it('sends the code to the normalized address and says it went out "if the email is linked" (the hub never says whether it is)', async () => {
    const api = installFakeApi({ 'POST /auth/email/send': () => undefined })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await user.type(screen.getByTestId('login-email-input'), '  Me@Example.COM ')
    await user.click(screen.getByTestId('login-email-send'))
    const sent = await screen.findByTestId('login-email-sent')
    expect(sent).toHaveTextContent(/If this email is linked to an account, a code has been sent|如果这个邮箱绑定了账号，验证码已经发出/)
    expect(sent).toHaveTextContent('me@example.com')
    expect(api.callsTo('POST /auth/email/send')).toHaveLength(1)
    expect(api.callsTo('POST /auth/email/send')[0]!.body).toEqual({ email: 'me@example.com' })
    expect(api.callsTo('POST /auth/email/send')[0]!.headers['X-Anycon']).toBe('1') // CSRF 头
  })

  it('hints that the email must be linked in the account settings first', () => {
    installFakeApi({})
    render(<EmailLogin onSignedIn={signedIn} />)
    expect(screen.getByText(/Link your email in Account settings first|邮箱要先在账号设置里绑定/)).toBeInTheDocument()
  })

  it('does not send anything until the address looks like an email', async () => {
    const api = installFakeApi({ 'POST /auth/email/send': () => undefined })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    const send = screen.getByTestId('login-email-send')
    expect(send).toBeDisabled()
    await user.type(screen.getByTestId('login-email-input'), 'not-an-email')
    expect(send).toBeDisabled()
    await user.type(screen.getByTestId('login-email-input'), '@x.io')
    expect(send).toBeEnabled()
    expect(api.calls).toHaveLength(0)
  })

  it('a rate limit (429) stays on step 1 and says how long to wait', async () => {
    installFakeApi({ 'POST /auth/email/send': () => ({ status: 429, body: errorBody('rate_limited'), headers: { 'Retry-After': '42' } }) })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await user.type(screen.getByTestId('login-email-input'), 'me@example.com')
    await user.click(screen.getByTestId('login-email-send'))
    expect(await screen.findByText(/42/)).toBeInTheDocument()
    expect(screen.queryByTestId('login-code-input')).toBeNull()
  })

  it('says so when the hub cannot be reached', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => Promise.reject(new TypeError('offline'))))
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await user.type(screen.getByTestId('login-email-input'), 'me@example.com')
    await user.click(screen.getByTestId('login-email-send'))
    expect(await screen.findByText(/Could not reach the hub|连不上 hub/)).toBeInTheDocument()
  })
})

describe('EmailLogin — step 2', () => {
  it('the code box is a one-time-code numeric input', async () => {
    installFakeApi({ 'POST /auth/email/send': () => undefined })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    const input = screen.getByTestId('login-code-input')
    expect(input).toHaveAttribute('inputmode', 'numeric')
    expect(input).toHaveAttribute('autocomplete', 'one-time-code')
  })

  it('submits by itself as soon as the sixth digit is in (not before) and then signs in', async () => {
    const api = installFakeApi({ 'POST /auth/email/send': () => undefined, 'POST /auth/email/verify': () => undefined })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    await user.type(screen.getByTestId('login-code-input'), '12345')
    expect(api.callsTo('POST /auth/email/verify')).toHaveLength(0)
    await user.type(screen.getByTestId('login-code-input'), '6')
    await waitFor(() => expect(signedIn).toHaveBeenCalledTimes(1))
    expect(api.callsTo('POST /auth/email/verify')).toHaveLength(1)
    expect(api.callsTo('POST /auth/email/verify')[0]!.body).toEqual({ email: 'me@example.com', code: '123456' })
  })

  it('a pasted "123 456" or "123-456" is cleaned up and submitted once', async () => {
    const api = installFakeApi({ 'POST /auth/email/send': () => undefined, 'POST /auth/email/verify': () => undefined })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    await user.click(screen.getByTestId('login-code-input'))
    await user.paste('123 456')
    await waitFor(() => expect(signedIn).toHaveBeenCalledTimes(1))
    expect(api.callsTo('POST /auth/email/verify')).toHaveLength(1)
    expect(api.callsTo('POST /auth/email/verify')[0]!.body).toEqual({ email: 'me@example.com', code: '123456' })
  })

  it('letters are ignored', async () => {
    installFakeApi({ 'POST /auth/email/send': () => undefined })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    await user.type(screen.getByTestId('login-code-input'), 'a1b2c3')
    expect(screen.getByTestId('login-code-input')).toHaveValue('123')
  })

  it('a wrong code shows "wrong or expired", clears the box, and is not mistaken for an expired login session', async () => {
    installFakeApi({ 'POST /auth/email/send': () => undefined, 'POST /auth/email/verify': () => ({ status: 401, body: errorBody('invalid_code') }) })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    await user.type(screen.getByTestId('login-code-input'), '000000')
    expect(await screen.findByText(/That code is wrong or has expired|验证码不对或已过期/)).toBeInTheDocument()
    expect(screen.getByTestId('login-code-input')).toHaveValue('')
    expect(signedIn).not.toHaveBeenCalled()
    expect(unauthorized).not.toHaveBeenCalled()
    // 可以再试
    await user.type(screen.getByTestId('login-code-input'), '111111')
    await waitFor(() => expect(screen.getByTestId('login-code-input')).toHaveValue(''))
  })

  it('can go back and use a different email', async () => {
    installFakeApi({ 'POST /auth/email/send': () => undefined })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    await user.click(screen.getByTestId('login-email-change'))
    expect(screen.getByTestId('login-email-input')).toBeInTheDocument()
    expect(screen.queryByTestId('login-code-input')).toBeNull()
  })
})

describe('EmailLogin — what the hub says about resending', () => {
  it('starts the countdown from the hub’s resend_after_secs, not a hard-coded 60', async () => {
    installFakeApi({ 'POST /auth/email/send': () => ({ ok: true, resend_after_secs: 30 }) })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    expect(screen.getByTestId('login-code-resend')).toHaveAttribute('data-seconds', '30')
  })

  it('a malformed address that got past the client check is explained in our words', async () => {
    installFakeApi({ 'POST /auth/email/send': () => ({ status: 400, body: errorBody('invalid_email') }) })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await user.type(screen.getByTestId('login-email-input'), 'me@example.com')
    await user.click(screen.getByTestId('login-email-send'))
    expect(await screen.findByText(/Enter a valid email address|请输入有效的邮箱地址/)).toBeInTheDocument()
    expect(screen.queryByTestId('login-code-input')).toBeNull()
  })
})

describe('EmailLogin — resend countdown', () => {
  // 只把 Date 换成假的（时间由测试推进），定时器是真的：`useNow` 每秒的 tick 读到的就是假时间
  const tick = { timeout: 3000 }

  it('counts down from 60 s, only then lets the user ask for a new code, and the new code restarts the countdown', async () => {
    vi.useFakeTimers({ toFake: ['Date'] })
    vi.setSystemTime(1_000_000)
    const api = installFakeApi({ 'POST /auth/email/send': () => undefined })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    const resend = screen.getByTestId('login-code-resend')
    expect(resend).toBeDisabled()
    expect(resend).toHaveAttribute('data-seconds', '60')

    vi.setSystemTime(1_000_000 + 30_000)
    await waitFor(() => expect(resend).toHaveAttribute('data-seconds', '30'), tick)
    expect(resend).toBeDisabled()

    vi.setSystemTime(1_000_000 + 60_000)
    await waitFor(() => expect(resend).toBeEnabled(), tick)
    expect(resend).toHaveTextContent(/Send a new code|重新发送验证码/)

    await user.click(resend)
    await waitFor(() => expect(api.callsTo('POST /auth/email/send')).toHaveLength(2))
    await waitFor(() => expect(screen.getByTestId('login-code-resend')).toHaveAttribute('data-seconds', '60'))
    expect(screen.getByTestId('login-code-resend')).toBeDisabled()
  })

  it('a 429 on resend restarts the countdown with the hub’s Retry-After', async () => {
    vi.useFakeTimers({ toFake: ['Date'] })
    vi.setSystemTime(2_000_000)
    let n = 0
    installFakeApi({ 'POST /auth/email/send': () => (++n === 1 ? undefined : { status: 429, body: errorBody('rate_limited'), headers: { 'Retry-After': '25' } }) })
    const user = userEvent.setup()
    render(<EmailLogin onSignedIn={signedIn} />)
    await sendCodeTo(user)
    vi.setSystemTime(2_000_000 + 60_000)
    await waitFor(() => expect(screen.getByTestId('login-code-resend')).toBeEnabled(), tick)
    await user.click(screen.getByTestId('login-code-resend'))
    await waitFor(() => expect(screen.getByTestId('login-code-resend')).toHaveAttribute('data-seconds', '25'), tick)
    expect(screen.getByTestId('login-code-resend')).toBeDisabled()
  })
})
