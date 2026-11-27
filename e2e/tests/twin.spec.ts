/**
 * Twin（docs/spec/daemon.md § 26，web.md § 14.8）：真实 hub + 真实 daemon + 真实 web + 假的判断后端（`helpers/judge.ts`）。
 *
 * 用到判断后端的节点由这个文件自己起（`startExtraDaemon`，不动共享的 daemon），配置在启动前写进它的 `config.toml`：
 * `[judge.backends.e2efake]`（`kind = "api"`、`format = "anthropic"`、`base_url` 指向用例里起的假服务，密钥来自只在这个 daemon
 * 环境里的变量）。agent 是 mock（它声明了 `no_push` 和 `unattended` 的 `ask` 模式，派出的会话回显里能看到 `no_push=on`）。
 *
 * 串行，一个额外节点 + 一个配对的浏览器；节点和浏览器的设备在结束时去掉。节点的环境变量 `ANYCON_TWIN_COALESCE_MS`（只用于测试）
 * 把分拣的合并窗口从 60 秒缩短到 2 秒（§ 26.3），每个等唤醒的步骤都是对「假后端收到某个请求」的有界等待。
 *
 *   a. Twin 页：没配置时的状态；打开一个项目、设预算（第一次保存先暂停并自动试跑）；试跑显示假后端的计划，节点上什么都没派、没问
 *   b. 「记一项」里交给 Twin →「就这么办」唤醒 → 假后端回 `dispatch` → 开了 worktree 会话；清单里这一项显示 Twin 在处理；Twin 页的运行记录
 *      里有这次唤醒、决定和理由；请求里这一项在随机边界里、系统提示里没有项的内容
 *   c. 假后端回 `ask_user` → 「待处理」里是 Twin 的问题 → 回答时勾「以后都这样」→ 规则编辑框带着内容打开 → 保存后节点上有这条规则；
 *      下一次唤醒的请求里带着你的回答和这条规则
 *   d. 接手：在 Twin 派出的会话里发一条消息 → 这一项变成「我」处理，「经过」里有「你接手了」
 *   e. 暂停：新的触发不唤醒（假后端没收到请求）；同时等周期项（§ 26.9）按规则在下一分钟复制出一项（复制不唤醒）
 *   f. 继续：试跑的请求里没有接手的那一项；「就这么办」之后唤醒恢复，新的项被派出
 */
import { randomBytes } from 'node:crypto'

import { expect, test, type Page } from '@playwright/test'

import { runCli } from '../helpers/cli.js'
import { startExtraDaemon, type ExtraDaemon } from '../helpers/daemon.js'
import { git, makeRepoProject } from '../helpers/git.js'
import { containsText, exactText } from '../helpers/i18n.js'
import {
  dataBlocks,
  isTriage,
  itemsOf,
  startJudge,
  type FakeJudge,
  type JudgeRequest,
  type TwinItem,
  type TwinPlanItem,
} from '../helpers/judge.js'
import { waitFor } from '../helpers/process.js'
import { readState } from '../helpers/state.js'
import {
  COALESCE_MS,
  JUDGE_KEY,
  cliJson,
  judgeToml,
  removeNodesFromAccount,
  sessions,
  todoByTitle,
  todoHistory,
  todos,
  triageRequests,
  twinEnv,
  twinRuns,
  twinStatus,
  waitForTriage,
} from '../helpers/twin.js'
import { CONNECT_TIMEOUT, nodeBase, pairNewBrowser, type PairedBrowser } from '../helpers/ui.js'

const RUN = randomBytes(3).toString('hex')
const NODE_NAME = `e2e-twin-${RUN}`
const PROJECT = `e2e-twin-${RUN}`
const TITLE_DRY = `e2e-${RUN} dry run item`
const TITLE_DISPATCH = `e2e-${RUN} dispatch item`
const TITLE_ASK = `e2e-${RUN} ask item`
const TITLE_PAUSED = `e2e-${RUN} paused item`
const TITLE_PERIODIC = `e2e-${RUN} periodic item`
const QUESTION = `e2e-${RUN} which branch names should I use?`
const OPTION_A = 'Short branch names'
const OPTION_B = 'Long branch names'
const RULE_TEXT = `e2e-${RUN} prefer short branch names`
const NOTE = `e2e-${RUN} keep the change small`
const REASON = {
  dry: `e2e-${RUN} dry-run plan: dispatch`,
  dispatch: `e2e-${RUN} this one is ready to go`,
  ask: `e2e-${RUN} I need your preference first`,
  askDone: `e2e-${RUN} now I know the preference`,
  later: `e2e-${RUN} paused item is ready`,
}

