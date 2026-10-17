/**
 * 账号内接入（ADR-36）：同一个账号下的新浏览器不用短码，用节点口令 / 接入码 / 在已配对设备上批准来解锁自己的节点。
 * 真实 hub + 真实 daemon（mock agent）+ 真实 web（hub 嵌入的 dist）。
 *
 * 一个已配对的浏览器（owner）贯穿整个文件；每个用例再开全新的、存储隔离的上下文（visitor）登录同一个账号：
 *   - 这个浏览器还没有已配对的节点、账号下有 daemon → `/` 被导到 `/unlock/<node>`（以前是 `/setup`）
 *   - a 节点口令：owner 在节点设置 →「接入」里设口令 → visitor 的解锁页打开时扫码入口默认展开、但不自动开始（只有「显示二维码」按钮），
 *     点了才出二维码；页面进后台（脚本模拟 `visibilitychange`）二维码作废、回到前台不自动重开，要点「重新显示二维码」；
 *     然后 visitor 换到口令入口、改设备名、用口令解锁，daemon 里多了这台设备（改过的名字）
 *   - b 错口令：`unlock-error[data-code=wrong_secret]`，daemon 记一次失败；再输对的进去，失败计数清零
 *   - c 接入码：owner「添加设备」→「接入码」页拿 6 位码 → visitor 点「我有接入码」输入后解锁（一次性等细节在 cargo 的真 hub 用例里）
 *   - d 请求批准：visitor 点「在已配对设备上批准」→ owner 出现横幅 → 批准 → 读出 6 位码 → visitor 输入 → 解锁
 *   - e 拒绝：owner 拒绝 → visitor 看到 `unlock-error[data-code=denied]`
 *   - f 扫码批准：visitor 点「显示二维码」，二维码（元素截图）放到 owner 的「假摄像头」前（`installFakeCamera`：注入的 `getUserMedia` 返回 canvas 流），
 *     owner 的「扫码批准」对话框认出来 → 确认 → visitor 不用输入任何东西直接解锁。这是 web（二维码内容、base32、PSK）和 Rust daemon
 *     第一次真的对上：web 的 vitest 对的是 TS 写的 responder，Rust 的用例对的是 Rust 写的 initiator
 *   - g 已经配对了节点 A 的浏览器，在账号里另一台节点 B（测试自己起的第二个 daemon）上用口令解锁：这条路径用现有的设备身份
 *     在现有连接上发起（不开临时连接），hub 里不新建设备、只多一条配对；结束时把节点 B 从账号里去掉
 *   - h 「接入」标签的最近活动、日志里没有口令
 *
 * 状态：这个文件按名字排在 happy-path.spec.ts 之前，所以它会先把 daemon 配对到账号上（happy-path 第 1 步因此兼容两种顺序）。
 * 结束时清掉节点口令，不影响后面的用例。
 */
import { expect, test, type Browser, type BrowserContext, type Page } from '@playwright/test'

import { daemonDevices, runCli, startPairing } from '../helpers/cli.js'
import { startExtraDaemon } from '../helpers/daemon.js'
import { readLog } from '../helpers/process.js'
import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  expectNodeOnline,
  hubLogLeaks,
  login,
  loginHeadroom,
  loginViaApi,
  nodeBase,
  pairBrowser,
  pairNewBrowser,
  type PairedBrowser,
} from '../helpers/ui.js'

const PASSWORD = 'e2e-enroll-password-7391'
const WRONG_PASSWORD = 'e2e-definitely-not-the-password'
/** 口令派生（Argon2id）在 debug 构建里比较慢，两边都要算。 */
const UNLOCK_TIMEOUT = CONNECT_TIMEOUT + 30_000

