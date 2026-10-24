/**
 * 测试自己拥有的第二个 daemon（另一个节点，自己的 `ANYCON_HOME` 和 workspace，mock agent）：用来做“daemon 刚重启”这类
 * 不能动共享 daemon 的事。共享的那个 daemon 属于 global-setup、整套用例串行共用，重启它会打断别的用例，也没人替新进程收尾。
 * 这个 daemon 的生命周期完全在用例里（`stop()` 放在 `finally`）。
 */
import { execFileSync } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { mkdirSync } from 'node:fs'
import { join } from 'node:path'

import { cliBin, hubUrl, type E2EState } from './env.js'
import { readLog, spawnLogged, tailLog, waitFor, type Spawned } from './process.js'

export interface ExtraDaemon {
  /** 它的 `ANYCON_HOME`：传给 `anycon …` 命令（`startPairing` 等）用。 */
  home: string
  /** 它的 workspace 根：往里建仓库项目。 */
  workspaceDir: string
  logFile: string
  /** 停掉 daemon 进程，再起一个新的（同一个 home，所以是同一个节点）；等它重新连上 hub。 */
  restart: () => Promise<void>
  stop: () => Promise<void>
}

const CONNECTED = 'connected to hub'

function count(file: string, text: string): number {
  return readLog(file).split(text).length - 1
}

/**
 * 起一个新节点：`anycon init` 之后启动 `anycon daemon`，等它连上 hub。
 * 默认连共享的 hub；`options.hubUrl` 指向测试自己起的 hub（`helpers/multi-hub.ts`）。
 */
export async function startExtraDaemon(
  state: Pick<E2EState, 'tmpDir' | 'gitConfig'>,
  name: string,
  options: { hubUrl?: string } = {},
): Promise<ExtraDaemon> {
  // 目录名要短：本机 socket 在 `<home>/daemon.sock`，unix socket 路径有长度上限（macOS 104 字节，临时目录本身已经很长）
  const root = join(state.tmpDir, `n${randomBytes(3).toString('hex')}`)
  const home = join(root, 'h')
  const workspaceDir = join(root, 'w')
  const logFile = join(root, 'daemon.log')
  for (const dir of [home, workspaceDir]) mkdirSync(dir, { recursive: true })
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    ANYCON_HOME: home,
    ANYCON_MOCK_AGENT: '1',
    // 和 global-setup 一样：不跑登录 shell 解析环境，socket 立刻就绪
    ANYCON_AGENTS__ENV_FROM_LOGIN_SHELL: 'false',
    NO_COLOR: '1',
    GIT_CONFIG_GLOBAL: state.gitConfig,
    GIT_CONFIG_NOSYSTEM: '1',
  }
  execFileSync(
    cliBin,
    [
      'init',
      '--hub',
      options.hubUrl ?? hubUrl,
      '--workspace',
      workspaceDir,
      '--name',
      name,
      '--yes',
    ],
    { env, stdio: 'pipe', timeout: 30_000 },
  )
  // 本机装了真的 agent 时它会成为默认 agent；E2E 只用 mock
  for (const id of ['claude-code', 'codex']) {
    execFileSync(cliBin, ['config', 'set', `agents.${id}.enabled`, 'false'], {
      env,
      stdio: 'pipe',
      timeout: 30_000,
    })
  }

  let proc: Spawned | undefined
  const start = async (): Promise<void> => {
    const before = count(logFile, CONNECTED)
    const spawned = spawnLogged(name, cliBin, ['daemon'], { env, logFile })
    proc = spawned
    try {
      await waitFor(
        `${name} connected to the hub`,
        () => spawned.exited() || count(logFile, CONNECTED) > before,
        { timeoutMs: 30_000, intervalMs: 100 },
      )
      if (spawned.exited()) throw new Error('the daemon exited while starting')
    } catch (err) {
      await spawned.stop()
      throw new Error(`${String(err)}\n--- ${logFile} ---\n${tailLog(logFile)}`, { cause: err })
    }
  }
  const stop = async (): Promise<void> => {
    await proc?.stop()
    proc = undefined
  }
  await start()
  return {
    home,
    workspaceDir,
    logFile,
    stop,
    restart: async () => {
      await stop()
      await start()
    },
  }
}
