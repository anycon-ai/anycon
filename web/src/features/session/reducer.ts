// 会话事件 → 渲染模型（spec/web.md § 4.6）。纯函数，不碰 React / 网络 / 存储。
//
// - text_delta / thinking_delta 合并进同一个块，遇到别的块或 *_done 就收口
// - tool_output_delta / tool_result 按 tool id 挂回对应的 tool_call 块
// - plan 在同一 turn 内原地更新
// - permission_resolved 更新对应卡片
// - settings_changed 生成「模型已切换为 …」之类的系统行（对比上一份 settings）
// - git（ADR-34）生成「已在 worktree … 中开始」「已合并 main 的 3 个提交」之类的系统行
// - task（后台任务快照）按 id 只留最新一条：有 tool_id 且对应的工具块已加载就挂在那个块上，否则在第一次出现的位置单独成块
// - notice 生成一行提示（info / warning）；activity 是视图级的临时状态，内容事件 / turn_done / state none 时清掉
// - turn_done.usage 里的 context_window / context_tokens 记成视图的最新上下文占用
import type {
  AttachmentRef,
  EditKind,
  Event,
  FileChangeKind,
  GitAction,
  NoticeLevel,
  PermissionDetail,
  PermissionKind,
  PermissionOption,
  PlanEntry,
  SessionEvent,
  SessionSettings,
  SessionStatus,
  TaskAgent,
  TaskKind,
  TaskPhase,
  TaskStatus,
  TaskUsage,
  ToolCallDetail,
  ToolCategory,
  ToolResultDetail,
  TurnDoneReason,
  Usage,
} from '@/protocol'

interface BlockBase {
  /** React key，稳定。 */
  key: string
  /** 产生该块的第一个事件的 seq / 时间。 */
  seq: number
  ts: number
  turnId?: number
}

export interface UserBlock extends BlockBase {
  type: 'user'
  messageId: string
  content: string
  byDevice: string
  /** 消息带的图片附件（ADR-35），以 daemon 的记录为准（含宽高）。没有附件是空数组。 */
  attachments: AttachmentRef[]
}
export interface TurnStartBlock extends BlockBase {
  type: 'turn_start'
  /** agent 自己开始的 turn（`message_id` 为空串：后台任务结束后 CLI 自己再跑的一轮），没有对应的用户消息。 */
  agentStarted: boolean
}
export interface ThinkingBlock extends BlockBase {
  type: 'thinking'
  text: string
  done: boolean
  durationMs?: number
}
export interface AssistantBlock extends BlockBase {
  type: 'assistant'
  text: string
  done: boolean
}
export interface ToolResult {
  output: unknown
  isError: boolean
  detail?: ToolResultDetail | null
}
/** 后台任务的最新一条快照（`task` 事件，整条取代前一条）。 */
export interface TaskSnapshot {
  id: string
  kind: TaskKind
  /** 启动它的 tool_call 的 id（`tool_id`）。 */
  toolId?: string
  title: string
  status: TaskStatus
  usage?: TaskUsage
  lastTool?: string
  summary?: string
  phases: TaskPhase[]
  agents: TaskAgent[]
  /** 这条快照的 seq / 时间。 */
  seq: number
  ts: number
}
export interface ToolBlock extends BlockBase {
  type: 'tool'
  toolId: string
  name: string
  category: ToolCategory
  title: string
  input: unknown
  detail?: ToolCallDetail | null
  /** 子 agent 内部的调用：启动它的那次调用的 `toolId`（`tool_call.parent_id`）。 */
  parentId?: string
  /** tool_output_delta 累积出的流式输出。 */
  streamed: string
  result?: ToolResult
  /** `task.tool_id` 指向这次调用的任务（按第一次出现的顺序），进度画在工具行下面。 */
  tasks?: TaskSnapshot[]
}
/** 对应不上任何已加载工具块的任务，在第一次出现的位置单独成块。 */
export interface TaskBlock extends BlockBase {
  type: 'task'
  task: TaskSnapshot
}
export interface NoticeBlock extends BlockBase {
  type: 'notice'
  /** 稳定的 code（docs/protocol.md § 6）；不认识的按 level 显示 `message`。 */
  code: string
  level: NoticeLevel
  message: string
  modelFrom?: string
  modelTo?: string
  resetsAtMs?: number
}
export interface PlanBlock extends BlockBase {
  type: 'plan'
  entries: PlanEntry[]
}
export interface PermissionResolution {
  optionId: string | null
  byDevice: string
  ts: number
  /** kind = question 时设备给的回答：题目 id → 选项 id / 自由输入的文字，多选是数组。旧事件和 daemon 自己答复的没有。 */
  answers?: Record<string, string | string[]>
  /** 答复时附的话（拒绝的理由、跳过问题时的说明）。 */
  message?: string
}
export interface PermissionBlock extends BlockBase {
  type: 'permission'
  requestId: string
  kind: PermissionKind
  title: string
  detail: PermissionDetail
  options: PermissionOption[]
  expiresAtMs?: number | null
  resolved?: PermissionResolution
}
export interface TurnDoneBlock extends BlockBase {
  type: 'turn_done'
  reason: TurnDoneReason
  usage?: Usage | null
}
export interface ErrorBlock extends BlockBase {
  type: 'error'
  code: string
  message: string
  recoverable: boolean
}
export type SettingField = 'model' | 'thinking' | 'mode' | 'option'
export interface SettingChange {
  field: SettingField
  /** field = option 时是选项 id。 */
  id?: string
  from?: string
  to: string
}
export interface SettingsSystemBlock extends BlockBase {
  type: 'system'
  kind: 'settings_changed'
  changes: SettingChange[]
  /** 上一份 settings 未知（缓存窗口里没有更早的事件）。 */
  initial: boolean
}
/** daemon 自己做的 git 操作（ADR-34）：建 / 删 / 重建 worktree、同步 base。 */
export interface GitSystemBlock extends BlockBase {
  type: 'system'
  kind: 'git'
  action: GitAction
  branch: string
  base?: string
  /** `sync_merged`：合进来的提交数；`worktree_created`：从 `.worktreeinclude` 复制的文件数。 */
  count?: number
  /** `sync_conflicts`：冲突的文件。 */
  files: string[]
  note?: string
}
export type SystemBlock = SettingsSystemBlock | GitSystemBlock

