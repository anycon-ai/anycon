/**
 * 让 agent 处理一项清单（docs/spec/web.md § 4.12、§ 4.11）：真实 hub + daemon（mock agent）+ web + 真实 git。
 *
 *   a. 详情里「让 agent 处理」：mock agent 声明了 `no_push`，「不 push、不开 PR」缺省勾着、没有说明；取消勾选之后
 *      写明 agent 能 push。展开第一条消息的预览，和 `anycon todo run <id> --allow-push --dry-run` 打印的逐字相同。发送之后这一项出现在「待验收」组
 *   b. 「待处理」里有「待验收」段（个数只在段标题上，不进导航数字）；验收页先给结论（mock agent 的回复）、改了 1 个文件（worktree 里的
 *      anycon-mock.txt），点开能看到它的 diff
 *   c. 「再改改…」发一条消息给那个会话：这一项的结果多一条（`activity` 由 daemon 随会话改回处理中、再变成待验收；mock 的一轮只有
 *      几十毫秒，所以这里等的是结果条数和最后的 `review`，不去抓中间那个瞬间）
 *   d. 「通过」：状态变成 done（CLI 对照 daemon），验收页写明已关闭，并出现「提交、推送并开 PR」（不点：真的推送要有远端）
 *
 * 共用一个 daemon：项目和标题都带运行标记，结束时删掉自己建的项，和它派出的会话（连 worktree 和分支，`anycon delete --remove-worktree --delete-branch`）。显式等待（data-testid / expect.poll），没有固定睡眠。
 * 派发的标题里带 `write`：mock agent 看到这个关键词会在 cwd 里写 `anycon-mock.txt`（crates/adapters/src/mock/README.md）。
 */
import { randomBytes } from 'node:crypto'

import { expect, test, type Page } from '@playwright/test'

import { runCli } from '../helpers/cli.js'
import { makeRepoProject } from '../helpers/git.js'
import { readState } from '../helpers/state.js'
import { CONNECT_TIMEOUT, TURN_TIMEOUT, pairNewBrowser, type PairedBrowser } from '../helpers/ui.js'

interface DaemonTodo {
  id: string
  title: string
  state: string
  activity?: string
  sessions?: string[]
}

const RUN = randomBytes(3).toString('hex')
const PROJECT = `e2e-dispatch-${RUN}`
const TITLE = `e2e-${RUN} dispatch write the file`
const state = () => readState()
/** 工作树和第一次检查之间的冷缓存：项目的 git 信息可能要几秒才到。 */
const READY_TIMEOUT = 45_000
/** 左栏「待处理」旁边的数字（没有就是 0）：只算「现在」段。 */
async function navInboxCount(page: Page): Promise<number> {
  const el = page.getByTestId('sidebar-inbox-count')
  return (await el.count()) === 0 ? 0 : Number((await el.textContent()) ?? '0')
}

async function daemonTodo(): Promise<DaemonTodo | undefined> {
  const out = await runCli(state(), ['todo', 'ls', '--all', '--json'])
  const todos = (JSON.parse(out.trim().split('\n').at(-1)!) as { todos: DaemonTodo[] }).todos
  return todos.find((x) => x.title === TITLE)
}

/** `anycon todo history <id> --json` 里 `result` 的条数。 */
async function resultCount(id: string): Promise<number> {
  const out = await runCli(state(), ['todo', 'history', id.slice(0, 8), '--json'])
  const events = (JSON.parse(out.trim().split('\n').at(-1)!) as { events: Array<{ kind: string }> })
    .events
  return events.filter((e) => e.kind === 'result').length
}

