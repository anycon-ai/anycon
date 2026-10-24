// 解锁页的四个入口（spec/web.md § 4.3.1）：每个入口的成功路径和主要失败分支。传输层用假客户端（test/fakeEnrollClient），
// 密码学和线上序列由 transport/enroll.test.ts 守着。
// 四个入口都由用户操作才开始（扫码入口点「显示二维码」）；页面进后台时取消扫码会话，回到前台不自动重开。
import { StrictMode } from 'react'
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiDevice } from '@/api/types'
import { buildEnrollQr, parseEnrollQr } from '@/lib/enrollQr'
import { FakeEnrollClient } from '@/test/fakeEnrollClient'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'
import { EnrollError } from '@/transport/enroll'

const h = vi.hoisted(() => ({ client: null as unknown as FakeEnrollClient }))
vi.mock('@/transport/instance', () => ({ getClient: () => h.client }))
// 配额用完的说明里有一个去「配额」页的链接（`Link`）
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))
vi.mock('@/ui/QrCode', () => ({ QrCode: ({ text }: { text: string }) => <div data-testid="qr" data-text={text} /> }))

import { UnlockPanel } from './UnlockPanel'

const NODE_ID = 'd'.repeat(32)
const node: ApiDevice = { id: NODE_ID, name: 'mac-mini', kind: 'daemon', platform: 'macos-aarch64', created_at_ms: 1, online: true, paired_with: [] }
const QR = buildEnrollQr(NODE_ID, 'e'.repeat(32), new Uint8Array(32).fill(5))

let client: FakeEnrollClient
let done: ReturnType<typeof vi.fn<(id: string) => void>>

beforeEach(() => {
  client = new FakeEnrollClient()
  h.client = client
  done = vi.fn()
  useNodes.setState({ byId: {}, order: [], loaded: true })
})
afterEach(() => {
  vi.useRealTimers()
  Reflect.deleteProperty(document, 'visibilityState')
})

type User = ReturnType<typeof userEvent.setup>
const view = (online = true) => render(<UnlockPanel node={node} online={online} onDone={done} />)
const open = async (user: User, entry: 'scan' | 'approve' | 'code' | 'password') => user.click(screen.getByTestId(`unlock-trigger-${entry}`))
/** 扫码入口：点「显示二维码」，等 `enroll` 被调用。 */
const showQr = async (user: User) => {
  const before = client.calls.length
  await user.click(screen.getByTestId('unlock-show-qr'))
  await waitFor(() => expect(client.calls).toHaveLength(before + 1))
}
/** 让页面进后台 / 回到前台（jsdom 里 `visibilityState` 是只读的 getter，用自己的属性盖住它）。 */
const setVisibility = (state: 'visible' | 'hidden') =>
  act(() => {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state })
    document.dispatchEvent(new Event('visibilitychange'))
  })
/** 过一会儿，让「不该发生的事」有机会发生。 */
const settle = () => new Promise((r) => setTimeout(r, 40))
/** 让假客户端走到某一步。 */
const hooks = () => client.last.hooks

describe('the four entries', () => {
  it('shows the scan entry first, but starts nothing until "Show QR code" is pressed', async () => {
    const user = userEvent.setup()
    view()
    expect(screen.getByTestId('unlock-trigger-scan')).toHaveAttribute('aria-expanded', 'true')
    expect(screen.getByTestId('unlock-show-qr')).toBeEnabled()
    expect(screen.getByTestId('unlock-show-qr')).toHaveTextContent(/Show QR code|显示二维码/)
    await settle()
    expect(client.calls).toHaveLength(0) // 打开页面不占 hub 的限频和 daemon 的名额
    expect(screen.queryByTestId('unlock-scan-preparing')).toBeNull()
    await showQr(user)
    expect(client.last.req).toMatchObject({ node: NODE_ID, method: 'scan', approval: false, deviceName: 'Chrome · macOS · localhost' })
    expect(screen.getByTestId('unlock-scan-preparing')).toBeInTheDocument()
    expect(screen.queryByTestId('unlock-show-qr')).toBeNull()
  })

  it('every entry is disabled with an explanation while the node is offline; coming back online starts nothing, it only enables the button', async () => {
    const { rerender } = view(false)
    expect(screen.getByTestId('unlock-offline')).toBeInTheDocument()
    for (const e of ['scan', 'approve', 'code', 'password'] as const) expect(screen.getByTestId(`unlock-trigger-${e}`)).toBeDisabled()
    expect(screen.getByTestId('unlock-show-qr')).toBeDisabled()
    expect(client.calls).toHaveLength(0)
    rerender(<UnlockPanel node={{ ...node, online: true }} online onDone={done} />)
    await settle()
    expect(client.calls).toHaveLength(0)
    expect(screen.queryByTestId('unlock-offline')).toBeNull()
    expect(screen.getByTestId('unlock-show-qr')).toBeEnabled()
    expect(screen.getByTestId('unlock-trigger-password')).toBeEnabled()
  })

  it('choosing the scan entry again later does not start it either', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'password')
    await open(user, 'scan')
    await settle()
    expect(client.calls).toHaveLength(0)
    expect(screen.getByTestId('unlock-show-qr')).toBeEnabled()
  })
})

