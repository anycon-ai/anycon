/**
 * 分身用例（`twin.spec.ts`、`twin-links.spec.ts`）共用的帮助函数：节点侧的 CLI 读数、判断后端的配置片段、
 * 等假后端收到某个请求、把额外节点从账号里去掉。
 */
import { expect, type APIRequestContext } from '@playwright/test'

import { runCli } from './cli.js'
import type { E2EState } from './env.js'
import type { FakeJudge, JudgeRequest } from './judge.js'
import { isTriage } from './judge.js'
import { loginViaApi } from './ui.js'

/** 判断后端的 `api_key_env` 指向的环境变量名；值只在这个节点的 daemon 进程环境里。 */
export const JUDGE_KEY_ENV = 'E2E_JUDGE_KEY'
export const JUDGE_KEY = 'e2e-judge-key'

type Home = { daemonHome: string }

/** 追加到节点 `config.toml` 的判断后端段：分拣用假后端，调用次数放宽（整个文件会有几十次）。 */
export function judgeToml(judge: FakeJudge): string {
  return [
    '[judge]',
    'triage = "e2efake"',
    'daily_calls = 500',
    '',
    '[judge.backends.e2efake]',
    'kind = "api"',
    'format = "anthropic"',
    `base_url = "${judge.url}"`,
    'model = "e2e-judge"',
    `api_key_env = "${JUDGE_KEY_ENV}"`,
    'timeout = "30s"',
  ].join('\n')
}

/** 跑一条 `--json` 的 CLI，取最后一行 JSON。 */
export async function cliJson<T>(home: Home, args: string[]): Promise<T> {
  const out = await runCli(home, [...args, '--json'])
  return JSON.parse(out.trim().split('\n').at(-1)!) as T
}

export interface DaemonTodo {
  id: string
  title: string
  state: string
  handler?: string
  activity?: string
  project?: string
  sessions?: string[]
  remote?: { node_id: string; todo_id: string }[]
  source: { kind: string; device_id?: string; todo_id?: string }
}

export async function todos(home: Home): Promise<DaemonTodo[]> {
  return (await cliJson<{ todos: DaemonTodo[] }>(home, ['todo', 'ls', '--all'])).todos
}

export async function todoByTitle(home: Home, title: string): Promise<DaemonTodo | undefined> {
  return (await todos(home)).find((t) => t.title === title)
}

export interface TodoEvent {
  kind: string
  seq: number
  text?: string
  action?: string
  reason?: string
  node_id?: string
  session_id?: string
  todo_id?: string
}

export async function todoHistory(home: Home, id: string): Promise<TodoEvent[]> {
  return (await cliJson<{ events: TodoEvent[] }>(home, ['todo', 'history', id.slice(0, 8)])).events
}

export interface TwinStatus {
  scope: {
    paused: boolean
    projects: string[]
    agent?: string
    max_sessions: number
    daily_dispatches: number
  }
  state: string
  enabled: boolean
  configured: boolean
  today: { dispatches: number; judge_calls: number }
  working: string[]
  waiting: string[]
}

export async function twinStatus(home: Home, requester?: string): Promise<TwinStatus> {
  return cliJson<TwinStatus>(home, [
    'twin',
    'status',
    ...(requester ? ['--requester', requester] : []),
  ])
}

export interface TwinDecision {
  todo_id: string
  action: string
  reason: string
  outcome: string
  skip_reason?: string
  session_id?: string
  question_id?: string
  node?: string
  project?: string
}

export interface TwinRunRow {
  id: string
  finished_ms?: number
  triggers: string[]
  dry_run: boolean
  result: string
  items: string[]
  judge_calls: string[]
  decisions: TwinDecision[]
}

/** 最近的运行记录（新的在前）。 */
export async function twinRuns(home: Home): Promise<TwinRunRow[]> {
  return (await cliJson<{ runs: TwinRunRow[] }>(home, ['twin', 'runs'])).runs
}

export interface SessionRow {
  id: string
  status: string
  project?: string
  agent?: string
  worktree?: unknown
}

export async function sessions(home: Home): Promise<SessionRow[]> {
  return (await cliJson<{ sessions: SessionRow[] }>(home, ['ls'])).sessions
}

/** 假后端收到的分拣请求（不含别的位置的调用）。 */
export function triageRequests(judge: FakeJudge): JudgeRequest[] {
  return judge.requests.filter(isTriage)
}

/**
 * 等假后端收到第 `after` 个之后的一个满足条件的分拣请求，返回它。上限是调用方给的：分拣的合并窗口是 60 秒（daemon.md § 26.3），
 * 每次唤醒至少要等这么久，所以这是对一个具体事件的有界等待，不是固定睡眠。
 */
export async function waitForTriage(
  judge: FakeJudge,
  what: string,
  predicate: (request: JudgeRequest) => boolean,
  options: { after?: number; timeoutMs?: number } = {},
): Promise<JudgeRequest> {
  let found: JudgeRequest | undefined
  await expect
    .poll(
      () => {
        found = judge.requests.slice(options.after ?? 0).find((r) => isTriage(r) && predicate(r))
        return found !== undefined
      },
      {
        message: `the fake judge receives ${what}`,
        timeout: options.timeoutMs ?? 150_000,
        intervals: [500],
      },
    )
    .toBe(true)
  return found!
}

interface HubDevice {
  id: string
  name: string
  kind: string
  paired_with: string[]
}

/** hub 里这个账号的设备（`GET /api/devices`）；调用方已经登录过 `request`。 */
export async function hubDevices(
  request: APIRequestContext,
  state: E2EState,
): Promise<HubDevice[]> {
  const res = await request.get(`${state.hubUrl}/api/devices`)
  expect(res.ok(), `GET /api/devices → ${res.status()}`).toBe(true)
  return (await res.json()) as HubDevice[]
}

/**
 * 把测试自己起的节点（按名字）从账号里去掉：之后的用例（happy path 第 8 步）假定账号下只有一台节点。
 * 节点的分身设备（种类 `twin`，名字是「<节点名>'s twin」，docs/spec/daemon.md § 27.1）一并去掉。尽力而为：清不掉不挡住别的清理。
 */
export async function removeNodesFromAccount(
  request: APIRequestContext,
  state: E2EState,
  names: string[],
): Promise<void> {
  try {
    await loginViaApi(request, state)
    const mine = (d: HubDevice): boolean =>
      (d.kind === 'daemon' && names.includes(d.name)) ||
      (d.kind === 'twin' && names.some((n) => d.name.startsWith(n)))
    for (const d of (await hubDevices(request, state)).filter(mine)) {
      await request.delete(`${state.hubUrl}/api/devices/${d.id}`, { headers: { 'X-Anycon': '1' } })
    }
  } catch {
    // 清理尽力而为
  }
}
