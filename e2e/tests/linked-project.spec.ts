/**
 * 关联本机已有目录为项目（ADR-33）：真实 hub + daemon（mock agent）+ web。
 *
 *   a. 新建项目 →「已有目录」→ 在选择器里输入路径 → 选当前目录 → 名称默认取目录名 → 关联成功进入项目页（已关联 + 路径）；
 *      在里面发「run permission」，权限卡片上的 cwd 就是那个目录（agent 真的在关联目录里跑）；越界路径只给行内错误；
 *      隐藏目录打开「显示隐藏目录」后才列出，且不能关联（`data-blocked="hidden"`）
 *   b. 从列表移除：项目消失，目录和里面的文件都还在
 *   c. 目录被删掉之后：项目页显示「目录不存在」，没有「新会话」按钮，可以移除
 *   d. 关联之后把目录换成指向 daemon 数据目录的符号链接（ADR-33 第 5 条）：项目变成「目录不存在」，不能在里面开会话
 *   e. turn 停在权限请求上时把目录换掉，再点允许：请求按拒绝处理、turn 中止，工具没有执行
 *
 * global-setup 把 `workspace.link_roots` 设成 `<tmpDir>/external` 的 canonical 路径（默认的 `~` 不包含系统临时目录；
 * 系统临时目录在拒绝表的 `/private` 里，要显式写进 `link_roots` 才能关联；`/var/…` 是它的别名，两种写法都行）。
 * 约定：显式等待（data-testid），没有固定睡眠。每个用例自己配对自己的浏览器。
 */
import {
  existsSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs'
import { join } from 'node:path'

import { expect, test, type Page } from '@playwright/test'

import { readState } from '../helpers/state.js'
import {
  TURN_TIMEOUT,
  openNewSessionFromProject,
  pairNewBrowser,
  pendingCard,
} from '../helpers/ui.js'

test.beforeEach(() => {
  test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
})

/** `link_roots` 里唯一的那个目录（global-setup 建好的）。 */
function externalRoot(): string {
  return join(readState().tmpDir, 'external')
}

/** 在 `link_roots` 里建一个带文件的目录，返回 canonical 路径（macOS 的 /var → /private/var）。 */
function makeFolder(name: string): string {
  const dir = join(externalRoot(), name)
  mkdirSync(dir, { recursive: true })
  writeFileSync(join(dir, 'README.md'), `# ${name}\n`)
  return realpathSync(dir)
}

/** 从节点首页打开「新建项目 → 已有目录」，输入路径、选当前目录、提交；等跳到项目页。 */
async function linkViaPicker(page: Page, dir: string, name: string): Promise<void> {
  await page.getByTestId('project-new').click()
  await expect(page.getByTestId('project-new-dialog')).toBeVisible()
  await page.getByTestId('project-new-mode-existing').click()
  await expect(page.getByTestId('folder-picker')).toBeVisible({ timeout: 15_000 })

  await page.getByTestId('folder-path').fill(dir)
  await page.getByTestId('folder-path').press('Enter')
  await expect(page.getByTestId('folder-current')).toHaveAttribute('data-path', dir)
  await expect(page.getByTestId('folder-current')).toHaveAttribute('data-linkable', 'true')
  await page.getByTestId('folder-select-current').click()

  // 名称默认取目录名
  await expect(page.getByTestId('project-link-name')).toHaveValue(name)
  await page.getByTestId('project-new-submit').click()
  await expect(page).toHaveURL(new RegExp(`/p/${name}$`), { timeout: 15_000 })
  await expect(page.getByTestId('project-linked').first()).toBeVisible()
  await expect(page.getByTestId('project-path').first()).toContainText(dir)
}

/** 把 `mv dir dir.real && ln -s target dir` 撤回来；换目录之前就失败时什么都不做（不盖掉真正的失败原因）。 */
function restoreSwapped(dir: string): void {
  if (!existsSync(`${dir}.real`)) return
  rmSync(dir, { force: true })
  renameSync(`${dir}.real`, dir)
}

async function removeFromList(page: Page): Promise<void> {
  await page.getByTestId('project-menu').click()
  await page.getByTestId('project-remove').click()
  await expect(page.getByTestId('project-remove-dialog')).toBeVisible()
  await page.getByTestId('project-remove-confirm').click()
  await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: 15_000 })
}

