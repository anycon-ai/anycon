/**
 * `multi` 模式（v0.2，公开 hub）：邮箱验证码开放注册、`ANYCON_ADMIN_EMAILS` 管理员、配额、删账号、GitHub 登录入口。
 *
 * 这个文件**自己起一个 `ANYCON_MODE=multi` 的 hub**（`helpers/multi-hub.ts`：随机端口、临时目录、debug 日志；发信指向 global-setup 的假 SMTP），
 * 所有页面和 API 调用都打到它，不碰共享的 single hub。配置：管理员邮箱 `ADMIN_EMAIL`，小配额（设备 3、节点 1、同时在线 6、每日配对 9），
 * 假的 GitHub client id / secret（登录页因此有 GitHub 按钮；浏览器从不真的去 github.com，见 a）。
 *
 * a 登录页（multi）：注册措辞、没有 token、GitHub 按钮；`/auth/oauth/github/start` 的 302、cookie、PKCE 参数；回调的失败重定向和登录页的错误文案
 * b 管理员邮箱注册 → 落在 /setup，`is_admin`，管理页没有邀请、有「via ANYCON_ADMIN_EMAILS」徽标；`/auth/token`、`/api/invites` 是 404
 * c 第二个用户注册 → 不是管理员；设置里有「配额」页、登录方式里没有 token 只有「关联 GitHub」；管理员的账号列表看得到、搜得到
 * d 配额：配对第一台浏览器（节点 + 浏览器 = 2 台设备）、第二台（3/3，另一台浏览器的配额页靠 `quota_update` 推送实时变成「已满」）、第三台被拒
 * e 用户在资料页删除自己的账号（会话是新的，不用重新登录）→ /login 带「已删除」提示；会话失效；管理员列表里没了；daemon 的连接被 4004 断开
 * f 同一个邮箱再注册：新账号（id 不同，配额清零）
 * g hub 日志里没有邮箱、验证码、配对码、OAuth 的 state / PKCE / 密钥、会话 cookie
 *
 * **hub 的发码限速**：每 IP 每小时 5 次（这个 hub 进程自己的计数）；同一邮箱 60 秒一次（f 里重新注册同一个邮箱，要等到距离 c 里发码满 60 秒）。
 * 这个文件一共发 3 封登录码，用的都是只属于它的邮箱地址。
 */
import {
  expect,
  test,
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type Page,
} from '@playwright/test'

import { startPairing } from '../helpers/cli.js'
import { startExtraDaemon, type ExtraDaemon } from '../helpers/daemon.js'
import { startMultiHub, type MultiHub } from '../helpers/multi-hub.js'
import { readLog, waitFor } from '../helpers/process.js'
import { mailsTo, waitForCode } from '../helpers/smtp.js'
import { readState } from '../helpers/state.js'
import { CONNECT_TIMEOUT, expectNodeOnline, hubLogLeaks, pairBrowser } from '../helpers/ui.js'

const ADMIN_EMAIL = 'mx-admin@example.test'
const USER_EMAIL = 'mx-user@example.test'
const GITHUB = { clientId: 'e2e-github-client-id', clientSecret: 'e2e-github-client-secret-5d1f' }
const QUOTAS = { devices: 3, daemons: 1, connections: 6, dailyPairings: 9 }
/** hub 对同一邮箱发码的最小间隔是 60 秒；多留 2 秒。 */
const RESEND_MS = 62_000
const CSRF = { 'X-Anycon': '1' }

interface Me {
  id: string
  display_name: string
  email?: string | null
  is_admin: boolean
  login_methods: string[]
}
interface QuotaView {
  devices: number
  devices_max: number | null
  daemons: number
  daemons_max: number | null
  connections: number
  connections_max: number | null
  daily_pairings: number
  daily_pairings_max: number | null
}
interface AdminAccount {
  id: string
  email?: string | null
  is_admin: boolean
  admin_by_config: boolean
}

test.describe.configure({ mode: 'serial' })

