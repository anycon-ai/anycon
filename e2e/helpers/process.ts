import { spawn, type ChildProcess } from 'node:child_process'
import { createWriteStream, readFileSync, type WriteStream } from 'node:fs'

export interface Spawned {
  name: string
  child: ChildProcess
  logFile: string
  /** 进程已退出？ */
  exited: () => boolean
  stop: () => Promise<void>
}

/** 启动一个长期运行的子进程，stdout/stderr 写日志文件。 */
export function spawnLogged(
  name: string,
  command: string,
  args: string[],
  options: { cwd?: string; env?: NodeJS.ProcessEnv; logFile: string },
): Spawned {
  const log: WriteStream = createWriteStream(options.logFile, { flags: 'a' })
  const child = spawn(command, args, {
    cwd: options.cwd,
    env: { ...process.env, ...options.env },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  child.stdout?.pipe(log, { end: false })
  child.stderr?.pipe(log, { end: false })
  let done = false
  const exitPromise = new Promise<void>((resolve) => {
    child.once('exit', () => {
      done = true
      log.end()
      resolve()
    })
    child.once('error', (err) => {
      log.write(`[spawn error] ${String(err)}\n`)
      done = true
      resolve()
    })
  })
  return {
    name,
    child,
    logFile: options.logFile,
    exited: () => done,
    stop: async () => {
      if (done) return
      child.kill('SIGTERM')
      const timer = setTimeout(() => child.kill('SIGKILL'), 5000)
      await exitPromise
      clearTimeout(timer)
    },
  }
}

/** 轮询直到 `check` 返回 true；超时抛错（带 `what` 描述）。 */
export async function waitFor(
  what: string,
  check: () => Promise<boolean> | boolean,
  options: { timeoutMs?: number; intervalMs?: number } = {},
): Promise<void> {
  const timeoutMs = options.timeoutMs ?? 30_000
  const intervalMs = options.intervalMs ?? 200
  const deadline = Date.now() + timeoutMs
  let lastError: unknown
  while (Date.now() < deadline) {
    try {
      if (await check()) return
    } catch (err) {
      lastError = err
    }
    await new Promise((r) => setTimeout(r, intervalMs))
  }
  throw new Error(
    `timed out after ${timeoutMs}ms waiting for ${what}${lastError ? `: ${String(lastError)}` : ''}`,
  )
}

/** GET 一个 URL 看是不是 2xx（不走系统代理：本地地址）。 */
export async function httpOk(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(2000) })
    return res.ok
  } catch {
    return false
  }
}

/** 读整份日志文件；不存在返回空串。 */
export function readLog(file: string): string {
  try {
    return readFileSync(file, 'utf8')
  } catch {
    return ''
  }
}

/** 日志最后 `lines` 行，失败时拼进错误信息。 */
export function tailLog(file: string, lines = 40): string {
  const text = readLog(file)
  return text ? text.split('\n').slice(-lines).join('\n') : '(no log)'
}

export function logMatches(file: string, pattern: string | RegExp): boolean {
  const text = readLog(file)
  return typeof pattern === 'string' ? text.includes(pattern) : pattern.test(text)
}

/** 等日志里出现某段文字（条件轮询）。 */
export async function waitForLog(
  file: string,
  pattern: string | RegExp,
  options: { timeoutMs?: number; abortIf?: () => boolean } = {},
): Promise<void> {
  await waitFor(
    `${String(pattern)} in ${file}`,
    () => options.abortIf?.() === true || logMatches(file, pattern),
    { timeoutMs: options.timeoutMs ?? 30_000, intervalMs: 100 },
  )
}
