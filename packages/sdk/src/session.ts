/**
 * 会话句柄。`send()` 发送一条消息并返回事件流，迭代到本 turn 的 `turn_done`（或不可恢复的 `error`）结束。
 *
 * 协议细节（docs/protocol.md § 5.4、docs/spec/daemon.md § 7）：
 * - `session.event` 只发给 watcher；SDK 在有进行中的 `send()` / `events()` 时自动 `session.watch`，
 *   都结束后 `session.unwatch`（一个客户端最多 watch 5 个会话）。
 * - 消息要带客户端生成的 `message_id`（幂等）。事件流从「我的 user_message 之后的第一个 turn_start」开始，
 *   这样排在前一个 turn 后面的消息不会误收到前一个 turn 的尾巴，合并 turn 也能对上。
 * - 掉线重连后（重新握手）自动重新 watch，并用 `session.history(after_seq)` 补齐错过的事件，按 seq 去重。
 */
import type {
  AttachmentRef,
  ClearableSetting,
  DiffAgainst,
  Event,
  PermissionRequest,
  SessionDiffResult,
  SessionEvent,
  SessionGitStatus,
  SessionHistoryResult,
  SessionSendResult,
  SessionSettings,
  SessionSummary,
  SessionSyncBaseResult,
  SessionWorktree,
  TurnDoneReason,
  Usage,
  WorktreeDisposal,
} from '@anycon/protocol'

import { AsyncQueue } from './async-queue.js'
import { AnyconError, NodeOffline } from './errors.js'
import type { Node } from './node.js'
import type { PermissionPolicy } from './permissions.js'

/** 每个流事件都带的会话元数据（事件本身的字段在同一层，`ev.type` 直接可判别）。 */
export interface StreamMeta {
  session_id: string
  /** 会话内单调递增的 seq。 */
  seq: number
  ts_ms: number
  turn_id?: number
}

export type PermissionEventData = Extract<Event, { type: 'permission' }>

/** `permission` 事件上挂的答复方法。 */
export interface PermissionActions {
  /**
   * 允许这一次（选 `allow_once` 的 option）。请求里没有 `allow_once` 时**报错**（`no_matching_option`），
   * 不会悄悄改成 `allow_always`；要「本会话都允许」用 `allowSession()`，或者 `respond(optionId)`。
   */
  allow(message?: string): Promise<void>
  /** 本会话内都允许（选 `allow_always`，没有就退回 `allow_once`，只会收窄不会放宽）。 */
  allowSession(): Promise<void>
  /** 拒绝（选 `reject_once`），可附一句话给 agent。 */
  deny(message?: string): Promise<void>
  /** 按 option id 答复。 */
  respond(optionId: string, message?: string): Promise<void>
  /** `kind = question` 的答复。 */
  answer(answers: Record<string, string | string[]>): Promise<void>
}

export type PermissionStreamEvent = PermissionEventData & StreamMeta & PermissionActions

/** `send()` / `events()` 产出的事件。 */
export type StreamEvent =
  | (Exclude<Event, { type: 'permission' }> & StreamMeta)
  | PermissionStreamEvent

export interface SendOptions {
  /** 幂等键，默认随机 uuid。重发同一条消息时传同一个（附件也要一样，否则 `invalid`）。 */
  messageId?: string
  /**
   * 图片附件：`node.uploadImage()` 的结果，原样放进来（每条至多 20 个、合计 ≤ 20 MiB）。有附件时 `content` 可以是空串。
   * daemon 逐个核对，拒绝时 `send()` 的迭代以 `AttachmentError` 失败：blob 不存在 / 已回收 → `reason = 'blob'`（重新上传）；
   * 已经用在别的会话里 → `blob_in_use`；合计超限 → `too_large`。agent 不收这种图（看 `AgentInfo.capabilities.attachments`）
   * → `RpcError`（`unsupported`）。
   */
  attachments?: AttachmentRef[]
  /** 中止**迭代**（不会取消 turn；要取消用 `session.cancel()`）。 */
  signal?: AbortSignal
  /** 节点掉线后最多等多久，超时则迭代失败 `NodeOffline`。默认 60 秒；0 = 一直等。 */
  offlineTimeoutMs?: number
}

