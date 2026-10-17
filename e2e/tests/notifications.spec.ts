/**
 * 通知（ADR-37）：「该浏览器尚未启用通知」横幅、通知页的偏好开关（保存到 hub，刷新后还在）、本浏览器的推送开启 / 关闭（上报 hub 的订阅）。
 *
 * Playwright 的 Chromium 没有真的推送服务，所以只给 `PushManager.subscribe` / `getSubscription` 打桩（`stubPushManager`）：
 * 返回一个 endpoint 在 hub 白名单里（`https://fcm.googleapis.com/fcm/send/…`）、密钥合法（P-256 公钥 65 字节、auth 16 字节）的假订阅。
 * 页面的其余部分——通知权限、service worker、`POST /api/push/subscriptions`、hub 的校验和存储——都是真的。
 * **不要在这里触发 hub 真的去连推送服务**：不点「发送测试推送」，也不让 daemon 给这台（离线的）设备发 `notify`；结束时把订阅都删掉。
 *
 * `channel: 'chromium'`（新 headless）：只有它能让 `context.grantPermissions(['notifications'])` 生效，`headless_shell` 授予不了。
 *
 * 退出登录：普通「退出登录」也退订本浏览器的推送——浏览器里的订阅和 hub 里这台设备的订阅行都删（要在结束会话之前做），
 * 设备本身还在，重新登录后是同一台设备、推送是关着的。
 *
 * 偏好里的邮件开关要账号绑了邮箱才能用：这个文件用自己的地址绑定 1 次（hub 的绑定发码每 IP 每小时 5 次，见 email-login.spec.ts），结束时解绑。
 */
import { createECDH, randomBytes } from 'node:crypto'

import { expect, test, type BrowserContext, type Page } from '@playwright/test'

import { runCli, startPairing } from '../helpers/cli.js'
import {
  bindEmailViaApi,
  deleteAllPushSubscriptions,
  listPushSubscriptions,
  unbindEmailViaApi,
  webDeviceId,
} from '../helpers/account.js'
import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  expectLoginPage,
  expectNodeOnline,
  login,
  loginViaApi,
  nodeBase,
  pairNewBrowser,
  type PairedBrowser,
} from '../helpers/ui.js'

test.use({ channel: 'chromium' })
test.describe.configure({ mode: 'serial' })

const DEVICE_NAME = 'e2e-notify'
const EMAIL = 'e2e-notify@example.test'

const b64url = (buf: Buffer): string => buf.toString('base64url')

/** 一个假的浏览器推送订阅：endpoint 在 hub 的白名单里，密钥是真的（hub 会校验它们是曲线上的点 / 16 字节）。 */
function fakeSubscription(): { endpoint: string; p256dh: string; auth: string } {
  const ecdh = createECDH('prime256v1')
  ecdh.generateKeys()
  return {
    endpoint: `https://fcm.googleapis.com/fcm/send/e2e-${b64url(randomBytes(12))}`,
    p256dh: b64url(ecdh.getPublicKey()), // 未压缩点，65 字节
    auth: b64url(randomBytes(16)),
  }
}

/**
 * 给每个页面装上假的 `PushManager.subscribe` / `getSubscription`。订阅「存在浏览器里」：记在 localStorage，刷新页面后还在，
 * `unsubscribe()` 删掉它；同时记下页面订阅时用的 `applicationServerKey`（`e2e.push.key`），用例拿它和 hub 的 VAPID 公钥对。
 */