/**
 * hub 对 `enroll_begin` 按 IP 限速：令牌桶，容量 10、每 6 秒回 1 个（`Limits.enroll_begin_per_min_per_ip`，
 * docs/spec/hub.md § 4.1）。所有 visitor 都来自 127.0.0.1，一个文件里的几个用例（每选一个入口就开始一次）
 * 加起来会超限（超限时对方的回执还会被每秒一条的预算吞掉，页面等 20 秒才报「没有收到 hub 的回应」）。
 * 所以每个 visitor 打开之前，按它最多要开始的接入次数从本地的同一个桶里「预订」，不够就等桶回填
 * （对 hub 已知速率的有界等待，不是固定睡眠；多留 1 个令牌的余量）。解锁页打开时不再自动开始扫码（要点「显示二维码」），
 * 所以只有用例里真正点了的入口才算：登录后落在 `/unlock/<node>` 本身不占（包括 `pairNewBrowser` 的登录）。
 * 账号上还有每小时 30 次的限速（一次完整运行大约用 12 次），
 * 所以同一个 hub 上不要 `--repeat-each` 这个文件（每次运行 global-setup 都会起新的 hub，不受影响）。
 */
const BUCKET_CAPACITY = 10
const REFILL_MS = 6_000
let bucketTokens = BUCKET_CAPACITY
let bucketAt = Date.now()

async function reserveEnrollBegins(count: number): Promise<void> {
  for (;;) {
    const now = Date.now()
    bucketTokens = Math.min(BUCKET_CAPACITY, bucketTokens + (now - bucketAt) / REFILL_MS)
    bucketAt = now
    if (bucketTokens >= count + 1) {
      bucketTokens -= count
      return
    }
    await new Promise((resolve) => setTimeout(resolve, (count + 1 - bucketTokens) * REFILL_MS))
  }
}

interface Visitor {
  context: BrowserContext
  page: Page
}

/**
 * 开一个全新的上下文，登录同一个账号，等落到 `/unlock/<node>`。
 * `begins`：这个 visitor 最多会开始几次接入（用例里点了的入口；打开页面本身不开始）。
 */
async function openVisitor(browser: Browser, begins: number): Promise<Visitor> {
  await reserveEnrollBegins(begins)
  const context = await browser.newContext()
  try {
    const page = await context.newPage()
    await login(page, readState().adminToken)
    // 这个浏览器没有已配对的节点、账号下有 daemon：`/` 去 `/unlock`，只有一台节点时直接进它
    await expect(page).toHaveURL(/\/unlock\/[0-9a-f]+$/, { timeout: 15_000 })
    await expect(page.getByTestId('unlock-panel')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    return { context, page }
  } catch (err) {
    await context.close()
    throw err
  }
}

/** 解锁成功：落到 `/n/<node>`，节点在线。 */
async function expectUnlocked(page: Page): Promise<void> {
  await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: UNLOCK_TIMEOUT })
  await expectNodeOnline(page)
}

/** 节点设置 →「接入」标签（整页加载，等标签里的口令区块出现）。 */
async function openEnrollTab(page: Page): Promise<void> {
  await page.goto(`${nodeBase(page)}/settings`)
  await page.getByTestId('tab-enroll').click()
  await expect(page.getByTestId('enroll-password')).toBeVisible({ timeout: CONNECT_TIMEOUT })
}

/** 「接入」标签里设节点口令（页面要已经在这个标签上）。 */
async function setNodePassword(page: Page, password: string): Promise<void> {
  await expect(page.getByTestId('enroll-password')).toHaveAttribute('data-state', 'unset')
  await page.getByTestId('enroll-password-set').click()
  await page.getByTestId('enroll-pw-1').fill(password)
  await page.getByTestId('enroll-pw-2').fill(password)
  await page.getByTestId('enroll-pw-submit').click()
  await expect(page.getByTestId('enroll-password')).toHaveAttribute('data-state', 'enabled', {
    timeout: UNLOCK_TIMEOUT,
  })
}