describe('React StrictMode (development double mount)', () => {
  it('mounting twice starts nothing; pressing the button then ends up with exactly one scan in flight, and no "cancelled" error', async () => {
    const user = userEvent.setup()
    render(
      <StrictMode>
        <UnlockPanel node={node} online onDone={done} />
      </StrictMode>,
    )
    await settle()
    expect(client.calls).toHaveLength(0)
    await user.click(screen.getByTestId('unlock-show-qr'))
    await waitFor(() => expect(client.calls.length).toBeGreaterThanOrEqual(1))
    await settle()
    expect(client.overlaps).toBe(0)
    expect(screen.queryByTestId('unlock-error')).toBeNull()
    const live = client.calls.filter((c) => !c.cancelled)
    expect(live).toHaveLength(1)
  })

  it('quickly picking entries back and forth never starts two enrollments at once', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    await open(user, 'password')
    await open(user, 'scan')
    await open(user, 'code')
    await open(user, 'scan')
    await user.click(screen.getByTestId('unlock-show-qr'))
    await settle()
    expect(client.overlaps).toBe(0)
    expect(client.calls.filter((c) => !c.cancelled)).toHaveLength(1)
    expect(screen.queryByTestId('unlock-error')).toBeNull()
  })
})

describe('scan', () => {
  it('shows the QR (anycon-enroll:1:…) with a countdown once the daemon is waiting, then the progress, then finishes', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    act(() => {
      hooks().stage('starting')
      hooks().started?.({ enrollId: 'e'.repeat(32), expiresAtMs: Date.now() + 300_000 })
      hooks().stage('waiting')
      hooks().qr?.(QR)
    })
    expect(await screen.findByTestId('unlock-qr')).toBeInTheDocument()
    const qr = await screen.findByTestId('qr')
    expect(qr.getAttribute('data-text')).toBe(QR)
    expect(parseEnrollQr(qr.getAttribute('data-text')!)).toMatchObject({ nodeId: NODE_ID, enrollId: 'e'.repeat(32) })
    expect(screen.getByTestId('unlock-countdown')).toHaveTextContent(/[45]:\d\d/)
    expect(screen.getByText(/Scan to approve a new device|扫码批准新设备/)).toBeInTheDocument()

    act(() => hooks().stage('handshaking'))
    expect(screen.queryByTestId('qr')).toBeNull() // 批准之后不再展示
    expect(screen.getByTestId('unlock-progress')).toHaveAttribute('data-phase', 'handshaking')
    act(() => {
      hooks().stage('done')
      client.last.resolve(NODE_ID)
    })
    await waitFor(() => expect(done).toHaveBeenCalledWith(NODE_ID))
  })

  it('swaps in a fresh QR by itself when the old one runs out (a new session, a new secret), once the user has asked for one', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    act(() => client.last.reject(new EnrollError('timeout')))
    await waitFor(() => expect(client.calls).toHaveLength(2))
    expect(client.calls[1]!.req).toMatchObject({ method: 'scan' })
    expect(screen.queryByTestId('unlock-error')).toBeNull()
  })

  it('also gets a fresh QR after the hub connection dropped (temporary connections end after 10 minutes)', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    act(() => client.last.reject(new EnrollError('hub_unavailable')))
    await waitFor(() => expect(client.calls).toHaveLength(2))
    expect(screen.queryByTestId('unlock-error')).toBeNull()
  })

  it('stops swapping QR codes by itself after about an hour of nobody scanning', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    for (let i = 0; i < 12; i++) {
      const n = client.calls.length
      act(() => client.last.reject(new EnrollError('timeout')))
      await waitFor(() => expect(client.calls).toHaveLength(n + 1))
    }
    act(() => client.last.reject(new EnrollError('timeout')))
    expect(await screen.findByTestId('unlock-error')).toHaveAttribute('data-code', 'timeout')
    expect(client.calls).toHaveLength(13)
    expect(screen.getByTestId('unlock-restart')).toBeInTheDocument()
  })

  it('shows other failures with a button to start over', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    act(() => client.last.reject(new EnrollError('denied')))
    expect(await screen.findByTestId('unlock-error')).toHaveAttribute('data-code', 'denied')
    await user.click(screen.getByTestId('unlock-restart'))
    await waitFor(() => expect(client.calls).toHaveLength(2))
  })
})

