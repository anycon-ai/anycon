import type { AgentInfo, DiffFile, EnrollInfo, Event, NodeInfo, Project, PullRequestInfo, SessionEvent, SessionGitStatus, SessionSummary, SessionWorktree, WorktreeInfo } from '@/protocol'
import type { NodeEntry } from '@/stores/nodes'

let seq = 0
export function resetSeq(): void {
  seq = 0
}

export function ev(event: Event, over: Partial<SessionEvent> = {}): SessionEvent {
  seq += 1
  return { session_id: 'sess_a', seq, ts_ms: 1_000 + seq, event, ...over }
}

export function evs(events: Event[]): SessionEvent[] {
  return events.map((e) => ev(e))
}

export function summary(over: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: 'sess_a',
    agent: 'mock',
    title: 'hello',
    status: 'idle',
    settings: {},
    created_at_ms: 1,
    updated_at_ms: 2,
    last_seq: 0,
    pending_permissions: 0,
    ...over,
  }
}

export function mockAgent(over: Partial<AgentInfo> = {}): AgentInfo {
  return {
    id: 'mock',
    label: 'Mock',
    driver: 'mock',
    enabled: true,
    status: 'ready',
    probed_at_ms: 1,
    models: [
      {
        id: 'mock-large',
        label: 'Mock Large',
        default: true,
        thinking: {
          options: [
            { id: 'low', label: 'low' },
            { id: 'high', label: 'high' },
          ],
          default: 'high',
        },
      },
      { id: 'mock-small', label: 'Mock Small', thinking: null },
    ],
    modes: [
      { id: 'ask', label: 'Ask', default: true, risk: 'normal' },
      { id: 'plan', label: 'Plan', risk: 'safe' },
      { id: 'yolo', label: 'YOLO', risk: 'dangerous' },
    ],
    options: [{ id: 'verbose', kind: 'bool', label: 'Verbose', default: false }],
    capabilities: { env: false, interject: false, interrupt: true, resume: true, rewind: false, structured_output: false, thinking_stream: true, usage: true },
    ...over,
  }
}

/** 一个最小的 `NodeEntry`（组件测试用）：默认在线、没有 agent 和项目。 */
export function nodeEntry(over: Partial<NodeEntry> & { id: string }): NodeEntry {
  return {
    name: 'test-node',
    conn: 'ready',
    agents: [],
    agentsLoaded: true,
    agentDefaults: {},
    projects: [],
    projectsLoaded: true,
    trash: [],
    devices: [],
    lastSeen: 1,
    pairedAt: 1,
    synced: true,
    syncing: false,
    versionMismatch: false,
    dangerousForbidden: false,
    jobs: {},
    ...over,
  }
}

export function project(over: Partial<Project> & { name: string }): Project {
  return { path: `/workspace/${over.name}`, modified_at_ms: Date.now() - 60_000, active_sessions: 0, ...over }
}

/**
 * Codex（app-server）形状的 AgentInfo，对照 docs/spec/daemon.md § 5.4 / § 6：
 * 四档权限模式（`read_only` safe / `workspace` normal 默认 / `auto_review` normal / `full_access` dangerous）、
 * 按模型不同的思考等级（`model/list` 的 `supportedReasoningEfforts`）、
 * `fast_mode` / `plan_mode`（bool）和 `model_reasoning_summary`（select）三个选项，
 * 模型 id 只是测试数据（真实的 id 来自 `model/list`，adapter 里不内置）。
 */
export function codexAgent(over: Partial<AgentInfo> = {}): AgentInfo {
  return {
    id: 'codex',
    label: 'Codex',
    driver: 'codex-app-server',
    enabled: true,
    status: 'ready',
    version: '0.157.1',
    path: '/usr/local/bin/codex',
    probed_at_ms: 1,
    models: [
      {
        id: 'codex-large',
        label: 'Codex Large',
        description: 'Best for long tasks',
        default: true,
        thinking: {
          options: [
            { id: 'low', label: 'low', description: 'Fast responses with lighter reasoning' },
            { id: 'medium', label: 'medium' },
            { id: 'high', label: 'high' },
            { id: 'xhigh', label: 'xhigh' },
          ],
          default: 'medium',
        },
      },
      {
        id: 'codex-mini',
        label: 'Codex Mini',
        thinking: {
          options: [
            { id: 'minimal', label: 'minimal' },
            { id: 'low', label: 'low' },
          ],
          default: 'low',
        },
      },
      { id: 'codex-plain', label: 'Codex Plain', thinking: null },
    ],
    modes: [
      { id: 'read_only', label: 'Read only', description: 'Read files; ask before anything else', risk: 'safe' },
      { id: 'workspace', label: 'Workspace', description: 'Edit files in the project; ask for the rest', default: true, risk: 'normal' },
      { id: 'auto_review', label: 'Auto review', description: 'Codex’s reviewer approves requests', risk: 'normal' },
      { id: 'full_access', label: 'Full access', description: 'No sandbox, no approvals', risk: 'dangerous' },
    ],
    options: [
      { id: 'fast_mode', kind: 'bool', label: 'Fast mode', description: 'Faster responses, higher usage', default: false },
      { id: 'plan_mode', kind: 'bool', label: 'Plan mode', default: false },
      {
        id: 'model_reasoning_summary',
        kind: 'select',
        label: 'Reasoning summary',
        default: 'auto',
        choices: [
          { id: 'auto', label: 'auto' },
          { id: 'concise', label: 'concise' },
          { id: 'detailed', label: 'detailed' },
          { id: 'none', label: 'none' },
        ],
      },
    ],
    capabilities: { env: true, interject: true, interrupt: true, resume: true, rewind: false, structured_output: false, thinking_stream: true, usage: true },
    ...over,
  }
}

