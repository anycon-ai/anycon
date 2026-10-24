/**
 * dev server 预览（ADR-39）：真实 hub（`ANYCON_PREVIEW_URL=http://*.preview.localhost:<端口>`）+ daemon（mock agent）+ 真浏览器，
 * 上游是 global-setup 起的 dev server 替身（`helpers/upstream.ts`，端口在 daemon 的 `preview.allow_ports` 里）。
 * Noise 隧道、预览运行时（引导页、service worker、shim）、hub 的预览域名和令牌、daemon 的票据和转发第一次在真浏览器里串起来。
 *
 *   a. 会话页「预览」→「其他端口」填上游端口 → iframe 里出现页面（标题、正文）、同源 WebSocket 的回显、较大脚本（gzip，多帧）完整到达；
 *      iframe 的 sandbox 不给 `allow-top-navigation`；hub / daemon 的日志里没有票据密钥、令牌，也没有预览内容
 *   b. cookie（服务端设的 HttpOnly 和非 HttpOnly 的、页面自己设的）随请求发出、`document.cookie` 只看得到非 HttpOnly 的、
 *      浏览器原生 cookie 里什么都没有；请求头改写；大请求体和大响应体（daemon 自己压缩）原样往返
 *   c. 地址栏输入路径跳转；上游的 302（`location` 指向 `localhost:<上游端口>`）被改写，停在预览 origin 内
 *   d. 「在新标签页打开」：新页面的 origin 是 `http://<label>.preview.localhost:<端口>`、内容正确、地址栏里没有 fragment
 *   e. 「停止预览」：iframe 撤掉、出现「预览已结束」；别处开着的预览（新标签页）显示「预览已被停止，回到 Anycon 重新打开」；「重新开始」后又能用
 *   f. 节点设置的「预览」标签列出票据，「关闭」后列表变空；另一个窗口里 iframe 的下一次导航显示结束页
 *   g. 安全回归：Web UI 的 origin 上取不到预览运行时的任何文件；预览域名上没有 Web UI / API；
 *      预览页（sandbox 的 iframe）不能 `top.location = …`，也读不了父页面
 *
 * 前提：`bun run --cwd web build` 同时产出 `web/dist` 和 `web/dist-preview`，hub 编译时两个目录都已存在（global-setup 会检查，见 e2e/README.md）。
 * Web UI 要由 hub 提供（默认）：预览页的 `frame-ancestors` 只允许 hub 自己的 origin，`E2E_WEB_MODE=preview` / `E2E_WEB_URL` 下 iframe 嵌不进去。
 * 约定：显式等待（data-testid、expect.poll），没有固定睡眠。每个用例自己配对自己的浏览器。
 */
import { createHash } from 'node:crypto'

import { expect, test, type Browser, type Frame, type Page } from '@playwright/test'

import { readLog } from '../helpers/process.js'
import {
  PROBE_LABEL,
  decodeLaunchUrl,
  fetchPreviewHost,
  labelOf,
  previewOriginOf,
} from '../helpers/preview.js'
import { readState } from '../helpers/state.js'
import {
  CONNECT_TIMEOUT,
  TURN_TIMEOUT,
  hubLogLeaks,
  nodeBase,
  pairNewBrowser,
  type PairedBrowser,
} from '../helpers/ui.js'
import {
  LANDED_HEADING,
  PAGE_HEADING,
  PAGE_TITLE,
  WS_ECHO_PREFIX,
  WS_GREETING,
  bigScript,
} from '../helpers/upstream.js'

test.beforeEach(() => {
  const state = readState()
  test.skip(!state.daemonStarted, 'needs the e2e daemon (E2E_SKIP_DAEMON=1 was set)')
  test.skip(
    state.webMode !== 'hub',
    'the preview is only embeddable in the web UI served by the hub (frame-ancestors)',
  )
})

