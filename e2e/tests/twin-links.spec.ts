/**
 * 分身连接（docs/spec/daemon.md § 27，web.md § 14.8、§ 4.8）：A 的分身使用 B：真实 hub + 两个真实 daemon + 真实 web + A 的假判断后端
 * （`helpers/judge.ts`）。agent 用 `helpers/fake-claude.ts` 的假 `claude`（原因见那个文件）。
 *
 * 串行；两个额外节点 A（跑分身、有判断后端）和 B（被使用），一个同时和两者配对的浏览器。A、B 的 `[hub] group` 不同，用来走「分组不同
 * 多确认一次」。节点的环境变量 `ANYCON_TWIN_COALESCE_MS`（只用于测试）把分拣的合并窗口缩短到 2 秒（§ 26.3），等唤醒的步骤都是对「假后端收到某个请求」的有界等待。
 *
 *   a. A 的分身页「使用别的节点」：列表空；「添加」里 A 自己不在候选里，B 设了 `[twin] accept_links = false` 时灰掉并说明原因（B 上
 *      `anycon twin link-code` 同样被拒），改回来之后（热加载）可选；分组不同时多确认一次，确认之前什么都没连；确认后连上，列表里有 B、在线；
 *      页面和 hub / 两个 daemon 的日志里都没有配对短码
 *   b. B 的节点设置「设备」里有 A 的分身设备（标成分身），范围缺省什么都不开放：A 的假后端回 `dispatch { node: B, project }` →
 *      A 的运行记录里这条决定是 `remote_refused`，B 上什么都没有
 *   c. 在 B 的「范围」里开放一个项目，再派：B 上有一项来源是分身、有会话（mock agent 跑完）；A 这一项的「经过」里有「派到了 B」和 B 的结果
 *      （`remote_result`）；A 的假后端收到的请求里有节点表（在随机边界里，有 B 的名字和开放的项目），之后的请求里带着 B 的结果
 *   d. 停掉 B：A 的连接显示离线，A 派发 → `remote_unavailable`（请求里节点表标着 `available: false`）；B 回来之后连接恢复在线
 *   e. A 上「断开」：说明「要在 B 上吊销才算完全断开」，有去 B 的设备列表的入口；断开后 A 的列表空了、B 上那台分身设备还在；然后在 B 上吊销它
 *
 * 结束时把两个节点（和 A 的分身设备）从账号里去掉。
 */
import { randomBytes } from 'node:crypto'

import { expect, test, type Page } from '@playwright/test'

import { daemonDevices, runCli, runCliResult, startPairing } from '../helpers/cli.js'
import { startExtraDaemon, type ExtraDaemon } from '../helpers/daemon.js'
import { git, makeRepoProject } from '../helpers/git.js'
import { containsText, exactText } from '../helpers/i18n.js'
import {
  dataBlocks,
  isTriage,
  itemsOf,
  nodeRowsOf,
  startJudge,
  type FakeJudge,
  type JudgeRequest,
  type TwinItem,
  type TwinPlanItem,
} from '../helpers/judge.js'
import { readLog, waitFor } from '../helpers/process.js'
import { readState } from '../helpers/state.js'
import {
  cliJson,
  judgeToml,
  removeNodesFromAccount,
  sessions,
  todoByTitle,
  todoHistory,
  todos,
  twinRuns,
  twinEnv,
  twinStatus,
  waitForTriage,
} from '../helpers/twin.js'
import {
  CONNECT_TIMEOUT,
  expectNodeOnline,
  nodeBase,
  pairBrowser,
  type PairedBrowser,
  pairNewBrowser,
} from '../helpers/ui.js'

