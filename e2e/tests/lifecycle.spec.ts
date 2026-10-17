/**
 * 配对与设备生命周期、大消息：在 happy path 之外、每个用例自己配对自己的浏览器（互不依赖，任何一个失败不影响其它）。
 *
 *   a. 8 位短码：`anycon pair --short --json`（默认 16 位），输入短码配对；hub 的日志里没有短码
 *   b. 吊销后重新配对：A 吊销 B，B 提示需要重新配对，重新配对后恢复
 *   c. 退出并移除本设备：IndexedDB 清空、设备在 daemon 和 hub 两侧都已被吊销
 *   d. 大工具输出（mock 的 `big` 关键词，300 KB）：触发 daemon 的截断和 history 按字节分页，新设备 / 刷新后仍能加载会话
 *   e. 页面在后台：hidden 满 30 秒主动断开和 hub 的连接（hub 看到这台设备离线，`notify` 才会走推送），30 秒内回来不断开；
 *      回到前台自动重连并补同步（断开期间 CLI 新建的会话出现在列表里）
 *
 * 约定：显式等待（data-testid、expect.poll），没有固定睡眠。
 */
import { expect, test } from '@playwright/test'

import { deviceOnline, webDeviceId } from '../helpers/account.js'
import { daemonDevices, runCli, startPairing } from '../helpers/cli.js'
import { readLog } from '../helpers/process.js'
import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  REPAIR_TEXT,
  SHORT_CODE_RE,
  TURN_TIMEOUT,
  expectLoginPage,
  expectNodeOnline,
  hubLogLeaks,
  indexedDbSummary,
  loginViaApi,
  nodeBase,
  openDevicesTab,
  pairBrowser,
  pairNewBrowser,
} from '../helpers/ui.js'

test.beforeEach(() => {
  test.skip(!readState().daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
})

test.describe('8 位短码配对（--short）', () => {
  test('a. anycon pair --short --json：8 位短码、链接带 #code=，配对成功，hub 日志里没有短码', async ({
    browser,
  }) => {
    const state = readState()
    // 默认是 16 位（happy path 第 2 步）；这里走 `--short` 的 8 位手输路径。
    // pairNewBrowser 内部：startPairing({ short }) 校验了 8 位格式和 fragment 链接，输入短码，等配对完成、节点在线
    const paired = await pairNewBrowser(browser, state, {
      deviceName: 'e2e-short-code',
      short: true,
    })
    try {
      expect(paired.pairing.code).toMatch(SHORT_CODE_RE)
      expect(paired.pairing.link).toContain(`/pair#code=${paired.pairing.code}`)
      // 设备确实出现在 daemon 的已配对列表里
      expect((await daemonDevices(state)).filter((d) => d.name === 'e2e-short-code')).toHaveLength(
        1,
      )
      // 短码只在两端本地使用：hub 的日志里（含 debug）找不到
      expect(hubLogLeaks(state, [paired.pairing.code])).toEqual([])
    } finally {
      await paired.context.close()
    }
  })
})

test.describe('吊销', () => {
  test('b. 吊销后重新配对：A 吊销 B，B 提示需要重新配对，重新配对后恢复在线', async ({
    browser,
  }) => {
    const state = readState()
    const a = await pairNewBrowser(browser, state, { deviceName: 'e2e-revoker' })
    const b = await pairNewBrowser(browser, state, { deviceName: 'e2e-revoked' })
    try {
      const oldId = (await daemonDevices(state)).find((d) => d.name === 'e2e-revoked')?.device_id
      expect(oldId).toBeTruthy()

      // A：节点设置 → 设备 → 吊销 B
      const devices = await openDevicesTab(a.page)
      const row = devices.filter({ hasText: 'e2e-revoked' })
      await expect(row).toHaveCount(1)
      await row.getByRole('button', { name: /Revoke|吊销/ }).click()
      await a.page.getByTestId('device-revoke-confirm').click()
      await expect(devices.filter({ hasText: 'e2e-revoked' })).toHaveCount(0, { timeout: 15_000 })
      // daemon 一侧：旧设备已经不在
      await expect
        .poll(async () => (await daemonDevices(state)).some((d) => d.device_id === oldId))
        .toBe(false)

      // B：hub 断开它（4004 revoked）或 daemon 不认它的公钥——两种都会引导到「重新配对」
      const revoked = b.page.locator(
        '[data-testid="status-banner"][data-kind="hub_revoked"], [data-testid="noise-failed"]',
      )
      await expect(revoked.first()).toBeVisible({ timeout: 30_000 })
      await expect(b.page.getByText(REPAIR_TEXT).first()).toBeVisible()
      // 点「重新配对」（横幅里的链接，或节点页的按钮）→ 回到 /pair
      const repair = b.page.getByTestId('repair')
      if (await repair.isVisible()) await repair.click()
      else
        await b.page.getByTestId('status-banner').getByRole('link', { name: REPAIR_TEXT }).click()
      await expect(b.page).toHaveURL(/\/pair/)

      // 重新配对：用新短码，同一个浏览器
      const pairing = await startPairing(state)
      await pairBrowser(b.page, pairing.code, 'e2e-revoked')
      await expect(b.page).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: CONNECT_TIMEOUT })
      await expectNodeOnline(b.page)
      expect(await pairing.done).toBe(0)

      // 恢复之后：daemon 里有一台叫 e2e-revoked 的设备，是新的设备 id（旧的已被吊销，不会复活）
      const now = (await daemonDevices(state)).filter((d) => d.name === 'e2e-revoked')
      expect(now).toHaveLength(1)
      expect(now[0]?.device_id).not.toBe(oldId)
      // A 的设备列表里也看到它（重新进入页面拉最新）
      const devicesAgain = await openDevicesTab(a.page)
      await expect(devicesAgain.filter({ hasText: 'e2e-revoked' })).toHaveCount(1, {
        timeout: CONNECT_TIMEOUT,
      })
    } finally {
      await a.context.close()
      await b.context.close()
    }
  })

  test('c. 退出并移除本设备：IndexedDB 清空，设备在 daemon 和 hub 两侧都已被吊销', async ({
    browser,
    request,
  }) => {
    const state = readState()
    const c = await pairNewBrowser(browser, state, { deviceName: 'e2e-wipe' })
    try {
      const before = await indexedDbSummary(c.page)
      const countBefore = before.reduce(
        (n, db) => n + Object.values(db.stores).reduce((x, y) => x + y, 0),
        0,
      )
      expect(countBefore, 'a paired browser keeps keys / nodes in IndexedDB').toBeGreaterThan(0)
      expect((await daemonDevices(state)).some((d) => d.name === 'e2e-wipe')).toBe(true)

      // 账号菜单 → 退出并移除本设备 → 确认框里确认（先吊销各节点上的本设备，再退出登录、清本地数据，整页跳回登录页）
      await c.page.getByTestId('account-menu').click()
      await c.page.getByTestId('logout-remove').click()
      await c.page.getByTestId('remove-device-confirm').click()
      await expect(c.page).toHaveURL(/\/login/, { timeout: CONNECT_TIMEOUT })
      await expectLoginPage(c.page)

      // IndexedDB：所有库的所有 store 都是空的（库被删掉也算）
      await expect
        .poll(async () => {
          const after = await indexedDbSummary(c.page)
          return after.flatMap((db) => Object.entries(db.stores)).filter(([, n]) => n > 0)
        })
        .toEqual([])

      // localStorage 也被清了（偏好、上次选项这类）
      expect(await c.page.evaluate(() => localStorage.length)).toBe(0)

      // daemon 一侧：设备已吊销（公钥已删）
      await expect
        .poll(async () => (await daemonDevices(state)).some((d) => d.name === 'e2e-wipe'))
        .toBe(false)
      // hub 一侧：账号的设备列表里不再有它（吊销的设备不列出）
      await loginViaApi(request, state)
      const devices = (await (await request.get(`${state.hubUrl}/api/devices`)).json()) as {
        name: string
      }[]
      expect(devices.some((d) => d.name === 'e2e-wipe')).toBe(false)
    } finally {
      await c.context.close()
    }
  })
})

