/**
 * 规则（docs/spec/daemon.md § 23，docs/spec/web.md § 4.14）：真实 hub + daemon（mock agent）+ web。
 *
 *   a. 规则页 `/rules` 新建一条：节点上（`anycon rule ls`）是生效的、手写的；页面上有全文和字数预算
 *   b. 派发一项清单：对话框里列出会注入的规则；派出的会话的系统提示里有它们（mock agent 回显 `system_prompt_append` 的前 32 个字符，
 *      「经过」里的结果就是它的回复）；会话页的信息面板「交给 agent 的规则」里有全文
 *   c. 在 agent 的环境里（`ANYCON_AGENT=1`）用 CLI 提议一条：它在「待处理」的「稍后」段里，不展开、不进导航数字；点开能看到全文，
 *      确认之后节点上是生效的
 *   d. 权限请求上的「记成规则」：预填、agent 写的话和用户的选择分开、保存后出处是这个会话和请求；请求仍然在等
 *
 * 共用一个 daemon：规则文字都带运行标记，结束时删掉自己建的规则、清单项、会话（连 worktree 和分支）。显式等待，没有固定睡眠。
 */
import { randomBytes } from 'node:crypto'

import { expect, test, type Page } from '@playwright/test'

import { runCli } from '../helpers/cli.js'
import { makeRepoProject } from '../helpers/git.js'
import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  TURN_TIMEOUT,
  nodeBase,
  pairNewBrowser,
  pendingCard,
  type PairedBrowser,
} from '../helpers/ui.js'

interface DaemonRule {
  id: string
  text: string
  status: string
  project?: string
  source: { kind: string; session_id?: string; request_id?: string }
}
interface DaemonTodo {
  id: string
  title: string
  state: string
  activity?: string
  sessions?: string[]
}

const RUN = randomBytes(3).toString('hex')
const PROJECT = `e2e-rules-${RUN}`
const TITLE = `e2e-${RUN} rules dispatch`
const RULE_A = `e2e-${RUN} write the failing test first`
const RULE_B = `e2e-${RUN} never push without asking me`
const RULE_C = `e2e-${RUN} ask before running a command`
const state = () => readState()

async function daemonRules(): Promise<DaemonRule[]> {
  const out = await runCli(state(), ['rule', 'ls', '--all', '--json'])
  return (JSON.parse(out.trim().split('\n').at(-1)!) as { rules: DaemonRule[] }).rules
}
const ruleWith = async (text: string): Promise<DaemonRule | undefined> =>
  (await daemonRules()).find((r) => r.text === text)

async function daemonTodo(): Promise<DaemonTodo | undefined> {
  const out = await runCli(state(), ['todo', 'ls', '--all', '--json'])
  return (JSON.parse(out.trim().split('\n').at(-1)!) as { todos: DaemonTodo[] }).todos.find(
    (x) => x.title === TITLE,
  )
}

async function navInboxCount(page: Page): Promise<number> {
  const el = page.getByTestId('sidebar-inbox-count')
  return (await el.count()) === 0 ? 0 : Number((await el.textContent()) ?? '0')
}

