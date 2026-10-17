/**
 * 每会话 worktree（ADR-34）：真实 hub + daemon（mock agent）+ web + 真实 git。
 *
 *   a. 主流程：项目里有一个带提交的仓库（`.gitignore` + `.worktreeinclude` + 被忽略的 `.env`）→ 新建会话选「新 worktree」→
 *      发 `write` → 分支 chip 变成 active、`.env` 被复制进 worktree、主目录里没有 `anycon-mock.txt` →「改动」标签里有它和它的 diff →
 *      worktree 里提交、主目录的 base 分支再提交一次 →「同步 base」合并 1 个提交（消息流里有 `git` 事件）→
 *      再 `write`（留下未提交改动）→ 归档（自动）→ worktree 因为有未提交改动被保留 → 节点设置的 worktree 列表是「已保留」→
 *      勾「丢弃未提交的改动」删除 → 目录不在了，分支还在
 *   b. 「当前目录」模式：不建 worktree，「改动」标签只有「未提交」视图，`anycon-mock.txt` 是未跟踪文件
 *   c. 非 git 项目没有「工作位置」一行；仓库还没有提交（`worktree_ready = false`）时这一行禁用并说明，会话照常在项目目录里工作
 *   d. 选 base 和自定义分支名：不合法的分支名挡住发送，base 选中的分支决定 worktree 的内容，已存在的分支名创建失败
 *   e. 干净的 worktree 自动归档被删掉（分支有自己的提交所以保留）→ 取消归档按分支重建，`.worktreeinclude` 的文件复制回来，agent 照常工作
 *   f. 同步 base 遇到冲突：冲突文件和「让 agent 解决 / 放弃合并」、快捷消息发给 agent、放弃合并后回到合并之前
 *   g. 删除会话：选「删除 worktree」并勾「同时删除分支」（提示会丢提交）→ 目录和分支都没了，设置里没有这一行
 *   h. 删除会话时保留 worktree：它留在节点设置里（没有会话），在那里删除并同时删分支，行也没了
 *   i. worktree 目录被删：会话标「不见了」，下一条消息按分支重建
 *   j. worktree 目录被换成符号链接：不重建、不跟随链接，发消息被拒绝（`not_found`）
 *   k. 冷缓存 + git 元数据算得慢（git 包装脚本把 daemon 的 git 拖慢）：先是占位 `new-session-workspace-pending`（发送被挡），
 *      算完收到 `workspace.changed { updated }`，「工作位置」按钮自己出现（表单没有重挂载），会话建在 worktree 里
 *   k2. 同样的慢 git（约 9 秒，比前台 3 秒 + 补拉 3 秒还慢）：占位（`git_pending`）一直撑到 `updated` 之后，其间每 100 ms 采样，
 *      发送键一直是禁用的、占位一直在（以前补拉超时后有约 2.5 秒既没占位也没「工作位置」、发送放行，会悄悄建成「当前目录」）
 *   l. 已归档的 worktree 会话：没有输入框，只有「已归档」条和「取消归档」；CLI `send` 被拒（`invalid` / `archived`，退出码 2）；
 *      取消归档后输入框回来、worktree 按分支重建，再 `write` 写进重建的 worktree
 *   m. daemon 刚重启 + 一批（12 个）仓库项目同时是冷缓存（用例自己起一个节点，helpers/daemon.ts）：
 *      「工作位置」一定出现，会话建在 worktree 里
 *   n. 等审批时不能归档：「归档」按钮禁用 + `session-archive-blocked` 说明，daemon 的 `archive` 回 busy（CLI），
 *      什么都没变；答复之后才能归档
 *   o. 项目改名：`git worktree repair` 进行中（包装脚本拖慢）给会话发消息是 busy；改名返回之后 worktree 会话照常
 *      （`active`、没有 `worktree_missing`、没有再重建），马上从网页继续写，写进搬了家的 worktree
 *
 * 不预热 daemon 的 git 缓存：每个用例在配对浏览器之前才建仓库项目，所以浏览器连上时拉的第一次项目列表就是冷缓存
 * （daemon 最多等 3 秒，算不完的先不带 `git`，之后发 `workspace.changed { updated }`；网页在这段时间显示占位并挡住发送）。
 * 新建会话的流程因此是：从项目页点头部的「新会话」进到新会话页（`openNewSession`），等「工作位置」按钮出现（`waitForWorkspacePicker`）再选 / 发，
 * 不在占位期间点发送。分段控件、base、分支名都在这个按钮点开的浮层里（`openWorkspacePopover`），关掉之后选择仍保留在表单里。
 *
 * 环境：global-setup 给 daemon 设了 `GIT_CONFIG_GLOBAL`（临时 gitconfig，含 user.name / user.email）和 `GIT_CONFIG_NOSYSTEM=1`，
 * 所以「同步 base」的合并提交不依赖开发机的 git 配置；测试里的 git 命令用同一份配置（helpers/git.ts），带超时。
 * 约定：显式等待（data-testid、expect.poll），没有固定睡眠；每个用例自己配对自己的浏览器、自己建仓库项目。
 */
import {
  existsSync,
  lstatSync,
  readFileSync,
  readdirSync,
  realpathSync,
  renameSync,
  rmSync,
  symlinkSync,
} from 'node:fs'
import { randomBytes } from 'node:crypto'
import { join } from 'node:path'

import { expect, test, type Locator, type Page } from '@playwright/test'

import {
  INCLUDED_CONTENT,
  INCLUDED_FILE,
  commitFile,
  git,
  makeRepoProject,
  slowGit,
  writeFile,
} from '../helpers/git.js'
import { runCliResult } from '../helpers/cli.js'
import { startExtraDaemon } from '../helpers/daemon.js'
import { waitFor } from '../helpers/process.js'
import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  TURN_TIMEOUT,
  expectNodeOnline,
  nodeBase,
  openNewSessionFromProject,
  pairNewBrowser,
  pendingCard,
  resolvedCards,
} from '../helpers/ui.js'

test.beforeEach(() => {
  test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
})

/** mock agent 的 `write` 关键词写的文件（crates/adapters/src/mock/README.md）。 */
const MOCK_FILE = 'anycon-mock.txt'
const MOCK_LINE = /^mock write turn \d+$/

function uniqueName(prefix: string): string {
  return `${prefix}-${randomBytes(3).toString('hex')}`
}

/** 回节点首页（整页重载，等节点重新在线）。 */
async function gotoNodeHome(page: Page): Promise<void> {
  await page.goto(nodeBase(page))
  await expectNodeOnline(page)
}

/** 节点首页 → 项目卡片 → 项目页（头部有「新会话」按钮；项目页本身没有输入框）。 */
async function openProject(page: Page, name: string): Promise<void> {
  const card = page.getByTestId('project-card').filter({ hasText: name })
  await expect(card).toBeVisible({ timeout: 15_000 })
  await card.click()
  await expect(page).toHaveURL(new RegExp(`/p/${name}$`))
  await expect(page.getByTestId('project-new-session')).toBeVisible()
}

/** 节点首页 → 项目页 → 头部「新会话」→ 这个项目的新会话页（新建会话的表单在这一页）。 */
async function openNewSession(page: Page, name: string): Promise<void> {
  await openProject(page, name)
  await openNewSessionFromProject(page, name)
}

/** 还停在这个项目的新会话页（`/n/<node>/new?project=<name>`）。 */
async function expectOnNewSessionPage(page: Page, name: string): Promise<void> {
  await expect(page).toHaveURL(
    (url) => url.pathname.endsWith('/new') && url.searchParams.get('project') === name,
  )
}

/** 冷缓存 + 机器忙时「工作位置」可能要几秒才出现。 */
const PICKER_TIMEOUT = 45_000

/**
 * 等新建会话表单里的「工作位置」按钮（`new-session-workspace-button`，输入框下面选项行里）出现，返回它。
 *
 * 不预热 daemon 的 git 缓存：项目的 git 元数据还没到时，这一行先是占位
 * `new-session-workspace-pending`（「正在读取 git 信息…」，发送被挡），读到了才变成按钮（web.md § 4.5）。
 * 所以这里只等它出现，不在占位期间选 / 点发送。冷缓存最多等 3 秒（daemon）+ 补拉，机器忙时更久，给足时间。
 * 分段控件、base、分支名在按钮点开的浮层里：要看 / 改它们用 `openWorkspacePopover`。
 */
async function waitForWorkspacePicker(page: Page): Promise<Locator> {
  const button = page.getByTestId('new-session-workspace-button')
  // 记一笔：这次是不是先看到了占位（冷缓存的路径）。报告里能看出机器忙时有多少用例真的走了这条路
  if ((await page.getByTestId('new-session-workspace-pending').count()) > 0) {
    test.info().annotations.push({ type: 'cold-cache', description: 'git placeholder was shown' })
  }
  await expect(button).toBeVisible({ timeout: PICKER_TIMEOUT })
  await expect(page.getByTestId('new-session-workspace-pending')).toHaveCount(0)
  return button
}

/**
 * 点开「工作位置」浮层，返回里面的分段控件（`new-session-workspace`）。已经开着就不再点（再点一次会关掉）。
 * 浮层关掉时里面的内容不渲染；选的 base、分支名存在表单的状态里，关掉再开还在。
 */
