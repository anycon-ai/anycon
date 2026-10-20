/**
 * 起一套真实环境：hub（临时数据目录）+ daemon（mock agent，临时 ANYCON_HOME）+ web。
 * 不编译任何东西：二进制要先 `cargo build -p anycon-hub -p anycon-cli`，web 要先 `bun run --cwd web build`
 * （默认由 hub 嵌入的 dist 提供；`E2E_WEB_MODE=preview` 用 `vite preview`；`E2E_WEB_URL` 指向已有的服务）。
 * 返回的函数是 teardown：停进程、删临时目录。
 */
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  chmodSync,
  readFileSync,
  realpathSync,
  utimesSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { execFileSync } from 'node:child_process'
import { randomBytes } from 'node:crypto'

import {
  cliBin,
  e2eRoot,
  hubBin,
  hubPort,
  hubUrl,
  repoRoot,
  stateFile,
  webMode,
  webPort,
  webUrl,
  type E2EState,
} from './helpers/env.js'
import { startSmtp, type SmtpServer } from './helpers/smtp.js'
import {
  httpOk,
  spawnLogged,
  tailLog,
  waitFor,
  waitForLog,
  type Spawned,
} from './helpers/process.js'

// daemon 起不来默认就是失败（happy path 依赖它）；E2E_REQUIRE_DAEMON=0 降级为警告，E2E_SKIP_DAEMON=1 干脆不起。
const requireDaemon = process.env.E2E_REQUIRE_DAEMON !== '0'

/**
 * 在 `shimDir` 里写一个 `git` 包装脚本，供放在 daemon 的 PATH 最前面。
 *
 * 平时原样 `exec` 真正的 git。只有 `slowGitDir` 里有以当前目录名开头的规则文件 `<目录名>.<n>.rule`
 * （内容 `<秒数> <参数里要包含的字符串>`）时，第一条参数里含这个字符串的 git 先 `sleep` 那么久（`mv` 取走规则文件，
 * 并发的几条 git 里只有一条睡，而且每条规则只生效一次）。
 *
 * 用途：稳定地造出「git 元数据算得慢」（worktree.spec.ts 的 k），不靠机器忙闲。daemon 的 `workspace.list` 为每个项目并发跑
 * 5 条 git，冷缓存时前台最多等 3 秒，单条 git 的硬超时是 5 秒（超时按失败算）；`symbolic-ref` 失败后才会串行再跑一条
 * `rev-parse --short HEAD`。所以让 `symbolic-ref` 睡过 5 秒、再让后面那条睡 4 秒，整个计算约 9 秒。
 */
function installGitShim(shimDir: string, slowGitDir: string): void {
  const realGit = execFileSync('/bin/sh', ['-c', 'command -v git'], {
    encoding: 'utf8',
    timeout: 10_000,
  }).trim()
  if (!realGit.startsWith('/')) throw new Error(`git not found in PATH (got "${realGit}")`)
  for (const path of [realGit, slowGitDir]) {
    if (path.includes("'")) throw new Error(`unsupported path for the git shim: ${path}`)
  }
  const script = join(shimDir, 'git')
  writeFileSync(
    script,
    [
      '#!/bin/sh',
      `slow='${slowGitDir}'`,
      'here=$(pwd -P)',
      'proj=${here##*/}',
      'for flag in "$slow/$proj".*.rule; do',
      '  [ -f "$flag" ] || continue',
      "  secs=''; pat=''",
      '  read -r secs pat < "$flag" 2>/dev/null || continue',
      '  [ -n "$pat" ] || continue',
      '  case "$*" in *"$pat"*) ;; *) continue ;; esac',
      '  taken="$slow/$proj.taken.$$"',
      '  mv "$flag" "$taken" 2>/dev/null || continue',
      '  rm -f "$taken"',
      '  case "$secs" in ""|*[!0-9.]*) secs=0 ;; esac',
      '  sleep "$secs"',
      'done',
      `exec '${realGit}' "$@"`,
      '',
    ].join('\n'),
  )
  chmodSync(script, 0o755)
}

function run(cmd: string, args: string[], cwd: string): void {
  execFileSync(cmd, args, { cwd, stdio: 'inherit', timeout: 900_000 })
}

/**
 * `E2E_BUILD=1`：先把被测的东西都构建一遍（web → 触碰 hub 的 build.rs → cargo build）。
 * hub 在**编译时**嵌入 web/dist，所以只重建 web 不够，还得重编 hub，否则测的是旧 UI。
 */
