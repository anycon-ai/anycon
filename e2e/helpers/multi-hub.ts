/**
 * 测试自己拥有的第二个 hub：`ANYCON_MODE=multi`（公开 hub：邮箱验证码开放注册、`ANYCON_ADMIN_EMAILS` 管理员、配额、OAuth 按钮）。
 *
 * 共享的那个 hub 属于 global-setup，是 `single` 模式（管理员 token、邀请），整套用例共用；`multi` 的行为（没有 token / 邀请、
 * 开放注册、配额、删账号）只能在另一个进程里测。这个 hub 有自己的随机端口、临时目录（数据库、日志）和环境，
 * 但发信指向 global-setup 的假 SMTP 服务（`helpers/smtp.ts`，用例照常读 `readState().smtpDir`）。
 * 生命周期完全在用例里：`startMultiHub()` 放在 `beforeAll`，`stop()` 放在 `afterAll`。
 *
 * OAuth 的凭据是假的（语法上合法就行）：hub 只在用户点按钮时才会把浏览器 302 到提供方的授权页，用例从不让浏览器真的去那里。
 */
import { existsSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { hubBin, type E2EState } from './env.js'
import { httpOk, spawnLogged, tailLog, waitFor } from './process.js'

export interface MultiHubOptions {
  /** `ANYCON_ADMIN_EMAILS`：这些邮箱注册出来的账号是管理员（不写进库里的标记）。 */
  adminEmails: string[]
  /** `ANYCON_QUOTA_*`；没写的用 hub 的默认值。 */
  quotas?: { devices?: number; daemons?: number; connections?: number; dailyPairings?: number }
  /** `ANYCON_OAUTH_GITHUB_CLIENT_ID` / `_CLIENT_SECRET`：配了登录页才有 GitHub 按钮。 */
  github?: { clientId: string; clientSecret: string }
}

export interface MultiHub {
  /** `http://127.0.0.1:<port>`，也是它的 `ANYCON_PUBLIC_URL`（所以浏览器从这个地址打开时 OAuth 按钮可点）。 */
  url: string
  port: number
  /** 临时根目录：`hub.log`、数据库，以及测试起的额外 daemon 的目录都在里面。 */
  tmpDir: string
  /** hub 的日志（debug 级，hub 自己的模块）：`<tmpDir>/hub.log`，所以 `hubLogLeaks(hub, …)` 可以直接用。 */
  logFile: string
  /** 停进程并删掉临时目录（`E2E_KEEP_TMP=1` 时保留）。 */
  stop: () => Promise<void>
}

/** 向系统要一个空闲端口（释放到 hub 真正 bind 之间有极小的窗口，撞上了 hub 启动会失败并带日志报错）。 */
async function freePort(): Promise<number> {
  const server = createServer()
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => resolve())
  })
  const address = server.address()
  await new Promise<void>((resolve) => server.close(() => resolve()))
  if (!address || typeof address === 'string') throw new Error('could not pick a free port')
  return address.port
}

export async function startMultiHub(
  state: Pick<E2EState, 'smtpPort'>,
  options: MultiHubOptions,
): Promise<MultiHub> {
  if (!existsSync(hubBin)) {
    throw new Error(
      `hub binary not found at ${hubBin}. Run: cargo build -p anycon-hub -p anycon-cli`,
    )
  }
  // 目录名要短：测试起的 daemon 的本机 socket 在这个目录下（unix socket 路径有长度上限，macOS 104 字节）
  const tmpDir = mkdtempSync(join(tmpdir(), 'anycon-mh-'))
  const dataDir = join(tmpDir, 'hub')
  mkdirSync(dataDir, { recursive: true })
  const logFile = join(tmpDir, 'hub.log')
  const port = await freePort()
  const url = `http://127.0.0.1:${port}`
  const quotas = options.quotas ?? {}
  const hub = spawnLogged('hub-multi', hubBin, [], {
    logFile,
    env: {
      ANYCON_LISTEN: `127.0.0.1:${port}`,
      ANYCON_PUBLIC_URL: url,
      ANYCON_DATA_DIR: dataDir,
      ANYCON_MODE: 'multi',
      // multi 不生成也不读 bootstrap token；开发机环境里万一有这个变量，别让它触发「被忽略」的警告
      ANYCON_ADMIN_TOKEN: undefined,
      ANYCON_SMTP_URL: `smtp+plain://127.0.0.1:${state.smtpPort}`,
      ANYCON_EMAIL_FROM: 'Anycon <noreply@example.test>',
      ANYCON_ADMIN_EMAILS: options.adminEmails.join(','),
      ANYCON_QUOTA_DEVICES: quotas.devices === undefined ? undefined : String(quotas.devices),
      ANYCON_QUOTA_DAEMONS: quotas.daemons === undefined ? undefined : String(quotas.daemons),
      ANYCON_QUOTA_CONNECTIONS:
        quotas.connections === undefined ? undefined : String(quotas.connections),
      ANYCON_QUOTA_DAILY_PAIRINGS:
        quotas.dailyPairings === undefined ? undefined : String(quotas.dailyPairings),
      ANYCON_OAUTH_GITHUB_CLIENT_ID: options.github?.clientId,
      ANYCON_OAUTH_GITHUB_CLIENT_SECRET: options.github?.clientSecret,
      // 和共享 hub 一样：hub 自己的模块开 debug，用例要断言日志里没有邮箱 / 验证码 / OAuth 的 state
      ANYCON_LOG: process.env.ANYCON_LOG ?? 'info,anycon_hub=debug',
    },
  })
  const cleanup = (): void => {
    if (process.env.E2E_KEEP_TMP === '1') console.log(`[e2e] keeping ${tmpDir}`)
    else rmSync(tmpDir, { recursive: true, force: true })
  }
  try {
    await waitFor(
      'multi hub /healthz',
      async () => !hub.exited() && (await httpOk(`${url}/healthz`)),
      { timeoutMs: 30_000 },
    )
  } catch (err) {
    await hub.stop()
    const message = `${String(err)}\n--- ${logFile} ---\n${tailLog(logFile)}`
    cleanup()
    throw new Error(message, { cause: err })
  }
  return {
    url,
    port,
    tmpDir,
    logFile,
    stop: async () => {
      await hub.stop()
      cleanup()
    },
  }
}