async function stubPushManager(
  context: BrowserContext,
  sub: { endpoint: string; p256dh: string; auth: string },
): Promise<void> {
  await context.addInitScript((s) => {
    const SUB = 'e2e.push.subscription'
    const KEY = 'e2e.push.key'
    // 注入的脚本要自成一体（序列化后在页面里跑），这两个小函数只能放在这里
    // oxlint-disable-next-line unicorn/consistent-function-scoping
    const toB64Url = (bytes: Uint8Array): string =>
      btoa(String.fromCharCode(...bytes))
        .replaceAll('+', '-')
        .replaceAll('/', '_')
        .replaceAll('=', '')
    // oxlint-disable-next-line unicorn/consistent-function-scoping
    const fromB64Url = (text: string): Uint8Array =>
      Uint8Array.from(atob(text.replaceAll('-', '+').replaceAll('_', '/')), (c) => c.charCodeAt(0))
    const make = (): object => {
      const key = localStorage.getItem(KEY)
      return {
        endpoint: s.endpoint,
        expirationTime: null,
        options: {
          userVisibleOnly: true,
          applicationServerKey: key ? fromB64Url(key).buffer : null,
        },
        toJSON: () => ({
          endpoint: s.endpoint,
          expirationTime: null,
          keys: { p256dh: s.p256dh, auth: s.auth },
        }),
        getKey: () => null,
        unsubscribe: async () => {
          localStorage.removeItem(SUB)
          return true
        },
      }
    }
    PushManager.prototype.getSubscription = async function () {
      return localStorage.getItem(SUB) ? (make() as PushSubscription) : null
    }
    PushManager.prototype.subscribe = async function (options?: PushSubscriptionOptionsInit) {
      const raw = options?.applicationServerKey
      const bytes =
        raw instanceof ArrayBuffer
          ? new Uint8Array(raw)
          : ArrayBuffer.isView(raw)
            ? new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength)
            : null
      if (bytes) localStorage.setItem(KEY, toB64Url(bytes))
      localStorage.setItem(SUB, '1')
      return make() as PushSubscription
    }
  }, sub)
}

/** 等 service worker 激活：「开启推送」要用它的注册（`navigator.serviceWorker.ready`）。 */
async function serviceWorkerReady(page: Page): Promise<void> {
  await page.evaluate(async () => {
    await navigator.serviceWorker.ready
  })
}

