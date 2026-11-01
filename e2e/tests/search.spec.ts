/**
 * 会话内容搜索（docs/spec/web.md § 9.1）：真实 hub + daemon（mock agent）+ web，浏览器和 daemon 之间走真的 `session.search`。
 *
 *   a. ⌘K 输入后停手才搜；命中带标题、片段（命中词高亮）、时间；片段里的 HTML（`<img onerror>`、`<script>`）只显示成文字，
 *      页面里没有因此多出元素、没有脚本执行；选中命中打开会话并高亮那条消息，`?seq=` 随后从地址里去掉；键盘 ↓ Enter 同样能选
 *   b. 很靠前的消息（会话里有几百个事件，最新的 200 个里没有它）：选中后把历史往前加载到它、滚到视口里并高亮
 *   c. 手机宽度：左侧抽屉里的搜索按钮打开同一个面板，面板贴近顶部
 *
 * mock agent 会回显 prompt（`You said: …`，crates/adapters/src/mock/README.md），所以用户消息和 assistant 文字里都有这个词。
 * prompt 里别带 `run` / `plan` / `permission` 这类 mock 的关键词。约定：显式等待（data-testid），没有固定睡眠。
 */
import { expect, test, type Page } from '@playwright/test'

import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  TURN_TIMEOUT,
  nodeBase,
  pairNewBrowser,
  type PairedBrowser,
} from '../helpers/ui.js'

