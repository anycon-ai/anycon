// 真实环境夹具（real.e2e.test.ts 用）：起 `target/debug/anycon-hub` 和 `target/debug/anycon daemon`（mock agent），
// 都用临时目录和随机端口，不碰真实的 ~/.anycon。
//
// 用法见 real.e2e.test.ts 文件头。二进制路径可用 `ANYCON_BIN_DIR` 覆盖（默认仓库的 target/debug）。
//
// 受限链路：本机回环链路上，hub 几毫秒内就处理完全部在途数据，退回回执总是早于重发计时，
// 乱序、逐出这类只在「链路一受限、在途数据远多于一轮重发排得空的量」时才出现的问题测不到。所以夹具可以在
// daemon ↔ hub 之间（`daemonLink`，daemon 的 `hub.url` 经 `ANYCON_HUB__URL` 指向代理）和 SDK ↔ hub 之间
// （`clientLink`，用 `stack.clientUrl`）各插一个限速 TCP 代理，并给 hub 传环境变量（比如调低 daemon 的速率预算）。
import { spawn, type ChildProcess } from 'node:child_process'
import { existsSync, mkdtempSync, rmSync } from 'node:fs'
import { createServer, connect as netConnect, type Server, type Socket } from 'node:net'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

export const ADMIN_TOKEN = 'sdk-real-test-admin-token-0123456789'

const binDir =
  process.env.ANYCON_BIN_DIR ?? fileURLToPath(new URL('../../../target/debug/', import.meta.url))
export const HUB_BIN = join(binDir, 'anycon-hub')
export const CLI_BIN = join(binDir, 'anycon')

export const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

/** 不带 `ANYCON_*` 的环境，再叠加给定的变量：外面的配置不能漏进测试。 */
function cleanEnv(extra: Record<string, string>): Record<string, string> {
  const env: Record<string, string> = {}
  for (const [k, v] of Object.entries(process.env)) {
    if (v !== undefined && !k.startsWith('ANYCON_')) env[k] = v
  }
  return { ...env, NO_COLOR: '1', ...extra }
}

async function freePort(): Promise<number> {
  return await new Promise<number>((resolve, reject) => {
    const srv = createServer()
    srv.once('error', reject)
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address()
      srv.close(() => resolve(typeof addr === 'object' && addr ? addr.port : 0))
    })
  })
}

export const MIB = 1024 * 1024

/** 限速代理的参数，字节/秒；缺省 = 不限速。测试里可以随时改（代理每个数据块都重新读）。 */
export interface ProxyLimits {
  /** 客户端 → hub 方向（daemon 的上行，或 SDK 的上行）。 */
  up?: number
  /** hub → 客户端方向（daemon 或 SDK 的下行）。 */
  down?: number
  /**
   * 「间歇」模式（作用在下行）：每 `periodMs` 里先正常读 `periodMs - stallMs`，再停读 `stallMs`，循环。
   * 停读 = 不从 hub 读数据，对端 TCP 窗口满了就写不进去，模拟手机切后台再回来。
   */
  stallDown?: { periodMs: number; stallMs: number }
}

export interface ThrottleProxy {
  port: number
  /** `http://127.0.0.1:<port>`，可以当 hub 地址用。 */
  url: string
  limits: ProxyLimits
  /** 已经从客户端侧读到并转给 hub 的字节数。 */
  readonly bytesUp: number
  /** 已经从 hub 读到并转给客户端的字节数。 */
  readonly bytesDown: number
  close(): Promise<void>
}

/**
 * 限速 TCP 代理（Node `net`，大约 30 行核心）：把 `127.0.0.1:<proxyPort>` 上的连接转到 `127.0.0.1:<targetPort>`，
 * 两个方向各自按字节配额节流——每个数据块写出之后 `pause()` 源 socket，等到按配额算出的时间点再 `resume()`，
 * 背压一路传回发送方：读得慢 → 对方 TCP 窗口满 → 对方写阻塞。时间点按单调的「下一个空闲时刻」累加，
 * 计时器的误差不会累积，平均速率贴近配额。
 */