describe('the page going to the background (visibilitychange)', () => {
  /** 扫码入口显示着二维码。 */
  const showing = async (user: User) => {
    view()
    await showQr(user)
    act(() => {
      hooks().stage('starting')
      hooks().started?.({ enrollId: 'e'.repeat(32), expiresAtMs: Date.now() + 300_000 })
      hooks().stage('waiting')
      hooks().qr?.(QR)
    })
    expect(await screen.findByTestId('unlock-qr')).toBeInTheDocument()
  }

  it('withdraws the QR (enroll_cancel) when the tab goes to the background, and does not reopen it when the tab comes back', async () => {
    const user = userEvent.setup()
    await showing(user)
    const before = client.cancels
    setVisibility('hidden')
    await waitFor(() => expect(client.cancels).toBeGreaterThan(before))
    expect(client.calls[0]!.cancelled).toBe(true)
    // 二维码没了，换成说明和「重新显示二维码」
    await waitFor(() => expect(screen.queryByTestId('unlock-qr')).toBeNull())
    expect(screen.getByTestId('unlock-scan-paused')).toBeInTheDocument()
    expect(screen.getByTestId('unlock-show-qr')).toHaveTextContent(/Show the QR code again|重新显示二维码/)
    expect(screen.queryByTestId('unlock-error')).toBeNull() // 不是出错

    setVisibility('visible')
    await settle()
    expect(client.calls).toHaveLength(1) // 回到前台不自动重开
    expect(screen.getByTestId('unlock-show-qr')).toBeEnabled()

    await user.click(screen.getByTestId('unlock-show-qr'))
    await waitFor(() => expect(client.calls).toHaveLength(2))
    expect(client.last.req).toMatchObject({ method: 'scan' })
    expect(screen.queryByTestId('unlock-scan-paused')).toBeNull()
  })

  it('also withdraws a scan that is still connecting (no QR yet)', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    setVisibility('hidden')
    await waitFor(() => expect(client.calls[0]!.cancelled).toBe(true))
    expect(screen.getByTestId('unlock-show-qr')).toHaveTextContent(/again|重新/)
  })

  it('lets a scan that has already been approved finish (it takes a moment, and cancelling it would only make the user start over)', async () => {
    const user = userEvent.setup()
    await showing(user)
    act(() => hooks().stage('handshaking'))
    setVisibility('hidden')
    await settle()
    expect(client.calls[0]!.cancelled).toBe(false)
    act(() => client.last.resolve(NODE_ID))
    await waitFor(() => expect(done).toHaveBeenCalledWith(NODE_ID))
  })

  it('does not touch "approve on a paired device" while it waits: the user is off to approve it on the phone', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'approve')
    await user.click(screen.getByTestId('unlock-send-request'))
    await waitFor(() => expect(client.calls).toHaveLength(1))
    act(() => hooks().stage('waiting'))
    const before = client.cancels
    setVisibility('hidden')
    await settle()
    expect(client.cancels).toBe(before)
    expect(client.calls[0]!.cancelled).toBe(false)
    setVisibility('visible')
    // 批准后回来输码，照常走完
    const asked = new Promise<string | null>((resolve) => {
      act(() => {
        hooks().stage('code')
        void hooks().askCode?.({ attemptsLeft: null, wrong: false }).then(resolve)
      })
    })
    await user.type(await screen.findByTestId('unlock-approve-code'), '482913')
    await expect(asked).resolves.toBe('482913')
  })

  it('a QR that runs out while the page is in the background is not swapped for a new one behind the user\'s back', async () => {
    const user = userEvent.setup()
    await showing(user)
    // 后备：事件没来得及取消，会话自己到期了（页面其实已经在后台）
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' })
    act(() => client.last.reject(new EnrollError('timeout')))
    await waitFor(() => expect(screen.getByTestId('unlock-scan-paused')).toBeInTheDocument())
    await settle()
    expect(client.calls).toHaveLength(1)
    expect(screen.queryByTestId('unlock-error')).toBeNull()
  })

  it('stops listening when the page is left', async () => {
    const user = userEvent.setup()
    const { unmount } = view()
    await showQr(user)
    unmount()
    const before = client.cancels
    setVisibility('hidden')
    expect(client.cancels).toBe(before) // 卸载时已经取消过；没有残留的监听器再来一次
  })
})

