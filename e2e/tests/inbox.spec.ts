/**
 * 待处理 `/inbox`（docs/spec/web.md § 4.11）：真实 hub + daemon（mock agent）+ web。
 *
 *   a. `/` 落到 `/inbox`，左栏导航项带「现在」的个数、标当前页
 *   b. 桌面：权限请求在右半边的详情里，用的是会话页同一个卡片（命令、工作目录），行上没有答复按钮；允许之后条目消失、出现「没有等你的事」，
 *      会话那边显示「已由 … 允许」
 *   c. 从通知点进来的地址（`/inbox?node=&session=`）：定位那个会话的请求，地址里的参数被去掉
 *   d. 手机宽度：底部标签栏取代抽屉，第一条默认展开；会话 / 更多标签；进会话页标签栏收起，返回后恢复；在行内答复
 *
 * 每个会话用 mock agent 的 `run permission` 造一条待答复的 tool 请求（见 crates/adapters/src/mock/README.md）。
 * 约定：显式等待（data-testid），没有固定睡眠。
 */
import { expect, test, type Page } from '@playwright/test'

import { readState } from '../helpers/state.js'
import {
  ALLOWED_BY,
  CONNECT_TIMEOUT,
  TURN_TIMEOUT,
  nodeBase,
  pairNewBrowser,
  pendingCard,
  resolvedCards,
  type PairedBrowser,
} from '../helpers/ui.js'