test.describe('通知', () => {
  let c: PairedBrowser

  test.beforeAll(async ({ browser }) => {
    const state = readState()
    c = await pairNewBrowser(browser, state, {
      deviceName: DEVICE_NAME,
      contextOptions: { permissions: ['notifications'] },
      prepare: (context) => stubPushManager(context, fakeSubscription()),
    })
  })

  test.afterAll(async () => {
    if (!c) return
    const state = readState()
    try {
      // 失败后的清理：订阅、邮箱、被改过的偏好都还原
      await deleteAllPushSubscriptions(c.context.request, state)
      await c.context.request.put(`${state.hubUrl}/api/me/notifications`, {
        data: {
          categories: [
            { category: 'session_idle', push: false, email: false },
            { category: 'error', push: true, email: true },
          ],
        },
        headers: { 'X-Anycon': '1' },
      })
      await unbindEmailViaApi(c.context.request, state)
    } finally {
      await c.context.close()
    }
  })

  test('1. 配对过节点、浏览器能收推送、还没订阅：横幅提示一次，点「去开启」进通知页，之后不再出现', async () => {
    const { page } = c
    const base = nodeBase(page)
    const banner = page.locator('[data-testid="status-banner"][data-kind="notifications"]')
    await expect(banner).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('banner-notifications-enable').click()
    await expect(page).toHaveURL(/\/settings\/notifications$/)
    // 通知权限已授予、还没有订阅：通知页上是「未开启」，没有横幅（在这一页上不催）
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'off')
    await expect(page.getByTestId('status-banner')).toHaveCount(0)

    // 回到节点页：提示过一次了（记在 localStorage 里），不再出现，刷新也不
    await page.goto(base)
    await expect(page.getByTestId('node-status').first()).toBeVisible()
    await page.reload()
    await expect(page.getByTestId('node-status').first()).toBeVisible()
    await expect(banner).toHaveCount(0)
    const prefs = await page.evaluate(() => localStorage.getItem('anycon.prefs'))
    expect(
      (JSON.parse(prefs ?? '{}') as { state: { notifyBannerDismissed: boolean } }).state,
    ).toMatchObject({ notifyBannerDismissed: true })
  })

  test('2. 偏好开关：改了保存到 hub，刷新后还在', async () => {
    const { page } = c
    const state = readState()
    await page.goto('/settings/notifications')
    // 账号还没绑邮箱：邮件列禁用，并说明要先在账号设置里绑定（hub 配了 SMTP，所以不是「hub 没配邮件」）
    const emailError = page.getByTestId('notify-email-error')
    await expect(page.getByTestId('notify-email-why')).toHaveAttribute('data-why', 'bind')
    await expect(emailError).toBeDisabled()
    // 绑定之后（用 API 绑，邮箱本身的流程在 email-login.spec.ts）邮件列可用，说明消失
    await bindEmailViaApi(c.context.request, state, EMAIL)
    await page.reload()
    await expect(page.getByTestId('notify-email-why')).toHaveCount(0)
    await expect(emailError).toBeEnabled()
    const idlePush = page.getByTestId('notify-push-session_idle')
    const errorPush = page.getByTestId('notify-push-error')
    await expect(idlePush).toHaveAttribute('aria-checked', 'false') // 默认：会话空闲不推
    await expect(emailError).toHaveAttribute('aria-checked', 'true') // 默认：出错发邮件

    const saved = (): Promise<unknown> =>
      page.waitForResponse(
        (r) =>
          r.url().endsWith('/api/me/notifications') && r.request().method() === 'PUT' && r.ok(),
      )
    let put = saved()
    await idlePush.click()
    await put
    put = saved()
    await emailError.click()
    await put
    await expect(idlePush).toHaveAttribute('aria-checked', 'true')
    await expect(emailError).toHaveAttribute('aria-checked', 'false')

    // 刷新：还是改过的样子，没动的类别没变
    await page.reload()
    await expect(idlePush).toHaveAttribute('aria-checked', 'true')
    await expect(page.getByTestId('notify-email-error')).toHaveAttribute('aria-checked', 'false')
    await expect(errorPush).toHaveAttribute('aria-checked', 'true')
    await expect(page.getByTestId('notify-push-permission_requested')).toHaveAttribute(
      'aria-checked',
      'true',
    )
    // hub 里的值也是这样
    const res = await c.context.request.get(`${state.hubUrl}/api/me/notifications`)
    const body = (await res.json()) as {
      categories: Array<{ category: string; push: boolean; email: boolean }>
    }
    const row = (category: string) => body.categories.find((x) => x.category === category)
    expect(row('session_idle')).toMatchObject({ push: true, email: false })
    expect(row('error')).toMatchObject({ push: true, email: false })
    expect(row('permission_requested')).toMatchObject({ push: true, email: true })

    // 还原成默认（也验证关得回去）
    put = saved()
    await page.getByTestId('notify-push-session_idle').click()
    await put
    put = saved()
    await page.getByTestId('notify-email-error').click()
    await put
    await page.reload()
    await expect(page.getByTestId('notify-push-session_idle')).toHaveAttribute(
      'aria-checked',
      'false',
    )
    await expect(page.getByTestId('notify-email-error')).toHaveAttribute('aria-checked', 'true')
  })

  test('3. 开启推送：用 hub 的 VAPID 公钥订阅，上报后 hub 里有这台设备；刷新后仍是已开启', async () => {
    const { page } = c
    const state = readState()
    await page.goto('/settings/notifications')
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'off')
    await serviceWorkerReady(page)
    expect(await listPushSubscriptions(c.context.request, state)).toEqual([])

    await page.getByTestId('push-enable').click()
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'on')

    // hub：恰好一条订阅，属于这台浏览器的设备，推送服务的主机在白名单里（endpoint 本身不回显）
    const deviceId = await webDeviceId(c.context.request, state, DEVICE_NAME)
    const rows = await listPushSubscriptions(c.context.request, state)
    expect(rows).toHaveLength(1)
    expect(rows[0]).toMatchObject({ device_id: deviceId, endpoint_host: 'fcm.googleapis.com' })
    expect(JSON.stringify(rows)).not.toContain('/fcm/send/')

    // 页面订阅时用的 applicationServerKey 就是 hub 在 /api/config 里公布的 VAPID 公钥
    const config = (await (await c.context.request.get(`${state.hubUrl}/api/config`)).json()) as {
      vapid_public_key: string
      push_available: boolean
    }
    expect(config.push_available).toBe(true)
    expect(await page.evaluate(() => localStorage.getItem('e2e.push.key'))).toBe(
      config.vapid_public_key,
    )

    // 刷新：浏览器里还有订阅、hub 里也有，不会重复上报（行 id 不变）
    await page.reload()
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'on')
    await page.waitForTimeout(500)
    expect(await listPushSubscriptions(c.context.request, state)).toEqual(rows)
  })

  test('4. 关闭推送：浏览器退订，hub 里这台设备的订阅也没了', async () => {
    const { page } = c
    const state = readState()
    await page.goto('/settings/notifications')
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'on')
    await page.getByTestId('push-disable').click()
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'off')
    expect(await listPushSubscriptions(c.context.request, state)).toEqual([])
    expect(await page.evaluate(() => localStorage.getItem('e2e.push.subscription'))).toBeNull()

    // 再刷新也是关着的
    await page.reload()
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'off')
  })

  test('5. /setup 向导里「要不要开通知」：直接开启（权限 → 订阅 → 上报 hub），然后进「完成」', async ({
    browser,
  }) => {
    const state = readState()
    const context = await browser.newContext({ permissions: ['notifications'] })
    let deviceId: string | undefined
    try {
      await stubPushManager(context, fakeSubscription())
      const page = await context.newPage()
      await login(page, state.adminToken)
      await page.goto('/setup')
      await page.getByTestId('setup-next').click()
      const pairing = await startPairing(state)
      await page.getByTestId('pair-code-input').fill(pairing.code)
      await page.getByTestId('pair-submit').click()
      // 配对完成 → 节点口令（跳过）→ 通知：浏览器能收推送、权限已授予、还没订阅，所以会问
      await page.getByTestId('setup-pw-skip').click({ timeout: CONNECT_TIMEOUT })
      await serviceWorkerReady(page)
      const enable = page.getByTestId('setup-notify-enable')
      await expect(enable).toBeEnabled({ timeout: CONNECT_TIMEOUT })
      await enable.click()
      await expect(page.getByTestId('setup-finish')).toBeVisible()
      expect(await pairing.done).toBe(0)

      // hub 里有这台新设备的订阅
      const rows = await listPushSubscriptions(context.request, state)
      expect(rows).toHaveLength(1)
      expect(rows[0]).toMatchObject({ endpoint_host: 'fcm.googleapis.com' })
      deviceId = rows[0]!.device_id
      await page.getByTestId('setup-finish').click()
      await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/)
      await expectNodeOnline(page)
    } finally {
      // 清理：订阅随设备吊销一起删，daemon 那边也不再认这台设备
      if (deviceId) await runCli(state, ['revoke', deviceId]).catch(() => undefined)
      await deleteAllPushSubscriptions(context.request, state).catch(() => undefined)
      await context.close()
    }
  })

  test('6. 退出登录（保留设备）也退订推送：浏览器订阅和 hub 里的订阅行都没了；重新登录后是同一台设备，推送关着', async ({
    request,
  }) => {
    const { page } = c
    const state = readState()
    await page.goto('/settings/notifications')
    await serviceWorkerReady(page)
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'off')
    await page.getByTestId('push-enable').click()
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'on')
    const deviceId = await webDeviceId(c.context.request, state, DEVICE_NAME)
    expect(await listPushSubscriptions(c.context.request, state)).toHaveLength(1)
    expect(await page.evaluate(() => localStorage.getItem('e2e.push.subscription'))).not.toBeNull()

    // 账号菜单 → 退出登录 → 确认（确认框里说明推送会关掉）
    await page.getByTestId('account-menu').click()
    await page.getByTestId('logout').click()
    await expect(page.getByTestId('logout-dialog')).toContainText(
      /Push notifications for this browser are turned off|推送通知会关掉/,
    )
    await page.getByTestId('logout-confirm').click()
    await expectLoginPage(page)

    // 浏览器里的订阅退掉了；这个页面的登录态已经没了，所以 hub 那边换一个登录的请求上下文去看
    expect(await page.evaluate(() => localStorage.getItem('e2e.push.subscription'))).toBeNull()
    await loginViaApi(request, state)
    expect(await listPushSubscriptions(request, state)).toEqual([])

    // 设备本身没动（这是「退出登录」不是「退出并移除本设备」）：重新登录同一台设备，推送是关着的
    await login(page, state.adminToken)
    await page.goto('/settings/notifications')
    await expect(page.getByTestId('push-status')).toHaveAttribute('data-state', 'off')
    expect(await webDeviceId(c.context.request, state, DEVICE_NAME)).toBe(deviceId)
    expect(await listPushSubscriptions(c.context.request, state)).toEqual([])
  })
})