async function openWorkspacePopover(page: Page): Promise<Locator> {
  if ((await page.getByTestId('new-session-workspace-row').count()) === 0) {
    await page.getByTestId('new-session-workspace-button').click()
  }
  const picker = page.getByTestId('new-session-workspace')
  await expect(picker).toBeVisible()
  return picker
}

/** 关掉「工作位置」浮层（Esc；没开着就什么都不做）。点输入框、点发送之前先关，免得靠“点外面自动关”的时序。 */
async function closeWorkspacePopover(page: Page): Promise<void> {
  const row = page.getByTestId('new-session-workspace-row')
  if ((await row.count()) === 0) return
  await page.keyboard.press('Escape')
  await expect(row).toHaveCount(0)
}

/** 在新建会话的表单里发第一条消息，等跳到会话页。占位期间发送是禁用的：先等占位没了再点。 */
async function sendFirstMessage(page: Page, text: string): Promise<void> {
  await expect(page.getByTestId('new-session-workspace-pending')).toHaveCount(0, {
    timeout: PICKER_TIMEOUT,
  })
  await closeWorkspacePopover(page)
  await page.getByTestId('composer-input').fill(text)
  await page.getByTestId('composer-send').click()
  await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
}

/** 切到会话页主区的「聊天」（没有这个开关的会话——不在 git 仓库里——本来就是聊天）。 */
async function showChat(page: Page): Promise<void> {
  const tab = page.getByTestId('main-view-chat')
  if ((await tab.count()) > 0) await tab.click()
}

/** 切到主区的「改动」视图：它占满聊天区，和消息流、输入框互斥。 */
async function showChanges(page: Page): Promise<void> {
  await page.getByTestId('main-view-changes').click()
}

/** 在会话页的输入框里发一条消息。 */
async function sendMessage(page: Page, text: string): Promise<void> {
  await showChat(page)
  await page.getByTestId('composer-input').fill(text)
  await page.getByTestId('composer-send').click()
}

/** 等 turn 结束（点发送之后的状态会先变成运行中，所以调用方要先等到一个只有 turn 做完才有的现象再调它）。 */
async function expectIdle(page: Page): Promise<void> {
  await expect(page.getByTestId('session-status')).toHaveAttribute('data-status', 'idle', {
    timeout: TURN_TIMEOUT,
  })
}

/** `<项目>/.anycon/worktrees/` 下的目录名。 */
function worktreeNames(project: string): string[] {
  const dir = join(project, '.anycon', 'worktrees')
  return existsSync(dir) ? readdirSync(dir).toSorted() : []
}

/** 文件里的非空行。 */
function lines(file: string): string[] {
  return readFileSync(file, 'utf8').split('\n').filter(Boolean)
}

/** 某个 `git` 事件（消息流里的居中系统提示）。 */
function gitEvent(page: Page, action: string): Locator {
  return page.locator(`[data-testid="git-event"][data-action="${action}"]`)
}

function diffRow(page: Page, path: string): Locator {
  return page.locator(`[data-testid="diff-file-row"][data-path="${path}"]`)
}

/**
 * 在新会话页（调用方已经 `openNewSession`）新建一个 worktree 会话、发 `write`、等 turn 做完，再在 worktree 里提交 agent 写的文件
 * （分支上有自己的提交，没有未提交的改动）。返回分支名和 worktree 目录。
 */
async function startCommittedWorktreeSession(
  page: Page,
  state: ReturnType<typeof readState>,
  project: string,
  prompt: string,
): Promise<{ branch: string; worktree: string }> {
  await waitForWorkspacePicker(page)
  await sendFirstMessage(page, prompt)
  const chip = page.getByTestId('session-branch')
  await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
  const branch = (await chip.getByTestId('session-branch-name').textContent()) ?? ''
  await expect.poll(() => worktreeNames(project)).toHaveLength(1)
  const worktree = join(project, '.anycon', 'worktrees', worktreeNames(project)[0] ?? '')
  await expect
    .poll(() => existsSync(join(worktree, MOCK_FILE)), { timeout: TURN_TIMEOUT })
    .toBe(true)
  await showChanges(page)
  await expect(diffRow(page, MOCK_FILE)).toBeVisible({ timeout: TURN_TIMEOUT })
  await expectIdle(page)
  git(state, worktree, 'add', '-A')
  git(state, worktree, 'commit', '-m', 'add the mock file')
  expect(git(state, worktree, 'status', '--porcelain')).toBe('')
  await showChat(page)
  return { branch, worktree }
}

