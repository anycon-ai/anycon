/**
 * 清单 `/todos`（docs/spec/web.md § 4.12、§ 4.12.1）：真实 hub + daemon + web。
 *
 *   a. 桌面输入条（侧栏「＋」）记一项，出现在清单页；第二个浏览器上下文经 `todo.changed` 看到
 *   b. 手机宽度：抽屉里「清单」旁边的「＋」打开底部弹层，记一项
 *   c. 编辑标题、完成、重新打开、删除（二次确认），每一步用 `anycon todo ls --json` 对照 daemon
 *   d. agent 用 `ANYCON_AGENT=1 anycon todo add` 记的项出现在待处理的「稍后」段，不进清单页和导航数字；收下之后进清单
 *   e. 离线：掐掉页面到 hub 的连接（节点对页面不可达），记一项、改一项，恢复后重放，daemon 里的结果和界面一致
 *
 * 共用一个 daemon：每个用例用带运行标记的标题，结束时删掉自己建的项。显式等待（data-testid），没有固定睡眠。
 */
import { expect, test, type Page } from '@playwright/test'

import { runCli } from '../helpers/cli.js'
import { readState } from '../helpers/state.js'
import { CONNECT_TIMEOUT, pairNewBrowser, type PairedBrowser } from '../helpers/ui.js'

interface DaemonTodo {
  id: string
  title: string
  body: string
  state: string
  unreviewed: boolean
  source: { kind: string }
}

const RUN = Date.now().toString(36)
const t = (name: string): string => `e2e-${RUN}-${name}`
const state = () => readState()
const row = (p: Page, title: string) =>
  p.locator('[data-testid="todo-row"]').filter({ hasText: title })

/** daemon 里现在的全部项（含已关闭）。 */
async function daemonTodos(): Promise<DaemonTodo[]> {
  const out = await runCli(state(), ['todo', 'ls', '--all', '--json'])
  return (JSON.parse(out.trim().split('\n').at(-1)!) as { todos: DaemonTodo[] }).todos
}
async function daemonTodo(title: string): Promise<DaemonTodo | undefined> {
  return (await daemonTodos()).find((x) => x.title === title)
}