const RUN = randomBytes(3).toString('hex')
const NODE_A = `e2e-twin-a-${RUN}`
const NODE_B = `e2e-twin-b-${RUN}`
const GROUP_A = `home-${RUN}`
const GROUP_B = `work-${RUN}`
const PROJECT_A = `e2e-tl-a-${RUN}`
const PROJECT_OPEN = `e2e-tl-open-${RUN}`
const PROJECT_CLOSED = `e2e-tl-closed-${RUN}`
const TITLE_REMOTE = `e2e-${RUN} send this to the other node`
const TITLE_OFFLINE = `e2e-${RUN} send this while it is down`
/** B 上 mock agent 的回答（带着它收到的「不 push」选项）。 */
const REPLY = 'mode=ask] no_push=on'
const REASON = {
  refused: `e2e-${RUN} try the other node (not opened yet)`,
  sent: `e2e-${RUN} the other node has it now`,
  offline: `e2e-${RUN} try the other node (down)`,
}
const NOTE = `e2e-${RUN} remote note`

/** 配对短码（16 位，四组四位）。日志里和页面上都不该出现，不管是哪一次配对的。 */
const CODE_RE = /\b(?:[2-9A-HJKMNP-Z]{4}-){3}[2-9A-HJKMNP-Z]{4}\b/i

/** 第二个提示词（B 不在线）和第一个用同一份节点表：按名字找 B。 */
function peerRow(request: JudgeRequest): Record<string, unknown> | undefined {
  return nodeRowsOf(request)?.find((r) => r.name === NODE_B)
}

/** 派到 B：先试没开放的项目（被 B 拒绝），B 开放之后和 B 离线时同样派到 `PROJECT_OPEN`。 */
let target: string = PROJECT_CLOSED

function decide(item: TwinItem, request: JudgeRequest): TwinPlanItem {
  const row = peerRow(request)
  if (row && item.dispatches === 0 && item.title === TITLE_REMOTE) {
    return {
      id: item.id,
      action: 'dispatch',
      reason: target === PROJECT_CLOSED ? REASON.refused : REASON.sent,
      node: String(row.id),
      project: target,
      note: NOTE,
    }
  }
  if (row && item.dispatches === 0 && item.title === TITLE_OFFLINE) {
    return {
      id: item.id,
      action: 'dispatch',
      reason: REASON.offline,
      node: String(row.id),
      project: PROJECT_OPEN,
    }
  }
  return { id: item.id, action: 'wait', reason: 'e2e: wait' }
}

const respond = (request: JudgeRequest): { items: TwinPlanItem[] } => ({
  items: isTriage(request) ? itemsOf(request).map((i) => decide(i, request)) : [],
})

const inBlocks = (request: JudgeRequest, needle: string): boolean =>
  dataBlocks(request).some((b) => b.text.includes(needle))

const gitState = () => ({ gitConfig: readState().gitConfig })