export type Block =
  | UserBlock
  | TurnStartBlock
  | ThinkingBlock
  | AssistantBlock
  | ToolBlock
  | TaskBlock
  | NoticeBlock
  | PlanBlock
  | PermissionBlock
  | TurnDoneBlock
  | ErrorBlock
  | SystemBlock

export interface UsageSum {
  turns: number
  inputTokens: number
  outputTokens: number
  cacheReadTokens: number
  cacheWriteTokens: number
  costUsd: number | null
}

/** turn 进行中 agent 在做的「非输出」的事（`activity` 事件）：重试 API、压缩上下文。 */
export interface ActivityInfo {
  state: 'retrying' | 'compacting'
  attempt?: number
  maxAttempts?: number
  /** retrying：距下次重试的毫秒数（从 `ts` 起算）。 */
  delayMs?: number
  reason?: string
  seq: number
  ts: number
}

/** 最近一个带 `context_window` 和 `context_tokens` 的 `turn_done`：turn 结束时窗口里的 token 数。 */
export interface ContextUsage {
  window: number
  tokens: number
}

export interface SessionView {
  blocks: Block[]
  toolIndex: Record<string, number>
  /** 任务 id → 它所在块的下标（工具块或任务块）。 */
  taskIndex: Record<string, number>
  permIndex: Record<string, number>
  planByTurn: Record<number, number>
  openAssistant: number | null
  openThinking: number | null
  currentTurn: number | null
  firstSeq: number
  lastSeq: number
  /** 事件流里最近一次 status；窗口里没见过 status 事件时是 null（用会话摘要里的）。 */
  status: SessionStatus | null
  queuedMessages: number
  queuePosition: number | null
  title: string | null
  settings: SessionSettings | null
  changedFiles: Record<string, FileChangeKind>
  usage: UsageSum
  /** 当前的临时状态；收到内容事件 / `turn_done` / `state = none` 时是 null。显示时还要看会话是不是还在跑。 */
  activity: ActivityInfo | null
  context: ContextUsage | null
}

