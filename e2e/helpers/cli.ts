/**
 * 调 `anycon` CLI 的帮助函数。测试里的 daemon 用独立的 `ANYCON_HOME`，绝不碰真实的 ~/.anycon。
 */
import { execFile, spawn } from 'node:child_process'
import { promisify } from 'node:util'

import { cliBin, type E2EState } from './env.js'
import { waitFor } from './process.js'

const execFileAsync = promisify(execFile)

export function cliEnv(state: Pick<E2EState, 'daemonHome'>): NodeJS.ProcessEnv {
  return { ...process.env, ANYCON_HOME: state.daemonHome, ANYCON_MOCK_AGENT: '1', NO_COLOR: '1' }
}

/** 跑一条 `anycon …` 到结束，返回 stdout。 */
export async function runCli(
  state: Pick<E2EState, 'daemonHome'>,
  args: string[],
  options: { timeoutMs?: number } = {},
): Promise<string> {
  const { stdout } = await execFileAsync(cliBin, args, {
    env: cliEnv(state),
    timeout: options.timeoutMs ?? 30_000,
  })
  return stdout
}

/** 跑一条 `anycon …` 到结束，不管成功还是失败都返回退出码和输出（用来断言“应当失败”的命令）。 */
export async function runCliResult(
  state: Pick<E2EState, 'daemonHome'>,
  args: string[],
  options: { timeoutMs?: number } = {},
): Promise<{ code: number; stdout: string; stderr: string }> {
  try {
    const { stdout, stderr } = await execFileAsync(cliBin, args, {
      env: cliEnv(state),
      timeout: options.timeoutMs ?? 30_000,
    })
    return { code: 0, stdout, stderr }
  } catch (err) {
    const e = err as { code?: unknown; stdout?: string; stderr?: string }
    // 超时被杀、起不来之类不是“命令失败了”：原样抛出，别当作一个退出码
    if (typeof e.code !== 'number') throw err
    return { code: e.code, stdout: e.stdout ?? '', stderr: e.stderr ?? '' }
  }
}

/** `anycon pair --json` 输出的第一行 JSON。形状见 docs/spec/cli.md § 3.2（`anycon pair` 一行），链接形式见 docs/protocol.md § 4.1。 */
export interface PairInfo {
  /** 默认 16 位 `XXXX-XXXX-XXXX-XXXX`；`--short` 时是 8 位 `XXXX-XXXX`。 */
  code: string
  /** `<hub>/pair#code=<code>`：短码在 URL fragment 里，不会发给 hub。 */
  link: string
  expires_at_ms: number
}

export interface PairingSession extends PairInfo {
  /** CLI 进程退出（配对完成 / 超时 / 被 kill）。 */
  done: Promise<number | null>
  kill: () => void
}

export interface PairOptions {
  /** `anycon pair --short`：8 位短码（默认是 16 位，ADR-29 修订）。 */
  short?: boolean
  /** 等第一行 JSON 的上限，默认 20 秒。 */
  timeoutMs?: number
}

/** `anycon stats --json`（走本机 socket）里 daemon 与 hub 的连接状态。 */
export async function daemonHubConnected(state: Pick<E2EState, 'daemonHome'>): Promise<boolean> {
  const out = await runCli(state, ['stats', '--json'], { timeoutMs: 10_000 })
  const line = out.split('\n').find((l) => l.trim().startsWith('{'))
  return (
    (JSON.parse(line ?? '{}') as { stats?: { hub_connected?: boolean } }).stats?.hub_connected ===
    true
  )
}

/** 等 daemon 连上 hub（条件轮询，不是固定睡眠）：没连上时 `anycon pair` 会立即以退出码 3 失败（spec/cli.md § 4「节点离线」）。 */
export async function waitForDaemonOnline(
  state: Pick<E2EState, 'daemonHome'>,
  timeoutMs = 20_000,
): Promise<void> {
  await waitFor('daemon connected to hub (anycon stats --json)', () => daemonHubConnected(state), {
    timeoutMs,
    intervalMs: 250,
  })
}

