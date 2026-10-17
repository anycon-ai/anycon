/**
 * 跨用例共用的页面操作。UI 文案中英都可能出现（Playwright 默认 en-US），断言用正则兼容；
 * 能用 data-testid 的地方一律用 data-testid。
 */
import {
  expect,
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type BrowserContextOptions,
  type Locator,
  type Page,
} from '@playwright/test'

import { startPairing, type PairingSession } from './cli.js'
import { readLog } from './process.js'
import type { E2EState } from './env.js'

/** 首次与 daemon 建立连接（hub 认证 → peer_online → IK 握手 → 首次同步）的上限。 */
export const CONNECT_TIMEOUT = 45_000
/** 一个 turn（含权限往返）的上限。 */
export const TURN_TIMEOUT = 30_000

/** 页面里的 i18n 文案可能是英文或中文，用正则兼容两种。 */
export const ALLOWED_BY = /Allowed by|已由.*允许/
export const REPAIR_TEXT = /re-?pair|pair again|重新配对|需要重新配对/i

export const SHORT_CODE_RE = /^[2-9A-HJKMNP-Z]{4}-[2-9A-HJKMNP-Z]{4}$/
export const LONG_CODE_RE = /^([2-9A-HJKMNP-Z]{4}-){3}[2-9A-HJKMNP-Z]{4}$/

/**
 * hub 对 token 登录的限速（每 IP 令牌桶：容量 10、每 6 秒回 1 个，`auth_token_per_min`）的本地模拟：下面两个登录 helper 每成功发出一次就记一笔。
 * 用途是文件之间留余量：一个文件登录得很快（比如 enroll.spec 不再被 `enroll_begin` 的限速拖慢）时，后面文件里**裸点「登录」按钮**
 * 的步骤就会撞上 429。happy-path 第 9 步已经改用 `submitLogin`（会按提示等）；还有裸点按钮的地方 `await loginHeadroom(n)`。
 * Playwright 在同一个 worker 里按顺序跑各个文件，模块状态是共享的。
 */
const LOGIN_CAPACITY = 10
const LOGIN_REFILL_MS = 6_000
let loginTokens = LOGIN_CAPACITY
let loginAt = Date.now()

function refillLogins(): void {
  const now = Date.now()
  loginTokens = Math.min(LOGIN_CAPACITY, loginTokens + (now - loginAt) / LOGIN_REFILL_MS)
  loginAt = now
}

function spendLogin(): void {
  refillLogins()
  loginTokens = Math.max(0, loginTokens - 1)
}

/** 等到（本地模拟的）登录限速桶里至少有 `minTokens` 个令牌：对 hub 已知速率的有界等待，不是固定睡眠。 */
export async function loginHeadroom(minTokens: number): Promise<void> {
  const want = Math.min(minTokens, LOGIN_CAPACITY)
  for (;;) {
    refillLogins()
    if (loginTokens >= want) return
    await new Promise((resolve) => setTimeout(resolve, (want - loginTokens) * LOGIN_REFILL_MS))
  }
}

/**
 * 登录页上展开 token 表单。global-setup 起的 hub 配了 SMTP（ADR-37），登录页同时有邮箱验证码和 token，这时 token 表单折叠在
 * 「使用 token」（`login-use-token`）后面；没配 SMTP 的 hub（或已经展开了）直接就是表单，什么都不用点。
 */
export async function revealTokenForm(page: Page): Promise<void> {
  const input = page.getByTestId('login-token-input')
  const toggle = page.getByTestId('login-use-token')
  // 页面要先读到 /api/config 才知道有哪些登录方式
  await expect(input.or(toggle)).toBeVisible()
  if (await toggle.isVisible()) await toggle.click()
  await expect(input).toBeVisible()
}

/** 断言当前是登录页（token 表单可能折叠着，所以看「使用 token」或表单本身）。 */
export async function expectLoginPage(page: Page): Promise<void> {
  await expect(
    page.getByTestId('login-use-token').or(page.getByTestId('login-token-input')),
  ).toBeVisible()
}

/**
 * 粘贴 token 登录。hub 对 token 登录按 IP 限速（`auth_token_per_min`，默认每分钟 10 次），并行的用例多了会撞上：
 * 页面显示 429 的提示（「Too many attempts. Try again in N s.」）时，按 hub 给的秒数等一下再点，最多重试 90 秒。
 * 这是对服务端 `retry_after` 的有界等待，不是固定睡眠。
 */