export function emptyView(initialSettings: SessionSettings | null = null): SessionView {
  return {
    blocks: [],
    toolIndex: {},
    taskIndex: {},
    permIndex: {},
    planByTurn: {},
    openAssistant: null,
    openThinking: null,
    currentTurn: null,
    firstSeq: 0,
    lastSeq: 0,
    status: null,
    queuedMessages: 0,
    queuePosition: null,
    title: null,
    settings: initialSettings,
    changedFiles: {},
    usage: { turns: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: null },
    activity: null,
    context: null,
  }
}

/** 对比两份 settings，列出变化项（只关心 UI 展示的三项和 options）。 */
export function diffSettings(prev: SessionSettings | null, next: SessionSettings): SettingChange[] {
  const out: SettingChange[] = []
  if (next.model != null && next.model !== prev?.model) out.push({ field: 'model', from: prev?.model ?? undefined, to: next.model })
  if (next.thinking != null && next.thinking !== prev?.thinking)
    out.push({ field: 'thinking', from: prev?.thinking ?? undefined, to: next.thinking })
  if (next.mode != null && next.mode !== prev?.mode) out.push({ field: 'mode', from: prev?.mode ?? undefined, to: next.mode })
  for (const [id, v] of Object.entries(next.options ?? {})) {
    const before = prev?.options?.[id]
    if (JSON.stringify(before) !== JSON.stringify(v)) out.push({ field: 'option', id, from: before === undefined ? undefined : String(before), to: String(v) })
  }
  return out
}

/** 就地修改的工作副本：copy-on-write，只复制被改动的块。 */
class Work {
  readonly blocks: Block[]
  readonly toolIndex: Record<string, number>
  readonly taskIndex: Record<string, number>
  readonly permIndex: Record<string, number>
  readonly planByTurn: Record<number, number>
  openAssistant: number | null
  openThinking: number | null
  currentTurn: number | null
  firstSeq: number
  lastSeq: number
  status: SessionStatus | null
  queuedMessages: number
  queuePosition: number | null
  title: string | null
  settings: SessionSettings | null
  changedFiles: Record<string, FileChangeKind>
  usage: UsageSum
  activity: ActivityInfo | null
  context: ContextUsage | null

  constructor(v: SessionView) {
    this.blocks = v.blocks.slice()
    this.toolIndex = { ...v.toolIndex }
    this.taskIndex = { ...v.taskIndex }
    this.permIndex = { ...v.permIndex }
    this.planByTurn = { ...v.planByTurn }
    this.openAssistant = v.openAssistant
    this.openThinking = v.openThinking
    this.currentTurn = v.currentTurn
    this.firstSeq = v.firstSeq
    this.lastSeq = v.lastSeq
    this.status = v.status
    this.queuedMessages = v.queuedMessages
    this.queuePosition = v.queuePosition
    this.title = v.title
    this.settings = v.settings
    this.changedFiles = { ...v.changedFiles }
    this.usage = { ...v.usage }
    this.activity = v.activity
    this.context = v.context
  }

  freeze(): SessionView {
    return {
      blocks: this.blocks,
      toolIndex: this.toolIndex,
      taskIndex: this.taskIndex,
      permIndex: this.permIndex,
      planByTurn: this.planByTurn,
      openAssistant: this.openAssistant,
      openThinking: this.openThinking,
      currentTurn: this.currentTurn,
      firstSeq: this.firstSeq,
      lastSeq: this.lastSeq,
      status: this.status,
      queuedMessages: this.queuedMessages,
      queuePosition: this.queuePosition,
      title: this.title,
      settings: this.settings,
      changedFiles: this.changedFiles,
      usage: this.usage,
      activity: this.activity,
      context: this.context,
    }
  }