test.describe('multi 模式', () => {
  let hub: MultiHub
  let extra: ExtraDaemon | undefined
  let adminContext: BrowserContext
  let admin: Page
  let adminId = ''
  let userContext: BrowserContext
  let user: Page
  let userId = ''
  /** 用户的另一台已配对的浏览器（d 里配对，e 里确认账号删除后它的会话也失效）。 */
  let secondContext: BrowserContext | undefined
  /** 这个文件里发出过的登录码；每个邮箱最近一次点「发送」的时刻（等 60 秒的重发间隔用）。 */
  const issuedCodes: string[] = []
  const sentAt = new Map<string, number>()
  /** a 里看到的 OAuth state / PKCE challenge：hub 的日志里不能有（g）。 */
  const oauthSecrets: string[] = []
  const pairCodes: string[] = []

  const url = (path: string): string => `${hub.url}${path}`
  const newContext = (
    browser: Browser,
    storageState?: Awaited<ReturnType<BrowserContext['storageState']>>,
  ) => browser.newContext({ baseURL: hub.url, locale: 'en-US', storageState })

  const json = async <T>(ctx: BrowserContext | APIRequestContext, path: string): Promise<T> => {
    const request = 'request' in ctx ? ctx.request : ctx
    const res = await request.get(url(path))
    expect(res.status(), `GET ${path} → ${res.status()}`).toBe(200)
    return (await res.json()) as T
  }

  /** 管理员的账号列表（`GET /api/admin/accounts`）里现在有哪些账号 id。 */
  const adminSees = async (): Promise<string[]> =>
    (await json<AdminAccount[]>(adminContext, '/api/admin/accounts')).map((a) => a.id)

  /** 登录页：邮箱 → 发码 → 从假 SMTP 取码 → 输入（输满 6 位自动提交）。multi 里邮箱没有账号就建一个。 */
  async function signUpWithEmail(page: Page, address: string): Promise<void> {
    const state = readState()
    await page.goto('/login')
    await page.getByTestId('login-email-input').fill(address)
    // 同一邮箱 60 秒才能再发一次码（内存限速）：对 hub 已知间隔的有界等待，不是固定睡眠
    const wait = (sentAt.get(address) ?? 0) + RESEND_MS - Date.now()
    if (wait > 0) await page.waitForTimeout(wait)
    const before = mailsTo(state.smtpDir, address).length
    sentAt.set(address, Date.now())
    await page.getByTestId('login-email-send').click()
    await expect(page.getByTestId('login-email-sent')).toContainText(address)
    const code = await waitForCode(state.smtpDir, address, { after: before })
    issuedCodes.push(code)
    await page.getByTestId('login-code-input').fill(code)
    await expect(page).not.toHaveURL(/\/login/, { timeout: 15_000 })
  }

  test.beforeAll(async () => {
    hub = await startMultiHub(readState(), {
      adminEmails: [ADMIN_EMAIL],
      quotas: QUOTAS,
      github: GITHUB,
    })
  })

  test.afterAll(async () => {
    await secondContext?.close()
    await userContext?.close()
    await adminContext?.close()
    await extra?.stop()
    await hub?.stop()
  })

  test('a. 登录页：注册措辞、没有 token、GitHub 按钮；OAuth 的 start 与失败回调', async ({
    browser,
    playwright,
  }) => {
    const anonymous = await playwright.request.newContext()
    const config = await json<{ mode: string; login_methods: string[]; public_url?: string }>(
      anonymous,
      '/api/config',
    )
    await anonymous.dispose()
    expect(config).toMatchObject({ mode: 'multi', login_methods: ['github', 'email'] })
    expect(config.public_url).toBe(hub.url)

    const context = await newContext(browser)
    // 浏览器绝不能真的去 github.com。Playwright 对重定向的目标不一定再走 route，所以不能指望“拦下 github.com 的请求”：
    // 在 hub 的 start 这一步拦住——自己发这个请求（不跟随重定向，cookie 照常进这个上下文的 cookie jar），
    // 记下 302，给浏览器一个桩页面。再用 request 事件兜底：这个上下文发过任何到 github.com 的请求都算失败
    const providerRedirects: Array<{ status: number; location: string }> = []
    await context.route(/\/auth\/oauth\/github\/start(\?|$)/, async (route) => {
      const res = await route.fetch({ maxRedirects: 0 })
      providerRedirects.push({ status: res.status(), location: res.headers().location ?? '' })
      await route.fulfill({
        status: 200,
        contentType: 'text/html',
        body: '<title>provider stub</title>',
      })
    })
    const githubRequests: string[] = []
    context.on('request', (request) => {
      if (new URL(request.url()).hostname.endsWith('github.com')) githubRequests.push(request.url())
    })
    try {
      const page = await context.newPage()
      await page.goto('/login')
      await expect(page.getByTestId('login-email-form')).toBeVisible()
      await expect(page.getByText(/create an account if you don.t have one yet/)).toBeVisible()
      await expect(page.getByText(/If there is no account for this address yet/)).toBeVisible()
      // multi 没有 token 登录：没有表单，也没有「使用 token」
      await expect(page.getByTestId('login-token-input')).toHaveCount(0)
      await expect(page.getByTestId('login-use-token')).toHaveCount(0)
      // 只配了 GitHub：有它的按钮（普通链接），没有 Google
      const github = page.getByTestId('oauth-github')
      await expect(github).toHaveText(/Continue with GitHub/)
      await expect(github).toHaveAttribute('href', '/auth/oauth/github/start')
      await expect(page.getByTestId('oauth-google')).toHaveCount(0)
      await expect(page.getByTestId('oauth-origin-note')).toHaveCount(0)

      // 登录后要回到哪里，带在 start 的 `next` 里
      await page.goto('/login?redirect=/settings')
      await expect(page.getByTestId('oauth-github')).toHaveAttribute(
        'href',
        '/auth/oauth/github/start?next=%2Fsettings',
      )

      // 不在 hub 的公开地址上（localhost 而不是 127.0.0.1）：回调到不了，按钮禁用并说明
      const elsewhere = await context.newPage()
      await elsewhere.goto(`http://localhost:${hub.port}/login`)
      await expect(elsewhere.getByTestId('oauth-origin-note')).toContainText(hub.url)
      await expect(elsewhere.getByTestId('oauth-github')).toBeDisabled()
      await elsewhere.close()

      // ---- start 路由（不跟随重定向）----
      const api = await playwright.request.newContext()
      const started = await api.get(url('/auth/oauth/github/start'), { maxRedirects: 0 })
      expect(started.status()).toBe(302)
      expect(started.headers()['cache-control']).toBe('no-store')
      const authorize = new URL(started.headers().location ?? '')
      expect(`${authorize.origin}${authorize.pathname}`).toBe(
        'https://github.com/login/oauth/authorize',
      )
      const q = authorize.searchParams
      expect(q.get('client_id')).toBe(GITHUB.clientId)
      expect(q.get('redirect_uri')).toBe(`${hub.url}/auth/callback/github`)
      expect(q.get('response_type')).toBe('code')
      expect(q.get('scope')).toBe('read:user user:email')
      expect(q.get('code_challenge_method')).toBe('S256')
      expect(q.get('code_challenge')).toMatch(/^[A-Za-z0-9_-]{43}$/)
      const state = q.get('state') ?? ''
      expect(state).toMatch(/^[0-9a-f]{32,}$/)
      oauthSecrets.push(state, q.get('code_challenge') ?? '')
      // `anycon_oauth` cookie：值就是 state，只发给回调路径，JS 读不到；公开地址是 http，所以没有 Secure
      const cookies = started
        .headersArray()
        .filter((h) => h.name.toLowerCase() === 'set-cookie')
        .map((h) => h.value)
      const oauthCookie = cookies.find((c) => c.startsWith('anycon_oauth='))
      expect(oauthCookie, `Set-Cookie: ${cookies.join(' | ')}`).toBeDefined()
      expect(oauthCookie).toContain(`anycon_oauth=${state}`)
      expect(oauthCookie).toContain('HttpOnly')
      expect(oauthCookie).toContain('Path=/auth/callback')
      expect(oauthCookie).toContain('SameSite=Lax')
      expect(oauthCookie).toContain('Max-Age=600')
      expect(oauthCookie).not.toContain('Secure')

      // 没配的提供方：404；没带 state 的回调：302 回登录页，带 invalid_state，并清掉 cookie
      const google = await api.get(url('/auth/oauth/google/start'), { maxRedirects: 0 })
      expect(google.status()).toBe(404)
      expect(((await google.json()) as { error: { code: string } }).error.code).toBe('not_found')
      const noState = await api.get(url('/auth/callback/github'), { maxRedirects: 0 })
      expect(noState.status()).toBe(302)
      expect(noState.headers().location).toBe('/login?oauth_error=invalid_state&provider=github')
      const cleared = noState
        .headersArray()
        .find((h) => h.name.toLowerCase() === 'set-cookie' && h.value.startsWith('anycon_oauth='))
      expect(cleared?.value).toMatch(/Max-Age=0/)
      // state 对，但这个浏览器没有 `anycon_oauth` cookie（在别的浏览器里开始的）：同样是 invalid_state
      const otherBrowser = await playwright.request.newContext()
      const foreign = await otherBrowser.get(
        url(`/auth/callback/github?code=whatever&state=${state}`),
        { maxRedirects: 0 },
      )
      expect(foreign.headers().location).toBe('/login?oauth_error=invalid_state&provider=github')
      await otherBrowser.dispose()
      await api.dispose()

      // 登录页把 oauth_error 说成人话
      await page.goto('/login?oauth_error=invalid_state&provider=github')
      await expect(page.getByTestId('oauth-error')).toHaveAttribute('data-code', 'invalid_state')
      await expect(page.getByTestId('oauth-error')).toContainText(
        /expired, or was started in another browser/,
      )

      // ---- 真的点按钮：浏览器请求 hub 的 start，hub 的 302 去 GitHub 授权页（被拦在上面），cookie 进了浏览器 ----
      await page.goto('/login')
      await page.getByTestId('oauth-github').click()
      await expect.poll(() => providerRedirects.length).toBe(1)
      const redirect = providerRedirects[0]!
      expect(redirect.status).toBe(302)
      const clicked = new URL(redirect.location)
      expect(`${clicked.origin}${clicked.pathname}`).toBe(
        'https://github.com/login/oauth/authorize',
      )
      const clickedState = clicked.searchParams.get('state') ?? ''
      expect(clickedState).toMatch(/^[0-9a-f]{32,}$/)
      oauthSecrets.push(clickedState)
      // cookie 的 Path 是 /auth/callback：按回调地址取才取得到
      const callbackUrl = url('/auth/callback/github')
      const jar = (await context.cookies(callbackUrl)).find((c) => c.name === 'anycon_oauth')
      expect(jar).toMatchObject({ value: clickedState, httpOnly: true, path: '/auth/callback' })
      // 用户在 GitHub 上点了「取消」：提供方带 error=access_denied 回来，state 和 cookie 都对 → cancelled
      await page.goto(`/auth/callback/github?error=access_denied&state=${clickedState}`)
      await expect(page).toHaveURL(/\/login\?oauth_error=cancelled&provider=github$/)
      await expect(page.getByTestId('oauth-error')).toHaveAttribute('data-code', 'cancelled')
      await expect(page.getByTestId('oauth-error')).toContainText(/GitHub sign-in was cancelled/)
      // state 是一次性的，cookie 也被清了：再回来就是 invalid_state
      expect((await context.cookies(callbackUrl)).some((c) => c.name === 'anycon_oauth')).toBe(
        false,
      )
      await page.goto(`/auth/callback/github?error=access_denied&state=${clickedState}`)
      await expect(page.getByTestId('oauth-error')).toHaveAttribute('data-code', 'invalid_state')
      expect(githubRequests, 'the browser must never reach github.com').toEqual([])
    } finally {
      await context.close()
    }
  })

  test('b. ANYCON_ADMIN_EMAILS 的邮箱注册 → 管理员；管理页没有邀请；token / 邀请接口是 404', async ({
    browser,
  }) => {
    adminContext = await newContext(browser)
    admin = await adminContext.newPage()
    await signUpWithEmail(admin, ADMIN_EMAIL)
    // 新账号还没有任何设备：和别的新账号一样落在 /setup
    await expect(admin).toHaveURL(/\/setup$/)

    const me = await json<Me>(adminContext, '/api/me')
    adminId = me.id
    expect(me).toMatchObject({ email: ADMIN_EMAIL, is_admin: true })
    expect(me.display_name).toBe('mx-admin') // 邮箱的本地部分
    expect(me.login_methods).toEqual(['email']) // 没有 token

    await admin.goto('/admin')
    await expect(admin.getByTestId('admin-accounts')).toBeVisible()
    await expect(admin.getByTestId('admin-invites')).toHaveCount(0)
    await expect(admin.getByTestId('invite-new')).toHaveCount(0)
    const row = admin.getByTestId(`admin-account-${adminId}`)
    await expect(row).toContainText(ADMIN_EMAIL)
    await expect(admin.getByTestId(`admin-by-config-${adminId}`)).toHaveText(
      'Admin via ANYCON_ADMIN_EMAILS',
    )
    // 配置里的管理员在页面上取消不了，自己也删不了
    await expect(admin.getByTestId(`admin-revoke-${adminId}`)).toBeDisabled()
    await expect(admin.getByTestId(`admin-delete-${adminId}`)).toBeDisabled()
    await expect(admin.getByTestId('admin-status')).toContainText('multi')

    const accounts = await json<AdminAccount[]>(adminContext, '/api/admin/accounts')
    expect(accounts).toEqual([
      expect.objectContaining({ id: adminId, is_admin: true, admin_by_config: true }),
    ])

    // multi 里没有 token 登录、邀请和访问 token
    const request = adminContext.request
    for (const path of ['/api/invites', '/api/me/tokens']) {
      const res = await request.get(url(path))
      expect(res.status(), `GET ${path}`).toBe(404)
    }
    const token = await request.post(url('/auth/token'), {
      data: { token: 'anycon_not-a-real-token' },
      headers: CSRF,
    })
    expect(token.status()).toBe(404)
    expect(((await token.json()) as { error: { code: string } }).error.code).toBe('not_found')
    const invite = await request.post(url('/auth/invite'), {
      data: { code: 'whatever' },
      headers: CSRF,
    })
    expect(invite.status()).toBe(404)
  })

  test('c. 第二个用户注册：不是管理员；设置里有配额页，登录方式里没有 token、可以关联 GitHub', async ({
    browser,
  }) => {
    userContext = await newContext(browser)
    user = await userContext.newPage()
    await signUpWithEmail(user, USER_EMAIL)
    await expect(user).toHaveURL(/\/setup$/)

    const me = await json<Me>(userContext, '/api/me')
    userId = me.id
    expect(userId).not.toBe(adminId)
    expect(me).toMatchObject({ email: USER_EMAIL, is_admin: false })
    expect(me.login_methods).toEqual(['email'])
    // 不是管理员：管理页只说「仅限管理员」，接口 403
    await user.goto('/admin')
    await expect(user.getByText('Administrators only')).toBeVisible()
    expect((await userContext.request.get(url('/api/admin/accounts'))).status()).toBe(403)

    // 设置 → 配额：四项的上限就是 hub 的 ANYCON_QUOTA_*，还什么都没用
    await user.goto('/settings')
    await user.getByTestId('settings-nav-quota').click()
    await expect(user).toHaveURL(/\/settings\/quota$/)
    await expect(user.getByTestId('quota-list')).toBeVisible()
    for (const [row, max] of [
      ['devices', QUOTAS.devices],
      ['daemons', QUOTAS.daemons],
      ['connections', QUOTAS.connections],
      ['dailyPairings', QUOTAS.dailyPairings],
    ] as const) {
      await expect(user.getByTestId(`quota-${row}`)).toHaveAttribute('data-max', String(max))
      await expect(user.getByTestId(`quota-${row}`)).toHaveAttribute('data-used', '0')
      await expect(user.getByTestId(`quota-${row}-value`)).toHaveText(`0 of ${max}`)
    }
    await expect(user.getByTestId('quota-devices-link')).toBeVisible()
    expect(await json<QuotaView>(userContext, '/api/quota')).toEqual({
      devices: 0,
      devices_max: QUOTAS.devices,
      daemons: 0,
      daemons_max: QUOTAS.daemons,
      connections: 0,
      connections_max: QUOTAS.connections,
      daily_pairings: 0,
      daily_pairings_max: QUOTAS.dailyPairings,
    })

    // 设置 → 登录方式：没有 token 一节；GitHub 还没关联，可以「关联」（带 link=1 的 start）；没配 Google
    await user.getByTestId('settings-nav-login').click()
    await expect(user).toHaveURL(/\/settings\/login$/)
    await expect(user.getByTestId('linked-accounts')).toBeVisible()
    await expect(user.getByTestId('tokens-section')).toHaveCount(0)
    await expect(user.getByTestId('token-new')).toHaveCount(0)
    await expect(user.getByTestId('identity-github')).toHaveAttribute('data-linked', 'false')
    const link = user.getByTestId('identity-link-github')
    await expect(link).toHaveText('Link')
    await expect(link).toHaveAttribute('href', '/auth/oauth/github/start?link=1')
    await expect(user.getByTestId('identity-google')).toHaveCount(0)
    expect(await json<unknown[]>(userContext, '/api/me/identities')).toEqual([])

    // 管理员的账号列表里看得到这个用户（不是管理员，可以被授予）；搜索按名字 / 邮箱子串
    await admin.goto('/admin')
    const userRow = admin.getByTestId(`admin-account-${userId}`)
    await expect(userRow).toContainText(USER_EMAIL)
    await expect(admin.getByTestId(`admin-by-config-${userId}`)).toHaveCount(0)
    await expect(admin.getByTestId(`admin-grant-${userId}`)).toBeEnabled()
    await admin.getByTestId('admin-account-search').fill('mx-user')
    await expect(admin.getByTestId(`admin-account-${adminId}`)).toHaveCount(0)
    await expect(userRow).toBeVisible()
  })

  test('d. 配额：配对两台浏览器后设备数满，第三台被拒并指向配额页；配额页靠 quota_update 实时更新', async ({
    browser,
  }) => {
    test.setTimeout(240_000)
    const state = readState()
    // 一个新节点连到 multi hub（还没配对到任何账号）
    extra = await startExtraDaemon({ tmpDir: hub.tmpDir, gitConfig: state.gitConfig }, 'mx-node', {
      hubUrl: hub.url,
    })
    const home = { daemonHome: extra.home }

    // 1. 第一台浏览器：节点 + 浏览器 = 2 台设备
    await user.goto('/pair')
    const first = await startPairing(home, { short: true })
    pairCodes.push(first.code)
    await pairBrowser(user, first.code, 'mx-browser-1')
    await expect(user).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: CONNECT_TIMEOUT })
    await expectNodeOnline(user)
    expect(await first.done).toBe(0)
    expect(await json<QuotaView>(userContext, '/api/quota')).toMatchObject({
      devices: 2,
      devices_max: QUOTAS.devices,
      daemons: 1,
      daemons_max: QUOTAS.daemons,
    })

    // 这台浏览器停在配额页上
    await user.goto('/settings/quota')
    await expect(user.getByTestId('quota-devices-value')).toHaveText(`2 of ${QUOTAS.devices}`)
    await expect(user.getByTestId('quota-daemons-value')).toHaveText('1 of 1')

    // 2. 第二台浏览器（同一个账号：带着同一个会话 cookie，但没有设备密钥）配对 → 3/3
    const session = await userContext.storageState()
    secondContext = await newContext(browser, session)
    const second = await secondContext.newPage()
    await second.goto('/pair')
    const secondPairing = await startPairing(home, { short: true })
    pairCodes.push(secondPairing.code)
    await pairBrowser(second, secondPairing.code, 'mx-browser-2')
    await expect(second).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: CONNECT_TIMEOUT })
    await expectNodeOnline(second)
    expect(await secondPairing.done).toBe(0)
    // 第一台浏览器没有刷新：配额页收到 hub 推的 quota_update 后重新拉了用量
    await expect(user.getByTestId('quota-devices-value')).toHaveText('3 of 3', {
      timeout: CONNECT_TIMEOUT,
    })
    await expect(user.getByTestId('quota-devices')).toHaveAttribute('data-used', '3')
    await expect(user.getByTestId('quota-devices').getByText('Full', { exact: true })).toBeVisible()

    // 3. 第三台浏览器：再加一台设备就超过 3，配对失败（两边都收到 pair_failed quota），什么都没写
    const thirdContext = await newContext(browser, session)
    try {
      const third = await thirdContext.newPage()
      await third.goto('/pair')
      const refused = await startPairing(home, { short: true })
      pairCodes.push(refused.code)
      try {
        await pairBrowser(third, refused.code, 'mx-browser-3')
        await expect(
          third.getByText(/reached a limit on devices, nodes or daily pairing attempts/),
        ).toBeVisible({
          timeout: CONNECT_TIMEOUT,
        })
        await expect(third.getByText(/Account settings → Devices/)).toBeVisible()
        await expect(third.getByTestId('quota-link')).toHaveAttribute('href', '/settings/quota')
        await expect(third).toHaveURL(/\/pair$/)
        // 节点那边也收到了 pair_failed（原因 quota）。`anycon pair` 本身只轮询“有没有新设备”，不会因此退出，
        // 所以看 daemon 的日志，最后 kill 掉它
        await waitFor(
          'the node being told that the pairing failed (quota)',
          () => /pairing failed[^\n]*Quota/.test(readLog(extra!.logFile)),
          { timeoutMs: 15_000 },
        )
      } finally {
        refused.kill()
      }
      // 配额页上的链接能点过去
      await third.getByTestId('quota-link').click()
      await expect(third).toHaveURL(/\/settings\/quota$/)
      await expect(third.getByTestId('quota-devices-value')).toHaveText('3 of 3')
    } finally {
      await thirdContext.close()
    }
    const after = await json<QuotaView>(userContext, '/api/quota')
    expect(after).toMatchObject({ devices: 3, devices_max: 3, daemons: 1, daemons_max: 1 })
    expect(after.daily_pairings).toBeGreaterThanOrEqual(3)
    expect(after.daily_pairings).toBeLessThanOrEqual(QUOTAS.dailyPairings)
  })

  test('e. 用户在资料页删除自己的账号：回到登录页带「已删除」，会话失效，管理员列表里没了，节点被断开', async () => {
    expect(await adminSees()).toContain(userId)

    await user.goto('/settings')
    await expect(user.getByTestId('danger-zone')).toBeVisible()
    await user.getByTestId('delete-account').click()
    const dialog = user.getByTestId('delete-account-dialog')
    await expect(dialog).toBeVisible()
    await expect(dialog).toContainText('Session history lives on your nodes and is not deleted.')
    // 会话是新的（注册不到 10 分钟）：不需要重新登录
    await user.getByTestId('delete-account-confirm').click()
    await expect(user).toHaveURL(/\/login/, { timeout: 30_000 })
    await expect(user.getByText('Your account has been deleted.')).toBeVisible()
    await expect(user.getByTestId('login-email-form')).toBeVisible()

    // 账号和它的会话都没了：这个浏览器的、另一台浏览器的 cookie 一样失效
    expect((await userContext.request.get(url('/api/me'))).status()).toBe(401)
    expect((await secondContext!.request.get(url('/api/me'))).status()).toBe(401)
    // 管理员的列表里没有了（页面和接口）
    await admin.goto('/admin')
    await expect(admin.getByTestId(`admin-account-${adminId}`)).toBeVisible()
    await expect(admin.getByTestId(`admin-account-${userId}`)).toHaveCount(0)
    expect(await adminSees()).not.toContain(userId)
    // 账号下的设备被一起删掉：节点的连接被 4004 断开，daemon 知道自己被吊销了
    await waitFor(
      'the node noticing that its account is gone',
      () => readLog(extra!.logFile).includes('this device was revoked on the hub'),
      { timeoutMs: 20_000 },
    )
  })

  test('f. 同一个邮箱再注册：新账号（id 不同、配额清零、不是管理员）', async ({ browser }) => {
    test.setTimeout(150_000) // 要等到距离 c 里发码满 60 秒
    const context = await newContext(browser)
    try {
      const page = await context.newPage()
      await signUpWithEmail(page, USER_EMAIL)
      await expect(page).toHaveURL(/\/setup$/)
      const me = await json<Me>(context, '/api/me')
      expect(me.id).not.toBe(userId)
      expect(me).toMatchObject({ email: USER_EMAIL, is_admin: false })
      expect(await json<QuotaView>(context, '/api/quota')).toMatchObject({
        devices: 0,
        daemons: 0,
        daily_pairings: 0,
      })
      // 管理员的列表里是新的这个，旧的没有回来
      await admin.goto('/admin')
      await expect(admin.getByTestId(`admin-account-${me.id}`)).toContainText(USER_EMAIL)
      await expect(admin.getByTestId(`admin-account-${userId}`)).toHaveCount(0)
    } finally {
      await context.close()
    }
  })

  test('g. hub 的日志（debug 级）里没有邮箱、验证码、配对码、OAuth 的 state / PKCE / 密钥、会话 cookie', async () => {
    // 去掉每行开头的时间戳：它的微秒部分是 6 位数字，会和验证码撞上
    const lines = readLog(hub.logFile)
      .split('\n')
      .map((line) => line.replace(/^\S+\s+/, ''))
    const log = lines.join('\n').toLowerCase()
    expect(issuedCodes).toHaveLength(3)
    expect(oauthSecrets.length).toBeGreaterThanOrEqual(3)
    const sessionCookies = (await adminContext.cookies(hub.url)).map((c) => c.value)
    expect(sessionCookies.length).toBeGreaterThan(0)
    for (const secret of [
      ADMIN_EMAIL,
      USER_EMAIL,
      'example.test',
      'mx-admin',
      'mx-user',
      GITHUB.clientSecret,
      ...oauthSecrets,
      ...sessionCookies,
    ]) {
      expect(log, 'the hub log must not contain a secret or an address').not.toContain(
        secret.toLowerCase(),
      )
    }
    for (const code of issuedCodes) {
      expect(log, 'the hub log must not contain a verification code').not.toMatch(
        new RegExp(`(^|\\D)${code.slice(0, 3)}[ -]?${code.slice(3)}(\\D|$)`),
      )
    }
    expect(hubLogLeaks(hub, pairCodes)).toEqual([])
    // 确实记了这些事（邮箱用哈希前 8 位代替），不是日志本来就没有内容
    expect(lines.filter((l) => /email code sent to=[0-9a-f]{8} /.test(l))).toHaveLength(3)
    expect(lines.some((l) => /oauth sign-in started/.test(l))).toBe(true)
  })
})
