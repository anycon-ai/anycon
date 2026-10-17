/**
 * happy path（README 快速开始的那条路径）：真实 hub + 真实 daemon（mock agent）+ 真实 web（hub 嵌入的 dist）。
 *
 * 串行 9 步，共用一个浏览器上下文（第 8、9 步另开新的、存储隔离的上下文）：
 *   1 登录  2 配对（含可跳过的节点口令一步）  3 新建项目  4 从项目页进入新会话页，切换模型 / 思考 / 权限模式  5 发 `run permission` 并允许
 *   6 tool 块 + 用量  7 刷新后历史仍在  8 第二台设备答复权限，第一台显示「已由 … 允许」
 *   9 配对链接 `/pair#code=…`（短码在 URL fragment 里）：先登录，确认后配对
 * 配对、吊销后重新配对、退出并移除本设备、大输出各有独立用例，见 lifecycle.spec.ts。
 *
 * 约定：全部走 data-testid 做显式等待（不用固定睡眠）；mock agent 的行为见 crates/adapters/src/mock/README.md。
 * 第一次连 daemon（首次 probe / Noise 握手）可能比较慢，所以关键等待给 30 秒以上。
 */
import { expect, test, type Browser, type BrowserContext, type Page } from '@playwright/test'

import { startPairing } from '../helpers/cli.js'
import { readState } from '../helpers/state.js'
import {
  ALLOWED_BY,
  CONNECT_TIMEOUT,
  LONG_CODE_RE,
  TURN_TIMEOUT,
  expectNodeOnline,
  hubLogLeaks,
  login,
  openNewSessionFromProject,
  pairBrowser,
  pendingCard,
  resolvedCards,
  submitLogin,
} from '../helpers/ui.js'

const PROJECT = 'e2e-project'
const SECOND_DEVICE = 'e2e-second-browser'