  closeOpen(): void {
    if (this.openAssistant !== null) {
      const b = this.blocks[this.openAssistant]
      if (b?.type === 'assistant' && !b.done) this.blocks[this.openAssistant] = { ...b, done: true }
      this.openAssistant = null
    }
    if (this.openThinking !== null) {
      const b = this.blocks[this.openThinking]
      if (b?.type === 'thinking' && !b.done) this.blocks[this.openThinking] = { ...b, done: true }
      this.openThinking = null
    }
  }

  push(b: Block): number {
    this.blocks.push(b)
    return this.blocks.length - 1
  }
}

function mergeFileKind(prev: FileChangeKind | undefined, next: FileChangeKind): FileChangeKind {
  if (prev === 'created' && next === 'modified') return 'created'
  return next
}

function applyOne(w: Work, se: SessionEvent): void {
  const ev: Event = se.event
  const base = { seq: se.seq, ts: se.ts_ms, turnId: se.turn_id ?? w.currentTurn ?? undefined }

  switch (ev.type) {
    case 'user_message': {
      if (w.blocks.some((b) => b.type === 'user' && b.messageId === ev.message_id)) return
      w.closeOpen()
      w.activity = null
      w.push({ ...base, key: `u-${ev.message_id}`, type: 'user', messageId: ev.message_id, content: ev.content, byDevice: ev.by_device, attachments: ev.attachments ?? [] })
      return
    }
    case 'turn_start': {
      w.closeOpen()
      w.activity = null
      w.currentTurn = ev.turn_id
      w.push({ ...base, turnId: ev.turn_id, key: `t-${se.seq}`, type: 'turn_start', agentStarted: ev.message_id === '' })
      return
    }
    case 'text_delta': {
      w.activity = null
      if (w.openThinking !== null) w.closeOpen()
      if (w.openAssistant !== null) {
        const b = w.blocks[w.openAssistant]
        if (b?.type === 'assistant') {
          w.blocks[w.openAssistant] = { ...b, text: b.text + ev.text }
          return
        }
      }
      w.openAssistant = w.push({ ...base, key: `a-${se.seq}`, type: 'assistant', text: ev.text, done: false })
      return
    }
    case 'text_done': {
      if (w.openAssistant !== null) {
        const b = w.blocks[w.openAssistant]
        if (b?.type === 'assistant') w.blocks[w.openAssistant] = { ...b, done: true }
        w.openAssistant = null
      }
      return
    }
    case 'thinking_delta': {
      w.activity = null
      if (w.openAssistant !== null) w.closeOpen()
      if (w.openThinking !== null) {
        const b = w.blocks[w.openThinking]
        if (b?.type === 'thinking') {
          w.blocks[w.openThinking] = { ...b, text: b.text + ev.text }
          return
        }
      }
      w.openThinking = w.push({ ...base, key: `th-${se.seq}`, type: 'thinking', text: ev.text, done: false })
      return
    }
    case 'thinking_done': {
      if (w.openThinking !== null) {
        const b = w.blocks[w.openThinking]
        if (b?.type === 'thinking') w.blocks[w.openThinking] = { ...b, done: true, durationMs: ev.duration_ms }
        w.openThinking = null
      } else {
        // 没有可收口的思考块（缓存窗口起点之后才看到 done）：补一个空块，让时长还能显示。
        w.push({ ...base, key: `th-${se.seq}`, type: 'thinking', text: '', done: true, durationMs: ev.duration_ms })
      }
      return
    }
    case 'tool_call': {
      w.closeOpen()
      w.activity = null
      const existing = w.toolIndex[ev.id]
      if (existing !== undefined) {
        const b = w.blocks[existing]
        if (b?.type === 'tool') {
          w.blocks[existing] = { ...b, name: ev.name, category: ev.category, title: ev.title, input: ev.input, detail: ev.detail, parentId: ev.parent_id ?? undefined }
          return
        }
      }
      w.toolIndex[ev.id] = w.push({
        ...base,
        key: `tool-${ev.id}`,
        type: 'tool',
        toolId: ev.id,
        name: ev.name,
        category: ev.category,
        title: ev.title,
        input: ev.input,
        detail: ev.detail,
        parentId: ev.parent_id ?? undefined,
        streamed: '',
      })
      return
    }
    case 'tool_output_delta': {
      w.activity = null
      const idx = w.toolIndex[ev.id]
      const b = idx === undefined ? undefined : w.blocks[idx]
      if (idx !== undefined && b?.type === 'tool') {
        w.blocks[idx] = { ...b, streamed: b.streamed + ev.text }
      } else {
        w.closeOpen()
        w.toolIndex[ev.id] = w.push({
          ...base,
          key: `tool-${ev.id}`,
          type: 'tool',
          toolId: ev.id,
          name: ev.id,
          category: 'other',
          title: ev.id,
          input: {},
          streamed: ev.text,
        })
      }
      return
    }
    case 'tool_result': {
      w.activity = null
      const result: ToolResult = { output: ev.output, isError: ev.is_error ?? false, detail: ev.detail }
      const idx = w.toolIndex[ev.id]
      const b = idx === undefined ? undefined : w.blocks[idx]
      if (idx !== undefined && b?.type === 'tool') {
        w.blocks[idx] = { ...b, result }
      } else {
        w.closeOpen()
        w.toolIndex[ev.id] = w.push({
          ...base,
          key: `tool-${ev.id}`,
          type: 'tool',
          toolId: ev.id,
          name: ev.id,
          category: 'other',
          title: ev.id,
          input: {},
          streamed: '',
          result,
        })
      }
      return
    }
    case 'file_changed': {
      w.changedFiles[ev.path] = mergeFileKind(w.changedFiles[ev.path], ev.kind)
      return
    }
    case 'plan': {
      w.closeOpen()
      w.activity = null
      const turn = w.currentTurn ?? 0
      const idx = w.planByTurn[turn]
      const b = idx === undefined ? undefined : w.blocks[idx]
      if (idx !== undefined && b?.type === 'plan') {
        w.blocks[idx] = { ...b, entries: ev.entries }
      } else {
        w.planByTurn[turn] = w.push({ ...base, key: `plan-${se.seq}`, type: 'plan', entries: ev.entries })
      }
      return
    }
    case 'permission': {
      w.closeOpen()
      w.activity = null
      const existing = w.permIndex[ev.request_id]
      const next: PermissionBlock = {
        ...base,
        key: `perm-${ev.request_id}`,
        type: 'permission',
        requestId: ev.request_id,
        kind: ev.kind,
        title: ev.title,
        detail: ev.detail,
        options: ev.options,
        expiresAtMs: ev.expires_at_ms,
      }
      if (existing !== undefined) {
        const b = w.blocks[existing]
        w.blocks[existing] = { ...next, seq: b?.seq ?? next.seq, resolved: b?.type === 'permission' ? b.resolved : undefined }
      } else {
        w.permIndex[ev.request_id] = w.push(next)
      }
      return
    }
    case 'permission_resolved': {
      const idx = w.permIndex[ev.request_id]
      const b = idx === undefined ? undefined : w.blocks[idx]
      if (idx !== undefined && b?.type === 'permission') {
        const resolved: PermissionResolution = { optionId: ev.option_id ?? null, byDevice: ev.by_device, ts: se.ts_ms }
        const answers = answerMap(ev.answers)
        if (answers) resolved.answers = answers
        if (ev.message?.trim()) resolved.message = ev.message.trim()
        w.blocks[idx] = { ...b, resolved }
      }
      return
    }
    case 'turn_done': {
      w.closeOpen()
      w.activity = null
      w.currentTurn = null
      w.push({ ...base, turnId: ev.turn_id, key: `td-${se.seq}`, type: 'turn_done', reason: ev.reason, usage: ev.usage })
      w.usage.turns += 1
      if (ev.usage) {
        w.usage.inputTokens += ev.usage.input_tokens ?? 0
        w.usage.outputTokens += ev.usage.output_tokens ?? 0
        w.usage.cacheReadTokens += ev.usage.cache_read_tokens ?? 0
        w.usage.cacheWriteTokens += ev.usage.cache_write_tokens ?? 0
        if (ev.usage.cost_usd != null) w.usage.costUsd = (w.usage.costUsd ?? 0) + ev.usage.cost_usd
        // 被取消 / 出错的 turn 常常没有这两个数：不覆盖上一次的
        if (ev.usage.context_window != null && ev.usage.context_tokens != null) w.context = { window: ev.usage.context_window, tokens: ev.usage.context_tokens }
      }
      return
    }
    case 'status': {
      // 没有 turn 在进行了：重试 / 压缩的临时状态不会再有下文
      if (ev.state !== 'running' && ev.state !== 'starting' && ev.state !== 'waiting_permission') w.activity = null
      w.status = ev.state
      w.queuedMessages = ev.queued_messages ?? 0
      w.queuePosition = ev.position ?? null
      return
    }
    case 'settings_changed': {
      w.closeOpen()
      const changes = diffSettings(w.settings, ev.settings)
      const initial = w.settings === null
      w.settings = ev.settings
      if (changes.length > 0) w.push({ ...base, key: `sys-${se.seq}`, type: 'system', kind: 'settings_changed', changes, initial })
      return
    }
    case 'title_changed': {
      w.title = ev.title
      return
    }
    case 'git': {
      w.closeOpen()
      w.push({
        ...base,
        key: `git-${se.seq}`,
        type: 'system',
        kind: 'git',
        action: ev.action,
        branch: ev.branch,
        base: ev.base ?? undefined,
        count: ev.count ?? undefined,
        files: ev.files ?? [],
        note: ev.note ?? undefined,
      })
      return
    }
    case 'error': {
      w.closeOpen()
      w.activity = null
      w.push({ ...base, key: `err-${se.seq}`, type: 'error', code: ev.code, message: ev.message, recoverable: ev.recoverable })
      return
    }
    case 'task': {
      const task: TaskSnapshot = {
        id: ev.id,
        kind: ev.kind,
        toolId: ev.tool_id ?? undefined,
        title: ev.title,
        status: ev.status,
        usage: ev.usage ?? undefined,
        lastTool: ev.last_tool ?? undefined,
        summary: ev.summary || undefined,
        phases: ev.phases ?? [],
        agents: ev.agents ?? [],
        seq: se.seq,
        ts: se.ts_ms,
      }
      // 同一个 id 的后一条整条取代前一条，留在第一次出现的位置
      const at = w.taskIndex[ev.id]
      const placed = at === undefined ? undefined : w.blocks[at]
      if (at !== undefined && placed?.type === 'task') {
        w.blocks[at] = { ...placed, task }
        return
      }
      if (at !== undefined && placed?.type === 'tool') {
        w.blocks[at] = { ...placed, tasks: (placed.tasks ?? []).map((t) => (t.id === ev.id ? task : t)) }
        return
      }
      // 第一次见到：启动它的工具块已加载就挂上去（不新增一行），否则单独成块
      const toolAt = ev.tool_id ? w.toolIndex[ev.tool_id] : undefined
      const tool = toolAt === undefined ? undefined : w.blocks[toolAt]
      if (toolAt !== undefined && tool?.type === 'tool') {
        w.blocks[toolAt] = { ...tool, tasks: [...(tool.tasks ?? []), task] }
        w.taskIndex[ev.id] = toolAt
        return
      }
      w.closeOpen()
      w.taskIndex[ev.id] = w.push({ ...base, key: `task-${ev.id}`, type: 'task', task })
      return
    }
    case 'notice': {
      w.closeOpen()
      w.push({
        ...base,
        key: `notice-${se.seq}`,
        type: 'notice',
        code: ev.code,
        level: ev.level,
        message: ev.message,
        modelFrom: ev.model_from ?? undefined,
        modelTo: ev.model_to ?? undefined,
        resetsAtMs: ev.resets_at_ms ?? undefined,
      })
      return
    }
    case 'activity': {
      w.activity =
        ev.state === 'none'
          ? null
          : {
              state: ev.state,
              attempt: ev.attempt ?? undefined,
              maxAttempts: ev.max_attempts ?? undefined,
              delayMs: ev.delay_ms ?? undefined,
              reason: ev.reason ?? undefined,
              seq: se.seq,
              ts: se.ts_ms,
            }
      return
    }
  }
}

