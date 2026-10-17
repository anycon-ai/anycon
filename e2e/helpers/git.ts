/**
 * 测试里跑 `git` 的帮助函数：建仓库项目、在主目录 / worktree 里提交。
 * 环境和 daemon 一致：`GIT_CONFIG_GLOBAL` 指向 global-setup 写的临时配置，`GIT_CONFIG_NOSYSTEM=1`，
 * 不读开发机的 `~/.gitconfig`；每条命令都带超时。
 */
import { execFileSync } from 'node:child_process'
import { mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'

import { runCli } from './cli.js'
import type { E2EState } from './env.js'
import { waitFor } from './process.js'

/** 单条 git 命令的上限（本地仓库的操作远小于这个值）。 */
const GIT_TIMEOUT_MS = 30_000

/** 干净的 git 环境：丢掉继承来的 `GIT_*`（比如跑在某个 hook 里），只留测试自己的配置。 */
export function gitEnv(state: Pick<E2EState, 'gitConfig'>): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {}
  for (const [key, value] of Object.entries(process.env)) {
    if (!key.startsWith('GIT_')) env[key] = value
  }
  return {
    ...env,
    GIT_CONFIG_GLOBAL: state.gitConfig,
    GIT_CONFIG_NOSYSTEM: '1',
    GIT_TERMINAL_PROMPT: '0',
    GIT_EDITOR: ':',
    GIT_PAGER: 'cat',
    LC_ALL: 'C',
  }
}

/** 在 `cwd` 里跑 `git …`，返回 stdout（去掉末尾换行）。失败时抛错，信息里带 stderr。 */
export function git(state: Pick<E2EState, 'gitConfig'>, cwd: string, ...args: string[]): string {
  try {
    return execFileSync('git', args, {
      cwd,
      env: gitEnv(state),
      encoding: 'utf8',
      timeout: GIT_TIMEOUT_MS,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).replace(/\n$/, '')
  } catch (err) {
    const e = err as { stderr?: string | Buffer; message?: string }
    throw new Error(
      `git ${args.join(' ')} (in ${cwd}) failed: ${e.stderr?.toString() || e.message}`,
      {
        cause: err,
      },
    )
  }
}

/** 写文件（上级目录不存在就建）。 */
export function writeFile(path: string, content: string): void {
  mkdirSync(dirname(path), { recursive: true })
  writeFileSync(path, content)
}

/** 被 `.gitignore` 忽略、由 `.worktreeinclude` 列出的文件：新 worktree 里应当有一份拷贝。 */
export const INCLUDED_FILE = '.env'
export const INCLUDED_CONTENT = 'FEATURE_FLAG=on\n'

export interface RepoOptions {
  /** 做一次初始提交（默认 true）。false = 空仓库（`worktree_ready = false`）。 */
  commit?: boolean
}

/**
 * 在 workspace 根下建一个 git 仓库项目：`README.md`、`.gitignore`（忽略 `.env`）、`.worktreeinclude`（列出 `.env`）
 * 已提交，`.env` 是被忽略的未跟踪文件。返回项目目录。
 */
export function makeRepoProject(
  state: Pick<E2EState, 'gitConfig' | 'workspaceDir'>,
  name: string,
  options: RepoOptions = {},
): string {
  const dir = join(state.workspaceDir, name)
  mkdirSync(dir, { recursive: true })
  git(state, dir, 'init', '-b', 'main')
  writeFile(join(dir, 'README.md'), `# ${name}\n`)
  writeFile(join(dir, '.gitignore'), `${INCLUDED_FILE}\n`)
  writeFile(join(dir, '.worktreeinclude'), `${INCLUDED_FILE}\n`)
  writeFile(join(dir, INCLUDED_FILE), INCLUDED_CONTENT)
  if (options.commit !== false) {
    git(state, dir, 'add', 'README.md', '.gitignore', '.worktreeinclude')
    git(state, dir, 'commit', '-m', 'initial commit')
  }
  return dir
}

/** 在 `dir` 里写一个文件并提交（`git add -A` + `git commit`）。返回新提交的短哈希。 */
export function commitFile(
  state: Pick<E2EState, 'gitConfig'>,
  dir: string,
  file: string,
  content: string,
  message: string,
): string {
  writeFile(join(dir, file), content)
  git(state, dir, 'add', '-A')
  git(state, dir, 'commit', '-m', message)
  return git(state, dir, 'rev-parse', '--short', 'HEAD')
}

/** 一条“让某类 git 调用变慢”的规则：参数里含 `match` 的第一条 git 先睡 `seconds` 秒。 */
export interface SlowGitRule {
  match: string
  seconds: number
}

/**
 * 让 daemon 在项目 `name`（workspace 根下的目录名）里跑的某些 git 变慢（每条规则只生效一次，其余原样转发）。
 *
 * 原理：global-setup 把一个 `git` 包装脚本放在 daemon 的 PATH 最前面，它看 `state.slowGitDir` 里有没有
 * `<项目目录名>.<n>.rule`；这里写这些文件，包装脚本取走匹配的那条并睡。daemon 的限制：冷缓存时 `workspace.list`
 * 最多等 3 秒，单条 git 硬超时 5 秒（见 global-setup 的 `installGitShim`）。返回清理函数（没被取走的规则删掉）。
 *
 * 要在**任何会让 daemon 第一次碰这个项目的事**之前调用：第一次碰 = 浏览器连上之后拉项目列表。
 */
export function slowGit(
  state: Pick<E2EState, 'slowGitDir'>,
  name: string,
  rules: SlowGitRule[],
): () => void {
  const flags = rules.map((rule, i) => {
    if (rule.match.includes('\n')) throw new Error('a slow-git match must be a single line')
    const flag = join(state.slowGitDir, `${name}.${i}.rule`)
    writeFileSync(flag, `${rule.seconds} ${rule.match}\n`)
    return flag
  })
  return () => {
    for (const flag of flags) rmSync(flag, { force: true })
  }
}

/** `anycon projects --json` 里和本测试有关的字段（`WorkspaceListResult.projects[]`）。 */
interface ListedProject {
  name: string
  git?: { branch?: string | null; worktree_ready?: boolean } | null
}

/**
 * 等 daemon 的项目列表里有这个项目的 git 元数据（`GitMeta`，含 `worktree_ready`），并返回 `worktree_ready`。
 *
 * **worktree 用例不用它做预热**（`workspace.list` 对没有缓存的项目最多等 3 秒，算不完再发 `workspace.changed`，
 * 网页补拉并显示占位，用例不必先把缓存暖好，直接面对冷缓存）。留着给需要“daemon 已经知道这个项目的 git 状态”的场合。
 */
export async function waitForProjectGit(
  state: Pick<E2EState, 'daemonHome'>,
  name: string,
): Promise<boolean> {
  let ready = false
  await waitFor(
    `daemon reports git metadata for project ${name} (anycon projects --json)`,
    async () => {
      const out = await runCli(state, ['projects', '--json'], { timeoutMs: 10_000 })
      const list = JSON.parse(out) as { projects?: ListedProject[] }
      const meta = list.projects?.find((p) => p.name === name)?.git
      if (!meta) return false
      // 线路上 `worktree_ready = false` 不序列化（skip_serializing_if），缺省就是 false
      ready = meta.worktree_ready === true
      return true
    },
    { timeoutMs: 20_000, intervalMs: 150 },
  )
  return ready
}