test.describe.serial('规则', () => {
  let device: PairedBrowser
  let page: Page
  let base = ''
  let permissionSession = ''

  test.beforeAll(async ({ browser }) => {
    test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    makeRepoProject(readState(), PROJECT)
    await runCli(state(), [
      'todo',
      'add',
      TITLE,
      '--project',
      PROJECT,
      '--body',
      'Nothing to write.',
    ])
    device = await pairNewBrowser(browser, readState(), { deviceName: 'e2e-rules' })
    page = device.page
    base = nodeBase(page)
  })

  test.afterAll(async () => {
    if (readState().daemonStarted) {
      for (const r of await daemonRules())
        if (r.text.includes(`e2e-${RUN}`))
          await runCli(state(), ['rule', 'rm', r.id.slice(0, 8)]).catch(() => undefined)
      const t = await daemonTodo()
      if (t) {
        for (const sid of t.sessions ?? [])
          await runCli(state(), [
            'delete',
            sid.slice(0, 12),
            '--remove-worktree',
            '--delete-branch',
            '--yes',
          ]).catch(() => undefined)
        await runCli(state(), ['todo', 'rm', t.id.slice(0, 8)]).catch(() => undefined)
      }
      if (permissionSession)
        await runCli(state(), ['delete', permissionSession.slice(0, 12), '--yes']).catch(
          () => undefined,
        )
    }
    await device?.context.close()
  })

  test('a. 规则页新建一条：节点上是生效的手写规则，页面上有全文和字数预算', async () => {
    await page.goto('/rules')
    await expect(page.getByTestId('rules-page')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(page.getByTestId('rules-add')).toBeEnabled({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('rules-add').click()
    await page.getByTestId('rule-editor-text').fill(RULE_A)
    await page.getByTestId('rule-editor-save').click()
    await expect(page.getByTestId('rule-editor')).toHaveCount(0)

    await page.getByTestId('rules-tab-active').click()
    const card = page.locator('[data-testid="rule-card"]').filter({ hasText: RULE_A })
    await expect(card).toBeVisible()
    await expect(card.getByTestId('rule-text')).toHaveText(RULE_A)
    await expect(card.getByTestId('rule-source')).toHaveAttribute('data-kind', 'manual')
    await expect(page.getByTestId('rules-budget-node-used')).toContainText('/ 4096 bytes')

    const r = await ruleWith(RULE_A)
    expect(r).toMatchObject({ status: 'active', source: { kind: 'manual' } })
  })

  test('b. 派发：对话框里列出规则，派出的会话的系统提示里有它，会话页的信息面板写着它', async () => {
    await page.goto('/todos')
    const row = page.locator('[data-testid="todo-row"]').filter({ hasText: TITLE })
    await expect(row).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await row.getByTestId('todo-main').click()
    await page.getByTestId('todo-dispatch').click()
    await expect(page.getByTestId('dispatch-dialog')).toBeVisible()
    if ((await page.getByTestId('dispatch-agent').count()) > 0)
      await page.getByTestId('dispatch-agent').selectOption('mock')
    await page.getByTestId('dispatch-no-push').uncheck()
    await expect(page.getByTestId('dispatch-send')).toBeEnabled({ timeout: 45_000 })
    // mock agent 声明了能收系统提示：列出规则文字（纯文字）和条数
    await expect(page.getByTestId('dispatch-rules-text')).toContainText(RULE_A, {
      timeout: CONNECT_TIMEOUT,
    })
    await expect(page.getByTestId('dispatch-rules-text')).toContainText(
      'Standing rules from the user.',
    )
    await expect(page.getByTestId('dispatch-rules-count')).toBeVisible()
    await expect(page.getByTestId('dispatch-rules-unsupported')).toHaveCount(0)

    await page.getByTestId('dispatch-send').click()
    await expect(page.getByTestId('dispatch-dialog')).toHaveCount(0)
    await expect
      .poll(async () => (await daemonTodo())?.activity, { timeout: TURN_TIMEOUT })
      .toBe('review')
    const todo = (await daemonTodo())!

    // 「经过」里 dispatched 记着给了几条；mock 回显系统提示的前 32 个字符
    const out = await runCli(state(), ['todo', 'history', todo.id.slice(0, 8), '--json'])
    const events = (
      JSON.parse(out.trim().split('\n').at(-1)!) as {
        events: Array<{ kind: string; rules?: string[]; text?: string }>
      }
    ).events
    expect(events.find((e) => e.kind === 'dispatched')?.rules?.length).toBeGreaterThanOrEqual(1)
    expect(events.find((e) => e.kind === 'result')?.text).toContain(
      'prompt_append=Standing rules from the user. Th',
    )

    // 会话页：信息面板里「交给 agent 的规则」是只读的全文
    await page.goto(`${base}/s/${todo.sessions![0]}`)
    await expect(page.getByTestId('info-rules')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(page.getByTestId('info-rules-text')).toContainText(RULE_A)
  })

  test('c. agent 环境里 CLI 提议的规则：在「稍后」段、不展开、不进数字；点开看全文，确认后生效', async () => {
    const before = await (async () => {
      await page.goto('/inbox')
      await expect(page.getByTestId('inbox-page')).toBeVisible({ timeout: CONNECT_TIMEOUT })
      return navInboxCount(page)
    })()
    const out = await runCli(state(), ['rule', 'add', RULE_B, '--json'], {
      env: { ANYCON_AGENT: '1' },
    })
    const added = JSON.parse(out.trim().split('\n').at(-1)!) as DaemonRule
    expect(added).toMatchObject({ status: 'pending', source: { kind: 'agent' } })

    const section = page.getByTestId('inbox-section-later')
    await expect(section).toBeVisible({ timeout: CONNECT_TIMEOUT })
    const row = section.locator('[data-kind="rule"]').filter({ hasText: RULE_B })
    await expect(row).toBeVisible()
    // 行上没有任何动作；不自动选中、不展开；导航上的数字没有变
    await expect(
      row.locator('[data-testid="inbox-rule-confirm"], [data-testid="inbox-accept"]'),
    ).toHaveCount(0)
    await expect(page.locator('[data-testid="inbox-detail"][data-kind="rule"]')).toHaveCount(0)
    expect(await navInboxCount(page)).toBe(before)

    await row.getByTestId('inbox-row-main').click()
    const detail = page.locator('[data-testid="inbox-detail"][data-kind="rule"]')
    await expect(detail.getByTestId('inbox-rule-text')).toHaveText(RULE_B)
    await expect(detail.getByTestId('rule-source')).toHaveAttribute('data-kind', 'agent')
    // 刚换到这一条的头一小会儿动作锁着，之后才能点
    await expect(detail.getByTestId('inbox-rule-confirm')).toBeEnabled()
    await detail.getByTestId('inbox-rule-confirm').click()
    await expect.poll(async () => (await ruleWith(RULE_B))?.status).toBe('active')
    await expect(row).toHaveCount(0)
  })

  test('d. 权限请求上的「记成规则」：预填、分开显示、出处是这个会话和请求；请求仍然在等', async () => {
    await page.goto(`${base}/new`)
    await expect(page.getByTestId('composer-input')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('composer-input').fill('run permission')
    await page.getByTestId('composer-send').click()
    await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
    await expect(pendingCard(page)).toBeVisible({ timeout: TURN_TIMEOUT })
    permissionSession = new URL(page.url()).pathname.split('/').pop()!

    // 共用的 daemon 上别的用例可能还留着等答复的请求：按会话定位到这一条（和通知点进来的地址一样）
    await page.goto(`/inbox?node=${base.split('/').pop()}&session=${permissionSession}`)
    const pane = page.getByTestId('inbox-detail-pane')
    await expect(pane.getByTestId('permission-card')).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(pane.getByTestId('inbox-open-session')).toHaveAttribute(
      'href',
      new RegExp(`${permissionSession}$`),
    )
    await pane.getByTestId('record-rule').click()
    const editor = page.getByTestId('rule-editor')
    await expect(editor.getByTestId('record-agent')).toBeVisible()
    await expect(editor.getByTestId('record-choice')).toBeVisible()
    await expect(editor).toContainText('does not approve anything')
    await editor.getByTestId('record-choice-allow').check()
    await expect(editor.getByTestId('rule-editor-text')).toHaveValue(/my answer is: /)
    await editor.getByTestId('rule-editor-text').fill(RULE_C)
    await editor.getByTestId('rule-editor-save').click()
    await expect(page.getByTestId('rule-editor')).toHaveCount(0)

    await expect.poll(async () => (await ruleWith(RULE_C))?.status).toBe('active')
    const rule = (await ruleWith(RULE_C))!
    expect(rule.source.kind).toBe('answer')
    expect(rule.source.session_id).toBe(permissionSession)
    expect(rule.source.request_id).toBeTruthy()
    // 请求没有被答复
    await expect(pane.getByTestId('permission-card')).toHaveAttribute('data-state', 'pending')
    // 清理：答复它（允许一次）
    await pane.getByTestId('permission-option-allow').click()
    await expect(page.getByTestId('inbox-empty')).toBeVisible({ timeout: 15_000 })
  })
})