/** 应用一批事件（按 seq 升序）。seq 不大于 view.lastSeq 的当作重复丢弃。返回新视图；没有变化时返回原对象。 */
export function applyEvents(view: SessionView, events: readonly SessionEvent[]): SessionView {
  let w: Work | null = null
  for (const se of events) {
    const last = w ? w.lastSeq : view.lastSeq
    if (se.seq <= last) continue
    w ??= new Work(view)
    applyOne(w, se)
    if (w.firstSeq === 0) w.firstSeq = se.seq
    w.lastSeq = se.seq
  }
  return w ? w.freeze() : view
}

export function buildView(events: readonly SessionEvent[], initialSettings: SessionSettings | null = null): SessionView {
  const sorted = events.slice().sort((a, b) => a.seq - b.seq)
  return applyEvents(emptyView(initialSettings), sorted)
}

// ---- 入库前的排序 / 间隙判断 ----

export interface IngestPlan {
  /** 可以直接应用的（紧接在 lastSeq 之后、连续的）。 */
  apply: SessionEvent[]
  /** 和已有事件之间有缺口、暂时不能应用的。 */
  ahead: SessionEvent[]
  /** 有缺口：需要 `session.history(after_seq = lastSeq)` 补齐。 */
  gap: boolean
}

/**
 * 判断一批新到的事件能不能直接接在已有的后面。
 * lastSeq = 0 表示还不知道任何事件，此时接受第一个到的作为起点。
 */