function buildEverything(): void {
  run('bun', ['run', '--cwd', 'web', 'build'], repoRoot)
  const buildRs = join(repoRoot, 'crates', 'hub', 'build.rs')
  const now = new Date()
  utimesSync(buildRs, now, now)
  run('cargo', ['build', '-p', 'anycon-hub', '-p', 'anycon-cli'], repoRoot)
}

/**
 * hub 服务的 `index.html` 必须就是当前的 `web/dist/index.html`。不一致说明 hub 编译时没选中 web/dist（占位页），
 * 或者是 release 构建里嵌的旧 dist：这时测的是错的 UI，直接报错比一堆莫名其妙的 UI 用例失败好。
 * （debug 构建下 rust-embed 读磁盘，所以只重建 web 也行，前提是 hub 编译时 web/dist 已经存在。）
 */
async function assertHubServesCurrentWeb(): Promise<void> {
  if (webMode !== 'hub' || process.env.E2E_ALLOW_STALE_WEB === '1') return
  const dist = join(repoRoot, 'web', 'dist', 'index.html')
  if (!existsSync(dist)) return // 下面的 console.warn 已经提示过
  const served = await fetch(`${hubUrl}/`, { signal: AbortSignal.timeout(5000) }).then((r) =>
    r.text(),
  )
  if (served !== readFileSync(dist, 'utf8')) {
    throw new Error(
      'the hub serves a different index.html than web/dist (a placeholder page, or an older embedded build). ' +
        'Run: bun run --cwd web build && touch crates/hub/build.rs && cargo build -p anycon-hub -p anycon-cli ' +
        '(or set E2E_BUILD=1 to do all of that, or E2E_ALLOW_STALE_WEB=1 to ignore this check).',
    )
  }
}