interface DaemonRule {
  id: string
  text: string
  status: string
  source: { kind: string; session_id?: string; request_id?: string }
}

/** 假后端现在的行为：试跑时让「试跑项」派发，真的唤醒时让它等着（试跑和唤醒的请求一样，靠这个区分）。 */
let phase: 'dry' | 'live' = 'dry'

const wait = (item: TwinItem): TwinPlanItem => ({
  id: item.id,
  action: 'wait',
  reason: 'e2e: wait',
})

/** 按项的标题和请求里它带的内容决定（内容里有 `dispatches: N`、之前的问答，所以不用记状态）。 */
function decide(item: TwinItem): TwinPlanItem {
  if (item.title === TITLE_DRY) {
    return phase === 'dry'
      ? { id: item.id, action: 'dispatch', reason: REASON.dry, note: NOTE }
      : wait(item)
  }
  if (item.title === TITLE_DISPATCH && item.dispatches === 0) {
    return { id: item.id, action: 'dispatch', reason: REASON.dispatch, note: NOTE }
  }
  if (item.title === TITLE_PAUSED && item.dispatches === 0) {
    return { id: item.id, action: 'dispatch', reason: REASON.later }
  }
  if (item.title === TITLE_ASK) {
    if (/\nanswer: (?!\(unanswered\))/.test(item.text) && item.dispatches === 0) {
      return { id: item.id, action: 'dispatch', reason: REASON.askDone }
    }
    if (!item.text.includes('question (')) {
      return {
        id: item.id,
        action: 'ask_user',
        reason: REASON.ask,
        question: QUESTION,
        options: [OPTION_A, OPTION_B],
      }
    }
  }
  return wait(item)
}

const respond = (request: JudgeRequest): { items: TwinPlanItem[] } => ({
  items: isTriage(request) ? itemsOf(request).map(decide) : [],
})

/** 提示里被随机边界包起来的部分之外的文字。 */
function outsideBlocks(request: JudgeRequest): string {
  let text = request.prompt
  for (const b of dataBlocks(request)) text = text.replace(b.text, '')
  return text
}

const inBlocks = (request: JudgeRequest, needle: string): boolean =>
  dataBlocks(request).some((b) => b.text.includes(needle))