describe('a hub that never answers enroll_begin', () => {
  it('says so (and why that may be), offers to start over, and does not retry by itself', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    act(() => client.last.reject(new EnrollError('hub_no_reply')))
    const err = await screen.findByTestId('unlock-error')
    expect(err).toHaveAttribute('data-code', 'hub_no_reply')
    expect(err).toHaveTextContent(/too often|太频繁/)
    expect(err).not.toHaveTextContent(/timed out|超时/)
    await settle()
    expect(client.calls).toHaveLength(1) // 自动换二维码只给到期 / 断线，不给这个——不然会接着撞限频
    await user.click(screen.getByTestId('unlock-restart'))
    await waitFor(() => expect(client.calls).toHaveLength(2))
  })
})

describe('switching entries and leaving', () => {
  it('cancels the running enrollment when another entry is picked', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    const before = client.cancels
    await open(user, 'password')
    await waitFor(() => expect(client.cancels).toBeGreaterThan(before))
    expect(client.calls).toHaveLength(1) // 口令入口等用户输入，不会自己开始
    expect(screen.getByTestId('unlock-trigger-password')).toHaveAttribute('aria-expanded', 'true')
  })

  it('cancels when the page is left (enroll_cancel)', async () => {
    const user = userEvent.setup()
    const { unmount } = view()
    await showQr(user)
    const before = client.cancels
    unmount()
    expect(client.cancels).toBeGreaterThan(before)
  })
})

describe('approve on a paired device', () => {
  it('sends the request only when asked, then takes the 6-digit code it shows and finishes', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'approve')
    await settle()
    expect(client.calls).toHaveLength(0) // 展开不等于发请求：批准请求会推送给你所有的设备
    await user.click(screen.getByTestId('unlock-send-request'))
    await waitFor(() => expect(client.calls).toHaveLength(1))
    expect(client.last.req).toMatchObject({ node: NODE_ID, method: 'code', approval: true })

    act(() => hooks().stage('waiting'))
    expect(screen.getByTestId('unlock-approve-code')).toBeDisabled() // 还没批准，先不让输
    const asked = new Promise<string | null>((resolve) => {
      act(() => {
        hooks().stage('code')
        void hooks().askCode?.({ attemptsLeft: null, wrong: false }).then(resolve)
      })
    })
    const input = screen.getByTestId('unlock-approve-code')
    await waitFor(() => expect(input).toBeEnabled())
    expect(input).toHaveAttribute('inputmode', 'numeric')
    expect(input).toHaveAttribute('autocomplete', 'one-time-code')
    await user.type(input, '482913')
    await expect(asked).resolves.toBe('482913') // 输满 6 位自动提交
  })

  it('"denied" is shown with a way to ask again', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'approve')
    await user.click(screen.getByTestId('unlock-send-request'))
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'code')).toBe(true))
    act(() => client.last.reject(new EnrollError('denied')))
    expect(await screen.findByTestId('unlock-error')).toHaveAttribute('data-code', 'denied')
  })
})

