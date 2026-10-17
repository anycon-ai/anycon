/**
 * E2E 的端口 / 地址 / 路径约定。playwright.config.ts、global-setup.ts 和测试共用，
 * 保证 `baseURL`（在 globalSetup 之前就要定）与实际启动的服务一致。
 *
 * 端口默认**随机**（向系统要空闲端口），所以可以并行跑多份、不怕 18080 被占。要固定就设环境变量。
 * 随机出来的端口会写回 `process.env`，Playwright 的 worker 进程继承它，所以主进程、globalSetup 和测试看到的是同一个值。
 *
 * 环境变量：
 *   E2E_HUB_PORT      hub 端口，默认随机
 *   E2E_WEB_MODE      hub（默认，用 hub 嵌入的 web/dist）| preview（`vite preview`，端口 E2E_WEB_PORT 默认随机）
 *   E2E_WEB_URL       已有的 web 地址（比如 `bun run --cwd web dev` 的 5173）。设了就不启动任何 web 服务
 *   E2E_HUB_BIN       hub 二进制，默认 <repo>/target/debug/anycon-hub
 *   E2E_CLI_BIN       anycon 二进制，默认 <repo>/target/debug/anycon
 *   E2E_SKIP_DAEMON   1 = 不启动 daemon（只测 hub + web，比如 smoke）
 *   E2E_REQUIRE_DAEMON  0 = daemon 起不来只警告（默认是整体失败，happy path 依赖它）
 *   E2E_KEEP_TMP      1 = 结束后保留临时目录，方便看 hub / daemon 日志
 *   E2E_BUILD         1 = 先构建 web、触碰 crates/hub/build.rs、cargo build hub + cli（hub 编译时嵌入 web/dist）
 *   E2E_ALLOW_STALE_WEB  1 = hub 服务的 index.html 与 web/dist 不一致也照跑（默认报错：占位页 / 旧的嵌入）
 */
import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

export const repoRoot = fileURLToPath(new URL('../../', import.meta.url))
export const e2eRoot = fileURLToPath(new URL('../', import.meta.url))

const env = process.env

/**
 * 同时向系统要 `count` 个空闲端口（子进程里把它们全部占住再一起释放，所以彼此不会重复）。
 * 同步，因为 playwright.config.ts 在顶层就要用。释放到真正 bind 之间有极小的窗口，撞上了 hub 启动会失败并带日志报错。
 */
function pickFreePorts(count: number): number[] {
  const script = `
    const net = require('node:net')
    const servers = []
    let left = ${count}
    const ports = []
    for (let i = 0; i < ${count}; i++) {
      const s = net.createServer()
      s.listen(0, '127.0.0.1', () => {
        ports.push(s.address().port)
        if (--left === 0) {
          console.log(ports.join(','))
          for (const x of servers) x.close()
        }
      })
      servers.push(s)
    }
  `
  const out = execFileSync(process.execPath, ['-e', script], { encoding: 'utf8', timeout: 10_000 })
  const ports = out.trim().split(',').map(Number)
  if (ports.length !== count || ports.some((p) => !Number.isInteger(p) || p <= 0)) {
    throw new Error(`could not pick ${count} free ports, got: ${JSON.stringify(out)}`)
  }
  return ports
}

if (!env.E2E_HUB_PORT || !env.E2E_WEB_PORT) {
  const [hub, web] = pickFreePorts(2)
  env.E2E_HUB_PORT ??= String(hub)
  env.E2E_WEB_PORT ??= String(web)
}

export const hubPort = Number(env.E2E_HUB_PORT)
export const hubUrl = `http://127.0.0.1:${hubPort}`

export type WebMode = 'hub' | 'preview' | 'external'

export const webMode: WebMode = env.E2E_WEB_URL
  ? 'external'
  : env.E2E_WEB_MODE === 'preview'
    ? 'preview'
    : 'hub'
export const webPort = Number(env.E2E_WEB_PORT)
export const webUrl =
  env.E2E_WEB_URL ?? (webMode === 'preview' ? `http://127.0.0.1:${webPort}` : hubUrl)

export const hubBin = env.E2E_HUB_BIN ?? `${repoRoot}target/debug/anycon-hub`
export const cliBin = env.E2E_CLI_BIN ?? `${repoRoot}target/debug/anycon`

/** globalSetup 写、测试读的状态文件（已 gitignore）。带端口，同时跑多份互不干扰。 */
export const stateFile = `${e2eRoot}.e2e-state-${hubPort}.json`

export interface E2EState {
  hubUrl: string
  webUrl: string
  webMode: WebMode
  /** 管理员 token：globalSetup 生成并通过 ANYCON_ADMIN_TOKEN 交给 hub（`E2E_ADMIN_TOKEN` 可固定它）。 */
  adminToken: string
  /** 临时根目录（hub 数据、daemon home、workspace 都在里面）。 */
  tmpDir: string
  hubDataDir: string
  /** daemon 的 `ANYCON_HOME`；没起 daemon 时为空串。 */
  daemonHome: string
  workspaceDir: string
  daemonStarted: boolean
  /** daemon 和测试里的 git 共用的全局配置文件（user.name / user.email / init.defaultBranch）；daemon 的 `GIT_CONFIG_GLOBAL` 指向它。 */
  gitConfig: string
  /**
   * daemon 的 PATH 最前面有一个 `git` 包装脚本（global-setup 写的）：平时原样转发；往这个目录里放规则文件
   * `<项目目录名>.<n>.rule`（内容 `<秒数> <参数里要包含的字符串>`），daemon 在那个目录里跑的、参数含该字符串的第一条 git
   * 就先睡这么久（每条规则一次性）。用来稳定地造出「git 元数据算得慢」（`helpers/git.ts` 的 `slowGit`）。
   */
  slowGitDir: string
  /** globalSetup 起的假 SMTP 服务（hub 的 `ANYCON_SMTP_URL` 指向它）和它收到的信所在的目录（`helpers/smtp.ts` 读）。 */
  smtpPort: number
  smtpDir: string
}