export async function startThrottleProxy(
  targetPort: number,
  limits: ProxyLimits = {},
): Promise<ThrottleProxy> {
  const sockets = new Set<Socket>()
  const t0 = Date.now()
  let bytesUp = 0
  let bytesDown = 0

  const pump = (src: Socket, dst: Socket, dir: 'up' | 'down'): void => {
    let nextFree = 0 // 这个方向最早什么时候能再放一块进来（绝对时间，毫秒）
    src.on('data', (chunk: Buffer) => {
      if (dir === 'up') bytesUp += chunk.length
      else bytesDown += chunk.length
      const writable = dst.write(chunk)
      const now = Date.now()
      const rate = limits[dir]
      if (rate !== undefined && rate > 0)
        nextFree = Math.max(nextFree, now) + (chunk.length / rate) * 1000
      let until = rate !== undefined && rate > 0 ? nextFree : 0
      const stall = dir === 'down' ? limits.stallDown : undefined
      if (stall) {
        const phase = (now - t0) % stall.periodMs
        if (phase >= stall.periodMs - stall.stallMs)
          until = Math.max(until, now + (stall.periodMs - phase))
      }
      const wait = until - now
      if (writable && wait <= 1) return
      src.pause()
      let pending = (writable ? 0 : 1) + (wait > 1 ? 1 : 0)
      const done = () => {
        pending -= 1
        if (pending === 0) src.resume()
      }
      if (!writable) dst.once('drain', done)
      if (wait > 1) setTimeout(done, wait)
    })
    src.on('end', () => dst.end())
    src.on('close', () => dst.end())
    src.on('error', () => {
      src.destroy()
      dst.destroy()
    })
  }

  const server: Server = createServer((client) => {
    const upstream = netConnect(targetPort, '127.0.0.1')
    sockets.add(client).add(upstream)
    for (const sock of [client, upstream]) sock.once('close', () => sockets.delete(sock))
    pump(client, upstream, 'up')
    pump(upstream, client, 'down')
  })
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => resolve())
  })
  const addr = server.address()
  const port = typeof addr === 'object' && addr ? addr.port : 0
  return {
    port,
    url: `http://127.0.0.1:${port}`,
    limits,
    get bytesUp() {
      return bytesUp
    },
    get bytesDown() {
      return bytesDown
    },
    close: () =>
      new Promise<void>((resolve) => {
        for (const sock of sockets) sock.destroy()
        server.close(() => resolve())
      }),
  }
}

export interface StackOptions {
  /** 额外传给 hub 进程的环境变量（`ANYCON_*`），比如 `ANYCON_RATE_DAEMON_BYTES_PER_MIN`。 */
  hubEnv?: Record<string, string>
  /**
   * hub 的日志过滤串，默认同 `ANYCON_SDK_REAL_LOG`（缺省 `warn`）。hub 把每次退回记在 debug 级别，
   * 想断言「确实退回过」要给 `'warn,anycon_hub=debug'`。
   */
  hubLog?: string
  /** 在 daemon ↔ hub 之间插一个限速代理（daemon 的 `hub.url` 经 `ANYCON_HUB__URL` 指向它）。 */
  daemonLink?: ProxyLimits
  /** 在 SDK ↔ hub 之间插一个限速代理；SDK 用 `stack.clientUrl` 去连。 */
  clientLink?: ProxyLimits
}

interface Captured {
  child: ChildProcess
  stdout: () => string
  stderr: () => string
  exited: Promise<number | null>
}

/** 起过的子进程：测试进程无论怎么退出（含 before 钩子失败、Ctrl+C）都不留 hub / daemon。 */
const children = new Set<ChildProcess>()
process.on('exit', () => {
  for (const c of children) if (c.exitCode === null && c.signalCode === null) c.kill('SIGKILL')
})

function run(cmd: string, args: string[], env: Record<string, string>): Captured {
  const child = spawn(cmd, args, { env, stdio: ['ignore', 'pipe', 'pipe'] })
  children.add(child)
  child.once('exit', () => children.delete(child))
  let out = ''
  let err = ''
  child.stdout?.on('data', (d: Buffer) => (out += d.toString()))
  child.stderr?.on('data', (d: Buffer) => (err += d.toString()))
  const exited = new Promise<number | null>((resolve) => {
    child.once('exit', (code) => resolve(code))
    child.once('error', () => resolve(-1))
  })
  return { child, stdout: () => out, stderr: () => err, exited }
}

/** 跑一条短命令到结束，非 0 退出码抛错。 */
async function runToEnd(cmd: string, args: string[], env: Record<string, string>): Promise<string> {
  const p = run(cmd, args, env)
  const timer = setTimeout(() => p.child.kill('SIGKILL'), 60_000)
  const code = await p.exited
  clearTimeout(timer)
  if (code !== 0) {
    throw new Error(`${cmd} ${args.join(' ')} exited ${code}\n${p.stdout()}\n${p.stderr()}`)
  }
  return p.stdout()
}