/**
 * 让 daemon 发起一次配对：启动 `anycon pair --json`，读到第一行 JSON（短码）就返回，
 * 进程继续阻塞到配对完成（CLI 的语义），用 `done` 等它、`kill()` 放弃。
 */
export async function startPairing(
  state: Pick<E2EState, 'daemonHome'>,
  options: PairOptions = {},
): Promise<PairingSession> {
  await waitForDaemonOnline(state)
  return startPairingOnce(state, options)
}

async function startPairingOnce(
  state: Pick<E2EState, 'daemonHome'>,
  options: PairOptions,
): Promise<PairingSession> {
  const args = ['pair', '--json', ...(options.short ? ['--short'] : [])]
  const child = spawn(cliBin, args, {
    env: cliEnv(state),
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  const done = new Promise<number | null>((resolve) => child.once('exit', (code) => resolve(code)))
  let stderr = ''
  child.stderr?.on('data', (d: Buffer) => (stderr += d.toString()))

  const info = await new Promise<PairInfo>((resolve, reject) => {
    let buf = ''
    const timer = setTimeout(
      () =>
        reject(
          new Error(
            `anycon ${args.join(' ')} printed no code within ${options.timeoutMs ?? 20_000}ms; stderr: ${stderr}`,
          ),
        ),
      options.timeoutMs ?? 20_000,
    )
    child.stdout?.on('data', (d: Buffer) => {
      buf += d.toString()
      const nl = buf.indexOf('\n')
      if (nl < 0) return
      clearTimeout(timer)
      try {
        resolve(parsePairInfo(buf.slice(0, nl), { short: options.short }))
      } catch (err) {
        reject(err)
      }
    })
    child.once('exit', (code) => {
      clearTimeout(timer)
      const message = `anycon ${args.join(' ')} exited with ${code} before printing a code; stderr: ${stderr}`
      reject(new Error(message))
    })
  })
  return { ...info, done, kill: () => child.kill('SIGTERM') }
}

/** 配对字母表：去掉 0 O 1 I L（crates/crypto）。 */
const GROUP = '[2-9A-HJKMNP-Z]{4}'
const SHORT_CODE = new RegExp(`^${GROUP}-${GROUP}$`)
const LONG_CODE = new RegExp(`^${GROUP}-${GROUP}-${GROUP}-${GROUP}$`)

export function parsePairInfo(line: string, options: { short?: boolean } = {}): PairInfo {
  const value = JSON.parse(line) as Partial<PairInfo>
  const re = options.short ? SHORT_CODE : LONG_CODE
  if (typeof value.code !== 'string' || !re.test(value.code)) {
    throw new Error(`unexpected pair code (short=${!!options.short}) in: ${line}`)
  }
  if (typeof value.link !== 'string' || typeof value.expires_at_ms !== 'number') {
    throw new Error(`pair --json must print { code, link, expires_at_ms }, got: ${line}`)
  }
  // 短码必须在 fragment 里：fragment 不会发给 hub，也不会进反代日志。
  if (!value.link.includes(`/pair#code=${value.code}`)) {
    throw new Error(
      `pair link must carry the code in the URL fragment (#code=), got: ${value.link}`,
    )
  }
  return { code: value.code, link: value.link, expires_at_ms: value.expires_at_ms }
}

/** `anycon devices --json`（走本机 socket，不经 hub）：daemon 视角的已配对设备。 */
export interface DaemonDevice {
  device_id: string
  name: string
  kind: string
  fingerprint: string
  online: boolean
}

export async function daemonDevices(state: Pick<E2EState, 'daemonHome'>): Promise<DaemonDevice[]> {
  const out = await runCli(state, ['devices', '--json'])
  return (JSON.parse(out) as { devices: DaemonDevice[] }).devices
}