/** `send()` 的返回值：可 `for await` 的事件流，另带 `messageId` 和 `session.send` 的答复。 */
export interface TurnStream extends AsyncIterableIterator<StreamEvent> {
  readonly messageId: string
  /** `session.send` 的答复（`turn_id`、是否排队）。 */
  readonly sent: Promise<SessionSendResult>
}

export interface RunResult {
  /** 本 turn 的 assistant 文本（`text_delta` 拼接）。 */
  text: string
  turnId: number
  reason: TurnDoneReason
  usage: Usage | undefined
  events: StreamEvent[]
}

export interface HistoryOptions {
  afterSeq?: number
  beforeSeq?: number
  limit?: number
}

/** `session.git_status` 的选项。 */
export interface GitStatusOptions {
  /** 同时用 `gh` 查这条分支的 PR（要联网，约 1–3 秒）；没装 / 没登录 `gh`、没有远端时结果里 `pr` 为空、`pr_error` 说明原因。 */
  includePr?: boolean
}

/** `session.diff` 的选项。 */
export interface DiffOptions {
  /**
   * 比较的起点：`base`（缺省，只有 worktree 会话：merge-base 到工作区）或 `uncommitted`（HEAD 到工作区）。
   * 两种都含未跟踪的文件。
   */
  against?: DiffAgainst
  /** 仓库内的相对路径（`files[].path`）：只给这一个文件，并附 unified diff；不带 = 只给文件列表和行数。 */
  path?: string
}

/** `session.sync_base` 的选项。 */
export interface SyncBaseOptions {
  /** 放弃一次有冲突的合并（`git merge --abort`）。 */
  abort?: boolean
}

/** `session.archive` 的选项。 */
export interface ArchiveOptions {
  /** 怎么处理会话的 worktree：`auto`（缺省：干净就删，有未提交改动就保留）/ `keep` / `remove`（丢弃未提交的改动）。没有 worktree 的会话忽略。 */
  worktree?: WorktreeDisposal
}

/** `session.delete` 的选项。 */
export interface DeleteOptions extends ArchiveOptions {
  /** 同时删掉 worktree 的分支（`git branch -D`），分支上没合并的提交会丢。只有 worktree 也被删掉时才生效。 */
  deleteBranch?: boolean
}

type Subscriber = (se: SessionEvent) => void

interface TurnState {
  /** user：等我的 user_message；turn：等 turn_start；streaming：转发中。 */
  phase: 'user' | 'turn' | 'streaming'
  userEvent: StreamEvent | undefined
  expectTurn: number | undefined
  turnId: number | undefined
  /** 已经喂进状态机的最大 seq（历史回放和实时事件会重叠，按 seq 去重）。 */
  fedSeq: number
}

export function randomId(): string {
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto
  if (c?.randomUUID) return c.randomUUID()
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (ch) => {
    const r = Math.floor(Math.random() * 16)
    return (ch === 'x' ? r : (r & 0x3) | 0x8).toString(16)
  })
}

export class Session {
  readonly id: string
  readonly node: Node
  /** 本会话的权限策略，覆盖 `connect()` 的 `onPermission`。 */
  policy: PermissionPolicy | undefined
  #summary: SessionSummary
  #lastSeq: number
  readonly #subs = new Set<Subscriber>()
  #watched = false
  #watchChain: Promise<unknown> = Promise.resolve()

  /** @internal 由 `Node` 创建。 */
  constructor(node: Node, summary: SessionSummary) {
    this.node = node
    this.id = summary.id
    this.#summary = summary
    this.#lastSeq = summary.last_seq
  }

  /** 最近一次从 daemon 拿到的摘要。`refresh()` 更新。 */
  get summary(): SessionSummary {
    return this.#summary
  }