test.describe.serial('让 agent 处理', () => {
  let device: PairedBrowser
  let page: Page
  let todoId = ''
  let navBefore = 0

  test.beforeAll(async ({ browser }) => {
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    // 先建仓库项目、记下这一项，再配对浏览器：浏览器连上时拉的第一次清单和项目列表就已经有它们
    makeRepoProject(readState(), PROJECT)
    await runCli(state(), [
      'todo',
      'add',
      TITLE,
      '--project',
      PROJECT,
      '--body',
      'Write the file once.',
    ])
    todoId = (await daemonTodo())!.id
    device = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-dispatch' })
    page = device.page
  })

  test.afterAll(async () => {
    if (readState().daemonStarted) {
      const t = await daemonTodo()
      if (t) {
        // 先删派出的会话、它的 worktree 和分支，再删这一项（删了项就找不到会话了）；清不掉不挡住别的清理
        for (const sid of t.sessions ?? []) {
          await runCli(state(), [
            'delete',
            sid.slice(0, 12),
            '--remove-worktree',
            '--delete-branch',
            '--yes',
          ]).catch(() => undefined)
        }
        await runCli(state(), ['todo', 'rm', t.id.slice(0, 8)])
      }
    }
    await device?.context.close()
  })

  test('a. 取消「不 push」之后才能派发；预览和 CLI 的 --dry-run 一字不差；派发后进「待验收」组', async () => {
    await page.goto('/todos')
    const row = page.locator('[data-testid="todo-row"]').filter({ hasText: TITLE })
    await expect(row).toBeVisible({ timeout: CONNECT_TIMEOUT })
    navBefore = await navInboxCount(page)
    await row.getByTestId('todo-main').click()
    await expect(page.getByTestId('todo-detail')).toBeVisible()
    await page.getByTestId('todo-dispatch').click()
    await expect(page.getByTestId('dispatch-dialog')).toBeVisible()

    // 有别的 agent（比如本机装了 Claude Code）时选 mock
    if ((await page.getByTestId('dispatch-agent').count()) > 0)
      await page.getByTestId('dispatch-agent').selectOption('mock')
    // mock agent 声明了 `no_push`：缺省勾着，没有「这个 agent 挡不住 push」的说明；取消勾选之后写明会让 agent 能 push
    await expect(page.getByTestId('dispatch-no-push')).toBeChecked()
    await expect(page.getByTestId('dispatch-no-push-warning')).toHaveCount(0)
    await page.getByTestId('dispatch-no-push').uncheck()
    await expect(page.getByTestId('dispatch-push-allowed')).toBeVisible()
    // 项目的 git 信息读完之前不能发（不猜工作位置）
    await expect(page.getByTestId('dispatch-send')).toBeEnabled({ timeout: READY_TIMEOUT })
    await expect(page.getByTestId('dispatch-workspace-worktree')).toBeChecked()

    // 预览 = CLI 的 --dry-run（取消勾选对应 --allow-push）
    await page.getByTestId('dispatch-preview-toggle').click()
    await expect(page.getByTestId('dispatch-preview')).toBeVisible()
    const shown = await page.getByTestId('dispatch-preview').textContent()
    const dry = await runCli(state(), [
      'todo',
      'run',
      todoId.slice(0, 8),
      '--allow-push',
      '--dry-run',
    ])
    expect(dry.replace(/\n$/, '')).toBe(shown)

    await page.getByTestId('dispatch-send').click()
    await expect(page.getByTestId('dispatch-dialog')).toHaveCount(0)
    // 派发之后 mock 的一轮结束、结果记下来，daemon 算出 review
    await expect
      .poll(async () => (await daemonTodo())?.activity, { timeout: TURN_TIMEOUT })
      .toBe('review')
    expect((await daemonTodo())?.state).toBe('open') // 状态不动
    await page.goto('/todos')
    const reviewGroup = page.getByTestId('todos-group-review')
    await expect(reviewGroup).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(
      reviewGroup.locator('[data-testid="todo-row"]').filter({ hasText: TITLE }),
    ).toBeVisible()
  })

  test('b. 「待处理」里有「待验收」段，数字不进导航；验收页先给结论、再给 1 个改动文件的 diff', async () => {
    await page.goto('/inbox')
    const section = page.getByTestId('inbox-section-review')
    await expect(section).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(page.getByTestId('inbox-count-review')).toHaveText('1')
    // 待验收的个数只在段标题上：导航上的数字（只算「现在」段）没有因为它变多
    expect(await navInboxCount(page)).toBe(navBefore)
    await expect(section.getByTestId('inbox-review-brief')).toContainText('Mock reply', {
      timeout: 15_000,
    })
    // 不自动选中、不自动展开
    await expect(page.getByTestId('inbox-review-detail')).toHaveCount(0)

    await page.goto('/todos')
    await page
      .locator('[data-testid="todo-row"]')
      .filter({ hasText: TITLE })
      .getByTestId('todo-main')
      .click()
    await page.getByTestId('todo-review-link').click()
    await expect(page.getByTestId('todo-review')).toBeVisible()
    await expect(page.getByTestId('review-result')).toContainText('Mock reply')
    await expect(page.getByTestId('review-changes')).toContainText('1 file changed', {
      timeout: 15_000,
    })
    await expect(page.getByTestId('review-file')).toHaveCount(1)
    await expect(page.getByTestId('review-file')).toHaveAttribute('data-path', 'anycon-mock.txt')
    await expect(page.getByTestId('review-diff').locator('[data-testid="diff-file"]')).toBeVisible({
      timeout: 15_000,
    })
    // 三个动作固定在底部
    for (const id of ['review-approve', 'review-rework', 'review-drop'])
      await expect(page.getByTestId(id)).toBeVisible()
  })

  test('c. 「再改改…」发给那个会话：结果多一条，再回到待验收', async () => {
    await page.getByTestId('review-rework').click()
    await page.getByTestId('review-rework-input').fill('please write the file again')
    await page.getByTestId('review-rework-send').click()
    await expect.poll(async () => await resultCount(todoId), { timeout: TURN_TIMEOUT }).toBe(2)
    await expect
      .poll(async () => (await daemonTodo())?.activity, { timeout: TURN_TIMEOUT })
      .toBe('review')
    // 验收页的结论换成最新一轮
    await expect(page.getByTestId('review-result')).toContainText('please write the file again', {
      timeout: 15_000,
    })
    await expect(page.getByTestId('review-activity')).toHaveAttribute('data-state', 'review')
  })

  test('d. 「通过」：状态变成 done，出现「提交、推送并开 PR」', async () => {
    await page.getByTestId('review-approve').click()
    await expect.poll(async () => (await daemonTodo())?.state).toBe('done')
    await expect(page.getByTestId('review-closed')).toHaveAttribute('data-state', 'done')
    await expect(page.getByTestId('review-push')).toBeVisible()
    // 点它之前先要确认：这里只检查入口在
    await expect(page.getByTestId('review-approve')).toBeDisabled()
  })
})