describe('I have an access code', () => {
  /** 假客户端替 `enroll` 去问码：返回第一次问到的结果。 */
  const ask = (ctx: { attemptsLeft: number | null; wrong: boolean }) => hooks().askCode!(ctx)

  it('starts when the 6th digit is typed and hands the typed code to the enrollment', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'code')
    const input = screen.getByTestId('unlock-code-input')
    expect(input).toHaveAttribute('inputmode', 'numeric')
    await user.type(input, '12345')
    expect(screen.getByTestId('unlock-code-submit')).toBeDisabled()
    expect(client.calls).toHaveLength(0)
    await user.type(input, '6')
    await waitFor(() => expect(client.calls).toHaveLength(1))
    expect(client.last.req).toMatchObject({ method: 'code', approval: false })
    await expect(ask({ attemptsLeft: null, wrong: false })).resolves.toBe('123456')
  })

  it('ignores anything but digits (pasting "123-456" works)', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'code')
    const input = screen.getByTestId('unlock-code-input')
    await user.click(input)
    await user.paste('12 3-45a6')
    expect(input).toHaveValue('123 456')
  })

  it('a wrong code shows the attempts left and lets the user type again in the same session', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'code')
    await user.type(screen.getByTestId('unlock-code-input'), '111111')
    await waitFor(() => expect(client.calls).toHaveLength(1))
    await expect(ask({ attemptsLeft: null, wrong: false })).resolves.toBe('111111')

    // 假 daemon 说码不对，还剩 2 次：同一个会话里再问一次
    let second: Promise<string | null> = Promise.resolve(null)
    act(() => {
      hooks().stage('code')
      second = hooks().askCode!({ attemptsLeft: 2, wrong: true })
    })
    expect(await screen.findByTestId('unlock-wrong-code')).toHaveTextContent(/wrong|不对/)
    expect(screen.getByTestId('unlock-wrong-code')).toHaveTextContent(/2/)
    const input = screen.getByTestId('unlock-code-input')
    expect(input).toHaveValue('') // 清空让用户重输
    await user.type(input, '222222')
    await expect(second).resolves.toBe('222222')
    expect(client.calls).toHaveLength(1) // 没有开新的接入
  })

  it('when all attempts are used up it says the code is void and offers to start over', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'code')
    await user.type(screen.getByTestId('unlock-code-input'), '111111')
    await waitFor(() => expect(client.calls).toHaveLength(1))
    act(() => client.last.reject(new EnrollError('wrong_secret', undefined, { attemptsLeft: 0 })))
    const err = await screen.findByTestId('unlock-error')
    expect(err).toHaveAttribute('data-code', 'wrong_secret')
    expect(err).toHaveTextContent(/no longer valid|已作废/)
    expect(screen.getByTestId('unlock-restart')).toBeInTheDocument()
  })

  it('"no_code" tells the user to make one on a paired device', async () => {
    const user = userEvent.setup()
    view()
    await open(user, 'code')
    await user.type(screen.getByTestId('unlock-code-input'), '111111')
    await waitFor(() => expect(client.calls).toHaveLength(1))
    act(() => client.last.reject(new EnrollError('no_code')))
    expect(await screen.findByTestId('unlock-error')).toHaveTextContent(/Add device|添加设备/)
  })
})

describe('node password', () => {
  const unlock = async (user: ReturnType<typeof userEvent.setup>, password = 'correct horse') => {
    await open(user, 'password')
    const input = screen.getByTestId('unlock-password-input')
    expect(input).toHaveAttribute('type', 'password')
    expect(input).toHaveAttribute('autocomplete', 'off') // 借来的电脑：别让浏览器存
    await user.type(input, password)
    await user.click(screen.getByTestId('unlock-password-submit'))
  }

  it('submits the password to the enrollment and clears the box straight away', async () => {
    const user = userEvent.setup()
    view()
    await unlock(user)
    await waitFor(() => expect(client.calls).toHaveLength(1))
    expect(client.last.req).toMatchObject({ node: NODE_ID, method: 'password', password: 'correct horse' })
    expect(screen.getByTestId('unlock-password-input')).toHaveValue('')
    act(() => hooks().stage('deriving'))
    expect(screen.getByTestId('unlock-progress')).toHaveAttribute('data-phase', 'deriving')
    expect(screen.getByTestId('unlock-progress')).toHaveTextContent(/Deriving keys|正在派生密钥/)
    act(() => client.last.resolve(NODE_ID))
    await waitFor(() => expect(done).toHaveBeenCalledWith(NODE_ID))
  })

  it('wrong password → "wrong password"', async () => {
    const user = userEvent.setup()
    view()
    await unlock(user, 'nope nope')
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'password')).toBe(true))
    act(() => client.last.reject(new EnrollError('wrong_secret')))
    expect(await screen.findByTestId('unlock-error')).toHaveTextContent(/password is wrong|口令不对/)
    expect(done).not.toHaveBeenCalled()
  })

  it('backoff: shows a countdown and keeps the button disabled until it is over', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime })
    view()
    await unlock(user)
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'password')).toBe(true))
    act(() => client.last.reject(new EnrollError('backoff', undefined, { retryAfterMs: 90_000 })))
    const err = await screen.findByTestId('unlock-error')
    expect(err).toHaveTextContent(/1:[23]\d|1:[01]\d/)
    await user.type(screen.getByTestId('unlock-password-input'), 'another try')
    expect(screen.getByTestId('unlock-password-submit')).toBeDisabled()
    await act(async () => {
      await vi.advanceTimersByTimeAsync(91_000)
    })
    await waitFor(() => expect(screen.getByTestId('unlock-password-submit')).toBeEnabled())
    expect(screen.getByTestId('unlock-error')).toHaveTextContent(/try again now|现在可以再试/)
  })

  it('locked: says how to turn it back on (anycon passwd --enable) and where', async () => {
    const user = userEvent.setup()
    view()
    await unlock(user)
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'password')).toBe(true))
    act(() => client.last.reject(new EnrollError('locked')))
    const err = await screen.findByTestId('unlock-error')
    expect(err).toHaveTextContent('anycon passwd --enable')
    expect(err).toHaveTextContent(/10/)
  })

  it.each(['password_not_set', 'method_disabled'] as const)('%s points to the other entries', async (code) => {
    const user = userEvent.setup()
    view()
    await unlock(user)
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'password')).toBe(true))
    act(() => client.last.reject(new EnrollError(code)))
    expect(await screen.findByTestId('unlock-error')).toHaveAttribute('data-code', code)
  })

  it.each([
    ['offline', /offline|离线/],
    ['busy', /too many|太多/],
    ['rate_limited', /Too many|太频繁/],
    ['quota', /limit|用完/],
    ['expired', /timed out|超时/],
  ] as const)('%s has its own explanation', async (code, text) => {
    const user = userEvent.setup()
    view()
    await unlock(user)
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'password')).toBe(true))
    act(() => client.last.reject(new EnrollError(code)))
    expect(await screen.findByTestId('unlock-error')).toHaveTextContent(text)
  })

  it('quota (enroll_failed { quota }) names the limits and links to Settings → Quota', async () => {
    const user = userEvent.setup()
    view()
    await unlock(user)
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'password')).toBe(true))
    act(() => client.last.reject(new EnrollError('quota')))
    const box = await screen.findByTestId('unlock-error')
    expect(box).toHaveTextContent(/Devices/)
    expect(within(box).getByTestId('quota-link')).toHaveAttribute('href', '/settings/quota')
  })

  it('key_changed offers to forget the old node, because the key is never overwritten silently', async () => {
    const user = userEvent.setup()
    useNodes.setState({ byId: { [NODE_ID]: { id: NODE_ID } as never }, order: [NODE_ID], loaded: true })
    view()
    await unlock(user)
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'password')).toBe(true))
    act(() => client.last.reject(new EnrollError('key_changed')))
    await user.click(await screen.findByTestId('unlock-forget'))
    await waitFor(() => expect(client.removed).toEqual([NODE_ID]))
  })
})