/** `anycon passwd --status --json`（本机 socket）：daemon 里的口令状态。 */
async function passwordStatus(): Promise<{
  set: boolean
  state: string
  failures: number
}> {
  const out = await runCli(readState(), ['passwd', '--status', '--json'], { timeoutMs: 15_000 })
  const line = out.split('\n').find((l) => l.trim().startsWith('{'))
  return (
    JSON.parse(line ?? '{}') as { password: { set: boolean; state: string; failures: number } }
  ).password
}

/** hub 的 `GET /api/devices` 里用到的字段。 */
interface HubDevice {
  id: string
  name: string
  kind: string
  paired_with: string[]
}

/** 6 位码的显示形式 `482 913` → `482913`。 */
function digitsOf(text: string | null): string {
  const digits = (text ?? '').replace(/\D/g, '')
  expect(digits).toMatch(/^\d{6}$/)
  return digits
}

/**
 * 假摄像头（在页面脚本之前注入）：`enumerateDevices` 报一个视频输入，`getUserMedia` 返回一个 canvas 的流，
 * canvas 里每 50 ms 画一次 `window.e2eFakeQr`（PNG 的 data URL，用例在需要时设置）——没设时是灰屏。
 * 这样「应用内扫码」走的还是真的 `getUserMedia` → `<video>` → 解码（`BarcodeDetector` 或打包的 jsQR）这条路，
 * 只是镜头前放的是另一个浏览器上的二维码截图。
 */
function installFakeCamera(): void {
  const md = navigator.mediaDevices
  if (!md) return
  md.enumerateDevices = () =>
    Promise.resolve([
      { kind: 'videoinput', deviceId: 'e2e-fake', label: 'e2e fake camera', groupId: 'e2e' },
    ] as MediaDeviceInfo[])
  md.getUserMedia = () => {
    const canvas = document.createElement('canvas')
    canvas.width = 640
    canvas.height = 640
    const ctx = canvas.getContext('2d')!
    let shown = ''
    let image: HTMLImageElement | null = null
    setInterval(() => {
      const url = (window as unknown as { e2eFakeQr?: string }).e2eFakeQr ?? ''
      if (url !== shown) {
        shown = url
        image = null
        if (url) {
          const img = new Image()
          img.onload = () => (image = img)
          img.src = url
        }
      }
      ctx.fillStyle = '#ffffff'
      ctx.fillRect(0, 0, 640, 640)
      if (image) {
        ctx.imageSmoothingEnabled = false
        ctx.drawImage(image, 80, 80, 480, 480)
      }
    }, 50)
    return Promise.resolve(canvas.captureStream(20))
  }
}