test.describe.serial('清单', () => {
  let device: PairedBrowser
  let second: PairedBrowser
  let page: Page
  test.beforeAll(async ({ browser }) => {
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    device = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-todos' })
    page = device.page
    second = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-todos-2' })
  })

  // b 把视口缩成手机大小：不管它失败在哪一步，后面的用例都回到桌面宽度
  test.afterEach(async () => {
    await page?.setViewportSize({ width: 1280, height: 800 })
  })

  test.afterAll(async () => {
    if (readState().daemonStarted) {
      for (const x of await daemonTodos()) {
        if (x.title.startsWith(`e2e-${RUN}-`))
          await runCli(state(), ['todo', 'rm', x.id.slice(0, 8)])
      }
    }
    await second?.context.close()
    await device?.context.close()
  })

  test('a. 桌面输入条记一项；另一个浏览器经 todo.changed 看到', async () => {
    await page.goto('/todos')
    await expect(page.getByTestId('todos-page')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await second.page.goto('/todos')
    await expect(second.page.getByTestId('todos-page')).toBeVisible({ timeout: CONNECT_TIMEOUT })

    await page.getByTestId('sidebar-capture').click()
    await expect(page.getByTestId('capture-bar')).toBeVisible()
    await page.getByTestId('capture-input').fill(t('bar'))
    await page.getByTestId('capture-send').click()
    // 发送之后输入条留着、清空
    await expect(page.getByTestId('capture-input')).toHaveValue('')
    await expect(row(page, t('bar'))).toBeVisible()
    await expect(row(second.page, t('bar'))).toBeVisible({ timeout: 15_000 })
    expect((await daemonTodo(t('bar')))?.state).toBe('open')
    await page.getByTestId('capture-input').press('Escape')
    await expect(page.getByTestId('capture-bar')).toHaveCount(0)
  })

  test('b. 手机宽度：抽屉里的「＋」打开底部弹层', async () => {
    await page.setViewportSize({ width: 390, height: 844 })
    await page.goto('/todos')
    await page.getByTestId('menu-button').filter({ visible: true }).click()
    const capture = page.getByTestId('sidebar').getByTestId('sidebar-capture')
    await expect(capture).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await capture.click()
    // 抽屉收起，弹层打开
    await expect(page.getByTestId('sidebar')).toBeHidden()
    await expect(page.getByTestId('capture-sheet')).toBeVisible()
    await page.getByTestId('capture-input').fill(`${t('sheet')}\nsecond line is the body`)
    await page.getByTestId('capture-send').click()
    await expect(page.getByTestId('capture-input')).toHaveValue('')
    await expect
      .poll(async () => (await daemonTodo(t('sheet')))?.body)
      .toBe('second line is the body')
    await page.keyboard.press('Escape')
    await expect(page.getByTestId('capture-sheet')).toHaveCount(0)
    await expect(row(page, t('sheet'))).toBeVisible()
    await page.setViewportSize({ width: 1280, height: 800 })
  })

  test('c. 编辑、完成、重新打开、删除', async () => {
    await page.goto('/todos')
    await row(page, t('bar')).getByTestId('todo-main').click()
    await expect(page.getByTestId('todo-detail')).toBeVisible()
    await page.getByTestId('todo-edit').click()
    await page.getByTestId('todo-edit-title').fill(t('edited'))
    await page.getByTestId('todo-save').click()
    await expect.poll(async () => (await daemonTodo(t('edited')))?.state).toBe('open')

    await page.getByTestId('todo-done').click()
    await expect.poll(async () => (await daemonTodo(t('edited')))?.state).toBe('done')
    await expect(page.getByTestId('todo-reopen')).toBeVisible()
    await page.getByTestId('todo-reopen').click()
    await expect.poll(async () => (await daemonTodo(t('edited')))?.state).toBe('open')

    await page.getByTestId('todo-delete').click()
    await page.getByTestId('todo-delete-confirm').click()
    await expect.poll(async () => await daemonTodo(t('edited'))).toBeUndefined()
  })

  test('d. agent 记的项在「稍后」里，收下后进清单', async () => {
    await runCli(state(), ['todo', 'add', t('agent')], { env: { ANYCON_AGENT: '1' } })
    expect((await daemonTodo(t('agent')))?.unreviewed).toBe(true)

    // 先等清单真的读到了（有一项已知的行、导航上有数字）再取基线：不然「没有」可能只是还没加载
    await page.goto('/todos')
    await expect(row(page, t('sheet'))).toBeVisible({ timeout: CONNECT_TIMEOUT })
    const count = page.getByTestId('sidebar-todos-count')
    await expect(count).toHaveText(/^\d+$/)
    const navBefore = await count.textContent()
    expect(Number(navBefore)).toBeGreaterThan(0)
    await page.goto('/inbox')
    const later = page.getByTestId('inbox-section-later')
    await expect(later).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(
      later.locator('[data-testid="inbox-row"]').filter({ hasText: t('agent') }),
    ).toBeVisible()
    // 不进导航数字（清单读到了，数字还是原来的）
    await expect(count).toHaveText(/^\d+$/)
    expect(await count.textContent()).toBe(navBefore)
    await page.goto('/todos')
    await expect(row(page, t('sheet'))).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(row(page, t('agent'))).toHaveCount(0)

    await page.goto('/inbox')
    await later
      .locator('[data-testid="inbox-row"]')
      .filter({ hasText: t('agent') })
      .getByTestId('inbox-accept')
      .click()
    await expect.poll(async () => (await daemonTodo(t('agent')))?.unreviewed).toBe(false)
    await page.goto('/todos')
    await expect(row(page, t('agent'))).toBeVisible()
  })

  test('e. 离线：记一项、改一项，恢复后重放', async () => {
    // 浏览器断网（`setOffline`）不会断掉已经连上的 WebSocket；用 `routeWebSocket` 把页面到 hub 的连接掐掉、并拒绝重连，节点对页面就是不可达
    let blocked = false
    const sockets: Array<{ close: () => void }> = []
    await page.routeWebSocket(/.*/, (ws) => {
      if (blocked) {
        void ws.close()
        return
      }
      ws.connectToServer()
      sockets.push(ws)
    })
    await page.goto('/todos')
    await expect(row(page, t('sheet'))).toBeVisible({ timeout: CONNECT_TIMEOUT })
    blocked = true
    for (const ws of sockets) void ws.close()
    await expect(page.getByTestId('todos-cached')).toBeVisible({ timeout: 30_000 })
    await page.getByTestId('sidebar-capture').click()
    await page.getByTestId('capture-input').fill(t('offline'))
    await page.getByTestId('capture-send').click()
    await expect(row(page, t('offline'))).toBeVisible()
    await row(page, t('sheet')).getByTestId('todo-main').click()
    await page.getByTestId('todo-done').click()
    // 界面标明没同步；离线期间 daemon 里什么都没变
    await expect(row(page, t('offline')).getByTestId('todo-unsynced')).toBeVisible({
      timeout: 30_000,
    })
    expect(await daemonTodo(t('offline'))).toBeUndefined()
    expect((await daemonTodo(t('sheet')))?.state).toBe('open')

    blocked = false
    await expect
      .poll(async () => (await daemonTodo(t('offline')))?.state, { timeout: 60_000 })
      .toBe('open')
    await expect
      .poll(async () => (await daemonTodo(t('sheet')))?.state, { timeout: 60_000 })
      .toBe('done')
    await expect(page.getByTestId('todo-unsynced')).toHaveCount(0, { timeout: 30_000 })
  })
})