/** iframe 里首屏（引导页 → service worker → 隧道握手 → 取页面）的上限。 */
const FRAME_TIMEOUT = CONNECT_TIMEOUT
/**
 * 一个用例的上限：配对一个新浏览器 + 新建会话 + 首屏通常 10 秒内，机器被别的构建占满时一个用例可以慢到一分多钟
 * （每一步都很快，只是整体变慢），留出余量。
 */
const TEST_TIMEOUT = 240_000

interface OpenedPreview {
  paired: PairedBrowser
  page: Page
  sessionUrl: string
  /** 控制台消息（页面和 iframe 的），按到达顺序。 */
  console: string[]
  /** 预览的 label 和 origin（从 iframe 的地址取）。 */
  label: string
  origin: string
}

/** 新建一个 scratch 会话（mock agent 回显一句话），返回会话页地址。 */
async function startScratchSession(page: Page): Promise<string> {
  await page.goto(`${nodeBase(page)}/new`)
  await expect(page.getByTestId('composer-input')).toBeVisible({ timeout: CONNECT_TIMEOUT })
  await page.getByTestId('composer-input').fill('hello preview')
  await page.getByTestId('composer-send').click()
  await expect(page).toHaveURL(/\/s\/sess_[0-9A-Za-z]+$/, { timeout: 20_000 })
  await expect(page.getByTestId('message-assistant').last()).toContainText(
    'You said: hello preview',
    { timeout: TURN_TIMEOUT },
  )
  return page.url()
}

/** 当前 iframe 的 `Frame`（iframe 每次重新打开都会换新的元素，所以每次现取）。 */
async function previewFrame(page: Page): Promise<Frame> {
  const handle = await page.getByTestId('preview-frame').elementHandle({ timeout: FRAME_TIMEOUT })
  const frame = await handle?.contentFrame()
  if (!frame) throw new Error('the preview iframe has no frame yet')
  return frame
}

/** 工具条上选「其他端口…」、填端口、点打开。 */
async function openOtherPort(page: Page, port: number): Promise<void> {
  await page.getByTestId('preview-port').selectOption('other')
  await page.getByTestId('preview-other-port').fill(String(port))
  await page.getByTestId('preview-other-open').click()
}

/**
 * 配对一个新浏览器、新建会话、切到「预览」、用「其他端口」打开上游，等 iframe 里出现上游的页面。
 * 上游的端口在 `preview.allow_ports` 里，所以端口列表里也有它（标「配置」），视图第一次拿到列表时会自动选它；
 * 这里仍然走「其他端口」的输入框（同一个端口再选一次，只是重新打开）。
 */
async function openPreview(browser: Browser, deviceName: string): Promise<OpenedPreview> {
  const state = readState()
  const paired = await pairNewBrowser(browser, state, { deviceName })
  const page = paired.page
  const consoleLines: string[] = []
  page.on('console', (msg) => consoleLines.push(`${msg.type()}: ${msg.text()}`))
  try {
    const sessionUrl = await startScratchSession(page)
    await page.getByTestId('main-view-preview').click()
    await expect(page.getByTestId('preview-view')).toBeVisible()
    // 端口列表加载完（加载中的占位消失）：之后选端口不会和“自动选中第一个”撞在一起
    await expect(page.getByTestId('preview-loading')).toHaveCount(0, { timeout: CONNECT_TIMEOUT })
    await openOtherPort(page, state.upstreamPort)
    await expect(page.getByTestId('preview-port')).toHaveValue(String(state.upstreamPort))
    const frame = page.frameLocator('[data-testid="preview-frame"]')
    await expect(frame.locator('#heading')).toHaveText(PAGE_HEADING, { timeout: FRAME_TIMEOUT })
    const f = await previewFrame(page)
    const label = labelOf(f.url())
    return {
      paired,
      page,
      sessionUrl,
      console: consoleLines,
      label,
      origin: previewOriginOf(label),
    }
  } catch (err) {
    await paired.context.close()
    throw err
  }
}