export async function login(page: Page, adminToken: string): Promise<void> {
  await page.goto('/login')
  await submitLogin(page, adminToken)
}

/**
 * 在已经打开的登录页上粘贴 token 并提交（不先跳 `/login`：比如配对链接把未登录的浏览器带到登录页，要保留它的回跳目标）。
 * 429 的处理同 `login`。
 */
export async function submitLogin(page: Page, adminToken: string): Promise<void> {
  await revealTokenForm(page)
  await page.getByTestId('login-token-input').fill(adminToken)
  // 提示由 Field 的 error 渲染成普通段落（没有 role）
  const limited = page.getByText(/Too many attempts|尝试次数过多/)
  const deadline = Date.now() + 90_000
  for (;;) {
    await page.getByTestId('login-submit').click()
    const outcome = await Promise.race([
      page
        .waitForURL((u) => !u.pathname.startsWith('/login'), { timeout: 15_000 })
        .then(() => 'ok' as const),
      limited.waitFor({ timeout: 15_000 }).then(() => 'limited' as const),
    ]).catch(() => 'unknown' as const)
    // 'ok' 和 'unknown' 都交给调用方的断言（它们会给出更具体的失败信息）
    if (outcome !== 'limited') {
      spendLogin()
      return
    }
    if (Date.now() > deadline) throw new Error('login stayed rate-limited for 90 s')
    const seconds = Number((await limited.textContent())?.match(/(\d+)/)?.[1] ?? 5)
    await page.waitForTimeout(Math.min(60, seconds) * 1000 + 250)
  }
}

/**
 * 不经页面、用 Playwright 的 `request` 做 token 登录（之后同一个 `request` 上下文带着会话 cookie 调 `/api/*`）。
 * 和 `login` 一样会遇到 hub 的按 IP 限速（429 + `Retry-After`）：按 hub 给的秒数等，最多 90 秒。
 */
export async function loginViaApi(request: APIRequestContext, state: E2EState): Promise<void> {
  const deadline = Date.now() + 90_000
  for (;;) {
    const res = await request.post(`${state.hubUrl}/auth/token`, {
      data: { token: state.adminToken },
      headers: { 'X-Anycon': '1' },
    })
    if (res.status() !== 429) {
      spendLogin()
      expect(res.ok(), `POST /auth/token → ${res.status()}`).toBe(true)
      return
    }
    if (Date.now() > deadline) throw new Error('token login stayed rate-limited for 90 s')
    const seconds = Number(res.headers()['retry-after'] ?? 5)
    await new Promise((resolve) => setTimeout(resolve, Math.min(60, seconds) * 1000 + 250))
  }
}

/** 在 `/pair` 里输入短码并提交（完成后落到 `/n/<node>`）。 */
export async function pairBrowser(page: Page, code: string, deviceName?: string): Promise<void> {
  await page.getByTestId('pair-code-input').fill(code)
  if (deviceName) {
    await page.getByRole('button', { name: /Change name|修改名称|更改名称|重命名/ }).click()
    await page.getByTestId('pair-device-name').fill(deviceName)
  }
  await page.getByTestId('pair-submit').click()
}

/** 等节点在线（顶栏 chip）。 */
export async function expectNodeOnline(page: Page): Promise<void> {
  await expect(page.getByTestId('node-status').first()).toHaveAttribute('data-state', 'ready', {
    timeout: CONNECT_TIMEOUT,
  })
}

/** 当前会话页里「待审批」的权限卡片。 */
export function pendingCard(page: Page): Locator {
  return page.locator('[data-testid="permission-card"][data-state="pending"]')
}

export function resolvedCards(page: Page): Locator {
  return page.locator('[data-testid="permission-card"][data-state="resolved"]')
}

/** 当前页面在 `/n/<node>` 下时，返回这个前缀（不含末尾斜杠）。 */
export function nodeBase(page: Page): string {
  const m = new URL(page.url()).pathname.match(/^\/n\/[^/]+/)
  if (!m) throw new Error(`not on a node page: ${page.url()}`)
  return m[0]
}

/**
 * 在项目页点头部的「新会话」按钮，进到该项目的新会话空白页（`/n/<node>/new?project=<name>`），等输入框出现。
 * 项目页本身没有输入框了：新建会话统一在空白页里做（左栏项目旁的「＋」`sidebar-project-new` 也是去这一页）。
 */
