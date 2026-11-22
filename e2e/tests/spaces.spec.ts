/**
 * 空间（docs/spec/web.md § 4.13）：真实 hub + daemon（只有一个节点）+ web。
 *
 *   a. 没有分组：和原来一样的节点过滤器（菜单里没有空间、没有「管理空间…」）
 *   b. 在节点设置 →「限制」里给这个节点设分组：切换器马上有了空间（`node.info` 被刷新），选中它之后按钮上是空间名
 *   c. 「管理空间…」里给空间改名：节点在线，改成功；切换器跟着变
 *   d. 清掉分组：回到只有节点过滤器的样子，偏好里存的空间不再生效
 *
 * 多个节点之间的行为（合并、移动、每个空间各自的缺省节点、离线节点改名）需要第二个 daemon，e2e 环境里只有一个，
 * 它们由单元测试覆盖（`SpaceSwitcher.test.tsx`、`ManageSpacesDialog.test.tsx`、`features/todos/move.test.tsx`），这里不假装。
 * 共用一个 daemon：用例结束时把分组清掉。
 */
import { expect, test, type Page } from '@playwright/test'

import { runCli } from '../helpers/cli.js'
import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  expectNodeOnline,
  nodeBase,
  pairNewBrowser,
  type PairedBrowser,
} from '../helpers/ui.js'

const SPACE = 'e2e-space'
const RENAMED = 'e2e-renamed'

test.describe.serial('空间', () => {
  let device: PairedBrowser
  let page: Page
  let base = ''

  test.beforeAll(async ({ browser }) => {
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    device = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-spaces' })
    page = device.page
    base = nodeBase(page)
  })

  test.afterAll(async () => {
    if (readState().daemonStarted) await runCli(readState(), ['config', 'set', 'hub.group', ''])
    await device?.context.close()
  })

  const openSwitcher = async (): Promise<void> => {
    await page.getByTestId('node-switcher').click()
    await expect(page.getByTestId('node-filter-item').first()).toBeVisible()
  }

  test('a. 没有分组：就是原来的节点过滤器', async () => {
    await page.goto('/inbox')
    await expect(page.getByTestId('node-switcher')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await openSwitcher()
    await expect(page.getByTestId('space-item')).toHaveCount(0)
    await expect(page.getByTestId('space-manage')).toHaveCount(0)
    await page.keyboard.press('Escape')
  })

  test('b. 在节点设置里设分组：切换器里出现这个空间，选中后按钮上是空间名', async () => {
    await page.goto(`${base}/settings?tab=limits`)
    const group = page.getByTestId('limit-hub.group')
    await expect(group).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await group.fill(SPACE)
    await page.getByTestId('limits-save').click()
    await expect(page.getByTestId('limits-save')).toBeDisabled()
    await page.goto('/inbox')
    await openSwitcher()
    await expect(page.getByTestId('space-item')).toHaveCount(1, { timeout: CONNECT_TIMEOUT })
    await expect(page.getByTestId('space-item')).toContainText(SPACE)
    await page.getByTestId('space-item').click()
    await expect(page.getByTestId('space-label')).toHaveText(SPACE)
    // 选中的空间记在这台设备上：刷新之后还在
    await page.reload()
    await expect(page.getByTestId('space-label')).toHaveText(SPACE, { timeout: CONNECT_TIMEOUT })
  })

  test('c. 管理空间：给空间改名（节点在线）', async () => {
    // 空间名可以来自缓存；节点设置页的在线 chip 表示 Noise 连接已经就绪。
    await page.goto(`${base}/settings?tab=limits`)
    await expectNodeOnline(page)
    await openSwitcher()
    await page.getByTestId('space-manage').click()
    const dialog = page.getByTestId('manage-spaces')
    await expect(dialog).toBeVisible()
    await dialog.getByTestId('rename-space').selectOption(SPACE)
    await dialog.getByTestId('rename-to').fill(RENAMED)
    await dialog.getByTestId('rename-apply').click()
    await expect(dialog.getByTestId('rename-results').locator('li')).toHaveAttribute(
      'data-status',
      'changed',
    )
    await page.keyboard.press('Escape')
    // 选中的正是这个空间：选择跟着改名走，按钮上显示新名字
    await expect(page.getByTestId('space-label')).toHaveText(RENAMED)
    await openSwitcher()
    await expect(page.getByTestId('space-item')).toContainText(RENAMED)
    await expect(page.getByTestId('space-item')).not.toContainText(SPACE)
    await page.keyboard.press('Escape')
  })

  test('d. 清掉分组：回到只有节点过滤器', async () => {
    await page.goto(`${base}/settings?tab=limits`)
    const group = page.getByTestId('limit-hub.group')
    await expect(group).toHaveValue(RENAMED, { timeout: CONNECT_TIMEOUT })
    await group.fill('')
    await page.getByTestId('limits-save').click()
    await expect(page.getByTestId('limits-save')).toBeDisabled()
    await page.goto('/inbox')
    await openSwitcher()
    await expect(page.getByTestId('space-item')).toHaveCount(0, { timeout: CONNECT_TIMEOUT })
    await expect(page.getByTestId('space-manage')).toHaveCount(0)
  })
})