export function planIngest(lastSeq: number, incoming: readonly SessionEvent[], opts: { allowGap?: boolean } = {}): IngestPlan {
  const sorted = incoming.filter((e) => e.seq > lastSeq).sort((a, b) => a.seq - b.seq)
  if (opts.allowGap) {
    // 补完历史后仍然缺的 seq 是永久缺口（daemon 对慢客户端合并 delta 时会这样）：接受，别无限追
    const seen = new Set<number>()
    const apply = sorted.filter((e) => (seen.has(e.seq) ? false : (seen.add(e.seq), true)))
    return { apply, ahead: [], gap: false }
  }
  const apply: SessionEvent[] = []
  let expect = lastSeq === 0 && sorted[0] ? sorted[0].seq : lastSeq + 1
  let i = 0
  for (; i < sorted.length; i++) {
    const e = sorted[i]!
    if (e.seq === expect) {
      apply.push(e)
      expect++
    } else if (e.seq < expect) {
      continue // 同一批里的重复
    } else {
      break
    }
  }
  const ahead = sorted.slice(i)
  return { apply, ahead, gap: ahead.length > 0 }
}

/**
 * 最近一次 `sync_*` 事件（同步 base 的结果）。冲突那条消息上的「让 agent 解决」「放弃合并」只给它，
 * 而且只在它是 `sync_conflicts` 的时候——之后有了别的同步结果，那场冲突就过去了。
 */
export function latestSyncBlock(blocks: readonly Block[]): GitSystemBlock | null {
  for (let i = blocks.length - 1; i >= 0; i--) {
    const b = blocks[i]!
    if (b.type === 'system' && b.kind === 'git' && b.action.startsWith('sync_')) return b
  }
  return null
}

/** `permission_resolved.answers` 收窄成「题目 id → 字符串或字符串数组」，别的值丢掉；什么都不剩就是没有。 */
function answerMap(raw: Record<string, unknown> | null | undefined): Record<string, string | string[]> | undefined {
  if (!raw) return undefined
  const out: Record<string, string | string[]> = {}
  for (const [k, v] of Object.entries(raw)) {
    if (typeof v === 'string') out[k] = v
    else if (Array.isArray(v)) out[k] = v.filter((x): x is string => typeof x === 'string')
  }
  return Object.keys(out).length > 0 ? out : undefined
}

/** 视图里还没处理的权限卡片。 */
export function pendingPermissions(view: SessionView): PermissionBlock[] {
  return view.blocks.filter((b): b is PermissionBlock => b.type === 'permission' && !b.resolved)
}

export type { EditKind }