test.describe.serial('待处理', () => {
  let device: PairedBrowser
  let page: Page
  let base = ''
  const sessions: string[] = []

  test.beforeAll(async ({ browser }) => {
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    device = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-inbox' })
    page = device.page
    // 配对完落在节点首页 `/n/<node>`
    base = nodeBase(page)
  })

  test.afterAll(async () => {
    await device?.context.close()
  })

  /** 新建 scratch 会话，发 `run permission`，等出现待答复的卡片（不答复）；返回会话 id。 */
  async function startPermissionSession(): Promise<string> {
    await page.goto(`${base}/new`)
    await expect(page.getByTestId('composer-input')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('composer-input').fill('run permission')
    await page.getByTestId('composer-send').click()
    await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
    await expect(pendingCard(page)).toBeVisible({ timeout: TURN_TIMEOUT })
    return new URL(page.url()).pathname.split('/').pop()!
  }

  const rows = (): ReturnType<Page['getByTestId']> => page.getByTestId('inbox-row')

  test('a. 「/」落到 /inbox：左栏导航项标当前页，没有已有的置顶组', async () => {
    sessions.push(await startPermissionSession())
    await page.goto('/')
    await expect(page).toHaveURL(/\/inbox$/, { timeout: CONNECT_TIMEOUT })
    const nav = page.getByTestId('sidebar-inbox')
    await expect(nav).toHaveAttribute('aria-current', 'page')
    await expect(page.getByTestId('sidebar-inbox-count')).toHaveText('1', {
      timeout: CONNECT_TIMEOUT,
    })
    await expect(page.getByTestId('sidebar-waiting')).toHaveCount(0)
    await expect(page.getByTestId('sidebar-follow-up')).toHaveCount(0)
    await expect(page).toHaveTitle(/^\(1\) Anycon/)
  })

  test('b. 桌面：详情里是会话页同一个权限卡片，行上没有按钮；允许之后清空，会话那边显示已允许', async () => {
    await expect(page.getByTestId('inbox-page')).toHaveAttribute('data-mode', 'split')
    await expect(rows()).toHaveCount(1, { timeout: CONNECT_TIMEOUT })
    await expect(rows().first()).toHaveAttribute('data-kind', 'permission')
    // 行上没有答复按钮
    await expect(rows().first().locator('[data-testid^="permission-option-"]')).toHaveCount(0)
    // 右半边：卡片、命令、工作目录、节点、所属会话
    const pane = page.getByTestId('inbox-detail-pane')
    const card = pane.getByTestId('permission-card')
    await expect(card).toBeVisible()
    await expect(card).toHaveAttribute('data-kind', 'tool')
    await expect(card.getByTestId('permission-command')).toBeVisible()
    await expect(pane.getByTestId('inbox-detail-node')).toHaveText(/e2e-node/)
    await expect(pane.getByTestId('inbox-detail-session')).not.toHaveText('')
    await expect(pane.getByTestId('inbox-open-session')).toHaveAttribute(
      'href',
      new RegExp(`${sessions[0]}$`),
    )

    await card.getByTestId('permission-option-allow').click()
    await expect(page.getByTestId('inbox-empty')).toBeVisible({ timeout: 15_000 })
    await expect(rows()).toHaveCount(0)
    await expect(page.getByTestId('sidebar-inbox-count')).toHaveCount(0)
    // 清空之后焦点落在「没有等你的事」上
    await expect(
      page.getByRole('heading', { name: /Nothing is waiting for you|没有等你的事/ }),
    ).toBeFocused()

    // 会话那边：已由这台设备允许
    await page.goto(`${base}/s/${sessions[0]}`)
    await expect(resolvedCards(page)).toHaveCount(1, { timeout: CONNECT_TIMEOUT })
    await expect(resolvedCards(page).first()).toContainText(ALLOWED_BY)
  })

  test('c. 通知的落点 /inbox?node=&session=：定位那个会话的请求，参数从地址里去掉', async () => {
    sessions.push(await startPermissionSession())
    sessions.push(await startPermissionSession())
    const [, first, second] = sessions
    const node = base.split('/').pop()!
    await page.goto(`/inbox?node=${node}&session=${second}`)
    await expect(rows()).toHaveCount(2, { timeout: CONNECT_TIMEOUT })
    await expect(
      page.getByTestId('inbox-detail-pane').getByTestId('inbox-open-session'),
    ).toHaveAttribute('href', new RegExp(`${second}$`))
    await expect(page).toHaveURL(/\/inbox$/)
    // 键盘：K 回到上一条，详情换成另一个会话（焦点在页面上、不在输入框里）
    await page.keyboard.press('k')
    await expect(
      page.getByTestId('inbox-detail-pane').getByTestId('inbox-open-session'),
    ).toHaveAttribute('href', new RegExp(`${first}$`))
    // 答复选中的那条（按钮在新显示的头一小会儿是禁用的，Playwright 会等它可用）：这是在这里处理的，所以选中落到剩下的那条
    await page.getByTestId('inbox-detail-pane').getByTestId('permission-option-allow').click()
    await expect(rows()).toHaveCount(1, { timeout: 15_000 })
    await expect(
      page.getByTestId('inbox-detail-pane').getByTestId('inbox-open-session'),
    ).toHaveAttribute('href', new RegExp(`${second}$`))
  })

  test('d. 手机宽度：底栏取代抽屉；第一条默认展开，在行内答复；会话页收起底栏、返回后恢复', async () => {
    await page.setViewportSize({ width: 390, height: 844 })
    await page.goto('/inbox')
    await expect(page.getByTestId('inbox-page')).toHaveAttribute('data-mode', 'inline', {
      timeout: CONNECT_TIMEOUT,
    })
    // 底部标签栏：待处理 / 会话 / 更多，没有左栏、没有抽屉的菜单按钮
    const bar = page.getByTestId('tab-bar')
    await expect(bar).toBeVisible()
    await expect(bar.getByRole('link')).toHaveCount(3)
    await expect(page.getByTestId('tab-inbox')).toHaveAttribute('aria-current', 'page')
    await expect(page.getByTestId('tab-inbox-count')).toHaveText('1', { timeout: CONNECT_TIMEOUT })
    await expect(page.getByTestId('sidebar')).toBeHidden()
    await expect(page.getByTestId('menu-button')).toHaveCount(0)

    // 第一条默认展开，里面是完整的卡片
    await expect(rows()).toHaveCount(1)
    const main = rows().first().getByTestId('inbox-row-main')
    await expect(main).toHaveAttribute('aria-expanded', 'true')
    await expect(rows().first().getByTestId('permission-command')).toBeVisible()

    // 「会话」标签：原来抽屉里的东西；点进会话页，底栏收起；返回走浏览器历史，底栏回来
    await page.getByTestId('tab-sessions').click()
    await expect(page).toHaveURL(/\/sessions$/)
    await expect(page.getByTestId('tab-sessions')).toHaveAttribute('aria-current', 'page')
    await expect(page.getByTestId('node-switcher')).toBeVisible()
    await page
      .getByRole('link', { name: /run permission/i })
      .first()
      .click()
    await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/)
    await expect(page.getByTestId('tab-bar')).toBeHidden()
    await page.getByTestId('header-back').click()
    await expect(page).toHaveURL(/\/sessions$/)
    await expect(page.getByTestId('tab-bar')).toBeVisible()

    // 「更多」：节点的首页和设置、设置
    await page.getByTestId('tab-more').click()
    await expect(page).toHaveURL(/\/more$/)
    await expect(page.getByTestId('more-node')).toHaveCount(1)
    await expect(page.getByTestId('more-node-home')).toBeVisible()
    await expect(page.getByTestId('more-node-settings')).toBeVisible()
    await expect(page.getByTestId('more-settings')).toBeVisible()
    await page.getByTestId('more-node-home').click()
    await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/)
    await expect(page.getByTestId('tab-bar')).toBeHidden()
    await page.getByTestId('header-back').click()

    // 回「待处理」，在行内答复：条目消失，焦点落到「没有等你的事」
    await page.getByTestId('tab-inbox').click()
    await expect(page).toHaveURL(/\/inbox$/)
    await rows().first().getByTestId('permission-option-allow').click()
    await expect(page.getByTestId('inbox-empty')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('tab-inbox-count')).toHaveCount(0)
    await page.setViewportSize({ width: 1280, height: 720 })
  })
})