test.describe.serial('会话内容搜索', () => {
  let device: PairedBrowser
  let page: Page
  let longSessionUrl = ''
  let shortSessionUrl = ''

  test.beforeAll(async ({ browser }) => {
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    device = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-search' })
    page = device.page
  })

  test.afterAll(async () => {
    await device?.context.close()
  })

  /** 发一条消息，等 mock 的回显出现、这一轮结束。会话长了消息流是虚拟化的，DOM 里的块数不是总数，所以不数 `turn-usage`。 */
  async function send(text: string): Promise<void> {
    await page.getByTestId('composer-input').fill(text)
    await page.getByTestId('composer-send').click()
    await expect(page.getByText(`You said: ${text}`)).toBeVisible({ timeout: TURN_TIMEOUT })
    await expect(page.getByTestId('session-status')).toHaveAttribute('data-status', 'idle', {
      timeout: TURN_TIMEOUT,
    })
  }

  /** 新建 scratch 会话并发出第一条消息，等第一轮结束；返回会话页的路径。 */
  async function startSession(prompt: string): Promise<string> {
    await page.goto(`${nodeBase(page)}/new`)
    await expect(page.getByTestId('composer-input')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('composer-input').fill(prompt)
    await page.getByTestId('composer-send').click()
    await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
    await expect(page.getByTestId('turn-usage')).toHaveCount(1, { timeout: TURN_TIMEOUT })
    return new URL(page.url()).pathname
  }

  async function openPalette(on: Page = page): Promise<void> {
    // 快捷键由外壳注册：等左栏出现（外壳已挂载）再按
    await expect(on.getByTestId('sidebar')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await on.keyboard.press('Control+K')
    await expect(on.getByRole('textbox', { name: /Search|搜索/ })).toBeFocused()
  }

  test('a. 命中带片段和高亮，HTML 只是文字，选中后打开会话并高亮那条消息', async () => {
    test.setTimeout(120_000)
    shortSessionUrl = await startSession(
      'note the zanzibarpotato <img src=x onerror="window.searchPwned=1"> and <script>window.searchPwned=2</script> &amp; done',
    )

    await page.goto(nodeBase(page))
    await openPalette()
    const input = page.getByRole('textbox', { name: /Search|搜索/ })
    await input.fill('zanzibarpot')
    // 停手之前不搜
    await expect(page.getByTestId('palette-searching')).toBeVisible()
    const hits = page.getByTestId('palette-hit')
    await expect(hits.first()).toBeVisible({ timeout: 15_000 })
    // 用户消息和 mock 的回显各有一条命中
    expect(await hits.count()).toBeGreaterThanOrEqual(2)

    const snippet = hits.first().getByTestId('palette-hit-snippet')
    await expect(snippet.locator('mark')).toHaveText(/zanzibarpot/i)
    // 片段里的标签按文字显示：`<img src=x onerror=…>`、`<script>` 原样出现在文字里，页面里没有这样的元素
    await expect(snippet).toContainText('<img src=x onerror="window.searchPwned=1">')
    await expect(snippet).toContainText('<script>window.searchPwned') // daemon 的片段只有十几个词，后面被截了
    await expect(page.locator('dialog img, [role=dialog] img, [role=dialog] script')).toHaveCount(0)
    await expect(hits.first().locator('time')).toBeVisible()
    // 片段里的 <img onerror> / <script> 如果被当成 HTML，`window.searchPwned` 就会被设上
    expect(
      await page.evaluate(() => (window as unknown as { searchPwned?: number }).searchPwned),
    ).toBeUndefined()

    // 键盘：默认选中第一条（标题匹配的本地项），↓ ↓ 到第二条内容命中，Enter 打开；落在会话页上、那条消息高亮，`?seq=` 随后被去掉
    await expect(
      page.getByRole('button', { name: /^note the zanzibarpotato/ }).first(),
    ).toHaveAttribute('aria-current', 'true')
    await input.press('ArrowDown')
    await expect(hits.first()).toHaveAttribute('aria-current', 'true')
    await input.press('ArrowDown')
    await expect(hits.nth(1)).toHaveAttribute('aria-current', 'true')
    await input.press('Enter')
    await expect(page).toHaveURL(new RegExp(`${shortSessionUrl}(\\?seq=\\d+)?$`), {
      timeout: 15_000,
    })
    await expect(page.getByTestId('message-list')).toBeVisible()
    await expect(page.locator('.search-flash')).toBeVisible({ timeout: 15_000 })
    await expect(page).not.toHaveURL(/seq=/, { timeout: 15_000 })
    await expect(page.getByTestId('palette-hit')).toHaveCount(0)
  })

  test('b. 很靠前的消息：加载历史、滚到视口里并高亮', async () => {
    test.setTimeout(300_000)
    longSessionUrl = await startSession('the quokkafirst message, long before the end')
    // 每个 turn 十来个事件：再来 25 轮，第一条消息远在最新的 200 个事件之前
    for (let i = 2; i <= 26; i++) await send(`filler number ${i}`)

    // 另一台设备（这个浏览器的缓存里没有这个会话的任何事件）：打开会话只加载最新的一屏，更早的在「加载更早的消息」后面
    const other = await pairNewBrowser(device.context.browser()!, readState(), {
      deviceName: 'e2e-search-2',
    })
    try {
      const p2 = other.page
      await p2.goto(longSessionUrl)
      await expect(p2.getByTestId('load-older')).toBeVisible({ timeout: CONNECT_TIMEOUT })
      await expect(
        p2.getByTestId('message-user').filter({ hasText: 'the quokkafirst message' }),
      ).toHaveCount(0)

      await openPalette(p2)
      await p2.getByRole('textbox', { name: /Search|搜索/ }).fill('quokkafirst')
      const hit = p2.getByTestId('palette-hit').filter({ hasText: 'quokkafirst' }).first()
      await expect(hit).toBeVisible({ timeout: 15_000 })
      await hit.click()

      const target = p2.getByTestId('message-user').filter({ hasText: 'the quokkafirst message' })
      await expect(target).toBeVisible({ timeout: 30_000 })
      await expect(target).toBeInViewport()
      await expect(p2.locator('.search-flash')).toBeVisible({ timeout: 15_000 })
      await expect(p2.getByTestId('message-list-finding')).toHaveCount(0)
      await expect(p2).not.toHaveURL(/seq=/, { timeout: 15_000 })
    } finally {
      await other.context.close()
    }
  })

  test('c. 手机宽度：抽屉里的搜索按钮打开同一个面板，面板贴近顶部', async () => {
    await page.setViewportSize({ width: 390, height: 844 })
    await page.goto(shortSessionUrl)
    await expect(page.getByTestId('menu-button')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('menu-button').click()
    await page.getByRole('button', { name: /Search|搜索/ }).click()
    const input = page.getByRole('textbox', { name: /Search|搜索/ })
    await expect(input).toBeFocused()
    await input.fill('zanzibarpot')
    await expect(page.getByTestId('palette-hit').first()).toBeVisible({ timeout: 15_000 })
    const box = await page.getByRole('dialog').boundingBox()
    expect(box).not.toBeNull()
    expect(box!.y).toBeLessThan(120)
    expect(box!.x + box!.width).toBeLessThanOrEqual(390)
  })
})
