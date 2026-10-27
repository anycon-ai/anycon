/**
 * 后台任务 / 提示 / 临时状态 / 权限里的链接 / 上下文占用（docs/spec/web.md § 4.6）：真实 hub + daemon（mock agent）+ web。
 *
 *   a. `show the tasks`：model_fallback 提示、Workflow 工具行下面的任务进度（经过 daemon 只剩第 1 条和终态快照）、
 *      展开后按阶段分组的三个 agent、信息面板的上下文占用
 *   b. `slow tasks`：三条快照都能看到——任务「运行中」时 Workflow 工具行是「后台运行」、输入框上方有「N 个后台任务在运行」的 chip，点开列出；
 *      「正在重试（2/10）…」的临时状态出现过；结束后工具行是「已完成」、chip 消失
 *   c. `open a link`：带完整 URL 和「打开」按钮（新标签页、noopener）的确认卡片；不点开链接（测试不碰网络），点确认答复
 *
 * mock agent 的行为见 crates/adapters/src/mock/README.md（`tasks`、`link`、`slow`）；prompt 里别带 `run` / `plan` 这类别的关键词。
 * 约定：显式等待（data-testid），没有固定睡眠。UI 文案中英都可能出现，断言用正则兼容。
 */
import { expect, test, type Page } from '@playwright/test'

import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  TURN_TIMEOUT,
  nodeBase,
  pairNewBrowser,
  pendingCard,
  resolvedCards,
  type PairedBrowser,
} from '../helpers/ui.js'