async function waitFor(
  what: string,
  timeoutMs: number,
  cond: () => boolean | Promise<boolean>,
): Promise<void> {
  const start = Date.now()
  while (!(await cond())) {
    if (Date.now() - start > timeoutMs) throw new Error(`timed out waiting for ${what}`)
    await sleep(50)
  }
}

async function stop(p: Captured | undefined): Promise<void> {
  if (!p || p.child.exitCode !== null || p.child.signalCode !== null) return
  p.child.kill('SIGTERM')
  const killer = setTimeout(() => p.child.kill('SIGKILL'), 10_000)
  await p.exited
  clearTimeout(killer)
}

/** 一次 `anycon pair --json`：第一行 stdout 是短码，进程阻塞到有设备加入。 */
export interface PairSession {
  code: string
  /** `${hub_public_url}/pair#code=…`（短码在 fragment 里）；CLI 没打印时为 undefined。 */
  link?: string
  /** 配对完成后 CLI 打印的最后一行 `{ "paired": … }`。 */
  done: Promise<Record<string, unknown>>
}

export interface RealStack {
  /** hub 的直连地址。 */
  hubUrl: string
  /** SDK 该用的地址：有 `clientLink` 时是代理，否则等于 `hubUrl`。 */
  clientUrl: string
  /** daemon ↔ hub 之间的限速代理（有 `daemonLink` 时）。 */
  daemonProxy?: ThrottleProxy
  /** SDK ↔ hub 之间的限速代理（有 `clientLink` 时）。 */
  clientProxy?: ThrottleProxy
  /** 管理员登录 cookie（`anycon_session=…`），用于第一次配对。 */
  adminCookie: string
  workspace: string
  daemonLog: () => string
  hubLog: () => string
  /** 让 daemon 开一个配对短码（`anycon pair --json`；默认 16 位，`short` = `--short`，8 位）。 */
  beginPair(options?: { short?: boolean }): Promise<PairSession>
  /** 在 daemon 的 home 里跑一条 CLI 命令，返回 stdout。 */
  cli(...args: string[]): Promise<string>
  stop(): Promise<void>
}

export function binariesPresent(): boolean {
  return existsSync(HUB_BIN) && existsSync(CLI_BIN)
}

