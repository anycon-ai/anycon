/**
 * 邮箱验证码登录（ADR-37）：真 hub（global-setup 配了 `ANYCON_SMTP_URL`）+ 真 web + 假 SMTP 服务（`helpers/smtp.ts`）。
 *
 * 账号设置里绑定邮箱（发码 → 从假 SMTP 取码 → 验证）→ 新的浏览器上下文在登录页用邮箱 + 码登录（先输错一次）→
 * 没绑定的邮箱得到同样的「已发出」提示，但假 SMTP 没收到信 → 解绑。
 *
 * **hub 的发码限速**：每 IP（这里都是 127.0.0.1）每小时 5 次，登录和绑定各一个桶；同一邮箱同一用途 60 秒一次。
 * 这个文件登录发码 2 次（已绑定的 + 没绑定的）、绑定发码 1 次，同一个 hub 上别的文件不发邮箱验证码
 * （`notifications.spec.ts` 用另一个邮箱地址绑定 1 次）。不要在同一个 hub 上 `--repeat-each` 这个文件。
 */
import { expect, test, type BrowserContext, type Page } from '@playwright/test'

import { readLog } from '../helpers/process.js'
import { mailHeader, mailsTo, mailText, readMails, waitForCode } from '../helpers/smtp.js'
import { readState } from '../helpers/state.js'
import { login } from '../helpers/ui.js'

const ADDRESS = 'e2e-owner@example.test'
/** 输入时故意写成大小写混合：hub 把邮箱规范化成小写。 */
const TYPED = 'E2E-Owner@Example.Test'
const UNBOUND = 'nobody-here@example.test'

/** 一个不等于 `real` 的 6 位数字码。 */
const wrongCode = (real: string): string => (real === '000000' ? '111111' : '000000')

test.describe.configure({ mode: 'serial' })