test.describe.serial('分身连接', () => {
  let judge: FakeJudge
  let a: ExtraDaemon
  let b: ExtraDaemon
  let device: PairedBrowser
  let page: Page
  let baseA = ''
  let baseB = ''
  let shortA = ''
  let bId = ''
  let twinDeviceId = ''
  const homeA = () => ({ daemonHome: a.home })
  const homeB = () => ({ daemonHome: b.home })

  /** 短码没有出现在页面上，也没有出现在 hub 和两个 daemon 的日志里。 */
  async function expectNoCodes(): Promise<void> {
    const state = readState()
    const text = await page.evaluate(() => document.documentElement.outerHTML)
    expect(text.match(CODE_RE)).toBeNull()
    for (const [name, file] of [
      ['hub', `${state.tmpDir}/hub.log`],
      ['A', a.logFile],
      ['B', b.logFile],
    ] as const) {
      expect(readLog(file).match(CODE_RE), `a pairing code in the ${name} log`).toBeNull()
    }
  }

  test.beforeAll(async ({ browser }) => {
    test.setTimeout(240_000)
    const state = readState()
    test.skip(!state.daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
    judge = await startJudge(respond)
    a = await startExtraDaemon(state, NODE_A, {
      env: twinEnv(),
      config: { 'hub.group': GROUP_A },
      toml: judgeToml(judge),
    })
    b = await startExtraDaemon(state, NODE_B, {
      env: twinEnv(),
      config: { 'hub.group': GROUP_B },
    })
    const gs = { ...gitState(), workspaceDir: a.workspaceDir }
    makeRepoProject(gs, PROJECT_A)
    for (const name of [PROJECT_OPEN, PROJECT_CLOSED]) {
      makeRepoProject({ ...gitState(), workspaceDir: b.workspaceDir }, name)
    }
    // A 的分身本身配好（范围是 A 上的项目）；连接是被测的内容
    await runCli(homeA(), ['twin', 'set', '--projects', PROJECT_A])

    // 一个同时和 A、B 配过对的浏览器：先用 A 的短码配对，再在同一个浏览器里用 B 的短码配对
    device = await pairNewBrowser(
      browser,
      { ...state, daemonHome: a.home },
      { deviceName: `e2e-twin-links-${RUN}` },
    )
    page = device.page
    baseA = nodeBase(page)
    shortA = baseA.split('/').pop()!
    await page.goto('/pair')
    const pairing = await startPairing(homeB())
    await pairBrowser(page, pairing.code)
    await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: CONNECT_TIMEOUT })
    await expectNodeOnline(page)
    expect(await pairing.done).toBe(0)
    baseB = nodeBase(page)
    expect(baseB).not.toBe(baseA)
  })

  test.afterAll(async ({ request }) => {
    await device?.context.close()
    await a?.stop()
    await b?.stop()
    await judge?.stop()
    await removeNodesFromAccount(request, readState(), [NODE_A, NODE_B])
  })

  test('a. 「使用别的节点」：列表空；B 不接受时灰掉；分组不同多确认一次；连上后 B 在线；没有短码', async () => {
    test.setTimeout(180_000)
    // B 不接受分身连接（热加载）：节点上要不到短码
    await runCli(homeB(), ['config', 'set', 'twin.accept_links', 'false'])
    const refused = await runCliResult(homeB(), ['twin', 'link-code'])
    expect(refused.code).not.toBe(0)
    expect(refused.stderr).toMatch(/forbidden|accept_links/i)

    await page.goto(`/twin/${shortA}`)
    const links = page.getByTestId('twin-links')
    await expect(links).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(links).toContainText(containsText('twinLinks.none'))
    await expect(page.getByTestId('twin-link')).toHaveCount(0)
    await page.getByTestId('twin-link-add').click()
    const dialog = page.getByTestId('twin-link-add-dialog')
    const candidate = dialog.getByTestId('twin-link-candidate')
    // A 自己不在候选里，只有 B；B 不接受时灰掉、写明原因和怎么办，不能选
    await expect(candidate).toHaveCount(1)
    await expect(candidate).toContainText(NODE_B)
    await expect(candidate).toHaveAttribute('data-blocked', 'acceptOff', { timeout: 30_000 })
    await expect(candidate).toContainText(containsText('twinLinks.blocked.acceptOff'))
    await expect(candidate.getByRole('radio')).toBeDisabled()
    await expect(dialog.getByTestId('twin-link-connect')).toBeDisabled()
    await page.keyboard.press('Escape')
    await expect(dialog).toHaveCount(0)

    // 改回来：B 的节点信息重读之后可选（页面重新连接时会再读一次 node.info）
    await runCli(homeB(), ['config', 'set', 'twin.accept_links', 'true'])
    await page.reload()
    await expect(links).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await page.getByTestId('twin-link-add').click()
    await expect(dialog.getByTestId('twin-link-candidate')).toHaveAttribute('data-blocked', '', {
      timeout: 30_000,
    })
    await dialog.getByTestId('twin-link-candidate').getByRole('radio').check()

    // 两个节点的分组不同：多确认一次，写明两边的分组；确认之前什么都没连
    await dialog.getByTestId('twin-link-connect').click()
    const confirm = dialog.getByTestId('twin-link-group-confirm')
    await expect(confirm).toBeVisible()
    await expect(confirm).toContainText(GROUP_A)
    await expect(confirm).toContainText(GROUP_B)
    expect((await cliJson<{ links: unknown[] }>(homeA(), ['twin', 'links'])).links).toEqual([])
    expect((await daemonDevices(homeB())).filter((d) => d.kind === 'twin')).toEqual([])
    // 可以退回列表，再继续
    await dialog.getByRole('button', { name: exactText('common.back') }).click()
    await expect(dialog.getByTestId('twin-link-candidate')).toBeVisible()
    await dialog.getByTestId('twin-link-candidate').getByRole('radio').check()
    await dialog.getByTestId('twin-link-connect').click()
    await expect(confirm).toBeVisible()
    await dialog.getByTestId('twin-link-connect').click()

    // 依次：B 要短码，A 用短码配对（短码只在内存里经过页面）；连上之后对话框关掉，列表里有 B、在线
    await expect(dialog).toHaveCount(0, { timeout: 90_000 })
    const row = page.getByTestId('twin-link')
    await expect(row).toHaveCount(1, { timeout: 30_000 })
    await expect(row).toContainText(NODE_B)
    // 节点上：传输握手好、取到 B 的 `twin.peer_status`（不保证在 `twin.link` 返回的那一刻）
    await expect
      .poll(
        async () => {
          const l = (
            await cliJson<{ links: { online: boolean; status?: { projects: string[] } }[] }>(
              homeA(),
              ['twin', 'links'],
            )
          ).links[0]
          return l?.online === true && l.status !== undefined
        },
        { timeout: 90_000 },
      )
      .toBe(true)
    // A 在 B 上线、取到 `twin.peer_status` 时发 `twin.changed`，页面据此重读 `twin.links`（daemon.md § 27.6）：不刷新，列表就跟着变
    // 在线、有分组名；B 还没有开放任何项目
    await expect(row.getByTestId('twin-link-online')).toHaveText(exactText('twinLinks.online'), {
      timeout: 60_000,
    })
    await expect(row.getByTestId('twin-link-projects')).toContainText(
      containsText('twinLinks.noProjects'),
      { timeout: 30_000 },
    )
    await expect(row).toContainText(GROUP_B)

    // 节点两侧：A 记下了这条连接，B 上多了一台分身设备
    const link = (
      await cliJson<{ links: { node_id: string; name: string; online: boolean }[] }>(homeA(), [
        'twin',
        'links',
      ])
    ).links
    expect(link).toHaveLength(1)
    expect(link[0]).toMatchObject({ name: NODE_B, online: true })
    bId = link[0]!.node_id
    const twinDevice = (await daemonDevices(homeB())).find((d) => d.kind === 'twin')
    expect(twinDevice?.name).toBe(`${NODE_A}'s twin`)
    twinDeviceId = twinDevice!.device_id
    // 短码不在页面上，也不在 hub / daemon 的日志里
    await expectNoCodes()
  })

  test('b. B 的「设备」里有 A 的分身设备，范围缺省什么都不开放：派发被拒绝（remote_refused），B 上什么都没有', async () => {
    test.setTimeout(240_000)
    await page.goto(`${baseB}/settings?tab=devices`)
    const list = page.getByTestId('device-list')
    await expect(list).toBeVisible({ timeout: CONNECT_TIMEOUT })
    const row = list.locator('li').filter({ hasText: `${NODE_A}'s twin` })
    await expect(row).toHaveCount(1)
    await expect(row).toContainText(containsText('twinScope.kind'))
    await row.getByTestId('device-twin-scope').click()
    const dialog = page.getByTestId('twin-scope-dialog')
    await expect(dialog).toBeVisible()
    await expect(dialog).toContainText(containsText('twinScope.nothingOpen'), { timeout: 15_000 })
    await expect(dialog.getByRole('checkbox', { name: PROJECT_OPEN })).not.toBeChecked()
    await expect(dialog.getByRole('checkbox', { name: PROJECT_CLOSED })).not.toBeChecked()
    expect(await twinStatus(homeB(), twinDeviceId)).toMatchObject({
      configured: false,
      scope: { projects: [], paused: false },
    })
    await page.keyboard.press('Escape')

    // A 记一项交给分身；它的假后端让这一项派到 B 的一个没开放的项目
    target = PROJECT_CLOSED
    const before = judge.requests.length
    await runCli(homeA(), ['todo', 'add', TITLE_REMOTE, '--project', PROJECT_A, '--twin'])
    const item = (await todoByTitle(homeA(), TITLE_REMOTE))!
    const request = await waitForTriage(
      judge,
      'the wake for the remote item',
      (r) => inBlocks(r, item.id),
      {
        after: before,
      },
    )
    // 节点表：B 在线，开放的项目是空的
    expect(peerRow(request)).toMatchObject({ id: bId, name: NODE_B, available: true, projects: [] })
    await expect
      .poll(
        async () =>
          (await twinRuns(homeA())).some((r) => r.decisions.some((d) => d.todo_id === item.id)),
        {
          timeout: 60_000,
        },
      )
      .toBe(true)
    const run = (await twinRuns(homeA())).find((r) =>
      r.decisions.some((d) => d.todo_id === item.id),
    )!
    const decision = run.decisions.find((d) => d.todo_id === item.id)!
    expect(decision).toMatchObject({
      action: 'dispatch',
      node: bId,
      project: PROJECT_CLOSED,
      outcome: 'skipped',
      skip_reason: 'remote_refused',
    })
    // B 的拒绝原因（范围）接在理由后面
    expect(decision.reason).toContain(REASON.refused)
    expect(decision.reason).toContain('out_of_scope')
    // B 上什么都没有：没有来源是分身的项，没有会话
    expect((await todos(homeB())).filter((t) => t.source.kind === 'twin')).toEqual([])
    expect(await sessions(homeB())).toEqual([])
    expect((await todoByTitle(homeA(), TITLE_REMOTE))?.remote ?? []).toEqual([])

    // A 的分身页：运行记录里这条决定写着「派到节点：B」和「另一个节点拒绝了」
    await page.goto(`/twin?node=${shortA}&run=${run.id}`)
    const shown = page.getByTestId('twin-decision').filter({ hasText: TITLE_REMOTE })
    await expect(shown).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await expect(shown.getByTestId('twin-decision-node')).toContainText(NODE_B)
    await expect(shown).toContainText(containsText('twin.skip.remote_refused'))
    await expect(shown).toContainText(REASON.refused)
  })

  test('c. 在 B 上开放一个项目再派：B 上有来源是分身的项和会话；A 的「经过」里有派到了 B 和 B 的结果；请求里有节点表', async () => {
    test.setTimeout(300_000)
    // B 的人在「范围」里给这条连接开放一个项目
    await page.goto(`${baseB}/settings?tab=devices`)
    const row = page
      .getByTestId('device-list')
      .locator('li')
      .filter({ hasText: `${NODE_A}'s twin` })
    await expect(row).toBeVisible({ timeout: CONNECT_TIMEOUT })
    await row.getByTestId('device-twin-scope').click()
    const dialog = page.getByTestId('twin-scope-dialog')
    await dialog.getByRole('checkbox', { name: PROJECT_OPEN }).check()
    const save = dialog.getByRole('button', { name: exactText('twin.save') })
    await expect(save).toBeEnabled({ timeout: 15_000 })
    await save.click()
    await expect
      .poll(async () => (await twinStatus(homeB(), twinDeviceId)).scope.projects)
      .toEqual([PROJECT_OPEN])
    await expect(dialog).not.toContainText(containsText('twinScope.nothingOpen'))
    // 只开放了这一个：另一个仍然关着
    expect(await twinStatus(homeB(), twinDeviceId)).toMatchObject({ configured: true })
    await page.keyboard.press('Escape')

    // 再派（手动唤醒）：这次派到开放的项目
    target = PROJECT_OPEN
    const item = (await todoByTitle(homeA(), TITLE_REMOTE))!
    const before = judge.requests.length
    await runCli(homeA(), ['twin', 'wake'])
    const request = await waitForTriage(
      judge,
      'the wake after opening the project',
      (r) => inBlocks(r, item.id),
      {
        after: before,
      },
    )
    // 节点表：在随机边界里，有 B 的名字和开放的项目；边界之外没有
    const peer = peerRow(request)
    expect(peer).toMatchObject({
      id: bId,
      name: NODE_B,
      group: GROUP_B,
      available: true,
      projects: [PROJECT_OPEN],
    })
    const marker = dataBlocks(request)[0]!.marker
    expect(request.prompt).toContain(`Linked nodes`)
    expect(
      dataBlocks(request).some((blk) => blk.marker === marker && blk.text.includes(NODE_B)),
    ).toBe(true)
    let outside = request.prompt
    for (const blk of dataBlocks(request)) outside = outside.replace(blk.text, '')
    expect(outside).not.toContain(NODE_B)
    expect(outside).not.toContain(PROJECT_OPEN)
    expect(request.system).toContain('Linked nodes')
    expect(request.system).not.toContain(NODE_B)

    // B 一侧：一项来源是分身（这台分身设备）的项，有会话；agent 跑完
    await waitFor(
      'B has the twin item and its session',
      async () =>
        (await todos(homeB())).some(
          (t) => t.source.kind === 'twin' && (t.sessions?.length ?? 0) === 1,
        ),
      { timeoutMs: 90_000, intervalMs: 1_000 },
    )
    const remote = (await todos(homeB())).find((t) => t.source.kind === 'twin')!
    expect(remote).toMatchObject({
      title: TITLE_REMOTE,
      project: PROJECT_OPEN,
      handler: 'me',
      source: { kind: 'twin', device_id: twinDeviceId },
    })
    const bSession = (await sessions(homeB())).find((s) => s.id === remote.sessions![0])
    expect(bSession).toMatchObject({ project: PROJECT_OPEN })
    await expect
      .poll(() => git(gitState(), `${b.workspaceDir}/${PROJECT_OPEN}`, 'worktree', 'list'), {
        timeout: 30_000,
      })
      .toMatch(/\.anycon\/worktrees\//)
    // B 记下这条连接今天派了一次
    expect((await twinStatus(homeB(), twinDeviceId)).today.dispatches).toBe(1)
    // A 的计划里的那条决定：执行了，派到了 B
    const run = (await twinRuns(homeA())).find((r) =>
      r.decisions.some((d) => d.reason === REASON.sent),
    )!
    expect(run.decisions.find((d) => d.reason.includes(REASON.sent))).toMatchObject({
      outcome: 'executed',
      node: bId,
      project: PROJECT_OPEN,
    })

    // A 这一项：记下了远程项；「经过」里有 remote_dispatched，B 的结果回到 A（remote_result）
    await expect
      .poll(async () => (await todoByTitle(homeA(), TITLE_REMOTE))?.remote?.length ?? 0, {
        timeout: 60_000,
      })
      .toBe(1)
    expect((await todoByTitle(homeA(), TITLE_REMOTE))!.remote![0]).toMatchObject({
      node_id: bId,
      todo_id: remote.id,
    })
    expect((await todoByTitle(homeA(), TITLE_REMOTE))?.sessions ?? []).toEqual([])
    await expect
      .poll(
        async () =>
          (await todoHistory(homeA(), item.id)).find((e) => e.kind === 'remote_result')?.text,
        {
          timeout: 60_000,
        },
      )
      .toContain(REPLY)
    const events = await todoHistory(homeA(), item.id)
    expect(events.find((e) => e.kind === 'remote_dispatched')).toMatchObject({
      node_id: bId,
      todo_id: remote.id,
      session_id: remote.sessions![0],
    })

    // 页面上：A 的清单项「经过」里「派到了 B」（这台设备也和 B 配过对，能点到 B 上的那一项和会话）和 B 的结果
    await page.goto(`/todos/${shortA}/${item.id}`)
    const sent = page.getByTestId('remote-dispatched')
    await expect(sent).toContainText(NODE_B, { timeout: CONNECT_TIMEOUT })
    await expect(
      sent.getByRole('link', { name: containsText('twin.remote.openItem', { node: NODE_B }) }),
    ).toBeVisible()
    await expect(
      sent.getByRole('link', { name: containsText('twin.remote.openSession', { node: NODE_B }) }),
    ).toBeVisible()
    const result = page.getByTestId('remote-result')
    await expect(result).toContainText(NODE_B)
    await expect(result.getByTestId('remote-result-text')).toContainText(REPLY)

    // B 的结果取回之后触发 A 的一次唤醒；之后的请求里带着这条结果
    const later = await waitForTriage(
      judge,
      'a wake that carries the remote result',
      (r) =>
        itemsOf(r).some((i) => i.id === item.id && i.text.includes(`earlier result (node ${bId}`)),
      { after: before + 1 },
    )
    expect(itemsOf(later).find((i) => i.id === item.id)!.text).toContain(REPLY)
    expect(itemsOf(later).find((i) => i.id === item.id)!.dispatches).toBe(1)
  })

  test('d. B 不在线：A 的连接显示离线，派发 → remote_unavailable；B 回来之后恢复在线', async () => {
    test.setTimeout(300_000)
    await page.goto(`/twin/${shortA}`)
    await expect(page.getByTestId('twin-link-online')).toHaveText(exactText('twinLinks.online'), {
      timeout: CONNECT_TIMEOUT,
    })
    await b.stop()
    await expect
      .poll(
        async () =>
          (await cliJson<{ links: { online: boolean }[] }>(homeA(), ['twin', 'links'])).links[0]
            ?.online,
        { timeout: 60_000 },
      )
      .toBe(false)
    // 页面开着没刷新：A 在连接下线时发 `twin.changed`，列表跟着变成离线
    await expect(page.getByTestId('twin-link-online')).toHaveText(exactText('twinLinks.offline'), {
      timeout: CONNECT_TIMEOUT,
    })

    target = PROJECT_OPEN
    const before = judge.requests.length
    await runCli(homeA(), ['todo', 'add', TITLE_OFFLINE, '--project', PROJECT_A, '--twin'])
    const item = (await todoByTitle(homeA(), TITLE_OFFLINE))!
    const request = await waitForTriage(
      judge,
      'the wake while B is down',
      (r) => inBlocks(r, item.id),
      {
        after: before,
      },
    )
    // 节点表里 B 标着不可用，只有 id 和名字
    expect(peerRow(request)).toEqual({ id: bId, name: NODE_B, available: false })
    await expect
      .poll(
        async () =>
          (await twinRuns(homeA())).some((r) => r.decisions.some((d) => d.todo_id === item.id)),
        {
          timeout: 60_000,
        },
      )
      .toBe(true)
    const run = (await twinRuns(homeA())).find((r) =>
      r.decisions.some((d) => d.todo_id === item.id),
    )!
    expect(run.decisions.find((d) => d.todo_id === item.id)).toMatchObject({
      action: 'dispatch',
      node: bId,
      outcome: 'skipped',
      skip_reason: 'remote_unavailable',
    })
    expect((await todoByTitle(homeA(), TITLE_OFFLINE))?.remote ?? []).toEqual([])
    await page.goto(`/twin?node=${shortA}&run=${run.id}`)
    await expect(
      page.getByTestId('twin-decision').filter({ hasText: TITLE_OFFLINE }),
    ).toContainText(containsText('twin.skip.remote_unavailable'), { timeout: CONNECT_TIMEOUT })
    // 这一项不再需要：删掉，免得之后的唤醒又去试
    await runCli(homeA(), ['todo', 'rm', item.id.slice(0, 8)])

    // B 回来：A 的连接恢复在线
    await b.restart()
    await expect
      .poll(
        async () =>
          (await cliJson<{ links: { online: boolean }[] }>(homeA(), ['twin', 'links'])).links[0]
            ?.online,
        {
          timeout: 90_000,
        },
      )
      .toBe(true)
    await page.goto(`/twin/${shortA}`)
    await expect(page.getByTestId('twin-link-online')).toHaveText(exactText('twinLinks.online'), {
      timeout: CONNECT_TIMEOUT,
    })
  })

  test('e. 「断开」：写明要在 B 上吊销；断开后 A 的列表空了、B 上那台分身设备还在；再在 B 上吊销', async () => {
    test.setTimeout(120_000)
    await page.goto(`/twin/${shortA}`)
    const row = page.getByTestId('twin-link')
    await expect(row).toHaveCount(1, { timeout: CONNECT_TIMEOUT })
    await row.getByTestId('twin-link-remove').click()
    const dialog = page.getByTestId('twin-link-remove-dialog')
    await expect(dialog).toBeVisible()
    await expect(dialog.getByTestId('twin-link-revoke-hint')).toContainText(NODE_B)
    await expect(dialog.getByTestId('twin-link-revoke-hint')).toContainText(/revoke|吊销/i)
    // 去 B 的设备列表的入口（这台设备也和 B 配过对）
    const open = dialog.getByRole('link', {
      name: containsText('twinLinks.openDevices', { name: NODE_B }),
    })
    await expect(open).toHaveAttribute('href', /\/n\/[0-9a-f]+\/settings\?(.*&)?tab=devices/)
    await dialog.getByTestId('twin-link-remove-confirm').click()

    // A 的列表空了；断开之后的提醒里同样有「要在 B 上吊销」和入口
    await expect(page.getByTestId('twin-link')).toHaveCount(0, { timeout: 30_000 })
    await expect(page.getByTestId('twin-links')).toContainText(containsText('twinLinks.none'))
    const notice = page.getByTestId('twin-link-revoke')
    await expect(notice).toContainText(NODE_B)
    expect((await cliJson<{ links: unknown[] }>(homeA(), ['twin', 'links'])).links).toEqual([])
    // A 只是忘掉这一条：B 上那台分身设备和它的范围还在
    expect((await daemonDevices(homeB())).filter((d) => d.kind === 'twin')).toHaveLength(1)
    expect((await twinStatus(homeB(), twinDeviceId)).scope.projects).toEqual([PROJECT_OPEN])

    // 照提醒里的入口去 B 的设备列表吊销它
    await notice
      .getByRole('link', { name: containsText('twinLinks.openDevices', { name: NODE_B }) })
      .click()
    await expect(page).toHaveURL(/\/settings\?(.*&)?tab=devices/)
    const list = page.getByTestId('device-list')
    await expect(list).toBeVisible({ timeout: CONNECT_TIMEOUT })
    const twinRow = list.locator('li').filter({ hasText: `${NODE_A}'s twin` })
    await expect(twinRow).toHaveCount(1)
    await twinRow.getByRole('button', { name: exactText('devices.revoke') }).click()
    await page.getByTestId('device-revoke-confirm').click()
    await expect(list.locator('li').filter({ hasText: `${NODE_A}'s twin` })).toHaveCount(0, {
      timeout: 15_000,
    })
    await expect
      .poll(async () => (await daemonDevices(homeB())).filter((d) => d.kind === 'twin'))
      .toEqual([])
    // A 自己的分身不受影响
    expect(await twinStatus(homeA())).toMatchObject({ scope: { projects: [PROJECT_A] } })
    await expectNoCodes()
  })
})