export async function startStack(options: StackOptions = {}): Promise<RealStack> {
  // 排查时 `ANYCON_SDK_REAL_LOG=debug`（或 `anycon_hub=debug,warn` 之类的过滤串）让 hub / daemon 多说话。
  const logLevel = process.env.ANYCON_SDK_REAL_LOG ?? 'warn'
  if (!binariesPresent()) {
    throw new Error(
      `missing ${HUB_BIN} or ${CLI_BIN}: run \`cargo build -p anycon-hub -p anycon-cli\` first`,
    )
  }
  const root = mkdtempSync(join(tmpdir(), 'anycon-sdk-real-'))
  const port = await freePort()
  const hubUrl = `http://127.0.0.1:${port}`
  const home = join(root, 'daemon-home')
  const workspace = join(root, 'workspace')

  const hub = run(
    HUB_BIN,
    ['serve'],
    cleanEnv({
      ANYCON_PUBLIC_URL: hubUrl,
      ANYCON_LISTEN: `127.0.0.1:${port}`,
      ANYCON_DATA_DIR: join(root, 'hub-data'),
      ANYCON_ADMIN_TOKEN: ADMIN_TOKEN,
      ...options.hubEnv,
      ANYCON_LOG: options.hubLog ?? logLevel,
    }),
  )
  let daemon: Captured | undefined
  const proxies: ThrottleProxy[] = []
  const pairs: Captured[] = []
  const cleanup = async () => {
    for (const p of pairs) await stop(p)
    await stop(daemon)
    for (const px of proxies) await px.close()
    await stop(hub)
    try {
      rmSync(root, { recursive: true, force: true })
    } catch {
      // 临时目录删不掉不影响结果
    }
  }

  try {
    await waitFor('the hub to answer /healthz', 20_000, async () => {
      if (hub.child.exitCode !== null) throw new Error(`hub exited early:\n${hub.stderr()}`)
      try {
        return (await fetch(`${hubUrl}/healthz`)).ok
      } catch {
        return false
      }
    })

    // 管理员登录 → cookie：第一次配对必须带它（节点还没有账号，见 README「账号归属」）。
    const login = await fetch(`${hubUrl}/auth/token`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-anycon': '1' },
      body: JSON.stringify({ token: ADMIN_TOKEN }),
    })
    if (login.status !== 204) throw new Error(`admin login failed: HTTP ${login.status}`)
    const adminCookie = (login.headers.getSetCookie()[0] ?? '').split(';')[0] ?? ''
    if (!adminCookie.startsWith('anycon_session=')) throw new Error('no anycon_session cookie')

    // 限速代理：必须在 daemon 起来之前就位。
    let daemonProxy: ThrottleProxy | undefined
    let clientProxy: ThrottleProxy | undefined
    if (options.daemonLink) {
      daemonProxy = await startThrottleProxy(port, options.daemonLink)
      proxies.push(daemonProxy)
    }
    if (options.clientLink) {
      clientProxy = await startThrottleProxy(port, options.clientLink)
      proxies.push(clientProxy)
    }

    const baseEnv = {
      ANYCON_HOME: home,
      ANYCON_MOCK_AGENT: '1',
      // 默认启动时要跑 `$SHELL -ilc` 解析环境，用户的 rc 重的话要好几秒；测试不需要。
      ANYCON_AGENTS__ENV_FROM_LOGIN_SHELL: 'false',
      ANYCON_LOG: logLevel,
    }
    // `init` 写进配置文件的是直连地址；daemon 和 CLI 运行时用环境变量把 `hub.url` 改指向代理。
    await runToEnd(
      CLI_BIN,
      ['init', '--hub', hubUrl, '--workspace', workspace, '--name', 'sdk-node', '--yes'],
      cleanEnv(baseEnv),
    )
    const env = cleanEnv({
      ...baseEnv,
      ...(daemonProxy ? { ANYCON_HUB__URL: daemonProxy.url } : {}),
    })
    // 不依赖本机有没有装 claude / codex：只留 mock（两个内置真实 agent 没有配置段也会被探测，所以都要显式关掉）。
    await runToEnd(CLI_BIN, ['config', 'set', 'agents.claude-code.enabled', 'false'], env)
    await runToEnd(CLI_BIN, ['config', 'set', 'agents.codex.enabled', 'false'], env)
    daemon = run(CLI_BIN, ['daemon'], env)
    const d = daemon
    await waitFor('the daemon socket', 30_000, () => {
      if (d.child.exitCode !== null) throw new Error(`daemon exited early:\n${d.stderr()}`)
      return existsSync(join(home, 'daemon.sock'))
    })

    const cli = (...args: string[]) => runToEnd(CLI_BIN, args, env)

    const beginPair = async (pairOptions: { short?: boolean } = {}): Promise<PairSession> => {
      // daemon 刚起来时可能还没连上 hub（pair 立即退出码 3）：重试到连上为止。
      const deadline = Date.now() + 30_000
      for (;;) {
        const p = run(CLI_BIN, ['pair', '--json', ...(pairOptions.short ? ['--short'] : [])], env)
        const first = await Promise.race([
          new Promise<string>((resolve) => {
            let buf = ''
            p.child.stdout?.on('data', (c: Buffer) => {
              buf += c.toString()
              const nl = buf.indexOf('\n')
              if (nl >= 0) resolve(buf.slice(0, nl))
            })
          }),
          p.exited.then(() => ''),
        ])
        if (first) {
          pairs.push(p)
          const { code, link } = JSON.parse(first) as { code: string; link?: string }
          const done = p.exited.then((exit) => {
            if (exit !== 0) throw new Error(`anycon pair exited ${exit}\n${p.stderr()}`)
            const last = p.stdout().trim().split('\n').at(-1) ?? '{}'
            return JSON.parse(last) as Record<string, unknown>
          })
          done.catch(() => undefined) // 没人等时不要变成 unhandled rejection
          return { code, ...(link ? { link } : {}), done }
        }
        if (Date.now() > deadline) {
          throw new Error(`anycon pair never printed a code:\n${p.stdout()}\n${p.stderr()}`)
        }
        await sleep(200)
      }
    }

    return {
      hubUrl,
      clientUrl: clientProxy?.url ?? hubUrl,
      ...(daemonProxy ? { daemonProxy } : {}),
      ...(clientProxy ? { clientProxy } : {}),
      adminCookie,
      workspace,
      daemonLog: () => d.stderr() + d.stdout(),
      hubLog: () => hub.stderr() + hub.stdout(),
      beginPair,
      cli,
      stop: cleanup,
    }
  } catch (err) {
    await cleanup()
    throw err
  }
}