/** `HH:MM`（节点和测试在同一台机器上，周期项按机器的本地时间算）。 */
function clock(d: Date): string {
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

const gitState = () => ({ gitConfig: readState().gitConfig })

test.describe.serial('Twin', () => {
  let judge: FakeJudge
  let node: ExtraDaemon
  let device: PairedBrowser
  let page: Page
  let base = ''
  let short = ''
  const home = () => ({ daemonHome: node.home })
  const rules = async (): Promise<DaemonRule[]> =>
    (await cliJson<{ rules: DaemonRule[] }>(home(), ['rule', 'ls', '--all'])).rules

  test.beforeAll(async ({ browser }) => {
    test.setTimeout(180_000)
    const state = readState()
    test.skip(!state.daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    judge = await startJudge(respond)
    node = await startExtraDaemon(state, NODE_NAME, {
      env: twinEnv(),
      toml: judgeToml(judge),
    })
    // 先建项目和「试跑项」，再配对浏览器：浏览器连上时拉的第一次清单和项目列表就已经有它们
    makeRepoProject({ ...gitState(), workspaceDir: node.workspaceDir }, PROJECT)
    await runCli(home(), ['todo', 'add', TITLE_DRY, '--project', PROJECT, '--twin'])
    device = await pairNewBrowser(
      browser,
      { ...state, daemonHome: node.home },
      { deviceName: `e2e-twin-${RUN}` },
    )
    page = device.page
    base = nodeBase(page)
    short = base.split('/').pop()!
  })

  test.afterAll(async ({ request }) => {
    await device?.context.close()
    await node?.stop()
    await judge?.stop()
    await removeNodesFromAccount(request, readState(), [NODE_NAME])
  })

  test('a. 没配置时的状态；在节点设置 → Twin 里打开项目、设预算；试跑显示计划，什么都没派没问', async () => {
    test.setTimeout(120_000)
    await page.goto(`/twin/${short}`)
    const card = page.getByTestId('twin-card')
    await expect(card).toBeVisible({ timeout: CONNECT_TIMEOUT })
    // 没配置：节点上和页面上都是「未配置」，没有项目；判断后端是配好的，所以不说缺后端。页面上只有一个「去设置」，没有操作按钮
    await expect(page.getByTestId('twin-state')).toHaveText(exactText('twin.state.not_configured'))
    await expect(card).toContainText(containsText('twin.noProjectsYet'))
    await expect(card).not.toContainText(containsText('twin.noTriage'))
    await expect(card.getByRole('button', { name: exactText('twin.dryRun') })).toHaveCount(0)
    await expect(card.getByRole('checkbox')).toHaveCount(0)
    expect(await twinStatus(home())).toMatchObject({
      state: 'not_configured',
      configured: false,
      enabled: true,
      scope: { paused: false, projects: [] },
    })

    // 范围和预算在节点设置 → Twin 里：从「去设置」进去
    await card.getByRole('link', { name: exactText('twin.setUp') }).click()
    await expect(page).toHaveURL(/\/settings\?(.*&)?tab=twin/)
    const tab = page.getByTestId('twin-tab')
    await expect(tab).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(page.getByTestId('twin-tab-state')).toHaveText(
      exactText('twin.state.not_configured'),
    )
    // 没配置时不能试跑
    await expect(tab.getByTestId('twin-tab-dry-run')).toBeDisabled()

    // 打开项目、设每天派发的预算；第一次配置先以暂停保存，再自动试跑（web.md § 4.8）
    const scope = page.getByTestId('twin-tab-scope')
    await scope.getByRole('checkbox', { name: PROJECT }).check()
    await scope.getByLabel(exactText('twin.dailyDispatches')).fill('5')
    await expect(scope.getByRole('button', { name: exactText('twin.save') })).toBeEnabled({
      timeout: 15_000,
    })
    await scope.getByRole('button', { name: exactText('twin.save') }).click()

    // 范围和预算存下来了，第一次保存是暂停着的
    await expect
      .poll(async () => (await twinStatus(home())).scope)
      .toMatchObject({
        paused: true,
        projects: [PROJECT],
        daily_dispatches: 5,
      })
    // 试跑的结果：假后端的计划，每项一行（动作、理由、给 agent 的说明）。第一次保存之后网页在暂停着时自动试跑
    const decision = tab.getByTestId('twin-decision')
    await expect(decision).toHaveCount(1, { timeout: 30_000 })
    await expect(decision).toContainText(TITLE_DRY)
    await expect(decision).toContainText(containsText('twin.action.dispatch'))
    await expect(decision).toContainText(REASON.dry)
    await expect(decision).toContainText(NOTE)

    // 节点一侧：范围和预算存下来了、暂停着；试跑只记了运行记录，什么都没派、没问
    expect(await twinStatus(home())).toMatchObject({
      state: 'paused',
      scope: { paused: true, projects: [PROJECT], daily_dispatches: 5 },
      today: { dispatches: 0 },
    })
    const dry = await todoByTitle(home(), TITLE_DRY)
    expect(dry?.sessions ?? []).toEqual([])
    expect(await sessions(home())).toEqual([])
    expect(
      (await cliJson<{ questions: unknown[] }>(home(), ['twin', 'questions'])).questions,
    ).toEqual([])
    const runs = await twinRuns(home())
    expect(runs).toHaveLength(1)
    expect(runs[0]).toMatchObject({ dry_run: true, result: 'done', items: [dry!.id] })
    expect(runs[0]!.decisions[0]).toMatchObject({
      todo_id: dry!.id,
      action: 'dispatch',
      outcome: 'executed',
    })

    // 假后端收到的请求：Messages API 的格式、密钥来自 daemon 的环境变量；这一项在随机边界里，边界外没有项的内容
    const [request] = triageRequests(judge)
    expect(triageRequests(judge)).toHaveLength(1)
    expect(request).toMatchObject({
      path: '/v1/messages',
      model: 'e2e-judge',
      apiKey: JUDGE_KEY,
      anthropicVersion: '2023-06-01',
      stream: true,
    })
    expect(itemsOf(request!).map((i) => i.id)).toEqual([dry!.id])
    expect(dataBlocks(request!)[0]!.marker).toMatch(/^ANYCON_TWIN_[0-9a-f]{32}$/)
    expect(outsideBlocks(request!)).not.toContain(TITLE_DRY)
    expect(request!.system).not.toContain(TITLE_DRY)
    // 没有 Twin 连接：系统提示不提节点表
    expect(request!.system).not.toContain('Linked nodes')
  })

  test('b. 「记一项」里交给 Twin → 唤醒 → dispatch → worktree 会话；清单和 Twin 页都看得到', async () => {
    test.setTimeout(240_000)
    // 「就这么办」：取消暂停并唤醒（合并窗口缩短到 2 秒）；窗口里再记一项，两件事合进同一次唤醒
    phase = 'live'
    await page.goto(`/twin/${short}`)
    // 暂停着：「恢复」先试跑并在对话框里给出计划，「就这么办」才取消暂停
    await page.getByRole('button', { name: exactText('twin.resume') }).click()
    const plan = page.getByTestId('twin-plan-dialog')
    await expect(plan.getByTestId('twin-decision')).toHaveCount(1, { timeout: 30_000 })
    const dryRequest = triageRequests(judge).at(-1)!
    // 试跑的请求和之前一样：「试跑项」这时让它等着
    expect(itemsOf(dryRequest)).toHaveLength(1)
    await plan.getByRole('button', { name: exactText('twin.doIt') }).click()
    await expect(page.getByTestId('twin-state')).toHaveText(exactText('twin.state.idle'), {
      timeout: 15_000,
    })
    expect((await twinStatus(home())).scope.paused).toBe(false)
    const before = judge.requests.length

    await page.goto('/todos')
    await expect(page.getByTestId('todos-page')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('sidebar-capture').click()
    await expect(page.getByTestId('capture-bar')).toBeVisible()
    await page.getByTestId('capture-handler').selectOption('twin')
    await page.getByTestId('capture-project').fill(PROJECT)
    await page.getByTestId('capture-input').fill(TITLE_DISPATCH)
    await page.getByTestId('capture-send').click()
    await expect(page.getByTestId('capture-input')).toHaveValue('')
    await expect.poll(async () => (await todoByTitle(home(), TITLE_DISPATCH))?.handler).toBe('twin')
    const item = (await todoByTitle(home(), TITLE_DISPATCH))!

    // 唤醒：假后端收到请求，这一项在边界里；回 dispatch 之后节点上开了会话
    const request = await waitForTriage(
      judge,
      'the wake for the dispatch item',
      (r) => inBlocks(r, item.id),
      { after: before },
    )
    expect(itemsOf(request).find((i) => i.id === item.id)).toMatchObject({
      title: TITLE_DISPATCH,
      project: PROJECT,
      dispatches: 0,
    })
    expect(outsideBlocks(request)).not.toContain(TITLE_DISPATCH)
    expect(request.system).not.toContain(TITLE_DISPATCH)
    await expect
      .poll(async () => (await todoByTitle(home(), TITLE_DISPATCH))?.sessions?.length ?? 0, {
        timeout: 30_000,
      })
      .toBe(1)
    const sessionId = (await todoByTitle(home(), TITLE_DISPATCH))!.sessions![0]!
    const listed = (await sessions(home())).find((s) => s.id === sessionId)
    expect(listed).toMatchObject({ project: PROJECT })
    // worktree 会话：项目里多了一条 anycon/ 分支和对应的 worktree
    await expect
      .poll(() => git(gitState(), `${node.workspaceDir}/${PROJECT}`, 'worktree', 'list'), {
        timeout: 30_000,
      })
      .toMatch(/\.anycon\/worktrees\//)
    // Twin 派发固定「不 push」：「经过」里的 dispatched 记着，说明接在第一条消息后面
    await expect
      .poll(
        async () => (await todoHistory(home(), item.id)).find((e) => e.kind === 'result')?.text,
        {
          timeout: 30_000,
        },
      )
      .toContain('mode=ask] no_push=on')
    const events = await todoHistory(home(), item.id)
    expect(events.find((e) => e.kind === 'dispatched')).toMatchObject({ session_id: sessionId })
    expect(events.find((e) => e.kind === 'twin_decision')).toMatchObject({
      action: 'dispatch',
      reason: REASON.dispatch,
    })

    // 清单页：这一项显示 Twin 在处理
    await page.goto('/todos')
    const row = page.locator('[data-testid="todo-row"]').filter({ hasText: TITLE_DISPATCH })
    await expect(row.getByTestId('todo-handler')).toHaveAttribute('data-state', 'twin', {
      timeout: CONNECT_TIMEOUT,
    })
    await row.getByTestId('todo-main').click()
    await expect(page.getByTestId('todo-detail')).toBeVisible()
    // 会话结束也会唤醒 Twin（之后的决定是「等」）：按理由找派发的那一条
    const decisionRow = page
      .locator('[data-testid="history-row"][data-kind="twin_decision"]')
      .filter({ hasText: REASON.dispatch })
    await expect(decisionRow).toHaveCount(1)
    // 「经过」里的「打开这次运行」直达 Twin 页里的这次运行：展开着，有这条决定
    await decisionRow.getByRole('link', { name: exactText('twin.openRun') }).click()
    await expect(page).toHaveURL(/\/twin\?.*run=/)
    await expect(page.getByTestId('twin-run').first()).toBeVisible({ timeout: 15_000 })
    const decision = page.getByTestId('twin-decision').filter({ hasText: TITLE_DISPATCH })
    await expect(decision).toContainText(REASON.dispatch)
    await expect(decision).toContainText(NOTE)
    // Twin 页的运行记录里有这次唤醒（不是试跑）
    const runs = await twinRuns(home())
    const wake = runs.find((r) => r.decisions.some((d) => d.todo_id === item.id))!
    expect(wake).toMatchObject({ dry_run: false, result: 'done' })
    expect(wake.decisions.find((d) => d.todo_id === item.id)).toMatchObject({
      action: 'dispatch',
      outcome: 'executed',
      session_id: sessionId,
    })
    expect(wake.triggers).toContain('todo')
  })

  test('c. ask_user → 待处理里的 Twin 问题 → 回答并勾「以后都这样」→ 规则编辑框 → 保存后节点上有规则；下一次唤醒带着回答', async () => {
    test.setTimeout(300_000)
    const before = judge.requests.length
    await runCli(home(), ['todo', 'add', TITLE_ASK, '--project', PROJECT, '--twin'])
    const item = (await todoByTitle(home(), TITLE_ASK))!
    await waitForTriage(judge, 'the wake that asks a question', (r) => inBlocks(r, item.id), {
      after: before,
    })

    // 待处理里是 Twin 的问题（模型写的，按「agent 写的」纯文字显示）
    await page.goto('/inbox')
    const row = page.locator('[data-testid="inbox-row"][data-kind="twin_question"]')
    await expect(row).toBeVisible({ timeout: 60_000 })
    await expect(row).toContainText(TITLE_ASK)
    await row.getByTestId('inbox-row-main').click()
    const detail = page.locator('[data-testid="inbox-detail"][data-kind="twin_question"]')
    await expect(detail.getByTestId('twin-question')).toContainText(QUESTION)
    await expect(detail.getByTestId('twin-question')).toContainText(
      containsText('twin.agentWritten'),
    )
    const questions = (
      await cliJson<{
        questions: { id: string; question: string; status: string; options: string[] }[]
      }>(home(), ['twin', 'questions'])
    ).questions
    expect(questions).toHaveLength(1)
    expect(questions[0]).toMatchObject({
      question: QUESTION,
      status: 'open',
      options: [OPTION_A, OPTION_B],
    })
    const questionId = questions[0]!.id

    // 回答：勾「以后都这样」再点选项；答复成功之后才打开「记成规则」的同一个编辑框
    await detail.getByTestId('always-do-this-box').check()
    const answerRequestsBefore = judge.requests.length
    await detail.getByRole('button', { name: OPTION_A }).click()
    const editor = page.getByTestId('rule-editor')
    await expect(editor).toBeVisible({ timeout: 15_000 })
    await expect(editor.getByTestId('record-agent')).toContainText(QUESTION)
    await expect(editor.getByTestId('record-choice')).toContainText(OPTION_A)
    await expect(editor.getByTestId('rule-editor-text')).toHaveValue(new RegExp(OPTION_A))
    // 回答已经送到节点（编辑框是答复成功之后才开的），规则还没有
    await expect
      .poll(
        async () =>
          (await cliJson<{ questions: unknown[] }>(home(), ['twin', 'questions'])).questions.length,
      )
      .toBe(0)
    expect(await rules()).toEqual([])
    // 预填原样不能保存：改成自己的话再存
    await editor.getByTestId('rule-editor-text').fill(RULE_TEXT)
    await editor.getByTestId('rule-editor-save').click()
    await expect(page.getByTestId('rule-editor')).toHaveCount(0)
    await expect
      .poll(async () => (await rules()).find((r) => r.text === RULE_TEXT)?.status)
      .toBe('active')
    const rule = (await rules()).find((r) => r.text === RULE_TEXT)!
    expect(rule.source).toMatchObject({ kind: 'answer', request_id: questionId })
    expect(rule.source.session_id).toBeUndefined()

    // 回答触发一次唤醒：请求里带着这项的问答；假后端回 dispatch，节点上派出会话。合并窗口很短，这次唤醒可能早于规则保存，
    // 所以规则要到之后（保存之后手动唤醒的）请求里找
    const answered = (r: JudgeRequest): boolean =>
      itemsOf(r).some((i) => i.id === item.id && i.text.includes(`answer: ${OPTION_A}`))
    const wake = await waitForTriage(judge, 'the wake after the answer', answered, {
      after: answerRequestsBefore,
    })
    expect(itemsOf(wake).find((i) => i.id === item.id)!.text).toContain(QUESTION)
    const afterRule = judge.requests.length
    await runCli(home(), ['twin', 'wake'])
    const withRule = await waitForTriage(judge, 'a wake after the rule was saved', answered, {
      after: afterRule,
    })
    expect(withRule.system + withRule.prompt).toContain(RULE_TEXT)
    await expect
      .poll(async () => (await todoByTitle(home(), TITLE_ASK))?.sessions?.length ?? 0, {
        timeout: 30_000,
      })
      .toBe(1)
    const run = (await twinRuns(home())).find((r) =>
      r.decisions.some((d) => d.question_id === questionId),
    )!
    expect(run.decisions.find((d) => d.question_id === questionId)).toMatchObject({
      action: 'ask_user',
      outcome: 'executed',
    })
  })

  test('d. 接手：在 Twin 派出的会话里发一条消息，这一项变成「我」处理，「经过」里有「你接手了」', async () => {
    test.setTimeout(120_000)
    const item = (await todoByTitle(home(), TITLE_DISPATCH))!
    expect(item.handler).toBe('twin')
    const sessionId = item.sessions![0]!
    await page.goto(`${base}/s/${sessionId}`)
    await expect(page.getByTestId('composer-input')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('composer-input').fill(`e2e-${RUN} I will take it from here`)
    await page.getByTestId('composer-send').click()
    await expect
      .poll(async () => (await todoByTitle(home(), TITLE_DISPATCH))?.handler, {
        timeout: 30_000,
      })
      .toBe('me')
    const events = await todoHistory(home(), item.id)
    expect(events.some((e) => e.kind === 'taken_over')).toBe(true)

    await page.goto(`/todos/${short}/${item.id}`)
    const taken = page.locator('[data-testid="history-row"][data-kind="taken_over"]')
    await expect(taken).toContainText(containsText('twin.takenOver'), { timeout: CONNECT_TIMEOUT })
    // Twin 不再碰它：它不在「处理中」「等你」里
    const status = await twinStatus(home())
    expect(status.working).not.toContain(item.id)
    expect(status.waiting).not.toContain(item.id)
  })

  test('e. 暂停：新的触发不唤醒；周期项到点复制出一项（复制也不唤醒）', async () => {
    test.setTimeout(240_000)
    await page.goto(`/twin/${short}`)
    await page.getByRole('button', { name: exactText('twin.pause') }).click()
    await expect(page.getByTestId('twin-state')).toHaveText(exactText('twin.state.paused'), {
      timeout: 15_000,
    })
    expect((await twinStatus(home())).scope.paused).toBe(true)
    // 暂停之前已经在跑的唤醒要先收尾（它的请求已经发出），再记下现在收到了多少
    await expect
      .poll(async () => (await twinRuns(home())).every((r) => r.finished_ms !== undefined))
      .toBe(true)
    const mark = judge.requests.length

    // 新的触发：一项交给 Twin；一个周期项（模板的复制交给 Twin），到点的时间设成下一分钟
    const addedAt = Date.now()
    await runCli(home(), ['todo', 'add', TITLE_PAUSED, '--project', PROJECT, '--twin'])
    const template = new Date(addedAt + 60_000)
    await runCli(home(), [
      'todo',
      'add',
      TITLE_PERIODIC,
      '--project',
      PROJECT,
      '--twin',
      '--repeat',
      `daily@${clock(template)}`,
    ])
    const periodic = (await todos(home())).find(
      (t) => t.title === TITLE_PERIODIC && t.source.kind !== 'repeat',
    )!
    // 周期项：到点后（daemon 每分钟看一次）复制出新的一项，来源是这个模板，处理人照抄
    let copy: Awaited<ReturnType<typeof todos>>[number] | undefined
    await waitFor(
      'the periodic template is copied (daemon checks once a minute)',
      async () => {
        copy = (await todos(home())).find(
          (t) => t.source.kind === 'repeat' && t.source.todo_id === periodic.id,
        )
        return copy !== undefined
      },
      { timeoutMs: 130_000, intervalMs: 2_000 },
    )
    expect(copy).toMatchObject({ title: TITLE_PERIODIC, handler: 'twin', project: PROJECT })
    expect((await todoHistory(home(), periodic.id)).some((e) => e.kind === 'repeat_created')).toBe(
      true,
    )
    // 合并窗口从第一个触发算起，最长等 5 个窗口：过了它，假后端仍然什么都没收到
    await waitFor(
      'the longest merge wait has passed',
      () => Date.now() - addedAt > 6 * COALESCE_MS,
      {
        timeoutMs: 30_000,
        intervalMs: 1_000,
      },
    )
    expect(judge.requests.slice(mark)).toEqual([])
    expect((await twinStatus(home())).state).toBe('paused')
  })

  test('f. 继续：试跑的请求里没有接手的那一项；「就这么办」之后恢复唤醒', async () => {
    test.setTimeout(300_000)
    const taken = (await todoByTitle(home(), TITLE_DISPATCH))!
    const paused = (await todoByTitle(home(), TITLE_PAUSED))!
    const copy = (await todos(home())).find(
      (t) => t.title === TITLE_PERIODIC && t.source.kind === 'repeat',
    )!
    const mark = judge.requests.length
    await page.goto(`/twin/${short}`)
    // 暂停着：恢复先试跑并在对话框里给出计划（web.md § 14.8）
    expect((await twinStatus(home())).scope.paused).toBe(true)
    await page.getByRole('button', { name: exactText('twin.resume') }).click()
    const plan = page.getByTestId('twin-plan-dialog')
    await expect(plan.getByTestId('twin-decision').filter({ hasText: TITLE_PAUSED })).toBeVisible({
      timeout: 30_000,
    })
    const dryRun = triageRequests(judge).at(-1)!
    const ids = itemsOf(dryRun).map((i) => i.id)
    expect(ids).toContain(paused.id)
    expect(ids).toContain(copy.id)
    expect(ids).not.toContain(taken.id)

    await plan.getByRole('button', { name: exactText('twin.doIt') }).click()
    await expect(page.getByTestId('twin-state')).toHaveText(exactText('twin.state.idle'), {
      timeout: 15_000,
    })
    const live = await waitForTriage(
      judge,
      'the wake after resuming',
      (r) => inBlocks(r, paused.id),
      {
        after: mark + 1,
      },
    )
    expect(itemsOf(live).map((i) => i.id)).not.toContain(taken.id)
    await expect
      .poll(async () => (await todoByTitle(home(), TITLE_PAUSED))?.sessions?.length ?? 0, {
        timeout: 30_000,
      })
      .toBe(1)
    expect((await twinStatus(home())).today.dispatches).toBeGreaterThanOrEqual(3)
  })
})