test.describe('dev server 预览（ADR-39）', () => {
  test('a. 其他端口 → iframe 里有页面、WebSocket 回显、较大脚本；sandbox 不给 top 导航；日志里没有密钥和内容', async ({
    browser,
  }) => {
    test.setTimeout(TEST_TIMEOUT)
    const state = readState()
    const p = await openPreview(browser, 'e2e-preview-a')
    try {
      const frame = p.page.frameLocator('[data-testid="preview-frame"]')
      const f = await previewFrame(p.page)

      // 页面本身：标题、正文、页面脚本看到的 origin 就是预览 origin（不是 localhost:<上游端口>）
      expect(await f.title()).toBe(PAGE_TITLE)
      await expect(frame.locator('#origin')).toHaveText(p.origin)
      expect(new URL(f.url()).origin).toBe(p.origin)
      expect(p.origin).toBe(`http://${p.label}.preview.localhost:${new URL(state.hubUrl).port}`)

      // 同源 WebSocket 经 shim 走隧道，上游的回显出现在 DOM 里
      await expect(frame.locator('#ws-status')).toHaveText('open', { timeout: FRAME_TIMEOUT })
      await expect(frame.locator('#ws-echo')).toHaveText(`${WS_ECHO_PREFIX}${WS_GREETING}`)

      // 约 600 KiB 的脚本（上游 gzip）完整到达：脚本自己校验了长度和指纹
      await expect(frame.locator('#big')).toHaveText(bigScript().expected, {
        timeout: FRAME_TIMEOUT,
      })

      // 这个 WebSocket 之外再开一条：子协议原样回来，二进制消息原样回显
      const roundTrip = await f.evaluate(async () =>
        (
          window as unknown as {
            e2e: {
              wsRoundTrip: (
                m: string,
                protocols?: string[],
              ) => Promise<{ echo: string | number[]; protocol: string }>
            }
          }
        ).e2e.wsRoundTrip('second socket', ['e2e.v1', 'e2e.v2']),
      )
      expect(roundTrip).toEqual({ echo: `${WS_ECHO_PREFIX}second socket`, protocol: 'e2e.v1' })

      // sandbox：不给 allow-top-navigation（web.md § 13.1）
      const sandbox = (await p.page.getByTestId('preview-frame').getAttribute('sandbox')) ?? ''
      expect(sandbox.split(/\s+/)).toContain('allow-scripts')
      expect(sandbox).not.toContain('allow-top-navigation')

      // 隐私：启动参数在 iframe 的 src 里（fragment），hub 的日志里不能有票据密钥、令牌、票据 id，也不能有预览的内容；
      // daemon 的日志里没有票据密钥、令牌
      const src = (await p.page.getByTestId('preview-frame').getAttribute('src')) ?? ''
      const launch = decodeLaunchUrl(src)
      expect(launch.label).toBe(p.label)
      expect(launch.port).toBe(state.upstreamPort)
      expect(launch.psk).toMatch(/^[0-9a-f]{64}$/)
      const secrets = [launch.psk, launch.token, launch.ticket_id]
      const content = [PAGE_HEADING, PAGE_TITLE, WS_GREETING, '/big.js', '/app.js', 'hello preview']
      expect(hubLogLeaks(state, [...secrets, ...content])).toEqual([])
      const daemonLog = readLog(`${state.tmpDir}/daemon.log`)
      for (const secret of [launch.psk, launch.token]) expect(daemonLog).not.toContain(secret)
    } finally {
      await p.paired.context.close()
    }
  })

  test('b. cookie、请求头改写、大请求体和大响应体', async ({ browser }) => {
    test.setTimeout(TEST_TIMEOUT)
    const state = readState()
    const p = await openPreview(browser, 'e2e-preview-b')
    try {
      const frame = p.page.frameLocator('[data-testid="preview-frame"]')
      const f = await previewFrame(p.page)

      // ---- cookie ----
      // 一开始什么 cookie 都没有
      await expect(frame.locator('#cookies')).toHaveText('')
      expect(await f.evaluate(() => document.cookie)).toBe('')
      // 先访问设 cookie 的接口
      expect(await f.evaluate(async () => (await fetch('/api/set-cookie')).status)).toBe(200)
      // 再请求回显接口：上游收到的 Cookie 头里有它（service worker 的 cookie jar 加上的）
      const seen = await f.evaluate(
        async () => ((await (await fetch('/api/cookie')).json()) as { cookie: string }).cookie,
      )
      expect(seen).toContain('e2e_plain=plain-value')
      expect(seen).toContain('e2e_secret=secret-value')
      // 页面的 document.cookie（shim 提供）只有非 HttpOnly 的
      await expect.poll(() => f.evaluate(() => document.cookie)).toBe('e2e_plain=plain-value')
      // 页面自己设的 cookie 也会随之后的请求发出去
      await f.evaluate(() => {
        document.cookie = 'e2e_page=from-page; path=/'
      })
      await expect
        .poll(() =>
          f.evaluate(
            async () => ((await (await fetch('/api/cookie')).json()) as { cookie: string }).cookie,
          ),
        )
        .toContain('e2e_page=from-page')
      // 刷新：页面脚本读到的 document.cookie 写进 DOM（初值来自 service worker 注入的 data-cookies）
      await f.evaluate(() => {
        setTimeout(() => location.reload(), 0)
      })
      await expect(frame.locator('#cookies')).toContainText('e2e_plain=plain-value', {
        timeout: FRAME_TIMEOUT,
      })
      await expect(frame.locator('#cookies')).toContainText('e2e_page=from-page')
      await expect(frame.locator('#cookies')).not.toContainText('e2e_secret')
      // 浏览器原生的 cookie 里什么都没有：否则它们会随 /v1/ws、/__anycon/* 的请求发给 hub
      expect(await p.paired.context.cookies([p.origin])).toEqual([])

      // ---- 请求头改写（docs/spec/daemon.md § 19.5）----
      const g = await previewFrame(p.page)
      const headers = await g.evaluate(
        async () =>
          ((await (await fetch('/api/headers')).json()) as { headers: Record<string, string> })
            .headers,
      )
      expect(headers['host']).toBe(`localhost:${state.upstreamPort}`)
      expect(headers['accept-encoding']).toContain('gzip')
      expect(
        Object.keys(headers).filter((k) => k.startsWith('x-forwarded') || k === 'forwarded'),
      ).toEqual([])
      expect(headers['connection']).not.toContain('upgrade')

      // ---- 大请求体（多帧上行）和同样大小的响应体（上游是 JSON，daemon 自己 gzip）原样往返 ----
      const body = '0123456789abcdef'.repeat(20_000)
      const echoed = await g.evaluate(async (payload) => {
        const res = await fetch('/api/echo?x=1', {
          method: 'POST',
          headers: { 'content-type': 'text/plain' },
          body: payload,
        })
        return (await res.json()) as {
          method: string
          search: string
          contentType: string
          body: string
        }
      }, body)
      expect(echoed.method).toBe('POST')
      expect(echoed.search).toBe('?x=1')
      expect(echoed.contentType).toBe('text/plain')
      expect(echoed.body.length).toBe(body.length)
      expect(echoed.body === body).toBe(true)

      // ---- 上游不压缩的大脚本：daemon 自己压缩，浏览器拿到的字节和上游发的一样 ----
      const big = bigScript().source
      const digest = await g.evaluate(async () => {
        const buf = await (await fetch('/big-identity.js')).arrayBuffer()
        const sum = await crypto.subtle.digest('SHA-256', buf)
        return {
          length: buf.byteLength,
          sha256: Array.from(new Uint8Array(sum), (b) => b.toString(16).padStart(2, '0')).join(''),
        }
      })
      expect(digest).toEqual({
        length: Buffer.byteLength(big),
        sha256: createHash('sha256').update(big).digest('hex'),
      })

      // 隐私：cookie 的值、请求体不在 hub 的日志里
      expect(hubLogLeaks(state, ['plain-value', 'secret-value', 'from-page', '/api/echo'])).toEqual(
        [],
      )
    } finally {
      await p.paired.context.close()
    }
  })

  test('c. 地址栏输入路径跳转；上游的 302 被改写，停在预览 origin 内', async ({ browser }) => {
    test.setTimeout(TEST_TIMEOUT)
    const state = readState()
    const p = await openPreview(browser, 'e2e-preview-c')
    try {
      const frame = p.page.frameLocator('[data-testid="preview-frame"]')
      const path = p.page.getByTestId('preview-path')
      await expect(path).toHaveValue('/')

      // 地址栏：回车跳转到 /landed
      await path.fill('/landed')
      await path.press('Enter')
      await expect(frame.locator('#heading')).toHaveText(LANDED_HEADING, {
        timeout: FRAME_TIMEOUT,
      })
      let f = await previewFrame(p.page)
      expect(f.url()).toBe(`${p.origin}/landed`)

      // 回到 /，再走 302：上游发的是 `location: http://localhost:<上游端口>/landed`
      await path.fill('/')
      await path.press('Enter')
      await expect(frame.locator('#heading')).toHaveText(PAGE_HEADING, { timeout: FRAME_TIMEOUT })
      await path.fill('/redirect')
      await path.press('Enter')
      await expect(frame.locator('#heading')).toHaveText(LANDED_HEADING, {
        timeout: FRAME_TIMEOUT,
      })
      f = await previewFrame(p.page)
      expect(f.url()).toBe(`${p.origin}/landed`)
      await expect(frame.locator('#origin')).toHaveText(p.origin)
      expect(f.url()).not.toContain(String(state.upstreamPort))

      // 页面里的 fetch 也一样：跟着跳转之后的地址在预览 origin 内
      const followed = await f.evaluate(async () => {
        const res = await fetch('/redirect')
        return { url: res.url, redirected: res.redirected, status: res.status }
      })
      expect(followed).toEqual({ url: `${p.origin}/landed`, redirected: true, status: 200 })

      // 粘贴整条 URL 也只取路径（web.md § 13.1）：地址栏不会让 iframe 去别的 origin
      await path.fill(`http://localhost:${state.upstreamPort}/landed?x=1`)
      await path.press('Enter')
      await expect(path).toHaveValue('/landed?x=1')
      await expect(frame.locator('#heading')).toHaveText(LANDED_HEADING, {
        timeout: FRAME_TIMEOUT,
      })
      f = await previewFrame(p.page)
      expect(f.url()).toBe(`${p.origin}/landed?x=1`)

      // Web UI 自己没有被带走
      expect(p.page.url()).toBe(p.sessionUrl)
    } finally {
      await p.paired.context.close()
    }
  })

  test('d. 在新标签页打开：origin 是 <label>.preview.localhost，内容正确，地址栏里没有 fragment', async ({
    browser,
  }) => {
    test.setTimeout(TEST_TIMEOUT)
    const state = readState()
    const p = await openPreview(browser, 'e2e-preview-d')
    try {
      // 地址栏里先输一个路径：新标签页从这个路径开始
      await p.page.getByTestId('preview-path').fill('/landed')
      await p.page.getByTestId('preview-path').press('Enter')
      await expect(
        p.page.frameLocator('[data-testid="preview-frame"]').locator('#heading'),
      ).toHaveText(LANDED_HEADING, { timeout: FRAME_TIMEOUT })

      const [tab] = await Promise.all([
        p.paired.context.waitForEvent('page'),
        p.page.getByTestId('preview-open-tab').click(),
      ])
      try {
        await expect(tab.locator('#heading')).toHaveText(LANDED_HEADING, {
          timeout: FRAME_TIMEOUT,
        })
        const url = new URL(tab.url())
        expect(url.origin).toBe(`http://${p.label}.preview.localhost:${new URL(state.hubUrl).port}`)
        expect(url.origin).toBe(p.origin)
        expect(url.pathname).toBe('/landed')
        // 启动参数（密钥和令牌）在 fragment 里：进了页面就被 replaceState 抹掉，历史记录里也不留
        expect(url.hash).toBe('')
        expect(url.search).toBe('')
        expect(
          await tab.evaluate(() => ({ length: history.length, hash: location.hash })),
        ).toMatchObject({ hash: '' })

        // 新标签页里的页面也是完整可用的：WebSocket、较大脚本
        await tab.goto(`${p.origin}/`)
        await expect(tab.locator('#ws-echo')).toHaveText(`${WS_ECHO_PREFIX}${WS_GREETING}`, {
          timeout: FRAME_TIMEOUT,
        })
        await expect(tab.locator('#big')).toHaveText(bigScript().expected, {
          timeout: FRAME_TIMEOUT,
        })
        expect(await tab.title()).toBe(PAGE_TITLE)
        // 原来的 Web UI 页面还在原地
        expect(p.page.url()).toBe(p.sessionUrl)
      } finally {
        await tab.close()
      }
    } finally {
      await p.paired.context.close()
    }
  })

  test('e. 停止预览：iframe 撤掉，别处开着的预览显示已结束，重新开始后又能用', async ({
    browser,
  }) => {
    test.setTimeout(TEST_TIMEOUT)
    const p = await openPreview(browser, 'e2e-preview-e')
    try {
      const [tab] = await Promise.all([
        p.paired.context.waitForEvent('page'),
        p.page.getByTestId('preview-open-tab').click(),
      ])
      try {
        await expect(tab.locator('#heading')).toHaveText(PAGE_HEADING, { timeout: FRAME_TIMEOUT })
        await expect(tab.locator('#ws-echo')).toHaveText(`${WS_ECHO_PREFIX}${WS_GREETING}`)

        // 工具条「停止预览」→ 确认
        await p.page.getByTestId('preview-stop').click()
        await expect(p.page.getByTestId('preview-stop-dialog')).toBeVisible()
        await p.page.getByTestId('preview-stop-confirm').click()
        await expect(p.page.getByTestId('preview-stopped')).toBeVisible()
        await expect(p.page.getByTestId('preview-frame')).toHaveCount(0)
        await expect(p.page.getByTestId('preview-stop')).toBeDisabled()

        // 新标签页是别处开着的预览：隧道被 daemon 以 GOAWAY { closed } 断开，之后的页面请求得到“已停止”的说明页。
        // 断开消息经 hub 到 service worker 要一点时间，所以刷新到出现说明页为止（有界轮询）。
        await expect(async () => {
          await tab.reload()
          await expect(tab.getByRole('heading', { level: 1 })).toHaveText(
            /This preview was (stopped|closed)|这个预览已被(停止|关闭)/,
            { timeout: 5000 },
          )
        }).toPass({ timeout: 30_000 })
        await expect(tab.locator('body')).toContainText(/open the preview again|重新打开预览/)
        await expect(tab.locator('#heading')).toHaveCount(0)

        // 「重新开始」：新票据（同一个端口，label 不变）→ iframe 里页面又出现
        await p.page.getByTestId('preview-restart').click()
        await expect(
          p.page.frameLocator('[data-testid="preview-frame"]').locator('#heading'),
        ).toHaveText(PAGE_HEADING, { timeout: FRAME_TIMEOUT })
        const f = await previewFrame(p.page)
        expect(labelOf(f.url())).toBe(p.label)
      } finally {
        await tab.close()
      }
    } finally {
      await p.paired.context.close()
    }
  })

  test('f. 节点设置的「预览」标签列出票据，「关闭」后列表变空，开着的 iframe 下一次导航显示结束页', async ({
    browser,
  }) => {
    test.setTimeout(TEST_TIMEOUT)
    const state = readState()
    const p = await openPreview(browser, 'e2e-preview-f')
    const settings = await p.paired.context.newPage()
    try {
      await settings.goto(`${nodeBase(p.page)}/settings`)
      await settings.getByTestId('tab-preview').click()
      await expect(settings.getByTestId('preview-tab')).toBeVisible({ timeout: CONNECT_TIMEOUT })
      const rows = settings.locator(`[data-testid="preview-row"][data-label="${p.label}"]`)
      // 这台设备开了一张票据，端口是上游的；其他用例的设备留下的票据也在里面（同一个端口、同一个 label）
      const mine = rows.filter({ hasText: 'e2e-preview-f' })
      await expect(mine).toHaveCount(1, { timeout: CONNECT_TIMEOUT })
      await expect(mine.getByTestId('preview-row-port')).toHaveText(String(state.upstreamPort))
      await expect(mine.getByTestId('preview-row-device')).toContainText('e2e-preview-f')
      await expect(mine.getByTestId('preview-row-session')).toBeVisible()
      await expect(mine.getByTestId('preview-row-expires')).toBeVisible()
      // 这台设备的 iframe 里 service worker 连着隧道，此刻至少一条
      await expect(mine.getByTestId('preview-row-tunnels')).toContainText(/[1-9]/)
      // 端口策略说明（只读）
      await expect(settings.getByTestId('preview-policy')).toContainText('preview.allow_ports')

      // 关闭：确认对话框 → 列表变空（同一个 label 的全部票据一起作废）
      await rows.first().getByTestId('preview-row-close').click()
      await expect(settings.getByTestId('preview-close-dialog')).toBeVisible()
      await settings.getByTestId('preview-close-confirm').click()
      await expect(settings.getByTestId('preview-list-empty')).toBeVisible({ timeout: 15_000 })
      await expect(settings.getByTestId('preview-row')).toHaveCount(0)

      // 另一个窗口里的 iframe：隧道被断开，地址栏再跳一次，iframe 里是“已停止”的说明页
      const path = p.page.getByTestId('preview-path')
      await expect(async () => {
        await path.fill('/landed')
        await path.press('Enter')
        const frame = p.page.frameLocator('[data-testid="preview-frame"]')
        await expect(frame.getByRole('heading', { level: 1 })).toHaveText(
          /This preview was (stopped|closed)|这个预览已被(停止|关闭)/,
          { timeout: 5000 },
        )
      }).toPass({ timeout: 30_000 })
      await expect(
        p.page.frameLocator('[data-testid="preview-frame"]').locator('body'),
      ).toContainText(/open the preview again|重新打开预览/)
    } finally {
      await p.paired.context.close()
    }
  })

  test('g. 安全回归：主 origin 上没有预览运行时；预览域名上没有 Web UI；iframe 不能导航 top、读不了父页面', async ({
    browser,
  }) => {
    test.setTimeout(TEST_TIMEOUT)
    const state = readState()
    const p = await openPreview(browser, 'e2e-preview-g')
    try {
      // ---- Web UI 的 origin：预览运行时的文件取不到（否则那里能被注册一个隧道到别人节点的 service worker）----
      const main = await p.page.evaluate(async () => {
        const index = await (await fetch('/')).text()
        const enter = await fetch('/__anycon/enter')
        const registrations = await navigator.serviceWorker.getRegistrations()
        return {
          sw: (await fetch('/__anycon/sw.js')).status,
          shim: (await fetch('/__anycon/shim.js')).status,
          config: (await fetch('/__anycon/config.json')).status,
          // 没有扩展名的路径走 SPA 回退：回的是 Web UI 自己的 index.html，不是预览的引导页
          enterIsWebUi: (await enter.text()) === index,
          scripts: registrations.map((r) => r.active?.scriptURL ?? r.installing?.scriptURL ?? ''),
        }
      })
      expect(main.sw).toBe(404)
      expect(main.shim).toBe(404)
      expect(main.config).toBe(404)
      expect(main.enterIsWebUi).toBe(true)
      for (const script of main.scripts) expect(script).not.toContain('/__anycon/')

      // ---- 预览域名：只有预览运行时，没有 Web UI / API / auth ----
      const sw = await fetchPreviewHost('/__anycon/sw.js', { label: p.label })
      expect(sw.status).toBe(200)
      expect(sw.headers['service-worker-allowed']).toBe('/')
      expect(sw.headers['x-frame-options']).toBeUndefined()
      const enter = await fetchPreviewHost('/__anycon/enter', { label: PROBE_LABEL })
      expect(enter.status).toBe(200)
      expect(String(enter.headers['content-security-policy'])).toContain(
        `frame-ancestors ${state.hubUrl}`,
      )
      const api = await fetchPreviewHost('/api/me', { label: p.label })
      expect(api.status).toBe(200)
      expect(String(api.headers['content-type'])).toContain('text/html')
      expect(api.body).toBe(enter.body) // 引导页，不是 /api/me 的 JSON，也不是 Web UI
      expect(
        (await fetchPreviewHost('/auth/token', { label: p.label, method: 'POST' })).status,
      ).toBe(405)
      // 没有启动参数的浏览器（全新的存储，没有 service worker）直接打开预览地址：引导页只说“要从 Anycon 打开”，
      // 不会因为知道 label 就连上隧道
      const stranger = await browser.newContext()
      try {
        const strangerPage = await stranger.newPage()
        await strangerPage.goto(`${p.origin}/some/path`)
        await expect(strangerPage.locator('[data-state="needs_launch"]')).toBeVisible({
          timeout: FRAME_TIMEOUT,
        })
        await expect(strangerPage.getByRole('heading', { level: 1 })).toHaveText(
          /Open this preview from Anycon|请从 Anycon 打开这个预览/,
        )
      } finally {
        await stranger.close()
      }
      // label 格式不对：404
      expect((await fetchPreviewHost('/', { label: 'not-a-label' })).status).toBe(404)

      // ---- 预览页（sandbox 的 iframe）不能 top.location = …，也读不了父页面 ----
      const f = await previewFrame(p.page)
      const hijack = `${state.hubUrl}/hijacked`
      // sandbox 没有 allow-top-navigation：赋值被浏览器拒绝（SecurityError），Web UI 没有被导航走
      const outcome = await f.evaluate((target) => {
        try {
          window.top!.location.href = target
          return 'navigated'
        } catch (err) {
          return `blocked: ${(err as Error).name}`
        }
      }, hijack)
      expect(outcome).toBe('blocked: SecurityError')
      expect(p.page.url()).toBe(p.sessionUrl)
      // 别的导航方式同理：window.open(…, '_top')、<a target=_top> 的点击也不会带走 top
      await f.evaluate((target) => {
        window.open(target, '_top')
      }, hijack)
      await expect
        .poll(() => p.console.join('\n'), { timeout: 15_000 })
        .toMatch(/allow-top-navigation|sandboxed/i)
      expect(p.page.url()).toBe(p.sessionUrl)
      expect(
        await f.evaluate(() => {
          try {
            return `read: ${window.parent.document.title}`
          } catch (err) {
            return `blocked: ${(err as Error).name}`
          }
        }),
      ).toBe('blocked: SecurityError')
      // 页面仍然可用（刚才的尝试没有把 iframe 弄坏）
      await expect(
        p.page.frameLocator('[data-testid="preview-frame"]').locator('#heading'),
      ).toHaveText(PAGE_HEADING)
    } finally {
      await p.paired.context.close()
    }
  })
})