test.describe('每会话 worktree', () => {
  test('a. 新 worktree → write → 改动 / diff → 同步 base → 未提交时归档被保留 → 在节点设置里删除', async ({
    browser,
  }) => {
    test.setTimeout(180_000)
    const state = readState()
    const name = uniqueName('wt-flow')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-a' })
    const { page } = paired
    try {
      let worktree = ''
      let branch = ''

      await test.step('新建会话：工作位置默认「新 worktree」，base 是 main；发 write', async () => {
        await openNewSession(page, name)
        const button = await waitForWorkspacePicker(page)
        await expect(button).toHaveAttribute('data-mode', 'worktree')
        // 浮层里：分段控件默认「新 worktree」，base 是 main
        const picker = await openWorkspacePopover(page)
        await expect(picker.getByTestId('new-session-workspace-worktree')).toHaveAttribute(
          'data-state',
          'on',
        )
        await expect(page.getByTestId('new-session-base')).toHaveValue('main')
        // 仓库根目录 + 有提交：没有「不能开 worktree」的说明
        await expect(page.getByTestId('new-session-workspace-disabled')).toHaveCount(0)
        await sendFirstMessage(page, 'write the notes file please')
      })

      await test.step('分支 chip 变成 active，git 事件说复制了 1 个文件，磁盘上有 worktree 和 .env', async () => {
        const chip = page.getByTestId('session-branch')
        await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
        await expect(chip.getByTestId('session-branch-name')).toHaveText(
          /^anycon\/write-the-notes-file/,
        )
        await expect(chip.getByTestId('session-branch-base')).toContainText('main')
        branch = (await chip.getByTestId('session-branch-name').textContent()) ?? ''

        await expect(gitEvent(page, 'worktree_created')).toContainText(/copied 1 file|复制了\s*1/, {
          timeout: TURN_TIMEOUT,
        })

        // worktree 在 <项目>/.anycon/worktrees/<目录名>，目录名取分支名最后一段
        await expect.poll(() => worktreeNames(project)).toHaveLength(1)
        const dirName = worktreeNames(project)[0] ?? ''
        worktree = join(project, '.anycon', 'worktrees', dirName)
        expect(dirName).toBe(branch.split('/').pop())
        // `.worktreeinclude` 列出的被忽略文件被复制进来了
        expect(readFileSync(join(worktree, INCLUDED_FILE), 'utf8')).toBe(INCLUDED_CONTENT)
        // 分支建在 main 的提交上，worktree 登记在主仓库里
        expect(git(state, project, 'rev-parse', '--verify', `refs/heads/${branch}`)).toBe(
          git(state, project, 'rev-parse', 'main'),
        )
        expect(git(state, project, 'worktree', 'list', '--porcelain')).toContain(
          `branch refs/heads/${branch}`,
        )
        // `/.anycon/` 在 info/exclude 里：主目录的状态是干净的
        expect(git(state, project, 'status', '--porcelain')).toBe('')
        expect(
          readFileSync(join(project, '.git', 'info', 'exclude'), 'utf8').split('\n'),
        ).toContain('/.anycon/')
      })

      await test.step('turn 结束后：文件在 worktree 里，主目录里没有', async () => {
        await expect
          .poll(() => existsSync(join(worktree, MOCK_FILE)), { timeout: TURN_TIMEOUT })
          .toBe(true)
        await expectIdle(page)
        expect(lines(join(worktree, MOCK_FILE))).toHaveLength(1)
        expect(lines(join(worktree, MOCK_FILE))[0]).toMatch(MOCK_LINE)
        expect(existsSync(join(project, MOCK_FILE))).toBe(false)
        // 主目录的 HEAD 没动
        expect(git(state, project, 'rev-parse', '--abbrev-ref', 'HEAD')).toBe('main')
      })

      await test.step('「改动」标签（有 worktree 的会话默认停在这里）：文件列表只有 anycon-mock.txt，展开看到 diff', async () => {
        await showChanges(page)
        await expect(page.getByTestId('changes-tab')).toBeVisible()
        const row = diffRow(page, MOCK_FILE)
        await expect(row).toBeVisible({ timeout: TURN_TIMEOUT })
        // 相对 base：.env 被忽略、.worktreeinclude 已提交，所以只有这一个文件，是未跟踪的新文件
        await expect(page.getByTestId('diff-file-row')).toHaveCount(1)
        await expect(row).toHaveAttribute('data-status', 'added')
        await expect(row).toHaveAttribute('data-untracked', 'true')
        await expect(row.getByTestId('diff-file-counts')).toContainText('+1')
        await expect(page.getByTestId('session-branch-uncommitted')).toBeVisible()

        await row.getByTestId('diff-file-toggle').click()
        const patch = row.getByTestId('diff-patch')
        await expect(patch).toBeVisible()
        await expect(patch).toContainText('mock write turn')
      })

      await test.step('worktree 里提交，主目录的 base 分支再提交一次 →「同步 base」合并 1 个提交', async () => {
        git(state, worktree, 'add', '-A')
        git(state, worktree, 'commit', '-m', 'add the mock notes')
        commitFile(state, project, 'base-change.txt', 'main moved on\n', 'advance main')
        expect(existsSync(join(worktree, 'base-change.txt'))).toBe(false)

        await page.getByTestId('git-sync').click()
        const result = page.getByTestId('sync-result')
        await expect(result).toHaveAttribute('data-outcome', 'merged', { timeout: TURN_TIMEOUT })
        await expect(result).toContainText(/\b1\b/)
        // 消息流里出现对应的 git 事件
        await showChat(page)
        await expect(gitEvent(page, 'sync_merged')).toBeVisible()
        await expect(gitEvent(page, 'sync_merged')).toContainText(/\b1\b/)

        // 磁盘上：base 的提交合进来了，是一个合并提交（需要 daemon 环境里的 user.name / user.email）
        expect(readFileSync(join(worktree, 'base-change.txt'), 'utf8')).toBe('main moved on\n')
        expect(git(state, worktree, 'log', '--merges', '--format=%s')).toMatch(/^Merge /)
        // 提交人来自 global-setup 给 daemon 的临时 gitconfig，不是开发机的配置
        expect(git(state, worktree, 'log', '--merges', '--format=%an <%ae>')).toBe(
          'anycon e2e <e2e@anycon.test>',
        )
        expect(git(state, worktree, 'status', '--porcelain')).toBe('')
        // 状态刷新之后没有未提交的改动了
        await expect(page.getByTestId('session-branch-uncommitted')).toHaveCount(0)

        // 再同步一次：base 没有新提交
        await showChanges(page)
        await page.getByTestId('git-sync').click()
        await expect(result).toHaveAttribute('data-outcome', 'up_to_date', {
          timeout: TURN_TIMEOUT,
        })
        await showChat(page)
        await expect(gitEvent(page, 'sync_up_to_date')).toBeVisible()
      })

      await test.step('再 write：留下未提交的改动（已跟踪文件的修改）', async () => {
        await sendMessage(page, 'write once more')
        await expect(page.getByTestId('session-branch-uncommitted')).toBeVisible({
          timeout: TURN_TIMEOUT,
        })
        await expectIdle(page)
        expect(lines(join(worktree, MOCK_FILE))).toHaveLength(2)
        expect(git(state, worktree, 'status', '--porcelain')).toBe(` M ${MOCK_FILE}`)

        await showChanges(page)
        await page.getByTestId('diff-against-uncommitted').click()
        const row = diffRow(page, MOCK_FILE)
        await expect(row).toBeVisible()
        await expect(row).toHaveAttribute('data-status', 'modified')
        await expect(row).toHaveAttribute('data-untracked', 'false')
        await expect(page.getByTestId('diff-file-row')).toHaveCount(1)
      })

      await test.step('归档（自动）：有未提交的改动，所以 worktree 被保留', async () => {
        // 归档 / 删除按钮在「概览」里
        await page.getByTestId('session-archive').click()
        const dialog = page.getByTestId('archive-worktree-dialog')
        await expect(dialog).toBeVisible()
        await expect(dialog.getByTestId('archive-option-auto')).toHaveAttribute(
          'data-state',
          'checked',
        )
        // 说明「有 1 个未提交的改动，所以会保留」
        await expect(dialog.getByTestId('archive-auto-keeps')).toBeVisible()
        await dialog.getByTestId('archive-option-auto').click()
        await dialog.getByTestId('archive-confirm').click()
        await expect(dialog).toHaveCount(0)

        await expect(page.getByTestId('session-branch')).toHaveAttribute('data-state', 'kept', {
          timeout: TURN_TIMEOUT,
        })
        await showChat(page)
        await expect(gitEvent(page, 'worktree_kept')).toBeVisible()
        // 目录和里面的改动都还在
        expect(existsSync(worktree)).toBe(true)
        expect(git(state, worktree, 'status', '--porcelain')).toBe(` M ${MOCK_FILE}`)
      })

      await test.step('节点设置 → Workspace → Worktree：这一行是「已保留」', async () => {
        await page.goto(`${nodeBase(page)}/settings`)
        await page.getByTestId('tab-workspace').click()
        await expect(page.getByTestId('worktree-section')).toBeVisible({ timeout: 45_000 })
        const row = page.getByTestId('worktree-row').filter({ hasText: name })
        await expect(row).toHaveCount(1, { timeout: TURN_TIMEOUT })
        await expect(row).toHaveAttribute('data-state', 'kept')
        await expect(row.getByTestId('worktree-state')).toHaveText(/Kept|已保留/)
        await expect(row.getByTestId('worktree-branch')).toContainText(branch)
        await expect(row.getByTestId('worktree-uncommitted')).toContainText(/\b1\b/)
        // 所属会话已归档：可以删除，不再提示「先归档」
        await expect(row.getByTestId('worktree-archive-first')).toHaveCount(0)
        await expect(row.getByTestId('worktree-remove')).toBeEnabled()
      })

      await test.step('删除：必须勾「丢弃未提交的改动」；目录没了，分支还在', async () => {
        const row = page.getByTestId('worktree-row').filter({ hasText: name })
        await row.getByTestId('worktree-remove').click()
        const dialog = page.getByTestId('worktree-remove-dialog')
        await expect(dialog).toBeVisible()
        const confirm = dialog.getByTestId('worktree-remove-confirm')
        // 有未提交的改动，没勾 force 之前删除按钮是禁用的
        await expect(confirm).toBeDisabled()
        // 分支上有自己的提交：提示删分支会丢提交
        await expect(dialog.getByTestId('worktree-remove-commits-lost')).toBeVisible()
        await dialog.getByTestId('worktree-remove-force').click()
        await expect(dialog.getByTestId('worktree-remove-force')).toHaveAttribute(
          'data-state',
          'checked',
        )
        await expect(confirm).toBeEnabled()
        await confirm.click()
        await expect(dialog).toHaveCount(0)

        // 会话还在（已归档），所以这一行还在，状态是「已删除」
        await expect(row).toHaveAttribute('data-state', 'removed', { timeout: TURN_TIMEOUT })
        await expect.poll(() => existsSync(worktree)).toBe(false)
        expect(worktreeNames(project)).toEqual([])
        // 没勾「同时删除分支」：分支和它上面的提交都在
        expect(git(state, project, 'rev-parse', '--verify', `refs/heads/${branch}`)).toMatch(
          /^[0-9a-f]{40}$/,
        )
        expect(git(state, project, 'worktree', 'list', '--porcelain')).not.toContain(
          `refs/heads/${branch}`,
        )
        // 主目录没被碰过
        expect(git(state, project, 'status', '--porcelain')).toBe('')
        expect(existsSync(join(project, MOCK_FILE))).toBe(false)
      })
    } finally {
      await paired.context.close()
    }
  })

  test('b. 「当前目录」模式：没有 worktree，「改动」标签只有「未提交」视图，anycon-mock.txt 是未跟踪文件', async ({
    browser,
  }) => {
    test.setTimeout(120_000)
    const state = readState()
    const name = uniqueName('wt-local')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-b' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      const button = await waitForWorkspacePicker(page)
      await openWorkspacePopover(page)
      await page.getByTestId('new-session-workspace-local').click()
      await expect(page.getByTestId('new-session-workspace-local')).toHaveAttribute(
        'data-state',
        'on',
      )
      // 当前目录模式没有 base / 分支名
      await expect(page.getByTestId('new-session-base')).toHaveCount(0)
      await expect(page.getByTestId('new-session-branch')).toHaveCount(0)
      await closeWorkspacePopover(page)
      await expect(button).toHaveAttribute('data-mode', 'local')
      await sendFirstMessage(page, 'write in place')

      // 分支 chip：本地会话，只显示分支名（没有 base）
      const chip = page.getByTestId('session-branch')
      await expect(chip).toHaveAttribute('data-state', 'local', { timeout: TURN_TIMEOUT })
      await expect(chip.getByTestId('session-branch-name')).toHaveText('main')
      await expect(chip.getByTestId('session-branch-base')).toHaveCount(0)

      // 文件写在项目目录里；没有建 worktree
      await expect
        .poll(() => existsSync(join(project, MOCK_FILE)), { timeout: TURN_TIMEOUT })
        .toBe(true)
      expect(existsSync(join(project, '.anycon'))).toBe(false)
      expect(
        git(state, project, 'worktree', 'list', '--porcelain').match(/^worktree /gm),
      ).toHaveLength(1)
      await expect(page.getByTestId('git-event')).toHaveCount(0)

      // 没有 worktree 的会话默认停在「概览」；切到「改动」
      await page.getByTestId('main-view-changes').click()
      await expect(page.getByTestId('changes-tab')).toBeVisible()
      await expect(page.getByTestId('diff-uncommitted-only')).toBeVisible()
      await expect(page.getByTestId('diff-against')).toHaveCount(0)
      // 本地会话没有 base：没有「同步 base」和「创建 PR」
      await expect(page.getByTestId('git-sync')).toHaveCount(0)
      await expect(page.getByTestId('git-create-pr')).toHaveCount(0)

      const row = diffRow(page, MOCK_FILE)
      await expect(row).toBeVisible({ timeout: TURN_TIMEOUT })
      await expect(row).toHaveAttribute('data-status', 'added')
      await expect(row).toHaveAttribute('data-untracked', 'true')
      await expect(page.getByTestId('diff-file-row')).toHaveCount(1)
      await row.getByTestId('diff-file-toggle').click()
      await expect(row.getByTestId('diff-patch')).toContainText('mock write turn')
      await expectIdle(page)

      // 本地会话的归档不弹 worktree 的确认框：直接归档
      await page.getByTestId('session-archive').click()
      await expect(page.getByTestId('archive-worktree-dialog')).toHaveCount(0)
      await expect(page.getByTestId('session-archive')).toHaveText(/Unarchive|取消归档/, {
        timeout: TURN_TIMEOUT,
      })
      expect(existsSync(join(project, MOCK_FILE))).toBe(true)
    } finally {
      await paired.context.close()
    }
  })

  test('c. 非 git 项目没有「工作位置」；没有提交的仓库这一行禁用并说明，会话照常在项目目录里工作', async ({
    browser,
  }) => {
    test.setTimeout(120_000)
    const state = readState()
    const plain = uniqueName('wt-plain')
    const empty = uniqueName('wt-empty')
    const plainDir = join(state.workspaceDir, plain)
    writeFile(join(plainDir, 'notes.txt'), 'not a repository\n')
    const emptyDir = makeRepoProject(state, empty, { commit: false })
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-c' })
    const { page } = paired
    try {
      // 非 git 项目：没有这一行，会话没有分支 chip 也没有「改动」标签
      await openNewSession(page, plain)
      await expect(page.getByTestId('new-session-form')).toBeVisible()
      // 没有 `git` 元数据的项目先是占位（还不知道是不是仓库，发送被挡），补拉回来仍没有才当非 git 项目：等占位没了再断言
      await expect(page.getByTestId('new-session-workspace-pending')).toHaveCount(0, {
        timeout: PICKER_TIMEOUT,
      })
      await expect(page.getByTestId('new-session-workspace-button')).toHaveCount(0)
      await expect(page.getByTestId('new-session-workspace-row')).toHaveCount(0)
      await expect(page.getByTestId('new-session-workspace')).toHaveCount(0)
      await sendFirstMessage(page, 'write in a plain folder')
      await expect
        .poll(() => existsSync(join(plainDir, MOCK_FILE)), { timeout: TURN_TIMEOUT })
        .toBe(true)
      await expectIdle(page)
      await expect(page.getByTestId('info-panel')).toBeVisible()
      await expect(page.getByTestId('session-branch')).toHaveCount(0)
      await expect(page.getByTestId('main-view')).toHaveCount(0)
      await expect(page.getByTestId('git-event')).toHaveCount(0)
      expect(existsSync(join(plainDir, '.anycon'))).toBe(false)

      // 还没有任何提交的仓库：这一行出现，但禁用并写明原因，不拉分支、没有 base / 分支名输入
      await gotoNodeHome(page)
      await openNewSession(page, empty)
      const button = await waitForWorkspacePicker(page)
      await expect(button).toHaveAttribute('data-mode', 'local')
      const picker = await openWorkspacePopover(page)
      await expect(page.getByTestId('new-session-workspace-disabled')).toBeVisible()
      await expect(picker.getByTestId('new-session-workspace-worktree')).toBeDisabled()
      await expect(picker.getByTestId('new-session-workspace-local')).toBeDisabled()
      await expect(page.getByTestId('new-session-base')).toHaveCount(0)
      await expect(page.getByTestId('new-session-branch')).toHaveCount(0)
      // 仍然可以发消息：不带 workspace，daemon 按「当前目录」处理，不建 worktree
      await sendFirstMessage(page, 'write in an empty repository')
      await expect
        .poll(() => existsSync(join(emptyDir, MOCK_FILE)), { timeout: TURN_TIMEOUT })
        .toBe(true)
      await expectIdle(page)
      expect(existsSync(join(emptyDir, '.anycon'))).toBe(false)
      await expect(gitEvent(page, 'worktree_created')).toHaveCount(0)
    } finally {
      await paired.context.close()
    }
  })

  test('d. 选 base 和自定义分支名：不合法的名字挡住发送，base 决定 worktree 的内容，已存在的分支名创建失败', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-base')
    const project = makeRepoProject(state, name)
    // release 停在初始提交上；main 往前走一步（多一个 extra.txt）
    git(state, project, 'branch', 'release')
    commitFile(state, project, 'extra.txt', 'only on main\n', 'main moves ahead of release')
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-d' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      await waitForWorkspacePicker(page)
      // 先把消息写好：填输入框会让「工作位置」浮层关掉，写完再点开浮层选 base / 分支名，之后发送键只可能因为分支名而禁用
      await page.getByTestId('composer-input').fill('write on release')
      await openWorkspacePopover(page)
      const base = page.getByTestId('new-session-base')
      await expect(base).toHaveValue('main')
      // 本地分支都在下拉里
      await expect(base.locator('option[value="release"]')).toHaveCount(1)
      await base.selectOption('release')
      await expect(base).toHaveValue('release')

      // 分支名不合法：浮层里的行内错误，发送被挡住并写明原因
      const branch = page.getByTestId('new-session-branch')
      await branch.fill('main')
      await expect(page.getByTestId('new-session-branch-error')).toBeVisible()
      await expect(page.getByTestId('composer-send')).toBeDisabled()
      await expect(page.getByTestId('composer-blocked')).toBeVisible()
      await branch.fill('has space')
      await expect(page.getByTestId('new-session-branch-error')).toBeVisible()
      await expect(page.getByTestId('composer-send')).toBeDisabled()

      // 合法的名字：创建在 release 上，分支名就是输入的
      await branch.fill('e2e/custom-branch')
      await expect(page.getByTestId('new-session-branch-error')).toHaveCount(0)
      await expect(page.getByTestId('composer-send')).toBeEnabled()
      await closeWorkspacePopover(page)
      await page.getByTestId('composer-send').click()
      await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })

      const chip = page.getByTestId('session-branch')
      await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
      await expect(chip.getByTestId('session-branch-name')).toHaveText('e2e/custom-branch')
      await expect(chip.getByTestId('session-branch-base')).toContainText('release')
      await expect.poll(() => worktreeNames(project)).toEqual(['custom-branch'])
      const worktree = join(project, '.anycon', 'worktrees', 'custom-branch')
      // 内容来自 release：有 README.md，没有只在 main 上的 extra.txt
      expect(existsSync(join(worktree, 'README.md'))).toBe(true)
      expect(existsSync(join(worktree, 'extra.txt'))).toBe(false)
      expect(git(state, project, 'rev-parse', 'e2e/custom-branch')).toBe(
        git(state, project, 'rev-parse', 'release'),
      )
      await expect
        .poll(() => existsSync(join(worktree, MOCK_FILE)), { timeout: TURN_TIMEOUT })
        .toBe(true)
      await expectIdle(page)

      // 同一个分支名再来一次：创建失败，留在新会话页，文字放回输入框，没有多出 worktree
      await gotoNodeHome(page)
      await openNewSession(page, name)
      await waitForWorkspacePicker(page)
      await openWorkspacePopover(page)
      await page.getByTestId('new-session-branch').fill('e2e/custom-branch')
      await closeWorkspacePopover(page)
      await sendMessage(page, 'write again')
      await expect(page.getByText(/branch already exists|分支已经存在/).first()).toBeVisible({
        timeout: 15_000,
      })
      await expectOnNewSessionPage(page, name)
      await expect(page.getByTestId('composer-input')).toHaveValue('write again')
      // 项目页的会话列表和左栏都标着第一个会话的分支（新会话页没有会话列表：回项目页看）
      await page.goto(`${nodeBase(page)}/p/${name}`)
      await expectNodeOnline(page)
      await expect(page.getByTestId('session-list-branch')).toContainText('e2e/custom-branch')
      await expect(page.locator('a[data-branch="e2e/custom-branch"]').first()).toBeVisible()
      expect(worktreeNames(project)).toEqual(['custom-branch'])
    } finally {
      await paired.context.close()
    }
  })

  test('e. 干净的 worktree 自动归档被删掉（分支有自己的提交所以保留），取消归档按分支重建并复制 .worktreeinclude', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-restore')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-e' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      await waitForWorkspacePicker(page)
      await sendFirstMessage(page, 'write then archive')
      const chip = page.getByTestId('session-branch')
      await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
      const branch = (await chip.getByTestId('session-branch-name').textContent()) ?? ''
      await expect.poll(() => worktreeNames(project)).toHaveLength(1)
      const worktree = join(project, '.anycon', 'worktrees', worktreeNames(project)[0] ?? '')
      await expect
        .poll(() => existsSync(join(worktree, MOCK_FILE)), { timeout: TURN_TIMEOUT })
        .toBe(true)
      await showChanges(page)
      await expect(diffRow(page, MOCK_FILE)).toBeVisible({ timeout: TURN_TIMEOUT })
      await expectIdle(page)
      await showChat(page)

      // 提交 agent 写的文件：没有未提交的改动（被忽略的 .env 不算），分支上有自己的提交
      git(state, worktree, 'add', '-A')
      git(state, worktree, 'commit', '-m', 'add the mock file')
      expect(git(state, worktree, 'status', '--porcelain')).toBe('')

      await test.step('自动归档：worktree 删了，分支留着', async () => {
        await page.getByTestId('session-archive').click()
        const dialog = page.getByTestId('archive-worktree-dialog')
        await expect(dialog).toBeVisible()
        await dialog.getByTestId('archive-confirm').click()
        await expect(dialog).toHaveCount(0)
        await expect(chip).toHaveAttribute('data-state', 'removed', { timeout: TURN_TIMEOUT })
        await showChat(page)
        await expect(gitEvent(page, 'worktree_removed')).toContainText(/branch kept|分支保留/)
        await expect.poll(() => existsSync(worktree)).toBe(false)
        expect(git(state, project, 'rev-parse', '--verify', `refs/heads/${branch}`)).toMatch(
          /^[0-9a-f]{40}$/,
        )
      })

      await test.step('取消归档：按分支重建，.env 也复制回来，文件还在', async () => {
        await page.getByTestId('session-archive').click()
        await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
        await expect(gitEvent(page, 'worktree_restored')).toBeVisible()
        await expect.poll(() => existsSync(worktree)).toBe(true)
        expect(readFileSync(join(worktree, INCLUDED_FILE), 'utf8')).toBe(INCLUDED_CONTENT)
        expect(lines(join(worktree, MOCK_FILE))).toHaveLength(1)
        expect(git(state, worktree, 'rev-parse', '--abbrev-ref', 'HEAD')).toBe(branch)
        expect(git(state, worktree, 'status', '--porcelain')).toBe('')
        await expect(page.getByTestId('session-archive')).toHaveText(/Archive|归档/)
      })

      await test.step('重建后的 worktree 里 agent 照常工作', async () => {
        await sendMessage(page, 'write after restore')
        await expect(page.getByTestId('session-branch-uncommitted')).toBeVisible({
          timeout: TURN_TIMEOUT,
        })
        await expectIdle(page)
        expect(lines(join(worktree, MOCK_FILE))).toHaveLength(2)
        expect(existsSync(join(project, MOCK_FILE))).toBe(false)
      })
    } finally {
      await paired.context.close()
    }
  })

  test('f. 同步 base 遇到冲突：冲突文件和按钮、让 agent 解决（快捷消息）、放弃合并', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-conflict')
    const project = makeRepoProject(state, name)
    commitFile(state, project, 'shared.txt', 'original\n', 'add shared.txt')
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-f' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      await waitForWorkspacePicker(page)
      // 注意：mock agent 会对消息里的关键词（write / run / plan / …）有反应，分支名和快捷消息里都带着这条消息的词，所以用没有关键词的
      await sendFirstMessage(page, 'conflict flow')
      const chip = page.getByTestId('session-branch')
      await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
      await expect(chip.getByTestId('session-branch-name')).toHaveText('anycon/conflict-flow')
      await expect(page.getByTestId('turn-usage')).toHaveCount(1, { timeout: TURN_TIMEOUT })
      await expectIdle(page)
      const worktree = join(project, '.anycon', 'worktrees', 'conflict-flow')
      expect(existsSync(worktree)).toBe(true)

      // 两边改同一个文件的同一行
      commitFile(
        state,
        worktree,
        'shared.txt',
        'worktree side\n',
        'edit shared.txt in the worktree',
      )
      commitFile(state, project, 'shared.txt', 'main side\n', 'edit shared.txt on main')

      await test.step('同步：冲突，合并停在中途', async () => {
        await showChanges(page)
        await page.getByTestId('git-sync').click()
        const result = page.getByTestId('sync-result')
        await expect(result).toHaveAttribute('data-outcome', 'conflicts', { timeout: TURN_TIMEOUT })
        await expect(result).toContainText(/\b1 file/)
        // 「改动」标签：冲突横幅、冲突文件，同步按钮换成「让 agent 解决 / 放弃合并」
        await expect(page.getByTestId('git-merge-banner')).toBeVisible()
        await expect(page.getByTestId('git-conflict-files')).toContainText('shared.txt')
        await expect(page.getByTestId('git-resolve')).toBeVisible()
        await expect(page.getByTestId('git-abort')).toBeVisible()
        await expect(page.getByTestId('git-sync')).toHaveCount(0)
        // 分支 chip 标「合并中」
        await expect(chip).toContainText(/merging|合并中/i)
        // 消息流里的 git 事件列出冲突文件，带「让 agent 解决 / 放弃合并」
        await showChat(page)
        const event = gitEvent(page, 'sync_conflicts')
        await expect(event).toContainText('shared.txt')
        await expect(event.getByTestId('git-event-resolve')).toBeVisible()
        await expect(event.getByTestId('git-event-abort')).toBeVisible()
        // 磁盘上：合并停在冲突里
        expect(git(state, worktree, 'status', '--porcelain')).toBe('UU shared.txt')
        expect(git(state, worktree, 'rev-parse', '-q', '--verify', 'MERGE_HEAD')).toMatch(
          /^[0-9a-f]{40}$/,
        )
      })

      await test.step('让 agent 解决：快捷消息发给 agent，带分支和 base，不列文件名', async () => {
        await gitEvent(page, 'sync_conflicts').getByTestId('git-event-resolve').click()
        const sent = page.getByTestId('message-user').last()
        await expect(sent).toContainText(
          'Merging main into anycon/conflict-flow hit conflicts. Resolve the files that `git status` lists as conflicted and finish the merge commit.',
        )
        // 快捷消息不列文件名（文件名是仓库里的内容，不该原样拼进发给 agent 的指令里）
        await expect(sent).not.toContainText('shared.txt')
        // mock agent 回了这条消息（第二个 turn 结束）
        await expect(page.getByTestId('turn-usage')).toHaveCount(2, { timeout: TURN_TIMEOUT })
        await expectIdle(page)
        // mock 不会真的去解决：合并仍停在冲突里
        expect(git(state, worktree, 'status', '--porcelain')).toBe('UU shared.txt')
      })

      await test.step('放弃合并：回到合并之前', async () => {
        await showChanges(page)
        await page.getByTestId('git-abort').click()
        const result = page.getByTestId('sync-result')
        await expect(result).toHaveAttribute('data-outcome', 'aborted', { timeout: TURN_TIMEOUT })
        await showChat(page)
        await expect(gitEvent(page, 'sync_aborted')).toBeVisible()
        await showChanges(page)
        await expect(page.getByTestId('git-merge-banner')).toHaveCount(0)
        await expect(page.getByTestId('git-sync')).toBeVisible()
        // 事件上的冲突按钮不再出现（合并结束了）
        await showChat(page)
        await expect(page.getByTestId('git-event-conflict-actions')).toHaveCount(0)
        expect(git(state, worktree, 'status', '--porcelain')).toBe('')
        expect(readFileSync(join(worktree, 'shared.txt'), 'utf8')).toBe('worktree side\n')
        expect(() => git(state, worktree, 'rev-parse', '-q', '--verify', 'MERGE_HEAD')).toThrow()
      })
    } finally {
      await paired.context.close()
    }
  })

  test('g. 删除会话：选「删除 worktree」并勾「同时删除分支」，提示会丢提交；目录和分支都没了', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-delete')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-g' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      const { branch, worktree } = await startCommittedWorktreeSession(
        page,
        state,
        project,
        'write delete me',
      )

      await page.getByTestId('session-delete').click()
      const dialog = page.getByTestId('delete-dialog')
      await expect(dialog).toBeVisible()
      await expect(dialog.getByTestId('delete-option-auto')).toHaveAttribute(
        'data-state',
        'checked',
      )
      await dialog.getByTestId('delete-option-remove').click()
      await expect(dialog.getByTestId('delete-option-remove')).toHaveAttribute(
        'data-state',
        'checked',
      )
      // 分支上有自己的提交：提示删分支会丢 1 个提交；勾上之后变成警告（role=alert）
      const lost = dialog.getByTestId('delete-commits-lost')
      await expect(lost).toBeVisible()
      await expect(lost).toContainText(/\b1\b/)
      await dialog.getByTestId('delete-delete-branch').click()
      await expect(dialog.getByTestId('delete-delete-branch')).toHaveAttribute(
        'data-state',
        'checked',
      )
      await expect(lost).toHaveAttribute('role', 'alert')
      await dialog.getByTestId('session-delete-confirm').click()

      // 删除后回到项目页
      await expect(page).toHaveURL(new RegExp(`/p/${name}$`), { timeout: 20_000 })
      await expect.poll(() => existsSync(worktree), { timeout: TURN_TIMEOUT }).toBe(false)
      expect(worktreeNames(project)).toEqual([])
      expect(git(state, project, 'branch', '--list', branch)).toBe('')
      expect(
        git(state, project, 'worktree', 'list', '--porcelain').match(/^worktree /gm),
      ).toHaveLength(1)
      expect(git(state, project, 'status', '--porcelain')).toBe('')

      // 会话和 worktree 都没了：节点设置的 worktree 列表里没有这个项目
      await page.goto(`${nodeBase(page)}/settings`)
      await page.getByTestId('tab-workspace').click()
      await expect(page.getByTestId('worktree-section')).toBeVisible({ timeout: 45_000 })
      await expect(page.getByTestId('worktree-refresh')).toBeEnabled()
      await expect(page.getByTestId('worktree-row').filter({ hasText: name })).toHaveCount(0)
    } finally {
      await paired.context.close()
    }
  })

  test('h. 删除会话时保留 worktree：它留在节点设置里（没有会话），在那里删除并同时删分支', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-orphan')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-h' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      const { branch, worktree } = await startCommittedWorktreeSession(
        page,
        state,
        project,
        'write keep me',
      )

      await page.getByTestId('session-delete').click()
      const dialog = page.getByTestId('delete-dialog')
      await expect(dialog).toBeVisible()
      await dialog.getByTestId('delete-option-keep').click()
      // 保留 worktree 时分支也留着：「同时删除分支」禁用
      await expect(dialog.getByTestId('delete-delete-branch')).toBeDisabled()
      await dialog.getByTestId('session-delete-confirm').click()
      await expect(page).toHaveURL(new RegExp(`/p/${name}$`), { timeout: 20_000 })
      // 目录和分支都还在
      expect(existsSync(join(worktree, MOCK_FILE))).toBe(true)
      expect(git(state, project, 'branch', '--list', branch)).toContain(branch)

      await page.goto(`${nodeBase(page)}/settings`)
      await page.getByTestId('tab-workspace').click()
      await expect(page.getByTestId('worktree-section')).toBeVisible({ timeout: 45_000 })
      const row = page.getByTestId('worktree-row').filter({ hasText: name })
      await expect(row).toHaveCount(1, { timeout: TURN_TIMEOUT })
      await expect(row).toHaveAttribute('data-state', 'kept')
      await expect(row.getByTestId('worktree-no-session')).toBeVisible()
      await expect(row.getByTestId('worktree-ahead')).toContainText(/\b1\b/)
      await expect(row.getByTestId('worktree-uncommitted')).toContainText(/\b0\b/)

      await row.getByTestId('worktree-remove').click()
      const removeDialog = page.getByTestId('worktree-remove-dialog')
      await expect(removeDialog).toBeVisible()
      // 没有未提交的改动：不需要勾「丢弃」
      await expect(removeDialog.getByTestId('worktree-remove-force')).toHaveCount(0)
      await expect(removeDialog.getByTestId('worktree-remove-commits-lost')).toBeVisible()
      await removeDialog.getByTestId('worktree-remove-delete-branch').click()
      await expect(removeDialog.getByTestId('worktree-remove-commits-lost')).toHaveAttribute(
        'role',
        'alert',
      )
      await removeDialog.getByTestId('worktree-remove-confirm').click()
      await expect(removeDialog).toHaveCount(0)

      // 会话已经删了，worktree 和分支也删了：连行一起删
      await expect(row).toHaveCount(0, { timeout: TURN_TIMEOUT })
      await expect.poll(() => existsSync(worktree)).toBe(false)
      expect(git(state, project, 'branch', '--list', branch)).toBe('')
      expect(git(state, project, 'status', '--porcelain')).toBe('')
    } finally {
      await paired.context.close()
    }
  })

  test('i. worktree 目录被删掉：会话标「不见了」，下一条消息按分支重建（有提交的改动都在）', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-gone')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-i' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      const { branch, worktree } = await startCommittedWorktreeSession(
        page,
        state,
        project,
        'write before the folder goes away',
      )
      const chip = page.getByTestId('session-branch')

      // 有人把目录删了（没有 `git worktree remove`）：周期检查（约 2 秒）发现，chip 变警告，「改动」标签只剩说明
      rmSync(worktree, { recursive: true, force: true })
      await expect(chip).toHaveAttribute('data-state', 'missing', { timeout: TURN_TIMEOUT })
      await showChat(page)
      await expect(gitEvent(page, 'worktree_missing')).toBeVisible()
      await showChanges(page)
      await expect(page.getByTestId('changes-unavailable')).toHaveAttribute('data-state', 'missing')
      expect(existsSync(worktree)).toBe(false)

      // 下一条消息：按分支重建（分支上的提交还在），agent 在重建的目录里继续写
      await sendMessage(page, 'write after the rebuild')
      await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
      await expect(gitEvent(page, 'worktree_restored')).toBeVisible()
      await expect
        .poll(() => lines(join(worktree, MOCK_FILE)).length, { timeout: TURN_TIMEOUT })
        .toBe(2)
      await expectIdle(page)
      expect(readFileSync(join(worktree, INCLUDED_FILE), 'utf8')).toBe(INCLUDED_CONTENT)
      expect(git(state, worktree, 'rev-parse', '--abbrev-ref', 'HEAD')).toBe(branch)
      expect(git(state, worktree, 'status', '--porcelain')).toBe(` M ${MOCK_FILE}`)
      expect(existsSync(join(project, MOCK_FILE))).toBe(false)
    } finally {
      await paired.context.close()
    }
  })

  test('j. worktree 目录被换成符号链接：会话标「不见了」，不重建、不跟随链接，发消息被拒绝', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-swap')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-j' })
    const { page } = paired
    let swapped = ''
    let moved = ''
    try {
      await openNewSession(page, name)
      const { worktree } = await startCommittedWorktreeSession(
        page,
        state,
        project,
        'write before the swap',
      )
      const chip = page.getByTestId('session-branch')

      // mv worktree worktree.real && ln -s <daemon 的数据目录> worktree（和关联项目的换目录攻击一样）
      moved = `${worktree}.real`
      renameSync(worktree, moved)
      symlinkSync(state.daemonHome, worktree)
      swapped = worktree
      await expect(chip).toHaveAttribute('data-state', 'missing', { timeout: TURN_TIMEOUT })

      // 发消息：被拒绝，不重建，链接还是链接，数据目录里没有被写进任何东西
      await sendMessage(page, 'write through the link')
      await expect(page.locator('[data-testid="error-block"][data-code="not_found"]')).toBeVisible({
        timeout: TURN_TIMEOUT,
      })
      await expect(gitEvent(page, 'worktree_restored')).toHaveCount(0)
      expect(lstatSync(worktree).isSymbolicLink()).toBe(true)
      expect(existsSync(join(state.daemonHome, MOCK_FILE))).toBe(false)
      await expect(chip).toHaveAttribute('data-state', 'missing')
    } finally {
      await paired.context.close()
      // 把目录换回来，让 teardown 的 rmSync 不会跟着链接走
      if (swapped && moved && existsSync(moved)) {
        rmSync(swapped, { force: true })
        renameSync(moved, swapped)
      }
    }
  })

  test('k. 冷缓存 + git 元数据算得慢：先是占位（发送被挡），算完「工作位置」自己出现，不刷新就能建 worktree 会话', async ({
    browser,
  }) => {
    // daemon 刚（重）启动或新项目没有缓存时，`workspace.list` 最多等 3 秒，超时的项目先不带 `git`，算完才发
    // `workspace.changed { updated }`。以前网页拿到没有 `git` 的项目就不显示「工作位置」，会话悄悄建成「当前目录」。
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-cold')
    const project = makeRepoProject(state, name)
    // 让 daemon 为这个项目算 git 元数据时慢一些（global-setup 的 git 包装脚本，每条规则只生效一次）：`symbolic-ref` 睡 6 秒
    // （单条 git 的硬超时是 5 秒，到时按失败算），失败后 daemon 串行再跑一条 `rev-parse --short HEAD`，它再睡 4 秒，整个计算约 9 秒。
    // 于是浏览器连上时的第一次 `workspace.list` 等 3 秒拿不到 `git`；打开表单后网页补拉一次（又等 3 秒）也拿不到；
    // 9 秒后算完才发 `workspace.changed { updated }`。必须在配对之前放：第一次碰这个项目的就是配对后浏览器拉的项目列表
    // （这里不预热 daemon 的缓存）。算出来的分支名是短哈希（`symbolic-ref` 失败的后备），这个用例不看它。
    const unslow = slowGit(state, name, [
      { match: 'symbolic-ref', seconds: 6 },
      { match: 'rev-parse --short', seconds: 4 },
    ])
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-k' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      const composer = page.getByTestId('composer-input')

      await test.step('占位：「正在读取 git 信息…」，没有「工作位置」按钮，发送被挡并说明；不会悄悄按「当前目录」发', async () => {
        await expect(page.getByTestId('new-session-workspace-pending')).toBeVisible()
        await expect(page.getByTestId('new-session-workspace-button')).toHaveCount(0)
        await expect(page.getByTestId('new-session-workspace-row')).toHaveCount(0)
        await expect(page.getByTestId('new-session-workspace')).toHaveCount(0)
        // 等的时候可以先写好消息（有草稿了，发送键禁用就只可能是因为占位）；草稿不会因为占位变成「工作位置」按钮而丢
        await composer.fill('write after the slow git')
        await expect(page.getByTestId('composer-send')).toBeDisabled()
        await expect(page.getByTestId('composer-blocked')).toBeVisible()
        // 把输入框打个记号：之后它还是同一个 DOM 节点，说明表单没有被重新挂载、页面没有整页重载
        await composer.evaluate((el) => el.setAttribute('data-e2e-same-node', '1'))
      })

      await test.step('算完之后：不刷新、不重新进新会话页，「工作位置」自己出现，默认「新 worktree」', async () => {
        const button = await waitForWorkspacePicker(page)
        await expect(button).toHaveAttribute('data-mode', 'worktree')
        const picker = await openWorkspacePopover(page)
        await expect(picker.getByTestId('new-session-workspace-worktree')).toHaveAttribute(
          'data-state',
          'on',
        )
        await expect(page.getByTestId('new-session-workspace-disabled')).toHaveCount(0)
        await expect(composer).toHaveAttribute('data-e2e-same-node', '1')
        await expect(composer).toHaveValue('write after the slow git')
        await expect(page.getByTestId('composer-send')).toBeEnabled()
        await expect(page.getByTestId('new-session-base')).toHaveValue('main')
        // 占位期间什么都没发出去：还在新会话页，没有 worktree
        await expectOnNewSessionPage(page, name)
        expect(worktreeNames(project)).toEqual([])
      })

      await test.step('发送：真的建了 worktree（不是悄悄退回「当前目录」）', async () => {
        await closeWorkspacePopover(page)
        await page.getByTestId('composer-send').click()
        await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
        const chip = page.getByTestId('session-branch')
        await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
        await expect.poll(() => worktreeNames(project)).toHaveLength(1)
        const worktree = join(project, '.anycon', 'worktrees', worktreeNames(project)[0] ?? '')
        await expect
          .poll(() => existsSync(join(worktree, MOCK_FILE)), { timeout: TURN_TIMEOUT })
          .toBe(true)
        expect(readFileSync(join(worktree, INCLUDED_FILE), 'utf8')).toBe(INCLUDED_CONTENT)
        expect(existsSync(join(project, MOCK_FILE))).toBe(false)
        expect(git(state, project, 'status', '--porcelain')).toBe('')
        await expectIdle(page)
      })
    } finally {
      unslow()
      await paired.context.close()
    }
  })

  // 计算比“前台 3 秒 + 网页补拉 3 秒”还慢（这里约 9 秒）时，占位一直撑到 `workspace.changed { updated }` 之后重新拉到的项目不再
  // `git_pending`（web.md § 4.5）。以前补拉超时后占位就消失，约 2.5 秒里既没有占位也没有「工作位置」按钮、发送放行，会悄悄建成「当前目录」。
  test('k2. 冷缓存算得比补拉还慢：占位一直撑到 updated 之后，其间发送一直被挡住（不会悄悄按「当前目录」发）', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-gap')
    makeRepoProject(state, name)
    const unslow = slowGit(state, name, [
      { match: 'symbolic-ref', seconds: 6 },
      { match: 'rev-parse --short', seconds: 4 },
    ])
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-k2' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      await page.getByTestId('composer-input').fill('write while git is still being read')
      const pending = page.getByTestId('new-session-workspace-pending')
      // 每 100 ms 采一次样直到「工作位置」按钮出现：任何一次“没有按钮、占位也不在，或者发送可点”都是悄悄退回「当前目录」的窗口。
      // 三样东西在页面里**一次**读出来（不能分开 count()：占位变成按钮是同一次渲染，分开读会在两次读之间错过它，误报“都没有”）
      const snapshot = (): Promise<{ picker: number; pending: number; sendEnabled: boolean }> =>
        page.evaluate(() => ({
          picker: document.querySelectorAll('[data-testid="new-session-workspace-button"]').length,
          pending: document.querySelectorAll('[data-testid="new-session-workspace-pending"]')
            .length,
          sendEnabled:
            document.querySelector<HTMLButtonElement>('[data-testid="composer-send"]')?.disabled ===
            false,
        }))
      const leaks: string[] = []
      const startedAt = Date.now()
      for (;;) {
        const now = await snapshot()
        if (now.picker > 0) break
        const at = Date.now() - startedAt
        if (at > 45_000) throw new Error('the workspace row never appeared')
        if (now.pending === 0) leaks.push(`${at}ms: no placeholder and no workspace button`)
        else if (now.sendEnabled) leaks.push(`${at}ms: send enabled`)
        await page.waitForTimeout(100)
      }
      expect(leaks).toEqual([])
      // 「工作位置」按钮不是一开始就在的：采样跨过了好几秒，包括补拉的 3 秒超时（以前占位就是在那之后消失的）
      expect(Date.now() - startedAt).toBeGreaterThan(2000)
      await expect(pending).toHaveCount(0)
      await expect(page.getByTestId('composer-send')).toBeEnabled()
    } finally {
      unslow()
      await paired.context.close()
    }
  })

  test('l. 已归档的 worktree 会话：没有输入框，只有「已归档 · 取消归档」；取消归档后 worktree 重建、能继续发消息', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-archived')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-l' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      const { branch, worktree } = await startCommittedWorktreeSession(
        page,
        state,
        project,
        'write then get archived',
      )
      const sessionId = page.url().match(/\/s\/(sess_[0-9A-Za-z]+)$/)?.[1] ?? ''
      expect(sessionId).not.toBe('')
      const chip = page.getByTestId('session-branch')
      const bar = page.getByTestId('session-archived-bar')
      const composer = page.getByTestId('composer-input')

      await test.step('没归档时有输入框，没有「已归档」条', async () => {
        await expect(composer).toBeVisible()
        await expect(bar).toHaveCount(0)
      })

      await test.step('归档（自动）：worktree 干净，目录删了，分支留着', async () => {
        await page.getByTestId('session-archive').click()
        const dialog = page.getByTestId('archive-worktree-dialog')
        await expect(dialog).toBeVisible()
        await dialog.getByTestId('archive-confirm').click()
        await expect(dialog).toHaveCount(0)
        await expect(chip).toHaveAttribute('data-state', 'removed', { timeout: TURN_TIMEOUT })
        await expect.poll(() => existsSync(worktree)).toBe(false)
      })

      await test.step('已归档：没有输入框和发送键，有「已归档」条和「取消归档」；刷新后仍然是这样', async () => {
        const check = async (): Promise<void> => {
          await expect(bar).toBeVisible({ timeout: CONNECT_TIMEOUT })
          await expect(page.getByTestId('session-archived-hint')).toBeVisible()
          await expect(page.getByTestId('session-unarchive')).toBeEnabled()
          await expect(composer).toHaveCount(0)
          await expect(page.getByTestId('composer')).toHaveCount(0)
          await expect(page.getByTestId('composer-send')).toHaveCount(0)
        }
        await check()
        await page.reload()
        await expectNodeOnline(page)
        await check()
      })

      await test.step('daemon 也拒绝给已归档的会话发消息（invalid / archived）：CLI 退出码 2，提示 anycon unarchive；不会重建 worktree', async () => {
        const sent = await runCliResult(state, [
          'send',
          sessionId,
          'write while archived',
          '--no-wait',
        ])
        expect(sent.code).toBe(2)
        expect(sent.stderr).toMatch(/unarchive/)
        expect(existsSync(worktree)).toBe(false)
        await expect(bar).toBeVisible()
      })

      await test.step('点「取消归档」：输入框回来，worktree 按分支重建（.env 也复制回来），文件还在', async () => {
        await page.getByTestId('session-unarchive').click()
        await expect(composer).toBeVisible({ timeout: TURN_TIMEOUT })
        await expect(bar).toHaveCount(0)
        await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
        await showChat(page)
        await expect(gitEvent(page, 'worktree_restored')).toBeVisible()
        await expect.poll(() => existsSync(worktree)).toBe(true)
        expect(readFileSync(join(worktree, INCLUDED_FILE), 'utf8')).toBe(INCLUDED_CONTENT)
        expect(lines(join(worktree, MOCK_FILE))).toHaveLength(1)
        expect(git(state, worktree, 'rev-parse', '--abbrev-ref', 'HEAD')).toBe(branch)
        expect(git(state, worktree, 'status', '--porcelain')).toBe('')
      })

      await test.step('再发 write：写进重建的 worktree，不在主目录里', async () => {
        await sendMessage(page, 'write after unarchive')
        await expect(page.getByTestId('session-branch-uncommitted')).toBeVisible({
          timeout: TURN_TIMEOUT,
        })
        await expectIdle(page)
        expect(lines(join(worktree, MOCK_FILE))).toHaveLength(2)
        expect(git(state, worktree, 'status', '--porcelain')).toBe(` M ${MOCK_FILE}`)
        expect(existsSync(join(project, MOCK_FILE))).toBe(false)
        await expect(bar).toHaveCount(0)
      })
    } finally {
      await paired.context.close()
    }
  })

  test('m. daemon 刚重启、一批仓库项目同时是冷缓存：打开新建会话表单后「工作位置」一定出现，会话建在 worktree 里', async ({
    browser,
  }) => {
    // daemon 刚重启、缓存全冷：workspace 里有十来个带提交的仓库，daemon 重启之后第一次 `workspace.list` 同时为所有项目现算 git
    // 元数据（每个项目 5 条 git 并发），以前只等 50 ms，几乎只有 2–4 个项目带得上 `git`。共享的 daemon 属于 global-setup、
    // 整套用例共用，不能重启，所以这个用例自己起一个节点（helpers/daemon.ts）、建好项目、重启它、再让浏览器配对上去。
    test.setTimeout(180_000)
    const state = readState()
    const node = await startExtraDaemon(state, uniqueName('e2e-restart-node'))
    try {
      const names = Array.from({ length: 12 }, (_, i) => uniqueName(`wt-boot${i}`))
      const repos = { gitConfig: state.gitConfig, workspaceDir: node.workspaceDir }
      const projects = names.map((n) => makeRepoProject(repos, n))
      const name = names.at(-1) ?? ''
      const project = projects.at(-1) ?? ''
      // 重启：进程换掉，缓存清空；项目都在磁盘上，还没有任何人问过它们的 git 状态
      await node.restart()

      const paired = await pairNewBrowser(
        browser,
        { ...state, daemonHome: node.home },
        { deviceName: 'e2e-worktree-m' },
      )
      const { page } = paired
      try {
        await openNewSession(page, name)
        // 占位（如果有的话）之后是「工作位置」按钮；不在占位期间点发送
        await waitForWorkspacePicker(page)
        const picker = await openWorkspacePopover(page)
        await expect(picker.getByTestId('new-session-workspace-worktree')).toHaveAttribute(
          'data-state',
          'on',
        )
        await sendFirstMessage(page, 'write after the restart')
        await expect(page.getByTestId('session-branch')).toHaveAttribute('data-state', 'active', {
          timeout: TURN_TIMEOUT,
        })
        await expect.poll(() => worktreeNames(project)).toHaveLength(1)
        const worktree = join(project, '.anycon', 'worktrees', worktreeNames(project)[0] ?? '')
        await expect
          .poll(() => existsSync(join(worktree, MOCK_FILE)), { timeout: TURN_TIMEOUT })
          .toBe(true)
        expect(existsSync(join(project, MOCK_FILE))).toBe(false)
        await expectIdle(page)
        // 别的项目也都是仓库：回项目首页，每个项目的新建会话表单都有这一行
        await gotoNodeHome(page)
        await openNewSession(page, names[0] ?? '')
        await waitForWorkspacePicker(page)
      } finally {
        await paired.context.close()
      }
    } finally {
      await node.stop()
    }
  })

  test('n. 等审批时不能归档：「归档」按钮禁用并说明、daemon 回 busy；答复之后才能归档', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-busy')
    const project = makeRepoProject(state, name)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-n' })
    const { page } = paired
    try {
      await openNewSession(page, name)
      await waitForWorkspacePicker(page)
      // mock 的 `run permission`：先要一个工具权限，答复之前 turn 一直挂着
      await sendFirstMessage(page, 'run permission')
      const sessionId = page.url().match(/\/s\/(sess_[0-9A-Za-z]+)$/)?.[1] ?? ''
      expect(sessionId).not.toBe('')
      const chip = page.getByTestId('session-branch')
      await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
      const card = pendingCard(page)
      await expect(card).toBeVisible({ timeout: TURN_TIMEOUT })
      await expect(page.getByTestId('session-status')).toHaveAttribute(
        'data-status',
        'waiting_permission',
      )
      await expect.poll(() => worktreeNames(project)).toHaveLength(1)
      const worktree = join(project, '.anycon', 'worktrees', worktreeNames(project)[0] ?? '')

      await test.step('Web：「归档」禁用，旁边说明先停止或答复；点不动', async () => {
        const archive = page.getByTestId('session-archive')
        await expect(archive).toBeDisabled()
        await expect(page.getByTestId('session-archive-blocked')).toBeVisible()
        await expect(archive).toHaveAttribute('aria-describedby', /.+/)
        // 强行点（force 跳过可操作性检查）也弹不出归档确认框
        await archive.click({ force: true })
        await expect(page.getByTestId('archive-worktree-dialog')).toHaveCount(0)
      })

      await test.step('daemon：archive 回 busy，什么都没变（turn 还在等、worktree 还在、没有归档）', async () => {
        const res = await runCliResult(state, ['archive', sessionId, '--yes'])
        expect(res.code).not.toBe(0)
        expect(res.stderr).toMatch(/busy: a turn is in progress or waiting for an answer/)
        await expect(pendingCard(page)).toBeVisible()
        await expect(page.getByTestId('session-status')).toHaveAttribute(
          'data-status',
          'waiting_permission',
        )
        await expect(chip).toHaveAttribute('data-state', 'active')
        await expect(page.getByTestId('session-archived-bar')).toHaveCount(0)
        expect(existsSync(worktree)).toBe(true)
      })

      await test.step('答复（允许）→ turn 结束 → 「归档」可用，说明消失；归档成功', async () => {
        await card.getByTestId('permission-option-allow').click()
        await expect(resolvedCards(page)).toHaveCount(1, { timeout: TURN_TIMEOUT })
        await expectIdle(page)
        await expect(page.getByTestId('session-archive-blocked')).toHaveCount(0)
        await expect(page.getByTestId('session-archive')).toBeEnabled()
        await page.getByTestId('session-archive').click()
        const dialog = page.getByTestId('archive-worktree-dialog')
        await expect(dialog).toBeVisible()
        await dialog.getByTestId('archive-confirm').click()
        await expect(dialog).toHaveCount(0)
        await expect(page.getByTestId('session-archived-bar')).toBeVisible({
          timeout: TURN_TIMEOUT,
        })
      })
    } finally {
      await paired.context.close()
    }
  })

  test('o. 项目改名：repair 进行中 send 是 busy；改名返回之后 worktree 会话照常（active、没有 worktree_missing），马上能继续写', async ({
    browser,
  }) => {
    test.setTimeout(150_000)
    const state = readState()
    const name = uniqueName('wt-rename')
    const renamed = uniqueName('wt-renamed')
    const project = makeRepoProject(state, name)
    const newProject = join(state.workspaceDir, renamed)
    const paired = await pairNewBrowser(browser, state, { deviceName: 'e2e-worktree-o' })
    const { page } = paired
    // 让改名后的 `git worktree repair` 慢一点（规则按“跑 git 时的目录名”，改名之后是新名字）：把“repair 进行中”拉成一个测得到的窗口
    const unslow = slowGit(state, renamed, [{ match: 'worktree repair', seconds: 5 }])
    try {
      await openNewSession(page, name)
      const { branch, worktree } = await startCommittedWorktreeSession(
        page,
        state,
        project,
        'write before the project is renamed',
      )
      const sessionId = page.url().match(/\/s\/(sess_[0-9A-Za-z]+)$/)?.[1] ?? ''
      expect(sessionId).not.toBe('')
      const chip = page.getByTestId('session-branch')
      const dirName = worktree.split('/').pop() ?? ''
      let restoredBefore = 0

      // 改名要求项目里没有会话的 agent 进程（空闲的也算，空闲 10 分钟才会被收掉）：先归档再取消归档，进程就收掉了，
      // 会话和 worktree 都原样回来（干净的 worktree 归档时删掉、取消归档时按分支重建），下一条消息才会重新拉起 agent
      await test.step('让会话没有 agent 进程：归档 → 取消归档', async () => {
        const archived = await runCliResult(state, ['archive', sessionId], { timeoutMs: 60_000 })
        expect(archived.code).toBe(0)
        await expect(page.getByTestId('session-archived-bar')).toBeVisible({
          timeout: TURN_TIMEOUT,
        })
        const back = await runCliResult(state, ['unarchive', sessionId], { timeoutMs: 60_000 })
        expect(back.code).toBe(0)
        await expect(page.getByTestId('composer-input')).toBeVisible({ timeout: TURN_TIMEOUT })
        await expect(chip).toHaveAttribute('data-state', 'active', { timeout: TURN_TIMEOUT })
        await expect.poll(() => existsSync(join(worktree, MOCK_FILE))).toBe(true)
        // 取消归档本身重建过一次 worktree：记下消息流里已有几条，改名之后不该再多
        await showChat(page)
        restoredBefore = await gitEvent(page, 'worktree_restored').count()
        expect(restoredBefore).toBe(1)
      })

      // 改名（CLI 直接走本机 socket，响应要等改名 + `git worktree repair` + 会话改名都做完）；不 await，先去探它进行中的窗口
      const renaming = runCliResult(state, ['projects', 'rename', name, renamed], {
        timeoutMs: 60_000,
      })
      await test.step('改名进行中（目录已经换名、repair 还在睡）：给会话发消息是 busy，什么都不会发生', async () => {
        await waitFor('the project directory to be renamed', () => existsSync(newProject), {
          timeoutMs: 20_000,
          intervalMs: 20,
        })
        const during = await runCliResult(state, [
          'send',
          sessionId,
          'write during the rename',
          '--no-wait',
        ])
        expect(during.code).not.toBe(0)
        expect(during.stderr).toMatch(/busy/)
      })

      await test.step('改名返回：成功，worktree 登记指向新目录', async () => {
        const done = await renaming
        expect(done.stderr).toBe('')
        expect(done.code).toBe(0)
        expect(existsSync(project)).toBe(false)
        const moved = join(newProject, '.anycon', 'worktrees', dirName)
        expect(existsSync(join(moved, MOCK_FILE))).toBe(true)
        // repair 之后主仓库登记的 worktree 路径是新的，worktree 里的 .git 指针也指回新的管理目录
        expect(git(state, newProject, 'worktree', 'list', '--porcelain')).toContain(
          `worktree ${realpathSync(moved)}`,
        )
        expect(git(state, moved, 'rev-parse', '--abbrev-ref', 'HEAD')).toBe(branch)
      })

      await test.step('马上从网页继续发消息：照常运行，写进（搬了家的）worktree，没有 missing / 重建', async () => {
        await sendMessage(page, 'write right after the rename')
        const moved = join(newProject, '.anycon', 'worktrees', dirName)
        await expect
          .poll(() => lines(join(moved, MOCK_FILE)).length, { timeout: TURN_TIMEOUT })
          .toBe(2)
        await expectIdle(page)
        await expect(chip).toHaveAttribute('data-state', 'active')
        await expect(gitEvent(page, 'worktree_missing')).toHaveCount(0)
        await expect(gitEvent(page, 'worktree_restored')).toHaveCount(restoredBefore)
        await expect(page.getByTestId('session-branch-uncommitted')).toBeVisible()
        expect(git(state, moved, 'status', '--porcelain')).toBe(` M ${MOCK_FILE}`)
        expect(existsSync(join(newProject, MOCK_FILE))).toBe(false)
      })
    } finally {
      unslow()
      await paired.context.close()
    }
  })
})