test.describe.serial('happy path', () => {
  let browserRef: Browser
  let context: BrowserContext
  let page: Page
  let secondContext: BrowserContext | undefined
  let sessionUrl = ''

  test.beforeAll(async ({ browser }) => {
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    browserRef = browser
    context = await browser.newContext()
    page = await context.newPage()
  })

  test.afterAll(async () => {
    await secondContext?.close()
    await context?.close()
  })

  test('1. 登录：粘贴管理员 token → 没有已配对的节点：账号下没有节点去 /setup（有则去 /unlock）', async () => {
    const { adminToken } = readState()
    await login(page, adminToken)
    // `/` 的去向取决于账号下有没有 daemon（hub 的设备列表，ADR-36）：这个文件按名字排在 enroll.spec.ts 之后，
    // 单独跑它时账号下还没有节点（→ /setup 向导），整套一起跑时 enroll.spec.ts 已经配对过 daemon（→ /unlock）
    const hasNode = await page.evaluate(async () => {
      const res = await fetch('/api/devices')
      return ((await res.json()) as { kind: string }[]).some((d) => d.kind === 'daemon')
    })
    if (hasNode) {
      await expect(page).toHaveURL(/\/unlock(\/[0-9a-f]+)?$/)
      await expect(page.getByTestId('unlock-panel')).toBeVisible()
      // 向导照样能手动进：下面的步骤从 /setup 开始
      await page.goto('/setup')
    }
    await expect(page).toHaveURL(/\/setup$/)
    await expect(page.getByTestId('setup-card')).toBeVisible()
  })

  test('2. /setup：用 `anycon pair --json` 拿短码（默认 16 位），输入后配对成功，进入节点首页', async () => {
    const state = readState()
    const pairing = await startPairing(state)
    expect(pairing.code).toMatch(LONG_CODE_RE)
    // 链接里的短码在 fragment（#code=），不是 query：浏览器不会把它发给 hub
    expect(pairing.link).toContain(`/pair#code=${pairing.code}`)
    expect(pairing.link).not.toContain('?code=')

    await page.getByTestId('setup-next').click()
    await page.getByTestId('pair-code-input').fill(pairing.code)
    await page.getByTestId('pair-submit').click()
    // 配对完成 → 可以跳过的「设置节点口令」一步（ADR-36）→ 可以跳过的「要不要开通知」一步（ADR-37；这个上下文里通知权限是被拒绝的，
    // 向导自己跳过它——会问的情形在 notifications.spec.ts 的 5）→ 向导最后一步
    await page.getByTestId('setup-pw-skip').click({ timeout: CONNECT_TIMEOUT })
    const skipNotify = page.getByTestId('setup-notify-skip')
    const finish = page.getByTestId('setup-finish')
    await expect(skipNotify.or(finish)).toBeVisible({ timeout: CONNECT_TIMEOUT })
    if (await skipNotify.isVisible()) await skipNotify.click()
    await finish.click({ timeout: CONNECT_TIMEOUT })
    await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/)
    await expectNodeOnline(page)
    // CLI 那一侧也应当看到配对完成并正常退出
    expect(await pairing.done).toBe(0)
  })

  test('3. 新建项目', async () => {
    await page.getByTestId('project-new').click()
    await expect(page.getByTestId('project-new-dialog')).toBeVisible()
    await page.getByTestId('project-new-name').fill(PROJECT)
    await page.getByTestId('project-new-submit').click()
    // 创建成功后直接进入项目页
    await expect(page).toHaveURL(new RegExp(`/p/${PROJECT}$`), { timeout: 15_000 })
    // 回节点首页，项目卡片在列表里
    await page
      .getByRole('link', { name: /e2e-node/ })
      .first()
      .click()
    await expect(page.getByTestId('project-card').filter({ hasText: PROJECT })).toBeVisible()
    await page.getByTestId('project-card').filter({ hasText: PROJECT }).click()
    await expect(page).toHaveURL(new RegExp(`/p/${PROJECT}$`))
  })

  test('4. 新建会话：模型 / 思考 / 权限模式各切换一次', async () => {
    // 项目页没有输入框，头部「新会话」进到空白的新会话页（项目已选中）
    await expect(page.getByTestId('composer-input')).toHaveCount(0)
    await openNewSessionFromProject(page, PROJECT)

    const model = page.getByTestId('option-model')
    const thinking = page.getByTestId('option-thinking')
    const mode = page.getByTestId('option-mode')

    // 默认：mock-large + thinking high + ask
    await expect(model).toHaveAttribute('data-value', 'mock-large', { timeout: 15_000 })
    await expect(mode).toHaveAttribute('data-value', 'ask')

    // 模型 → mock-small（没有 thinking，控件整体隐藏）→ 再切回 mock-large
    await model.click()
    await page.getByTestId('option-model-mock-small').click()
    await expect(model).toHaveAttribute('data-value', 'mock-small')
    await expect(thinking).toHaveCount(0)
    await model.click()
    await page.getByTestId('option-model-mock-large').click()
    await expect(model).toHaveAttribute('data-value', 'mock-large')

    // 思考 → low（下拉：打开才有各个等级，选中的等级项 aria-checked）
    await expect(thinking).toBeVisible()
    await thinking.click()
    await page.getByTestId('option-thinking-low').click()
    await expect(thinking).toHaveAttribute('data-value', 'low')
    await thinking.click()
    await expect(page.getByTestId('option-thinking-low')).toHaveAttribute('aria-checked', 'true')
    await page.keyboard.press('Escape')
    await expect(page.getByTestId('option-thinking-low')).toHaveCount(0)

    // 权限模式 → plan → 再回 ask
    await mode.click()
    await page.getByTestId('option-mode-plan').click()
    await expect(mode).toHaveAttribute('data-value', 'plan')
    await mode.click()
    await page.getByTestId('option-mode-ask').click()
    await expect(mode).toHaveAttribute('data-value', 'ask')
    await expect(model).toHaveAttribute('data-value', 'mock-large')
  })

  test('5. 发送「run permission」→ 出现权限卡片 → 允许', async () => {
    await page.getByTestId('composer-input').fill('run permission')
    await page.getByTestId('composer-send').click()
    // 创建会话后跳到会话页
    await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
    sessionUrl = new URL(page.url()).pathname

    const card = pendingCard(page)
    await expect(card).toBeVisible({ timeout: TURN_TIMEOUT })
    await expect(card).toHaveAttribute('data-kind', 'tool')
    await expect(page.getByTestId('session-status')).toHaveAttribute(
      'data-status',
      'waiting_permission',
    )
    await card.getByTestId('permission-option-allow').click()
    await expect(resolvedCards(page)).toHaveCount(1, { timeout: 15_000 })
    await expect(resolvedCards(page).first()).toContainText(ALLOWED_BY)
  })

  test('6. 看到 tool 块、回显的设置和 turn-usage', async () => {
    await expect(page.getByTestId('tool-block').first()).toBeVisible({ timeout: TURN_TIMEOUT })
    await expect(page.getByTestId('turn-usage').first()).toBeVisible({ timeout: TURN_TIMEOUT })
    await expect(page.getByTestId('message-assistant').first()).toContainText(
      '[model=mock-large thinking=low mode=ask]',
    )
    await expect(page.getByTestId('session-status')).toHaveAttribute('data-status', 'idle')
  })

  test('7. 刷新页面，历史仍在（IndexedDB + session.history）', async () => {
    await page.reload()
    await expect(page).toHaveURL(new RegExp(`${sessionUrl}$`))
    await expect(page.getByTestId('message-assistant').first()).toContainText(
      '[model=mock-large thinking=low mode=ask]',
      { timeout: CONNECT_TIMEOUT },
    )
    await expect(page.getByTestId('tool-block').first()).toBeVisible()
    await expect(page.getByTestId('turn-usage').first()).toBeVisible()
    await expect(page.getByTestId('message-user').first()).toContainText('run permission')
    await expectNodeOnline(page)
  })

  test('8. 第二个浏览器上下文配对为第二台设备，在它上面答复权限，第一台显示「已由 … 允许」', async () => {
    const state = readState()
    secondContext = await browserRef.newContext()
    const second = await secondContext.newPage()

    // 登录同一个账号 → 这个浏览器还没有已配对的节点、账号下有 daemon：/unlock/<node>（ADR-36；以前是 /setup）→ 直接去 /pair
    await login(second, state.adminToken)
    await expect(second).toHaveURL(/\/unlock\/[0-9a-f]+$/)
    await second.goto('/pair')
    const pairing = await startPairing(state)
    await pairBrowser(second, pairing.code, SECOND_DEVICE)
    await expect(second).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: CONNECT_TIMEOUT })
    await expectNodeOnline(second)
    expect(await pairing.done).toBe(0)

    // 第一台：同一会话再发一条，会弹出第二张权限卡片
    await page.getByTestId('composer-input').fill('run permission')
    await page.getByTestId('composer-send').click()
    const firstPending = pendingCard(page)
    await expect(firstPending).toBeVisible({ timeout: TURN_TIMEOUT })
    const requestId = await firstPending.getAttribute('data-request')
    expect(requestId).toBeTruthy()

    // 第二台：打开同一个会话，看到同一个待审批请求，在这里答复
    await second.goto(sessionUrl)
    const remote = second.locator(
      `[data-testid="permission-card"][data-state="pending"][data-request="${requestId}"]`,
    )
    await expect(remote).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await remote.getByTestId('permission-option-allow').click()

    // 第二台自己：卡片折叠为已决
    await expect(remote).toBeHidden({ timeout: 15_000 })

    // 第一台：待审批卡片消失，最新一张已决卡片写明是第二台设备允许的
    await expect(firstPending).toHaveCount(0, { timeout: 15_000 })
    await expect(resolvedCards(page)).toHaveCount(2)
    const latest = resolvedCards(page).last()
    await expect(latest).toContainText(ALLOWED_BY)
    await expect(latest).toContainText(SECOND_DEVICE)

    // 这一轮正常结束：两边都能看到第二个 turn-usage
    await expect(page.getByTestId('turn-usage')).toHaveCount(2, { timeout: TURN_TIMEOUT })
    await expect(second.getByTestId('turn-usage')).toHaveCount(2, { timeout: TURN_TIMEOUT })
  })

  test('9. 配对链接（二维码里的 /pair#code=…）：先登录，确认后配对', async () => {
    const state = readState()
    const pairing = await startPairing(state)
    expect(pairing.link).toContain(`/pair#code=${pairing.code}`)
    const third = await browserRef.newContext()
    try {
      const thirdPage = await third.newPage()
      // 还没登录：被带去登录页。短码不能出现在登录页的 URL（path / query / hash）里，否则刷新一次就进了
      // hub 和反代的访问日志；它只在 sessionStorage 里，刷新登录页也不会丢。
      // hub 的访问日志只记路由模板，所以这里直接监听浏览器发出的每一个请求（整页导航 + 子资源 + WebSocket）：
      // 任何 URL 都不能带短码（含 URL 编码形式）。fragment 本来就不会发出去，这条断言抓的是 query / path 泄露。
      const leaked: string[] = []
      const codeForms = [
        pairing.code,
        encodeURIComponent(pairing.code),
        pairing.code.replace(/-/g, ''),
      ]
      thirdPage.on('request', (req) => {
        const url = req.url()
        if (codeForms.some((c) => url.includes(c))) leaked.push(url)
      })
      await thirdPage.goto(pairing.link)
      await expect(thirdPage).toHaveURL(/\/login(\?|$)/, { timeout: 15_000 })
      const loginUrl = thirdPage.url()
      expect(loginUrl).not.toContain(pairing.code)
      expect(loginUrl).not.toContain('code=')
      expect(loginUrl).not.toContain('#')
      await thirdPage.reload()
      expect(thirdPage.url()).not.toContain(pairing.code)
      // 按 429 的提示等（前面的文件可能刚把登录限速的桶用空）
      await submitLogin(thirdPage, state.adminToken)
      // 从链接带进来的短码不会自动开始配对：先显示确认步骤，点了才开始，不用再输入短码
      await expect(thirdPage.getByTestId('pair-link-confirm')).toBeVisible({ timeout: 15_000 })
      await expect(thirdPage.getByTestId('pair-link-code')).toContainText(pairing.code)
      await thirdPage.getByTestId('pair-link-confirm-submit').click()
      await expect(thirdPage).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: CONNECT_TIMEOUT })
      await expectNodeOnline(thirdPage)
      expect(await pairing.done).toBe(0)
      // 节点设置 → 设备：三台浏览器都在（别的 spec 自己配对的浏览器也可能在列表里：按文件名排在前面的会先跑，所以是「至少」）
      await thirdPage.goto(sessionUrl.replace(/\/s\/.*$/, '/settings'))
      await thirdPage.getByTestId('tab-devices').click()
      const devices = thirdPage.getByTestId('device-list').locator('li')
      await expect
        .poll(() => devices.count(), { timeout: CONNECT_TIMEOUT })
        .toBeGreaterThanOrEqual(3)
      await expect(devices.filter({ hasText: SECOND_DEVICE })).toHaveCount(1)
      // 浏览器没有发出过任何带短码的请求，hub 的 debug 日志里也没有短码（match_id 这一层的检查）
      expect(leaked).toEqual([])
      expect(hubLogLeaks(state, [pairing.code])).toEqual([])
    } finally {
      await third.close()
    }
  })
})