export default async function globalSetup(): Promise<() => Promise<void>> {
  if (process.env.E2E_BUILD === '1') buildEverything()
  const started: Spawned[] = []
  const tmpDir = mkdtempSync(join(tmpdir(), 'anycon-e2e-'))
  // CI 失败时要把 hub.log / daemon.log 收进产物：临时目录的路径写到调用方指定的文件里（配合 E2E_KEEP_TMP=1）
  if (process.env.E2E_TMP_DIR_FILE) writeFileSync(process.env.E2E_TMP_DIR_FILE, `${tmpDir}\n`)
  const hubDataDir = join(tmpDir, 'hub')
  const daemonHome = join(tmpDir, 'daemon')
  const workspaceDir = join(tmpDir, 'workspace')
  // 「关联已有目录」用例（linked-project.spec.ts）的 link_roots：默认的 `~` 不包含系统临时目录
  const externalDir = join(tmpDir, 'external')
  for (const dir of [hubDataDir, daemonHome, workspaceDir, externalDir])
    mkdirSync(dir, { recursive: true })
  // daemon 和测试里的 git 都用这份全局配置（GIT_CONFIG_GLOBAL + GIT_CONFIG_NOSYSTEM），不读开发机的 ~/.gitconfig：
  // 每会话 worktree 的「同步 base」要做合并提交，需要 user.name / user.email（worktree.spec.ts）。
  const gitConfig = join(tmpDir, 'gitconfig')
  writeFileSync(
    gitConfig,
    [
      '[user]',
      '\tname = anycon e2e',
      '\temail = e2e@anycon.test',
      '[init]',
      '\tdefaultBranch = main',
      '[commit]',
      '\tgpgsign = false',
      '[merge]',
      '\tff = true',
      '',
    ].join('\n'),
  )

  // daemon 的 PATH 最前面放一个只在需要时才变慢的 git 包装（见 installGitShim）
  const gitShimDir = join(tmpDir, 'git-shim')
  const slowGitDir = join(tmpDir, 'slow-git')
  for (const dir of [gitShimDir, slowGitDir]) mkdirSync(dir, { recursive: true })
  installGitShim(gitShimDir, slowGitDir)

  // 假 SMTP 服务（hub 的邮箱验证码 / 通知邮件发到这里，收到的信落成文件，用例读）：先于 hub 起，它的端口要写进 hub 的环境
  let smtp: SmtpServer | undefined
  const teardown = async () => {
    for (const proc of started.toReversed()) await proc.stop()
    await smtp?.close()
    rmSync(stateFile, { force: true })
    if (process.env.E2E_KEEP_TMP === '1') console.log(`[e2e] keeping ${tmpDir}`)
    else rmSync(tmpDir, { recursive: true, force: true })
  }

  try {
    // ---- hub ----
    if (!existsSync(hubBin)) {
      throw new Error(
        `hub binary not found at ${hubBin}. Run: cargo build -p anycon-hub -p anycon-cli`,
      )
    }
    // 固定一个管理员 token（≥16 字符），不依赖读 <data>/admin-token，结果更确定。
    const adminToken = process.env.E2E_ADMIN_TOKEN ?? `e2e-${randomBytes(16).toString('hex')}`
    smtp = await startSmtp(join(tmpDir, 'smtp'))
    const hub = spawnLogged('hub', hubBin, [], {
      logFile: join(tmpDir, 'hub.log'),
      env: {
        ANYCON_ADMIN_TOKEN: adminToken,
        ANYCON_LISTEN: `127.0.0.1:${hubPort}`,
        ANYCON_PUBLIC_URL: hubUrl,
        ANYCON_DATA_DIR: hubDataDir,
        ANYCON_MODE: 'single',
        // 配了 SMTP：登录页多了邮箱验证码登录（token 表单折叠到「使用 token」后面），账号设置里能绑定邮箱（ADR-37）
        ANYCON_SMTP_URL: `smtp+plain://127.0.0.1:${smtp.port}`,
        ANYCON_EMAIL_FROM: 'Anycon <noreply@example.test>',
        // hub 自己的模块开 debug：用例要断言 hub 的日志里没有短码 / match_id（docs/spec/hub.md § 10）。
        ANYCON_LOG: process.env.ANYCON_LOG ?? 'info,anycon_hub=debug',
      },
    })
    started.push(hub)
    await waitFor(
      'hub /healthz',
      async () => !hub.exited() && (await httpOk(`${hubUrl}/healthz`)),
      {
        timeoutMs: 30_000,
      },
    ).catch((err: unknown) => {
      throw new Error(`${String(err)}\n--- hub.log ---\n${tailLog(join(tmpDir, 'hub.log'))}`)
    })
    await assertHubServesCurrentWeb()
    // ---- daemon（mock agent）----
    let daemonStarted = false
    if (process.env.E2E_SKIP_DAEMON !== '1') {
      try {
        daemonStarted = await startDaemon(started, {
          tmpDir,
          daemonHome,
          workspaceDir,
          gitConfig,
          gitShimDir,
        })
      } catch (err) {
        if (requireDaemon) throw err
        console.warn(
          `[e2e] daemon not started (E2E_REQUIRE_DAEMON=0 downgraded it to a warning): ${String(err)}`,
        )
      }
    }

    // ---- web ----
    if (webMode === 'preview') {
      const webDir = join(repoRoot, 'web')
      if (!existsSync(join(webDir, 'dist', 'index.html'))) {
        throw new Error('web/dist is missing. Run: bun run --cwd web build')
      }
      const preview = spawnLogged(
        'web-preview',
        'bunx',
        ['vite', 'preview', '--host', '127.0.0.1', '--port', String(webPort), '--strictPort'],
        { cwd: webDir, logFile: join(tmpDir, 'web.log'), env: { ANYCON_HUB_URL: hubUrl } },
      )
      started.push(preview)
      await waitFor('vite preview', async () => !preview.exited() && (await httpOk(webUrl)), {
        timeoutMs: 60_000,
      })
    } else if (webMode === 'external') {
      await waitFor(`web at ${webUrl}`, () => httpOk(webUrl), { timeoutMs: 30_000 })
    } else if (!existsSync(join(repoRoot, 'web', 'dist', 'index.html'))) {
      console.warn(
        '[e2e] web/dist not found: the hub serves its placeholder page, UI specs will fail. Run: bun run --cwd web build, then rebuild anycon-hub.',
      )
    }

    const state: E2EState = {
      hubUrl,
      webUrl,
      webMode,
      adminToken,
      tmpDir,
      hubDataDir,
      daemonHome: daemonStarted ? daemonHome : '',
      workspaceDir,
      daemonStarted,
      gitConfig,
      slowGitDir,
      smtpPort: smtp.port,
      smtpDir: smtp.dir,
    }
    writeFileSync(stateFile, JSON.stringify(state, null, 2), { mode: 0o600 })
    process.env.E2E_STATE_FILE = stateFile
    process.env.E2E_ADMIN_TOKEN = adminToken
    console.log(
      `[e2e] hub ${hubUrl}  web ${webUrl} (${webMode})  daemon ${daemonStarted ? 'up' : 'off'}  tmp ${tmpDir}`,
    )
    return teardown
  } catch (err) {
    await teardown()
    throw err
  }
}