  get settings(): SessionSettings {
    return this.#summary.settings
  }

  /**
   * 会话所在的 worktree（`id` / `branch` / `base` / `base_sha` / `path` / `state`）；在项目目录本身工作的会话为 `undefined`。
   * 来自摘要，随 `session.changed` 更新（`state` 起初是 `creating`，建好后 `active`）。
   */
  get worktree(): SessionWorktree | undefined {
    return this.#summary.worktree ?? undefined
  }

  /** 已处理到的最大事件 seq。 */
  get lastSeq(): number {
    return this.#lastSeq
  }

  /** 此刻是否有进行中的 `send()` / `events()`。SDK 只对这样的会话代答权限请求。 */
  get active(): boolean {
    return this.#subs.size > 0
  }

  /** @internal */
  updateSummary(summary: SessionSummary): void {
    this.#summary = summary
    if (summary.last_seq > this.#lastSeq && this.#subs.size === 0) this.#lastSeq = summary.last_seq
  }

  async refresh(): Promise<SessionSummary> {
    const summary = await this.node.call('session.get', { session_id: this.id })
    this.updateSummary(summary)
    return summary
  }

  // ---------- 发送 ----------

  /**
   * 发送一条消息，返回事件流。**立即发送**（不是惰性的），迭代到本 turn 的 `turn_done` 结束；
   * 中途 `break` 不取消 turn，只是不再消费（事件仍由 daemon 持久化，之后可 `history()`）。
   * 不可恢复的 `error` 事件、会话 `ended` / `error` 状态也会结束迭代。
   */
  send(content: string, options: SendOptions = {}): TurnStream {
    const messageId = options.messageId ?? randomId()
    const state: TurnState = {
      phase: 'user',
      userEvent: undefined,
      expectTurn: undefined,
      turnId: undefined,
      fedSeq: 0,
    }
    let release: (() => void) | undefined
    let unguard: (() => void) | undefined
    const cleanup = () => {
      release?.()
      release = undefined
      unguard?.()
      unguard = undefined
      options.signal?.removeEventListener('abort', onAbort)
    }
    const queue = new AsyncQueue<StreamEvent>(cleanup)
    const onAbort = () => queue.fail(options.signal?.reason ?? new Error('aborted'))
    const feed = (se: SessionEvent) => this.#feedTurn(state, queue, messageId, se)

    release = this.#subscribe(feed)
    // 节点掉线 / 关闭时给迭代一个上限，避免无限挂起。
    unguard = this.#guardQueue(queue, options.offlineTimeoutMs ?? 60_000)
    if (options.signal?.aborted) onAbort()
    else options.signal?.addEventListener('abort', onAbort, { once: true })

    const sent = (async () => {
      await this.#ensureWatching()
      const attachments = options.attachments ?? []
      const result = await this.node.call('session.send', {
        session_id: this.id,
        message_id: messageId,
        content,
        ...(attachments.length > 0 ? { attachments: [...attachments] } : {}),
      })
      if (state.phase !== 'streaming') {
        // 答复里的 turn_id 是「立即开始的 turn」。没等到我的 user_message（daemon 不回显，或这是重复发送）
        // 时靠它兜底；等到了也用它收紧匹配。
        state.phase = 'turn'
        state.expectTurn = result.turn_id ?? state.expectTurn
      }
      // 重复发送：原 turn 的事件早就发生了（或还在进行），从头回放历史，状态机按 message_id 找到它。
      if (result.duplicate) await this.#replay(feed, 0)
      return result
    })()
    sent.catch((err: unknown) => queue.fail(err))
    return Object.assign(queue, { messageId, sent })
  }

  /**
   * 发送并收完整个 turn，返回拼接的文本和 usage。
   * 权限请求按 `session.policy` / `connect({ onPermission })` 代答；两者都没有时一律 deny
   * （`run()` 没有调用方可以交互）。
   */
  async run(content: string, options: SendOptions = {}): Promise<RunResult> {
    const stream = this.send(content, options)
    const events: StreamEvent[] = []
    let text = ''
    let done: Extract<Event, { type: 'turn_done' }> | undefined
    const hasPolicy = this.policy !== undefined || this.node.hasPermissionPolicy
    for await (const ev of stream) {
      events.push(ev)
      if (ev.type === 'text_delta') text += ev.text
      else if (ev.type === 'permission' && !hasPolicy) {
        await ev.deny('run() has no permission policy').catch(() => undefined)
      } else if (ev.type === 'turn_done') done = ev
    }
    if (!done) throw new AnyconError('turn_incomplete', 'the turn ended without a turn_done event')
    return {
      text,
      turnId: done.turn_id,
      reason: done.reason,
      usage: done.usage ?? undefined,
      events,
    }
  }

  /**
   * 观察会话的全部事件（不限于某个 turn），直到 `return()` / `break` / `signal`。
   * `fromSeq`：先用 `session.history` 补发 seq 大于它的历史事件；补历史期间到达的实时事件先扣住，补完之后接在后面，
   * 所以消费者看到的 seq 严格递增、不重复（例如 `sessions.create({ prompt })` 之后 `events({ fromSeq: 0 })` 能从头看到第一个 turn）。
   */
  events(
    options: { fromSeq?: number; signal?: AbortSignal } = {},
  ): AsyncIterableIterator<StreamEvent> {
    let release: (() => void) | undefined
    const queue = new AsyncQueue<StreamEvent>(() => {
      release?.()
      release = undefined
      options.signal?.removeEventListener('abort', onAbort)
    })
    const onAbort = () => queue.fail(options.signal?.reason ?? new Error('aborted'))
    // 已经推给消费者的最大 seq；补历史和实时事件会重叠，按它去重、保序。
    let high = options.fromSeq ?? 0
    let replaying = options.fromSeq !== undefined
    const held: SessionEvent[] = []
    const emit = (se: SessionEvent) => {
      if (se.seq <= high) return
      high = se.seq
      queue.push(this.#toStream(se))
    }
    const live = (se: SessionEvent) => {
      if (replaying) held.push(se)
      else emit(se)
    }
    release = this.#subscribe(live)
    options.signal?.addEventListener('abort', onAbort, { once: true })
    queue.onDispose(this.#guardQueue(queue, 0))
    void (async () => {
      await this.#ensureWatching()
      if (options.fromSeq !== undefined) {
        await this.#replay(emit, options.fromSeq)
        replaying = false
        for (const se of held.toSorted((a, b) => a.seq - b.seq)) emit(se)
        held.length = 0
      }
    })().catch((err: unknown) => queue.fail(err))
    return queue
  }

  // ---------- 其他方法 ----------

  /**
   * 答复权限请求。`answer` 是 option id；`kind = question` 时传 `{ answers }`。
   * 重复答复会得到 `RpcError`（`code = 'already_resolved'`）。
   */
  async respond(
    requestId: string,
    answer: string | { answers: Record<string, string | string[]> },
    message?: string,
  ): Promise<void> {
    await this.node.call('permission.respond', {
      request_id: requestId,
      ...(typeof answer === 'string' ? { option_id: answer } : { answers: answer.answers }),
      ...(message !== undefined ? { message } : {}),
    })
  }

  /**
   * 改会话设置，下一 turn 生效。turn 进行中会得到 `busy`。返回实际生效的完整设置。
   *
   * `settings` 是增量：缺省 / `null` = 保持不变（`options` 里值为 `null` 表示删键）。要把某个字段恢复为「未设置」，
   * 用 `clear`（`model` / `thinking` / `mode` / `env` / `system_prompt_append` / `max_turns` / `max_budget_usd`）；
   * `clear` 在 `settings` 合并之后生效，两边都出现时以 `clear` 为准。
   */
  async setSettings(
    settings: SessionSettings,
    options: { clear?: readonly ClearableSetting[] } = {},
  ): Promise<SessionSettings> {
    const clear = options.clear ?? []
    const result = await this.node.call('session.set_settings', {
      session_id: this.id,
      settings,
      ...(clear.length > 0 ? { clear: [...clear] } : {}),
    })
    this.#summary = { ...this.#summary, settings: result.settings }
    return result.settings
  }

  async cancel(): Promise<void> {
    await this.node.call('session.cancel', { session_id: this.id })
  }

  history(options: HistoryOptions = {}): Promise<SessionHistoryResult> {
    return this.node.call('session.history', {
      session_id: this.id,
      after_seq: options.afterSeq ?? 0,
      ...(options.beforeSeq !== undefined ? { before_seq: options.beforeSeq } : {}),
      ...(options.limit !== undefined ? { limit: options.limit } : {}),
    })
  }

  async rename(title: string): Promise<void> {
    await this.node.call('session.rename', { session_id: this.id, title })
    this.#summary = { ...this.#summary, title }
  }

  /**
   * 归档。有 worktree 的会话可以用 `worktree` 选择怎么处置它（缺省 `auto`）；不带选项和以前一样。
   */
  async archive(options: ArchiveOptions = {}): Promise<void> {
    await this.node.call('session.archive', {
      session_id: this.id,
      ...(options.worktree !== undefined ? { worktree: options.worktree } : {}),
    })
  }

  async unarchive(): Promise<void> {
    await this.node.call('session.unarchive', { session_id: this.id })
  }

  async restart(): Promise<void> {
    await this.node.call('session.restart', { session_id: this.id })
  }

  /**
   * 删除会话。有 worktree 的会话可以用 `worktree`（缺省 `auto`）和 `deleteBranch` 选择怎么处置它；
   * 不带选项和以前一样。
   */
  async delete(options: DeleteOptions = {}): Promise<void> {
    await this.node.call('session.delete', {
      session_id: this.id,
      ...(options.worktree !== undefined ? { worktree: options.worktree } : {}),
      ...(options.deleteBranch !== undefined ? { delete_branch: options.deleteBranch } : {}),
    })
    this.node.forgetSession(this.id)
  }

  // ---------- git（ADR-34） ----------

  /**
   * `session.git_status`：会话工作目录的分支、HEAD、（worktree 会话）base / ahead / behind、未提交和冲突的文件数、
   * 是否在合并、上游和未推送数。`includePr` 时另用 `gh` 查 PR。会话不在 git 仓库里 → `RpcError`（`invalid`，`data.reason = 'not_git'`）。
   */
  gitStatus(options: GitStatusOptions = {}): Promise<SessionGitStatus> {
    return this.node.call('session.git_status', {
      session_id: this.id,
      ...(options.includePr !== undefined ? { include_pr: options.includePr } : {}),
    })
  }

  /**
   * `session.diff`（只读）：不带 `path` 给文件列表和行数；带 `path` 只给那一个文件和它的 unified diff。
   * 结果的 `truncated` 为 true 表示文件超过 1000 个；单个 `patch_truncated` 表示 patch 被截断（≤ 256 KiB）。
   */
  diff(options: DiffOptions = {}): Promise<SessionDiffResult> {
    return this.node.call('session.diff', {
      session_id: this.id,
      ...(options.against !== undefined ? { against: options.against } : {}),
      ...(options.path !== undefined ? { path: options.path } : {}),
    })
  }

  /**
   * `session.sync_base`：把 base 的最新提交合并进 worktree 的分支（base 是远端跟踪分支时先 fetch）。
   * `outcome`：`up_to_date` / `merged`（`commits` 个）/ `conflicts`（冲突文件在 `conflicts`）/ `aborted`（`abort: true`）。
   * 失败时抛 `RpcError`：turn 在跑 → `busy`；有未提交改动 → `invalid`（`uncommitted_changes`）；上次合并还停在冲突里 →
   * `invalid`（`merge_in_progress`）；fetch / merge 失败 → `internal`（`fetch_failed` / `git_failed`）。
   */
  syncBase(options: SyncBaseOptions = {}): Promise<SessionSyncBaseResult> {
    return this.node.call('session.sync_base', {
      session_id: this.id,
      ...(options.abort !== undefined ? { abort: options.abort } : {}),
    })
  }

  // ---------- 内部：事件分发 ----------

  /** @internal Node 收到 `session.event` 时调用。按 seq 去重。 */
  dispatch(se: SessionEvent): void {
    if (se.seq <= this.#lastSeq) return
    this.#lastSeq = se.seq
    for (const sub of Array.from(this.#subs)) {
      try {
        sub(se)
      } catch {
        // 订阅者的异常不能影响会话
      }
    }
    if (se.event.type === 'permission') this.node.observePermission(this.#permissionRequest(se))
    if (se.event.type === 'title_changed')
      this.#summary = { ...this.#summary, title: se.event.title }
    else if (se.event.type === 'settings_changed') {
      this.#summary = { ...this.#summary, settings: se.event.settings }
    }
  }

  /** @internal 重新握手后：重新 watch 并补齐错过的事件。 */
  async resync(): Promise<void> {
    if (this.#subs.size === 0) return
    this.#watched = false
    await this.#ensureWatching()
    const since = this.#lastSeq
    let after = since
    for (let guard = 0; guard < 50; guard++) {
      const page = await this.history({ afterSeq: after, limit: 1000 })
      for (const se of page.events) this.dispatch(se)
      const last = page.events.at(-1)
      if (!page.has_more || !last || last.seq <= after) break
      after = last.seq
    }
  }

  #subscribe(sub: Subscriber): () => void {
    this.#subs.add(sub)
    return () => {
      if (!this.#subs.delete(sub)) return
      if (this.#subs.size === 0) this.#unwatchSoon()
    }
  }

  /** 串行化 watch / unwatch，避免「刚退订又订阅」时乱序。 */
  #ensureWatching(): Promise<void> {
    const run = this.#watchChain.then(async () => {
      if (this.#watched || this.#subs.size === 0) return
      await this.node.call('session.watch', { session_id: this.id })
      this.#watched = true
    })
    this.#watchChain = run.catch(() => undefined)
    return run
  }

  #unwatchSoon(): void {
    this.#watchChain = this.#watchChain.then(async () => {
      if (!this.#watched || this.#subs.size > 0) return
      this.#watched = false
      try {
        await this.node.call('session.unwatch', { session_id: this.id })
      } catch {
        // 节点离线或会话已删除：daemon 侧的 watcher 随连接一起清掉了
      }
    })
  }

  /**
   * 迭代期间盯着节点：`Node.close()` 立即失败；节点掉线超过 `offlineTimeoutMs`（0 = 不限）失败 `NodeOffline`。
   * 返回迭代结束时要调用的撤销函数。
   */
  #guardQueue(queue: AsyncQueue<StreamEvent>, offlineTimeoutMs: number): () => void {
    let timer: ReturnType<typeof setTimeout> | undefined
    const clear = () => {
      if (timer !== undefined) clearTimeout(timer)
      timer = undefined
    }
    const offs: Array<() => void> = [
      this.node.on('closed', () =>
        queue.fail(new NodeOffline(this.node.name, 'the client was closed')),
      ),
    ]
    if (offlineTimeoutMs > 0) {
      offs.push(
        this.node.on('offline', () => {
          clear()
          timer = setTimeout(
            () =>
              queue.fail(
                new NodeOffline(this.node.name, 'the node stayed offline during the turn'),
              ),
            offlineTimeoutMs,
          )
        }),
        this.node.on('online', clear),
      )
    }
    return () => {
      clear()
      for (const off of offs) off()
    }
  }

  /** 把 `session.history` 的历史事件按序喂给某个订阅者（不经过 seq 去重）。 */
  async #replay(sub: Subscriber, afterSeq: number): Promise<void> {
    let after = afterSeq
    for (let guard = 0; guard < 50; guard++) {
      const page = await this.history({ afterSeq: after, limit: 1000 })
      for (const se of page.events) sub(se)
      const last = page.events.at(-1)
      if (!page.has_more || !last || last.seq <= after) return
      after = last.seq
    }
  }

  #feedTurn(
    state: TurnState,
    queue: AsyncQueue<StreamEvent>,
    messageId: string,
    se: SessionEvent,
  ): void {
    if (queue.closed || se.seq <= state.fedSeq) return
    state.fedSeq = se.seq
    const ev = se.event
    if (state.phase === 'user') {
      if (ev.type === 'user_message' && ev.message_id === messageId) {
        state.userEvent = this.#toStream(se)
        state.phase = 'turn'
        return
      }
      if (!(ev.type === 'turn_start' && ev.message_id === messageId)) return
    } else if (state.phase === 'turn' && state.turnId === undefined) {
      if (ev.type === 'user_message' && ev.message_id === messageId) {
        state.userEvent = this.#toStream(se)
        return
      }
      if (ev.type !== 'turn_start') return
      const mine =
        ev.message_id === messageId ||
        (state.expectTurn !== undefined
          ? ev.turn_id === state.expectTurn
          : state.userEvent !== undefined)
      if (!mine) return
    }
    if (state.phase !== 'streaming') {
      // 第一个属于我的 turn_start：先补上我的 user_message，再开始转发。
      if (ev.type !== 'turn_start') return
      state.phase = 'streaming'
      state.turnId = ev.turn_id
      if (state.userEvent) queue.push(state.userEvent)
    }
    queue.push(this.#toStream(se))
    const finished =
      (ev.type === 'turn_done' && ev.turn_id === state.turnId) ||
      (ev.type === 'error' && !ev.recoverable) ||
      (ev.type === 'status' && (ev.state === 'ended' || ev.state === 'error'))
    if (finished) queue.end()
  }

  /** 事件流里的 `permission` 事件还原成 `permission.request` 通知的形状。 */
  #permissionRequest(se: SessionEvent): PermissionRequest {
    const ev = se.event
    if (ev.type !== 'permission') throw new Error('not a permission event')
    return {
      session_id: se.session_id,
      request_id: ev.request_id,
      kind: ev.kind,
      title: ev.title,
      detail: ev.detail,
      options: ev.options,
      expires_at_ms: ev.expires_at_ms ?? null,
      created_at_ms: se.ts_ms,
    }
  }

  #toStream(se: SessionEvent): StreamEvent {
    const base = {
      ...se.event,
      session_id: se.session_id,
      seq: se.seq,
      ts_ms: se.ts_ms,
      ...(se.turn_id !== undefined && se.turn_id !== null && !('turn_id' in se.event)
        ? { turn_id: se.turn_id }
        : {}),
    }
    if (se.event.type !== 'permission') return base as StreamEvent
    const requestId = se.event.request_id
    const respondWith = (kinds: readonly string[], message?: string) => {
      const option = kinds
        .map(
          (kind) => se.event.type === 'permission' && se.event.options.find((o) => o.kind === kind),
        )
        .find(Boolean)
      if (!option) {
        return Promise.reject(
          new AnyconError(
            'no_matching_option',
            `no ${kinds[0]} option on permission request ${requestId}; refusing to pick a broader grant, answer with respond(optionId) instead`,
          ),
        )
      }
      return this.respond(requestId, option.id, message)
    }
    const actions: PermissionActions = {
      allow: (message) => respondWith(['allow_once'], message),
      allowSession: () => respondWith(['allow_always', 'allow_once']),
      deny: (message) => respondWith(['reject_once', 'reject_always'], message),
      respond: (optionId, message) => this.respond(requestId, optionId, message),
      answer: (answers) => this.respond(requestId, { answers }),
    }
    // 方法不可枚举：事件对象 JSON 化 / 深比较时只有数据字段。
    return Object.defineProperties(
      base,
      Object.fromEntries(
        Object.entries(actions).map(([name, value]) => [name, { value, enumerable: false }]),
      ),
    ) as unknown as StreamEvent
  }
}