export async function openNewSessionFromProject(page: Page, project: string): Promise<void> {
  await page.getByTestId('project-new-session').click()
  await expect(page).toHaveURL(
    (url) => /^\/n\/[^/]+\/new$/.test(url.pathname) && url.searchParams.get('project') === project,
    { timeout: 15_000 },
  )
  // 选项行要等节点的 agent 列表到了才出现（首次连接可能慢）；项目下拉已经选中这个项目
  await expect(page.getByTestId('new-session-project')).toHaveAttribute('data-value', project, {
    timeout: CONNECT_TIMEOUT,
  })
  await expect(page.getByTestId('composer-input')).toBeVisible()
}

export interface PairedBrowser {
  context: BrowserContext
  page: Page
  pairing: PairingSession
  deviceName: string
}

/**
 * 开一个全新的、存储隔离的浏览器上下文，登录管理员账号，用 `anycon pair --json [--short]` 的短码配对成一台新设备，
 * 等节点在线后返回。每个用例自己配对自己的浏览器，互不依赖。
 */
export async function pairNewBrowser(
  browser: Browser,
  state: E2EState,
  options: {
    deviceName: string
    short?: boolean
    /** 新上下文的选项（比如通知权限）。 */
    contextOptions?: BrowserContextOptions
    /** 开页面之前对新上下文做的准备（比如 `addInitScript` 打桩）。 */
    prepare?: (context: BrowserContext) => Promise<void>
  },
): Promise<PairedBrowser> {
  const context = await browser.newContext(options.contextOptions)
  try {
    await options.prepare?.(context)
    const page = await context.newPage()
    await login(page, state.adminToken)
    // 这个浏览器还没有已配对的节点：账号下还没有 daemon 时落到 /setup，已经有时落到 /unlock/<node>（ADR-36）；直接去 /pair
    await expect(page).toHaveURL(/\/(setup|unlock(\/[0-9a-f]+)?|n\/[0-9a-f]+)$/, {
      timeout: 15_000,
    })
    await page.goto('/pair')
    const pairing = await startPairing(state, { short: options.short })
    await pairBrowser(page, pairing.code, options.deviceName)
    await expect(page).toHaveURL(/\/n\/[0-9a-f]+$/, { timeout: CONNECT_TIMEOUT })
    await expectNodeOnline(page)
    expect(await pairing.done).toBe(0)
    return { context, page, pairing, deviceName: options.deviceName }
  } catch (err) {
    await context.close()
    throw err
  }
}

/** 节点设置 → 设备 标签。 */
export async function openDevicesTab(page: Page): Promise<Locator> {
  await page.goto(`${nodeBase(page)}/settings`)
  await page.getByTestId('tab-devices').click()
  const list = page.getByTestId('device-list')
  await expect(list).toBeVisible({ timeout: CONNECT_TIMEOUT })
  return list.locator('li')
}

/**
 * 当前浏览器 IndexedDB 的概要：每个库每个 object store 里有多少条。
 * 「退出并移除本设备」之后，所有 store 必须是空的（库被删了也算）。
 */
export async function indexedDbSummary(
  page: Page,
): Promise<{ db: string; stores: Record<string, number> }[]> {
  return page.evaluate(async () => {
    const infos = (await indexedDB.databases?.()) ?? []
    const out: { db: string; stores: Record<string, number> }[] = []
    for (const info of infos) {
      if (!info.name) continue
      const db = await new Promise<IDBDatabase>((resolve, reject) => {
        const req = indexedDB.open(info.name as string)
        req.onsuccess = () => resolve(req.result)
        req.onerror = () => reject(req.error)
      })
      const stores: Record<string, number> = {}
      for (const name of Array.from(db.objectStoreNames)) {
        stores[name] = await new Promise<number>((resolve, reject) => {
          const req = db.transaction(name, 'readonly').objectStore(name).count()
          req.onsuccess = () => resolve(req.result)
          req.onerror = () => reject(req.error)
        })
      }
      db.close()
      out.push({ db: info.name, stores })
    }
    return out
  })
}

/** hub 的日志里不应该出现的东西（短码、match_id）：整份日志里找，大小写不敏感，连字符有无都找。 */
export function hubLogLeaks(state: Pick<E2EState, 'tmpDir'>, secrets: string[]): string[] {
  const log = readLog(`${state.tmpDir}/hub.log`).toLowerCase()
  const hits: string[] = []
  for (const secret of secrets) {
    for (const needle of new Set([secret, secret.replaceAll('-', '')])) {
      if (needle && log.includes(needle.toLowerCase())) hits.push(needle)
    }
  }
  return hits
}