test.describe('大消息', () => {
  test('d. 工具输出 300 KB（mock 的 big）：被截断，历史按字节分页，新设备和刷新后都能加载', async ({
    browser,
  }) => {
    test.setTimeout(180_000)
    const state = readState()
    const d1 = await pairNewBrowser(browser, state, { deviceName: 'e2e-big-1' })
    const d2 = await pairNewBrowser(browser, state, { deviceName: 'e2e-big-2' })
    try {
      // d1：scratch 会话里连发 4 次 `big`，每次一个 300 KB 的工具输出（daemon 存储和发送前截到 64 KiB 以内）
      await d1.page.goto(`${nodeBase(d1.page)}/new`)
      const turns = 4
      await d1.page.getByTestId('composer-input').fill('big output 1')
      await d1.page.getByTestId('composer-send').click()
      await expect(d1.page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
      const sessionPath = new URL(d1.page.url()).pathname
      for (let i = 1; i <= turns; i++) {
        if (i > 1) {
          await expect(d1.page.getByTestId('session-status')).toHaveAttribute(
            'data-status',
            'idle',
            {
              timeout: TURN_TIMEOUT,
            },
          )
          await d1.page.getByTestId('composer-input').fill(`big output ${i}`)
          await d1.page.getByTestId('composer-send').click()
        }
        await expect(d1.page.getByTestId('turn-usage')).toHaveCount(i, { timeout: TURN_TIMEOUT })
      }
      await expect(d1.page.getByTestId('tool-block')).toHaveCount(turns)
      await expect(d1.page.getByTestId('session-status')).toHaveAttribute('data-status', 'idle')

      // 工具输出被 daemon 截断：保留开头和结尾，中间有标记；没有任何一次传输失败
      const lastTool = d1.page.getByTestId('tool-block').last()
      // 命令块默认收起，点标题行展开
      await lastTool.getByRole('button').first().click()
      const output = lastTool.getByTestId('tool-output')
      await expect(output).toContainText('BIGSTART')
      // 终端块默认只显示 20 行，展开后才有后面的内容
      await lastTool.getByRole('button', { name: /Show \d+ more lines|再显示 \d+ 行/ }).click()
      await expect(output).toContainText(/truncated \d+ bytes/)
      await expect(output).toContainText('BIGEND')

      // d1 刷新：IndexedDB 里的缓存 + 增量 session.history
      await d1.page.reload()
      await expect(d1.page.getByTestId('tool-block').last()).toBeVisible({
        timeout: CONNECT_TIMEOUT,
      })
      await expectNodeOnline(d1.page)

      // d2：全新设备、空缓存，只能走线上的 session.history（总量超过一页 192 KiB，要翻页；每页都得经分片 / 重组）
      await d2.page.goto(sessionPath)
      await expect(d2.page.getByTestId('tool-block').last()).toBeVisible({
        timeout: CONNECT_TIMEOUT,
      })
      await expectNodeOnline(d2.page)
      // 翻到最早的一页：每个 turn 都有自己的 tool 块和 turn-usage
      // 点按钮和「滚到顶自动加载」会撞车：按钮可能在 `isVisible()` 之后、点击之前就没了（最后一页加载完），裸 `click()` 会在这时
      // 一直等一个不会再出现的按钮直到整个用例超时（180 秒）。所以点击带短超时，点不到就重新判断；翻页次数也设上限
      const older = d2.page.getByTestId('load-older')
      for (let page = 0; page < 20 && (await older.isVisible()); page++) {
        await older.click({ timeout: 2_000 }).catch(() => {})
        await expect(older)
          .toBeHidden({ timeout: 2_000 })
          .catch(() => {})
      }
      await expect(older).toBeHidden()
      await expect(d2.page.getByTestId('turn-usage')).toHaveCount(turns, {
        timeout: CONNECT_TIMEOUT,
      })

      // 全程没有「消息太大被 hub 丢掉」：hub 没有 dropped / too_large，daemon 也没有因为解密失败重置传输
      const hubLog = readLog(`${state.tmpDir}/hub.log`)
      const daemonLog = readLog(`${state.tmpDir}/daemon.log`)
      expect(hubLog).not.toMatch(/too_large|TooLarge|slow_consumer/)
      expect(daemonLog).not.toMatch(/too_large|TooLarge|decryption failed|transport reset/i)
    } finally {
      await d1.context.close()
      await d2.context.close()
    }
  })
})

test.describe('页面在后台', () => {
  test('e. hidden 满 30 秒断开 hub 连接，30 秒内回来不断开；回到前台重连并补同步', async ({
    browser,
  }) => {
    // 约 8 + 35 + 重连的时间，再留足余量
    test.setTimeout(150_000)
    const state = readState()
    const c = await pairNewBrowser(browser, state, { deviceName: 'e2e-background' })
    try {
      const { page } = c
      const deviceId = await webDeviceId(c.context.request, state, 'e2e-background')
      const online = (): Promise<boolean> => deviceOnline(c.context.request, state, deviceId)
      // Playwright 的无头页面始终是 visible，这里用脚本模拟 `visibilitychange`（enroll.spec.ts 同样做法）
      const setVisibility = (visibility: 'hidden' | 'visible'): Promise<void> =>
        page.evaluate((v) => {
          Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v })
          document.dispatchEvent(new Event('visibilitychange'))
        }, visibility)
      await expect.poll(online, { timeout: 15_000 }).toBe(true)

      // 30 秒内回来：连接没有断过（hub 里一直在线，节点一直 ready）
      await setVisibility('hidden')
      await page.waitForTimeout(8_000)
      expect(await online()).toBe(true)
      await expectNodeOnline(page)
      await setVisibility('visible')
      await page.waitForTimeout(1_000)
      expect(await online()).toBe(true)
      await expectNodeOnline(page)

      // 满 30 秒：hub 看到这台设备离线（此后 daemon 的 `notify` 才会走 Web Push），不早于 25 秒
      const hiddenAt = Date.now()
      await setVisibility('hidden')
      await expect
        .poll(async () => !(await online()), { timeout: 45_000, intervals: [1_000] })
        .toBe(true)
      expect(Date.now() - hiddenAt).toBeGreaterThanOrEqual(25_000)

      // 断开期间 daemon 那边发生了事：CLI 新建一个会话（页面此刻收不到任何东西）
      const sessionId = (
        await runCli(state, ['new', 'created while the page slept', '--no-wait'])
      ).trim()
      expect(sessionId).toMatch(/^[0-9A-Za-z_-]+$/)
      const row = page.locator(`a[href$="/s/${sessionId}"]`)
      await expect(row).toHaveCount(0)

      // 回到前台：自动重连（hub 里又在线，节点 ready），并补同步——新会话出现在列表里，不用刷新页面
      await setVisibility('visible')
      await expect.poll(online, { timeout: 20_000 }).toBe(true)
      await expectNodeOnline(page)
      await expect(row.first()).toBeVisible({ timeout: CONNECT_TIMEOUT }) // 侧栏、列表里各有一处链接
    } finally {
      await c.context.close()
    }
  })
})