test.describe.serial('后台任务 / 提示 / 链接', () => {
  let device: PairedBrowser
  let page: Page

  test.beforeAll(async ({ browser }) => {
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    device = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-tasks' })
    page = device.page
  })

  test.afterAll(async () => {
    await device?.context.close()
  })

  /** 新建 scratch 会话并发出第一条消息；返回会话页。 */
  async function startSession(prompt: string): Promise<void> {
    await page.goto(`${nodeBase(page)}/new`)
    await expect(page.getByTestId('composer-input')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('composer-input').fill(prompt)
    await page.getByTestId('composer-send').click()
    await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
  }

  async function waitIdle(): Promise<void> {
    await expect(page.getByTestId('session-status')).toHaveAttribute('data-status', 'idle', {
      timeout: TURN_TIMEOUT,
    })
  }

  test('a. show the tasks：提示、任务进度、上下文占用', async () => {
    test.setTimeout(120_000)
    await startSession('show the tasks')
    await waitIdle()

    // 模型被换了：一行提示（不是错误），点开有 agent 的原文
    const notice = page.getByTestId('notice')
    await expect(notice).toHaveCount(1)
    await expect(notice).toHaveAttribute('data-code', 'model_fallback')
    await expect(notice).toHaveAttribute('data-level', 'warning')
    await expect(notice).toContainText(
      /Model switched from .*Mock Large.* to .*Mock Small|模型已从/i,
    )
    await expect(page.getByTestId('error-block')).toHaveCount(0)
    await notice.getByRole('button').click()
    await expect(page.getByTestId('notice-detail')).toContainText(
      'Model mock-large is overloaded; switched to mock-small.',
    )

    // 任务进度挂在 Workflow 工具行下面：经过 daemon 只有第 1 条和终态快照，最后停在「已完成」
    const tool = page.getByTestId('tool-block').filter({ hasText: 'Workflow' })
    await expect(tool).toHaveCount(1)
    const task = page.getByTestId('task')
    await expect(task).toHaveCount(1)
    await expect(task).toHaveAttribute('data-status', 'completed')
    await expect(task).toHaveAttribute('data-kind', 'workflow')
    await expect(task.getByTestId('task-title')).toHaveText('review')
    await expect(task.getByTestId('task-meta')).toContainText('3/3')
    await expect(task.getByTestId('task-meta')).toContainText('1.5k')
    await expect(task.getByTestId('task-summary')).toContainText('Review finished: no issues found')
    // 它画在工具行下面，不是单独成块：在同一个消息行里，紧跟着工具行
    const [toolBox, taskBox] = await Promise.all([tool.boundingBox(), task.boundingBox()])
    expect(toolBox && taskBox && taskBox.y >= toolBox.y + toolBox.height - 1).toBe(true)
    await expect(tool.locator('xpath=..').getByTestId('task')).toHaveCount(1)

    // 完成的任务收成一行；点开是按阶段分组的三个 agent
    await expect(page.getByTestId('task-agents')).toHaveCount(0)
    await task.getByRole('button').click()
    const phases = page.getByTestId('task-phase')
    await expect(phases).toHaveCount(2)
    await expect(phases.nth(0)).toContainText('Scan')
    await expect(phases.nth(1)).toContainText('Review')
    const agents = page.getByTestId('task-agent')
    await expect(agents).toHaveCount(3)
    for (let i = 0; i < 3; i++) await expect(agents.nth(i)).toHaveAttribute('data-state', 'done')
    await expect(agents.nth(0)).toContainText('scanner')
    await expect(agents.nth(2)).toContainText('reviewer-b')
    await expect(agents.nth(2).getByTestId('task-agent-attempt')).toContainText(/2/)

    // 任务已经完成：没有「N 个后台任务在运行」
    await expect(page.getByTestId('background-tasks-chip')).toHaveCount(0)

    // 信息面板：上下文占用（mock：12000 + 输入 + 输出 token，窗口 200000）
    await expect(page.getByTestId('info-context')).toBeVisible()
    await expect(page.getByTestId('info-context-value')).toContainText('6% · 12k / 200k')
  })

  test('b. slow tasks：运行中的任务、后台任务 chip、重试的临时状态', async () => {
    test.setTimeout(120_000)
    await page.goto(`${nodeBase(page)}/new`)
    await expect(page.getByTestId('composer-input')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    // 临时状态只存在 200 ms：在页面里记下它出现过的文字（先于发送装好）
    await page.evaluate(() => {
      const seen: string[] = []
      const w = window as unknown as { activitySeen: string[] }
      w.activitySeen = seen
      new MutationObserver(() => {
        const text = document.querySelector('[data-testid="activity"]')?.textContent
        if (text && !seen.includes(text)) seen.push(text)
      }).observe(document.body, { subtree: true, childList: true, characterData: true })
    })
    await page.getByTestId('composer-input').fill('slow tasks')
    await page.getByTestId('composer-send').click()
    await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })

    // 任务「运行中」大约两三秒（三条快照之间隔 1.3 秒）：这段时间里 chip 在、列表里有它
    const task = page.getByTestId('task')
    await expect(task).toHaveAttribute('data-status', 'running', { timeout: TURN_TIMEOUT })
    const chip = page.getByTestId('background-tasks-chip')
    await expect(chip).toBeVisible()
    await expect(chip).toHaveAttribute('data-count', '1')
    await expect(chip).toContainText(/1 background task running|1 个后台任务在运行/)
    // 跑着的任务默认展开
    await expect(page.getByTestId('task-agent').first()).toBeVisible()
    // 工具行的状态跟着任务走：任务在跑就是「后台运行」，任务完成后是「已完成」
    const tool = page.getByTestId('tool-block').filter({ hasText: 'Workflow' })
    await expect(tool).toHaveAttribute('data-state', 'background')
    // 点开 chip：列出在跑的任务
    await chip.click()
    await expect(page.getByTestId('background-task')).toContainText('review')
    await page.keyboard.press('Escape')

    // 结束：任务完成、chip 消失、会话回到空闲
    await expect(task).toHaveAttribute('data-status', 'completed', { timeout: TURN_TIMEOUT })
    await expect(tool).toHaveAttribute('data-state', 'done')
    await expect(chip).toHaveCount(0)
    await waitIdle()
    await expect(page.getByTestId('activity')).toHaveCount(0)

    const seen = await page.evaluate(
      () => (window as unknown as { activitySeen: string[] }).activitySeen,
    )
    expect(seen.join('\n')).toMatch(/Retrying \(2\/10\)|正在重试（2\/10）/)
  })

  test('c. open a link：完整的 URL 和「打开」按钮，确认后继续', async () => {
    test.setTimeout(120_000)
    await startSession('open a link')
    const card = pendingCard(page)
    await expect(card).toBeVisible({ timeout: TURN_TIMEOUT })
    await expect(card).toHaveAttribute('data-kind', 'other')
    await expect(card).toContainText('The mock-auth server needs you to sign in')
    await expect(card.getByTestId('permission-link-url')).toHaveText(
      'https://example.com/mock-auth',
    )
    // 链接在新标签页打开，且不带 opener；测试不点它（不碰网络）
    const open = card.getByTestId('permission-link-open')
    await expect(open).toHaveAttribute('href', 'https://example.com/mock-auth')
    await expect(open).toHaveAttribute('target', '_blank')
    await expect(open).toHaveAttribute('rel', 'noopener noreferrer')
    await expect(page.getByTestId('permission-link-blocked')).toHaveCount(0)

    // 确认（"I've completed it"）→ agent 继续
    await card.getByTestId('permission-option-accept').click()
    await expect(resolvedCards(page)).toHaveCount(1)
    await expect(page.getByTestId('message-assistant').last()).toContainText('Link confirmed.', {
      timeout: TURN_TIMEOUT,
    })
    await waitIdle()
  })
})