test.describe('关联已有目录', () => {
  test('a. 选择器关联目录 → 项目页标「已关联」→ agent 的 cwd 就是那个目录；越界路径只有行内错误', async ({
    browser,
  }) => {
    const state = readState()
    const dir = makeFolder('existing-app')
    // 隐藏目录（ADR-33 第 2 条）：列出来但不能关联
    makeFolder('.hidden-app')
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-linked-a' })
    const { page } = paired
    try {
      // 越界：link_roots 之外的路径 → 行内错误，选择器还在原来的目录
      await page.getByTestId('project-new').click()
      await page.getByTestId('project-new-mode-existing').click()
      await expect(page.getByTestId('folder-current')).toHaveAttribute(
        'data-path',
        realpathSync(externalRoot()),
        { timeout: 15_000 },
      )
      await page.getByTestId('folder-path').fill('/')
      await page.getByTestId('folder-path').press('Enter')
      await expect(page.getByTestId('folder-error')).toBeVisible()
      await expect(page.getByTestId('folder-current')).toHaveAttribute(
        'data-path',
        realpathSync(externalRoot()),
      )
      // 子目录出现在列表里
      await expect(
        page.locator('[data-testid="folder-entry"][data-name="existing-app"]'),
      ).toBeVisible()
      // 默认不列隐藏目录；打开「显示隐藏目录」后列出来，但不能关联，原因是 hidden
      const hidden = page.locator('[data-testid="folder-entry"][data-name=".hidden-app"]')
      await expect(hidden).toHaveCount(0)
      await page.locator('#folder-hidden').click()
      await expect(hidden).toBeVisible()
      await expect(hidden).toHaveAttribute('data-linkable', 'false')
      await expect(hidden).toHaveAttribute('data-blocked', 'hidden')
      await expect(hidden.getByTestId('folder-select')).toHaveCount(0)
      await page.keyboard.press('Escape')
      await expect(page.getByTestId('project-new-dialog')).toHaveCount(0)

      await linkViaPicker(page, dir, 'existing-app')

      // 在关联项目里开会话（项目页头部「新会话」→ 新会话页）：mock 的权限请求带着 spawn cwd
      await openNewSessionFromProject(page, 'existing-app')
      await page.getByTestId('composer-input').fill('run permission')
      await page.getByTestId('composer-send').click()
      await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
      const card = pendingCard(page)
      await expect(card).toBeVisible({ timeout: TURN_TIMEOUT })
      await expect(card).toContainText(dir)
      await card.getByTestId('permission-option-allow').click()
      await expect(page.getByTestId('session-status')).toHaveAttribute('data-status', 'idle', {
        timeout: TURN_TIMEOUT,
      })
    } finally {
      await paired.context.close()
    }
  })

  test('b. 从列表移除：项目消失，目录和文件都还在', async ({ browser }) => {
    const state = readState()
    const dir = makeFolder('keep-my-files')
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-linked-b' })
    const { page } = paired
    try {
      await linkViaPicker(page, dir, 'keep-my-files')
      await removeFromList(page)
      await expect(
        page.getByTestId('project-card').filter({ hasText: 'keep-my-files' }),
      ).toHaveCount(0)
      expect(existsSync(dir)).toBe(true)
      expect(readFileSync(join(dir, 'README.md'), 'utf8')).toBe('# keep-my-files\n')
    } finally {
      await paired.context.close()
    }
  })

  test('c. 目录被删掉：项目页显示「目录不存在」，不能新建会话，可以移除', async ({ browser }) => {
    const state = readState()
    const dir = makeFolder('goes-away')
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-linked-c' })
    const { page } = paired
    try {
      await linkViaPicker(page, dir, 'goes-away')
      const projectUrl = new URL(page.url()).pathname
      rmSync(dir, { recursive: true, force: true })

      await page.goto(projectUrl)
      await expect(page.getByTestId('project-missing-notice')).toBeVisible({ timeout: 15_000 })
      await expect(page.getByTestId('project-new-session')).toHaveCount(0)
      await expect(page.getByTestId('composer-input')).toHaveCount(0)

      await page.getByTestId('project-missing-remove').click()
      await expect(page.getByTestId('project-remove-dialog')).toBeVisible()
      await page.getByTestId('project-remove-confirm').click()
      await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: 15_000 })
      await expect(page.getByTestId('project-card').filter({ hasText: 'goes-away' })).toHaveCount(0)
    } finally {
      await paired.context.close()
    }
  })

  test('d. 关联后目录被换成指向数据目录的符号链接：项目变成「目录不存在」，不能开会话', async ({
    browser,
  }) => {
    const state = readState()
    const dir = makeFolder('swapped-later')
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-linked-d' })
    const { page } = paired
    try {
      await linkViaPicker(page, dir, 'swapped-later')
      const projectUrl = new URL(page.url()).pathname
      // 复现步骤：mv dir dir.real && ln -s <ANYCON_HOME> dir
      renameSync(dir, `${dir}.real`)
      symlinkSync(state.daemonHome, dir)

      await page.goto(projectUrl)
      await expect(page.getByTestId('project-missing-notice')).toBeVisible({ timeout: 15_000 })
      await expect(page.getByTestId('project-new-session')).toHaveCount(0)
      await expect(page.getByTestId('composer-input')).toHaveCount(0)
    } finally {
      await paired.context.close()
      restoreSwapped(dir)
    }
  })

  test('e. 权限请求挂着时目录被换掉，再点允许：按拒绝处理，工具不执行', async ({ browser }) => {
    const state = readState()
    const dir = makeFolder('swap-mid-turn')
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-linked-e' })
    const { page } = paired
    try {
      await linkViaPicker(page, dir, 'swap-mid-turn')
      await openNewSessionFromProject(page, 'swap-mid-turn')
      await page.getByTestId('composer-input').fill('run permission')
      await page.getByTestId('composer-send').click()
      await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
      const card = pendingCard(page)
      await expect(card).toBeVisible({ timeout: TURN_TIMEOUT })

      renameSync(dir, `${dir}.real`)
      symlinkSync(state.daemonHome, dir)
      // 换目录和点击之间恰好碰上 daemon 的周期检查（约每 2 秒）时，卡片会先被 daemon 收掉：
      // 点击只等一小会儿，两条路径（放行前复核 / 周期检查）都接受
      await card
        .getByTestId('permission-option-allow')
        .click({ timeout: 3_000 })
        .catch(() => undefined)

      // turn 以 not_found 收尾
      await expect(page.locator('[data-testid="error-block"][data-code="not_found"]')).toBeVisible({
        timeout: TURN_TIMEOUT,
      })
      // 答复的这台设备显示「因为项目目录变了被拒绝」，或者看到 daemon 自动拒绝的那一行
      await expect(
        page
          .locator(
            '[data-testid="permission-settled"], [data-testid="permission-card"][data-by="daemon"]',
          )
          .first(),
      ).toBeVisible()
      await expect(pendingCard(page)).toHaveCount(0)
      // turn 已经结束：mock 只有在允许之后才会发 tool_call
      await expect(page.getByTestId('tool-block')).toHaveCount(0)
    } finally {
      await paired.context.close()
      restoreSwapped(dir)
    }
  })
})