/** 本机装了但没登录的 Codex（`codex login status` 非 0）：status = not_authenticated，hint 是一条可复制的命令。 */
export function codexNotAuthenticated(): AgentInfo {
  return codexAgent({
    status: 'not_authenticated',
    models: [],
    modes: [],
    options: [],
    allow_custom_model: true,
    issue: { code: 'login_required', message: 'Codex is not signed in.', hint: 'codex login' },
  })
}

/** 支持 worktree 的节点信息（ADR-34）：`capabilities.worktree` 和 `worktree` 配置都在。 */
export function nodeInfo(over: Partial<NodeInfo> = {}): NodeInfo {
  return {
    node_id: 'n'.repeat(32),
    name: 'test-node',
    version: '0.0.0',
    protocol_version: 1,
    platform: 'linux-x86_64',
    started_at_ms: 1,
    capabilities: { audit: true, blobs: false, fs: true, search: false, workspace: true, worktree: true },
    worktree: { default_mode: 'worktree', branch_prefix: 'anycon' },
    ...over,
  }
}

/** 一个 git 项目（`worktree_ready` 缺省 true）。 */
export function gitProject(name: string, over: Partial<NonNullable<Project['git']>> = {}): Project {
  return project({ name, git: { branch: 'main', dirty: false, worktree_ready: true, ...over } })
}

export function worktree(over: Partial<SessionWorktree> = {}): SessionWorktree {
  return { id: 'wt_1', branch: 'anycon/fix-login', base: 'main', base_sha: 'a'.repeat(40), path: '.anycon/worktrees/fix-login', state: 'active', ...over }
}

export function gitStatus(over: Partial<SessionGitStatus> = {}): SessionGitStatus {
  return { branch: 'anycon/fix-login', head_sha: 'b'.repeat(40), base: 'main', base_sha: 'a'.repeat(40), ahead: 0, behind: 0, uncommitted: 0, conflicts: 0, ...over }
}

export function pullRequest(over: Partial<PullRequestInfo> = {}): PullRequestInfo {
  return { number: 12, state: 'open', title: 'Fix the login redirect', url: 'https://github.com/o/r/pull/12', base_branch: 'main', checks_passed: 3, checks_failed: 1, checks_pending: 2, ...over }
}

export function diffFile(over: Partial<DiffFile> = {}): DiffFile {
  return { path: 'src/a.ts', status: 'modified', additions: 3, deletions: 1, ...over }
}

export function worktreeInfo(over: Partial<WorktreeInfo> = {}): WorktreeInfo {
  return {
    id: 'wt_1',
    project: 'proj',
    branch: 'anycon/fix-login',
    base: 'main',
    path: '.anycon/worktrees/fix-login',
    state: 'kept',
    uncommitted: 0,
    ahead: 0,
    created_at_ms: Date.now() - 3_600_000,
    session_id: null,
    ...over,
  }
}

/** 一份最小的 unified diff（单文件，一处修改）。 */
export function patchFor(path: string): string {
  return [`diff --git a/${path} b/${path}`, 'index 1111111..2222222 100644', `--- a/${path}`, `+++ b/${path}`, '@@ -1,2 +1,2 @@', ' keep', '-old line', '+new line', ''].join('\n')
}

/** 一条别的设备发来的接入请求（`node.enroll_requested` / `node.enroll_list` 的元素）。默认是请求批准的 `code`。 */
export function enrollInfo(over: Partial<EnrollInfo> = {}): EnrollInfo {
  const now = Date.now()
  return {
    enroll_id: 'e'.repeat(32),
    method: 'code',
    approval: true,
    status: 'wait_approval',
    hint: { name: 'Chrome · Windows', kind: 'web', platform: 'browser' },
    created_at_ms: now,
    expires_at_ms: now + 300_000,
    ...over,
  }
}