describe('device name', () => {
  it('goes out as the hint and can be changed before starting', async () => {
    const user = userEvent.setup()
    view()
    await user.click(screen.getByRole('button', { name: /Change name|修改|Rename/ }))
    const input = screen.getByTestId('unlock-device-name')
    await user.clear(input)
    await user.type(input, 'Library PC')
    await open(user, 'password')
    await user.type(screen.getByTestId('unlock-password-input'), 'correct horse')
    await user.click(screen.getByTestId('unlock-password-submit'))
    await waitFor(() => expect(client.calls.some((c) => c.req.method === 'password')).toBe(true))
    expect(client.last.req).toMatchObject({ method: 'password', deviceName: 'Library PC' })
  })

  it('renaming before the QR is shown only changes the name the button will use', async () => {
    const user = userEvent.setup()
    view()
    await user.click(screen.getByRole('button', { name: /Change name|修改|Rename/ }))
    const input = screen.getByTestId('unlock-device-name')
    await user.clear(input)
    await user.type(input, 'Library PC')
    await user.tab() // 离开输入框
    await settle()
    expect(client.calls).toHaveLength(0) // 改名不会替用户开始扫码
    await showQr(user)
    expect(client.last.req).toMatchObject({ method: 'scan', deviceName: 'Library PC' })
  })

  it('renaming while the QR is up replaces the QR (its hint carried the old name)', async () => {
    const user = userEvent.setup()
    view()
    await showQr(user)
    await user.click(screen.getByRole('button', { name: /Change name|修改|Rename/ }))
    const input = screen.getByTestId('unlock-device-name')
    await user.clear(input)
    await user.type(input, 'Library PC')
    await user.tab() // 离开输入框
    await waitFor(() => expect(client.calls).toHaveLength(2))
    expect(client.calls[0]!.req.deviceName).toBe('Chrome · macOS · localhost')
    expect(client.calls[1]!.req).toMatchObject({ method: 'scan', deviceName: 'Library PC' })
    expect(client.calls[0]!.cancelled).toBe(true) // 旧的会话被取消（enroll_cancel）
  })
})