async function startDaemon(
  started: Spawned[],
  dirs: {
    tmpDir: string
    daemonHome: string
    workspaceDir: string
    gitConfig: string
    gitShimDir: string
  },
): Promise<boolean> {
  if (!existsSync(cliBin)) {
    throw new Error(
      `anycon binary not found at ${cliBin}. Run: cargo build -p anycon-hub -p anycon-cli`,
    )
  }
  const env = {
    ...process.env,
    ANYCON_HOME: dirs.daemonHome,
    ANYCON_MOCK_AGENT: '1',
    // 不跑 `$SHELL -ilc` 解析登录环境：用户 rc 重的话要 1–4 秒才开 socket，测试里没必要。
    ANYCON_AGENTS__ENV_FROM_LOGIN_SHELL: 'false',
    NO_COLOR: '1',
    // 最前面是 git 包装脚本（平时原样转发，见 installGitShim）；`init` / `config set` 等命令也用这份环境，不受影响
    PATH: `${dirs.gitShimDir}:${process.env.PATH ?? ''}`,
    // daemon 跑的 git 只读这份配置（见上），不读开发机的全局 / 系统 git 配置
    GIT_CONFIG_GLOBAL: dirs.gitConfig,
    GIT_CONFIG_NOSYSTEM: '1',
  }
  execFileSync(
    cliBin,
    ['init', '--hub', hubUrl, '--workspace', dirs.workspaceDir, '--name', 'e2e-node', '--yes'],
    { env, stdio: 'pipe', timeout: 30_000 },
  )
  // claude-code / codex 没有配置段也参与探测；本机装了真的 agent 时它是 ready，会成为默认 agent。E2E 只用 mock。
  for (const id of ['claude-code', 'codex']) {
    execFileSync(cliBin, ['config', 'set', `agents.${id}.enabled`, 'false'], {
      env,
      stdio: 'pipe',
      timeout: 30_000,
    })
  }
  execFileSync(
    cliBin,
    // 系统临时目录在拒绝表的 /private（/var 是它的别名）里面，要显式写进 link_roots 才能关联；
    // 写 canonical 路径或 /var/… 的短写法都可以（root 不能经过别名以外的符号链接，ADR-33 第 1 条）
    [
      'config',
      'set',
      'workspace.link_roots',
      JSON.stringify([realpathSync(join(dirs.tmpDir, 'external'))]),
    ],
    { env, stdio: 'pipe', timeout: 30_000 },
  )
  const daemon = spawnLogged('daemon', cliBin, ['daemon'], {
    env,
    logFile: join(dirs.tmpDir, 'daemon.log'),
  })
  started.push(daemon)
  // 等本机 socket 出现（`anycon pair --json` 等命令都走它）。
  await waitFor(
    'daemon socket',
    () => daemon.exited() || existsSync(join(dirs.daemonHome, 'daemon.sock')),
    { timeoutMs: 30_000 },
  ).catch((err: unknown) => {
    throw new Error(
      `${String(err)}\n--- daemon.log ---\n${tailLog(join(dirs.tmpDir, 'daemon.log'))}`,
    )
  })
  if (daemon.exited()) {
    throw new Error(
      `daemon exited during start\n--- daemon.log ---\n${tailLog(join(dirs.tmpDir, 'daemon.log'))}`,
    )
  }
  // 再等它连上 hub（日志里出现 `connected to hub`；之后用例用 `anycon stats --json` 的 hub_connected 确认在线）。
  const daemonLog = join(dirs.tmpDir, 'daemon.log')
  await waitForLog(daemonLog, 'connected to hub', {
    timeoutMs: 30_000,
    abortIf: () => daemon.exited(),
  }).catch((err: unknown) => {
    throw new Error(`${String(err)}\n--- daemon.log ---\n${tailLog(daemonLog)}`)
  })
  if (daemon.exited()) {
    throw new Error(
      `daemon exited before connecting to the hub\n--- daemon.log ---\n${tailLog(daemonLog)}`,
    )
  }
  return true
}

export { e2eRoot }