test.describe.serial('account enrollment', () => {
  let owner: PairedBrowser

  test.beforeAll(async ({ browser }) => {
    test.setTimeout(180_000)
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    // 第一台浏览器：用短码配对（账号下的第一台节点，或已经有了也行；已经有了的话登录后会落到 /unlock/<node>，不会自动开始接入）
    owner = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-enroll-owner' })
  })

  test.afterAll(async () => {
    test.setTimeout(120_000)
    await owner?.context.close()
    // 这个文件登录了十几次，而解锁页不再自动开始接入之后它跑得很快，桶里没剩几个令牌；后面的文件里
    // happy-path 第 9 步是裸点「登录」的（不会按 Retry-After 等），所以先等桶回填一点再交棒
    await loginHeadroom(5)
    // 把节点口令清掉：之后的用例不该受它影响（清除会把失败计数一并清零）
    try {
      await runCli(readState(), ['passwd', '--clear', '--yes'], { timeoutMs: 15_000 })
    } catch {
      // 没设口令 / daemon 已经停了：无所谓
    }
  })

  test('a. 已配对的浏览器设节点口令 → 另一个全新的浏览器登录同一账号，被导到 /unlock，用口令解锁', async ({
    browser,
  }) => {
    test.setTimeout(180_000)
    const state = readState()
    // owner：节点设置 → 接入 → 设置口令
    await openEnrollTab(owner.page)
    await setNodePassword(owner.page, PASSWORD)
    expect(await passwordStatus()).toMatchObject({ set: true, state: 'enabled', failures: 0 })

    const visitor = await openVisitor(browser, 3)
    try {
      const page = visitor.page
      // 扫码入口默认展开，但打开页面不自动开始：只有按钮，没有二维码（内容不在 DOM 里，这里只看它出现）
      await expect(page.getByTestId('unlock-trigger-scan')).toHaveAttribute('aria-expanded', 'true')
      await expect(page.getByTestId('unlock-show-qr')).toBeEnabled({ timeout: CONNECT_TIMEOUT })
      await expect(page.getByTestId('unlock-qr')).toHaveCount(0)
      await page.getByTestId('unlock-show-qr').click()
      await expect(page.getByTestId('unlock-qr')).toBeVisible({ timeout: CONNECT_TIMEOUT })

      // 页面进了后台：二维码作废（enroll_cancel），回到前台不自动重开，要用户点「重新显示二维码」
      // （Playwright 的无头页面始终是 visible，这里用脚本模拟 `visibilitychange`）
      const setVisibility = (visibility: 'hidden' | 'visible') =>
        page.evaluate((v) => {
          Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v })
          document.dispatchEvent(new Event('visibilitychange'))
        }, visibility)
      await setVisibility('hidden')
      await expect(page.getByTestId('unlock-qr')).toHaveCount(0)
      await expect(page.getByTestId('unlock-scan-paused')).toBeVisible()
      await setVisibility('visible')
      await page.waitForTimeout(500) // 给「不该发生的事」一点机会
      await expect(page.getByTestId('unlock-qr')).toHaveCount(0)
      await expect(page.getByTestId('unlock-error')).toHaveCount(0)
      await page.getByTestId('unlock-show-qr').click()
      await expect(page.getByTestId('unlock-qr')).toBeVisible({ timeout: CONNECT_TIMEOUT })

      // 换到口令入口（改名后失焦只在二维码显示着的时候才会换一个新的）
      await page.getByTestId('unlock-trigger-password').click()
      await page.getByRole('button', { name: /Change name|修改名称|更改名称|重命名/ }).click()
      await page.getByTestId('unlock-device-name').fill('e2e-by-password')
      await page.getByTestId('unlock-password-input').fill(PASSWORD)
      await page.getByTestId('unlock-password-submit').click()
      await expectUnlocked(page)

      // daemon 里多了这台设备，名字是刚才改的
      await expect
        .poll(
          async () =>
            (await daemonDevices(state)).some(
              (d) => d.name === 'e2e-by-password' && d.kind === 'web',
            ),
          { timeout: 15_000 },
        )
        .toBe(true)
    } finally {
      await visitor.context.close()
    }
  })

  test('b. 错口令显示错误（wrong_secret），daemon 记一次失败；再输对的进去，失败计数清零', async ({
    browser,
  }) => {
    test.setTimeout(180_000)
    const visitor = await openVisitor(browser, 2)
    try {
      const page = visitor.page
      await page.getByTestId('unlock-trigger-password').click()
      await page.getByTestId('unlock-password-input').fill(WRONG_PASSWORD)
      await page.getByTestId('unlock-password-submit').click()
      await expect(page.getByTestId('unlock-error')).toHaveAttribute('data-code', 'wrong_secret', {
        timeout: UNLOCK_TIMEOUT,
      })
      await expect(page).toHaveURL(/\/unlock\/[0-9a-f]+$/)
      await expect.poll(async () => (await passwordStatus()).failures, { timeout: 15_000 }).toBe(1)

      // 前两次错不退避：马上再试，输对
      await page.getByTestId('unlock-password-input').fill(PASSWORD)
      await page.getByTestId('unlock-password-submit').click()
      await expectUnlocked(page)
      await expect.poll(async () => (await passwordStatus()).failures, { timeout: 15_000 }).toBe(0)
    } finally {
      await visitor.context.close()
    }
  })

  test('c. 已配对的浏览器「添加设备」→「接入码」拿 6 位码 → 新浏览器用「我有接入码」解锁', async ({
    browser,
  }) => {
    test.setTimeout(180_000)
    const state = readState()
    await owner.page.goto(`${nodeBase(owner.page)}/settings`)
    await owner.page.getByTestId('tab-devices').click()
    await owner.page.getByTestId('device-add').click()
    await expect(owner.page.getByTestId('pair-begin-dialog')).toBeVisible({
      timeout: CONNECT_TIMEOUT,
    })
    await owner.page.getByTestId('add-page-code').click()
    await expect(owner.page.getByTestId('enroll-code')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    const code = digitsOf(await owner.page.getByTestId('enroll-code').textContent())

    const visitor = await openVisitor(browser, 1)
    try {
      const page = visitor.page
      await page.getByTestId('unlock-trigger-code').click()
      // 输满 6 位自动提交
      await page.getByTestId('unlock-code-input').fill(code)
      await expectUnlocked(page)
    } finally {
      await visitor.context.close()
    }
    // 新设备出现在 daemon 的设备列表里（owner + a、b、c 各一台）
    const web = (await daemonDevices(state)).filter((d) => d.kind === 'web')
    expect(web.length).toBeGreaterThanOrEqual(4)
    await owner.page.keyboard.press('Escape')
  })

  test('d. 新浏览器点「在已配对设备上批准」→ 已配对浏览器出现横幅 → 批准 → 读出 6 位码 → 新浏览器输入 → 解锁', async ({
    browser,
  }) => {
    test.setTimeout(180_000)
    const visitor = await openVisitor(browser, 1)
    try {
      const page = visitor.page
      await page.getByTestId('unlock-trigger-approve').click()
      await page.getByTestId('unlock-send-request').click()

      // owner：横幅 →「查看」→ 对话框 → 批准 → 码
      const banner = owner.page.locator('[data-testid="status-banner"][data-kind="enroll_request"]')
      await expect(banner).toBeVisible({ timeout: CONNECT_TIMEOUT })
      await owner.page.getByTestId('banner-enroll-review').click()
      await expect(owner.page.getByTestId('approve-dialog')).toBeVisible()
      await owner.page.getByTestId('approve-approve').click()
      await expect(owner.page.getByTestId('approve-code')).toBeVisible({ timeout: 20_000 })
      const code = digitsOf(await owner.page.getByTestId('approve-code').textContent())

      // visitor：输入码（输满自动提交）
      await page.getByTestId('unlock-approve-code').fill(code)
      await expectUnlocked(page)

      // owner：这个请求收尾了（码不再显示），横幅也收起
      await expect(owner.page.getByTestId('approve-resolved')).toBeVisible({ timeout: 20_000 })
      await expect(banner).toHaveCount(0, { timeout: 20_000 })
      await owner.page.keyboard.press('Escape')
    } finally {
      await visitor.context.close()
    }
  })

  test('e. 已配对的浏览器拒绝请求 → 新浏览器看到「被拒绝」', async ({ browser }) => {
    test.setTimeout(120_000)
    const visitor = await openVisitor(browser, 1)
    try {
      const page = visitor.page
      await page.getByTestId('unlock-trigger-approve').click()
      await page.getByTestId('unlock-send-request').click()
      await expect(
        owner.page.locator('[data-testid="status-banner"][data-kind="enroll_request"]'),
      ).toBeVisible({ timeout: CONNECT_TIMEOUT })
      await owner.page.getByTestId('banner-enroll-review').click()
      await expect(owner.page.getByTestId('approve-dialog')).toBeVisible()
      await owner.page.getByTestId('approve-deny').click()
      await expect(page.getByTestId('unlock-error')).toHaveAttribute('data-code', 'denied', {
        timeout: 20_000,
      })
      await expect(page).toHaveURL(/\/unlock\/[0-9a-f]+$/)
      await owner.page.keyboard.press('Escape')
    } finally {
      await visitor.context.close()
    }
  })

  test('f. 扫码批准：新浏览器的二维码被已配对浏览器（假摄像头对着它）扫到 → 确认 → 新浏览器直接解锁', async ({
    browser,
  }) => {
    test.setTimeout(120_000)
    const state = readState()
    // 注入要先于页面脚本：整页重新加载 owner 的节点设置页
    await owner.context.addInitScript(installFakeCamera)
    await owner.page.goto(`${nodeBase(owner.page)}/settings`)
    await owner.page.getByTestId('tab-devices').click()
    await expect(owner.page.getByTestId('device-scan-approve')).toBeVisible({
      timeout: CONNECT_TIMEOUT,
    })
    const before = (await daemonDevices(state)).length

    const visitor = await openVisitor(browser, 1)
    try {
      const page = visitor.page
      // 扫码入口默认展开，点「显示二维码」：二维码出现（收到 daemon 的 wait_approval 之后）。截图当作镜头前的画面
      await page.getByTestId('unlock-show-qr').click()
      const qr = page.getByTestId('unlock-qr').locator('div[role="img"]')
      await expect(qr).toBeVisible({ timeout: CONNECT_TIMEOUT })
      const png = await qr.screenshot()
      await owner.page.evaluate(
        (url) => {
          ;(window as unknown as { e2eFakeQr?: string }).e2eFakeQr = url
        },
        `data:image/png;base64,${png.toString('base64')}`,
      )

      // owner：设备页「扫码批准」→ 摄像头里认出二维码 → 确认页（显示新设备自报的名字）→ 批准
      await owner.page.getByTestId('device-scan-approve').click()
      await expect(owner.page.getByTestId('scan-dialog')).toBeVisible()
      await expect(owner.page.getByTestId('scan-confirm')).toBeVisible({ timeout: 30_000 })
      await expect(owner.page.getByTestId('scan-hint-name')).not.toBeEmpty()
      await owner.page.getByTestId('scan-approve').click()
      await expect(owner.page.getByTestId('scan-done')).toBeVisible({ timeout: 20_000 })

      // 新浏览器：不用输入任何东西，直接解锁
      await expectUnlocked(page)
      await expect
        .poll(async () => (await daemonDevices(state)).length, { timeout: 15_000 })
        .toBe(before + 1)
    } finally {
      await visitor.context.close()
      await owner.page.keyboard.press('Escape')
      await owner.page.evaluate(() => {
        ;(window as unknown as { e2eFakeQr?: string }).e2eFakeQr = ''
      })
    }
  })

  test('g. 已经配对了节点 A 的浏览器，在账号里的另一台节点 B 上用口令解锁：用现有的设备身份发起，不新建设备', async ({
    browser,
    request,
  }) => {
    test.setTimeout(240_000)
    const state = readState()
    const nodeB = await startExtraDaemon(state, 'e2e-node-b')
    const bHome = { daemonHome: nodeB.home }
    const bOwnerContext = await browser.newContext()
    let aOnly: PairedBrowser | undefined
    try {
      // 节点 B 先配对进这个账号：同一账号登录的另一个浏览器用短码配对它（它只认识 B）
      const bPage = await bOwnerContext.newPage()
      await login(bPage, state.adminToken)
      await bPage.goto('/pair')
      const pairing = await startPairing(bHome)
      await pairBrowser(bPage, pairing.code, 'e2e-b-owner')
      await expect(bPage).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: CONNECT_TIMEOUT })
      await expectNodeOnline(bPage)
      expect(await pairing.done).toBe(0)
      await openEnrollTab(bPage)
      await setNodePassword(bPage, PASSWORD)

      // 只认识节点 A 的浏览器（账号里现在有两台节点，登录后落在节点列表 /unlock）
      aOnly = await pairNewBrowser(browser, state, { deviceName: 'e2e-a-only' })
      const page = aOnly.page
      const nodeAUrl = page.url()
      await loginViaApi(request, state)
      const webDevices = async () =>
        ((await (await request.get(`${state.hubUrl}/api/devices`)).json()) as HubDevice[]).filter(
          (d) => d.kind === 'web',
        )
      const before = await webDevices()
      const self = before.find((d) => d.name === 'e2e-a-only')
      expect(self, 'the browser is a device of the account').toBeTruthy()
      const bId = (
        (await (await request.get(`${state.hubUrl}/api/devices`)).json()) as HubDevice[]
      ).find((d) => d.kind === 'daemon' && d.name === 'e2e-node-b')?.id
      expect(bId).toBeTruthy()
      expect(self?.paired_with).not.toContain(bId)

      // 节点切换器里 B 带锁：点它去 /unlock/<B>，用口令解锁
      await reserveEnrollBegins(1)
      await page.getByTestId('node-switcher').click()
      await page.getByTestId('node-locked').click()
      await expect(page).toHaveURL(/\/unlock\/[0-9a-f]+$/, { timeout: 15_000 })
      await expect(page.getByTestId('unlock-show-qr')).toBeEnabled({ timeout: CONNECT_TIMEOUT })
      await page.getByTestId('unlock-trigger-password').click()
      await page.getByTestId('unlock-password-input').fill(PASSWORD)
      await page.getByTestId('unlock-password-submit').click()
      await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: UNLOCK_TIMEOUT })
      expect(page.url()).not.toBe(nodeAUrl)
      await expectNodeOnline(page)

      // hub 里没有多出设备：还是同一台，只是多配了一台节点；daemon B 也认这台设备（名字、类型来自它原来的身份）
      const after = await webDevices()
      expect(after.map((d) => d.id).toSorted()).toEqual(before.map((d) => d.id).toSorted())
      await expect
        .poll(async () => (await webDevices()).find((d) => d.id === self?.id)?.paired_with ?? [], {
          timeout: 15_000,
        })
        .toContain(bId)
      expect(
        (await daemonDevices(bHome)).some((d) => d.name === 'e2e-a-only' && d.kind === 'web'),
      ).toBe(true)
    } finally {
      await aOnly?.context.close()
      await bOwnerContext.close()
      await nodeB.stop()
      // 把节点 B 从账号里去掉：之后的用例（happy path 第 8 步）假定账号下只有一台节点（`/unlock` 直接进它）
      try {
        await loginViaApi(request, state)
        const all = (await (await request.get(`${state.hubUrl}/api/devices`)).json()) as HubDevice[]
        for (const d of all.filter((x) => x.kind === 'daemon' && x.name === 'e2e-node-b')) {
          await request.delete(`${state.hubUrl}/api/devices/${d.id}`, {
            headers: { 'X-Anycon': '1' },
          })
        }
      } catch {
        // 清理尽力而为
      }
    }
  })

  test('h. 节点设置 →「接入」的最近活动里有这些接入；口令和码不在 hub / daemon 的日志里', async () => {
    const state = readState()
    await openEnrollTab(owner.page)
    const activity = owner.page.getByTestId('enroll-activity-list')
    await expect(activity).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect.poll(() => activity.locator('li').count(), { timeout: 15_000 }).toBeGreaterThan(0)
    // 口令（对的和错的）不会出现在任何日志里
    expect(hubLogLeaks(state, [PASSWORD, WRONG_PASSWORD])).toEqual([])
    const daemonLog = readLog(`${state.tmpDir}/daemon.log`)
    expect(daemonLog).not.toContain(PASSWORD)
    expect(daemonLog).not.toContain(WRONG_PASSWORD)
  })
})