test.describe('邮箱验证码登录', () => {
  let ownerContext: BrowserContext
  let owner: Page
  /** 登录页「已发出」提示的文字，邮箱地址换成占位符：绑定和没绑定的必须一模一样。 */
  let sentNotice = ''
  /** 这个文件里发出过的真验证码（第 5 个用例确认 hub 的日志里没有它们）。 */
  const issuedCodes: string[] = []

  test.beforeAll(async ({ browser }) => {
    ownerContext = await browser.newContext()
    owner = await ownerContext.newPage()
    await login(owner, readState().adminToken)
  })

  test.afterAll(async () => {
    await ownerContext?.close()
  })

  test('1. 账号设置里绑定邮箱：发码 → 从假 SMTP 取码 → 验证', async () => {
    const state = readState()
    await owner.goto('/settings')
    await expect(owner.getByTestId('email-section')).toBeVisible()
    await owner.getByTestId('email-link').click()
    await owner.getByTestId('email-input').fill(TYPED)

    const before = mailsTo(state.smtpDir, ADDRESS).length
    await owner.getByTestId('email-send').click()
    await expect(owner.getByTestId('email-sent')).toContainText(ADDRESS)
    const code = await waitForCode(state.smtpDir, ADDRESS, { after: before })
    expect(code).toMatch(/^\d{6}$/)
    issuedCodes.push(code)

    // 绑定码是一封发给规范化（小写）地址的信，来自配置的发件人
    const mails = mailsTo(state.smtpDir, ADDRESS)
    expect(mails).toHaveLength(before + 1)
    expect(mailHeader(mails.at(-1)!, 'from')).toContain('noreply@example.test')

    // 绑定时填错码：hub 回 400 invalid_code（登录着，所以不是 401）——只显示「码不对」，不能被当成登录失效踢回登录页
    await owner.getByTestId('email-code').fill(wrongCode(code))
    await expect(owner.getByText(/wrong or has expired|不对或已过期/)).toBeVisible()
    await expect(owner).toHaveURL(/\/settings$/)
    await expect(owner.getByTestId('email-code')).toHaveValue('')

    await owner.getByTestId('email-code').fill(code) // 输满 6 位自动提交
    await expect(owner.getByTestId('email-current')).toHaveText(ADDRESS)
    const me = await ownerContext.request.get(`${state.hubUrl}/api/me`)
    expect((await me.json()) as { email?: string; login_methods: string[] }).toMatchObject({
      email: ADDRESS,
      login_methods: expect.arrayContaining(['token', 'email']),
    })
  })

  test('2. 新的浏览器上下文（中文）在登录页用邮箱 + 码登录：先输错一次，再输对', async ({
    browser,
  }) => {
    const state = readState()
    // zh-CN：登录页是中文，hub 按 Accept-Language 发中文邮件（UTF-8 经 lettre 编码，用例要解码才取得到码）
    const context = await browser.newContext({ locale: 'zh-CN' })
    try {
      const page = await context.newPage()
      await page.goto('/login')
      await expect(page.getByTestId('login-token-input')).toHaveCount(0) // token 折叠在「使用 token」后面
      await page.getByTestId('login-email-input').fill(TYPED)

      const before = mailsTo(state.smtpDir, ADDRESS).length
      await page.getByTestId('login-email-send').click()
      const sent = page.getByTestId('login-email-sent')
      await expect(sent).toContainText(ADDRESS)
      sentNotice = ((await sent.innerText()) ?? '').replace(ADDRESS, '<email>')
      // 重发要等倒计时（hub 回的 resend_after_secs，60 秒）
      await expect(page.getByTestId('login-code-resend')).toBeDisabled()
      const real = await waitForCode(state.smtpDir, ADDRESS, { after: before })
      issuedCodes.push(real)
      expect(mailText(mailsTo(state.smtpDir, ADDRESS).at(-1)!)).toContain('登录码')

      // 错码：显示错误、输入框清空、还在登录页，没有会话
      await page.getByTestId('login-code-input').fill(wrongCode(real))
      await expect(page.getByText(/wrong or has expired|不对或已过期/)).toBeVisible()
      await expect(page.getByTestId('login-code-input')).toHaveValue('')
      await expect(page).toHaveURL(/\/login/)
      expect((await context.request.get(`${state.hubUrl}/api/me`)).status()).toBe(401)

      // 同一封信里的对的码：登录成功，离开登录页，会话属于绑定了这个邮箱的账号
      await page.getByTestId('login-code-input').fill(real)
      await expect(page).not.toHaveURL(/\/login/, { timeout: 15_000 })
      const me = await context.request.get(`${state.hubUrl}/api/me`)
      expect(me.status()).toBe(200)
      expect(((await me.json()) as { email?: string }).email).toBe(ADDRESS)

      // 码是一次性的：同一个码不能再登录第二次
      const second = await browser.newContext()
      try {
        const res = await second.request.post(`${state.hubUrl}/auth/email/verify`, {
          data: { email: ADDRESS, code: real },
          headers: { 'X-Anycon': '1' },
        })
        expect(res.status()).toBe(401)
        expect(((await res.json()) as { error: { code: string } }).error.code).toBe('invalid_code')
      } finally {
        await second.close()
      }
    } finally {
      await context.close()
    }
  })

  test('3. 没绑定的邮箱：同样的「已发出」提示，假 SMTP 没收到信，随便什么码都进不去', async ({
    browser,
  }) => {
    const state = readState()
    const context = await browser.newContext({ locale: 'zh-CN' }) // 和 2 同一种语言，提示才能逐字比较
    try {
      const page = await context.newPage()
      await page.goto('/login')
      await page.getByTestId('login-email-input').fill(UNBOUND)
      const mailsBefore = readMails(state.smtpDir).length
      await page.getByTestId('login-email-send').click()
      const sent = page.getByTestId('login-email-sent')
      await expect(sent).toContainText(UNBOUND)
      // 提示一字不差（hub 不告诉你邮箱有没有绑定）
      expect(((await sent.innerText()) ?? '').replace(UNBOUND, '<email>')).toBe(sentNotice)
      await expect(page.getByTestId('login-code-resend')).toBeDisabled()

      // hub 在后台任务里查账号、发信：留一点时间让「会发的话」有机会发出来，然后断言一封都没有
      await page.waitForTimeout(1500)
      expect(mailsTo(state.smtpDir, UNBOUND)).toEqual([])
      expect(readMails(state.smtpDir)).toHaveLength(mailsBefore)

      await page.getByTestId('login-code-input').fill('123456')
      await expect(page.getByText(/wrong or has expired|不对或已过期/)).toBeVisible()
      await expect(page).toHaveURL(/\/login/)
      expect((await context.request.get(`${state.hubUrl}/api/me`)).status()).toBe(401)
    } finally {
      await context.close()
    }
  })

  test('4. 解绑：邮箱不见了，设置里又能绑定', async () => {
    const state = readState()
    await owner.goto('/settings')
    await owner.getByTestId('email-unlink').click()
    await owner.getByTestId('email-unlink-confirm').click()
    await expect(owner.getByTestId('email-link')).toBeVisible()
    const me = await ownerContext.request.get(`${state.hubUrl}/api/me`)
    expect(((await me.json()) as { email?: string }).email).toBeUndefined()
  })

  test('5. hub 的日志（debug 级）里没有邮箱原文和验证码，只有邮箱的哈希标记', async () => {
    const state = readState()
    // 去掉每行开头的时间戳：它的微秒部分是 6 位数字，会和验证码撞上
    const lines = readLog(`${state.tmpDir}/hub.log`)
      .split('\n')
      .map((line) => line.replace(/^\S+\s+/, ''))
    const log = lines.join('\n').toLowerCase()
    expect(issuedCodes).toHaveLength(2)
    for (const secret of [ADDRESS, UNBOUND, 'example.test', 'e2e-owner']) {
      expect(log, `the hub log must not contain ${secret}`).not.toContain(secret)
    }
    for (const code of issuedCodes) {
      expect(log, 'the hub log must not contain a verification code').not.toMatch(
        new RegExp(`(^|\\D)${code.slice(0, 3)}[ -]?${code.slice(3)}(\\D|$)`),
      )
    }
    // 确实记了发码这件事（用邮箱哈希的前 8 位代替邮箱），不是日志本来就没有内容
    expect(lines.filter((l) => /email code sent to=[0-9a-f]{8} /.test(l))).toHaveLength(2)
    expect(lines.some((l) => /not sending to=[0-9a-f]{8}/.test(l))).toBe(true)
  })
})
