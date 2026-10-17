import { expect, test } from '@playwright/test'

import { readState } from '../helpers/state.js'
import { revealTokenForm } from '../helpers/ui.js'

test('hub 健康检查', async ({ request }) => {
  const { hubUrl } = readState()
  const res = await request.get(`${hubUrl}/healthz`)
  expect(res.ok()).toBe(true)
})

test('打开登录页：邮箱验证码在前，token 折叠在「使用 token」后面', async ({ page }) => {
  await page.goto('/login')
  // global-setup 起的 hub 配了 SMTP（ADR-37）
  await expect(page.getByTestId('login-email-input')).toBeVisible()
  await expect(page.getByTestId('login-token-input')).toHaveCount(0)
  await revealTokenForm(page)
  await expect(page.getByTestId('login-token-input')).toBeVisible()
})
