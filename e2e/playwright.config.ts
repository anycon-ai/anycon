import { defineConfig, devices } from '@playwright/test'

import { hubPort, webUrl } from './helpers/env.js'

/**
 * 只测 chromium。服务（hub + daemon + web）由 global-setup.ts 启动，baseURL 指向 web。
 * 环境变量见 helpers/env.ts。
 */
export default defineConfig({
  testDir: './tests',
  // 按 hub 端口分目录：Playwright 启动时会清空 outputDir，同时跑两份 e2e 共用一个目录会互相删掉对方的 trace（ENOENT）。
  outputDir: `./test-results/${hubPort}`,
  globalSetup: './global-setup.ts',
  // 用例共享一个 hub + daemon，串行跑，避免互相抢配对码 / 会话。
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  // 不重试：happy path 在共享的 hub / daemon 上一步步改状态（配对、项目、会话），重跑只会撞上已有状态，掩盖第一次的真实失败。
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: process.env.CI
    ? [['list'], ['html', { open: 'never', outputFolder: 'playwright-report' }]]
    : [['list']],
  use: {
    baseURL: webUrl,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'off',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
})
