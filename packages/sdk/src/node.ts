/**
 * 一个节点（daemon）的客户端视图：`info()` / `agents` / `projects` / `sessions`，
 * 以及 daemon → 客户端的通知分发（`session.event`、`permission.request`、`job.*` ……）。
 */
import {
  METHODS,
  PROTOCOL_VERSION,
  type AgentInfo,
  type AttachmentRef,
  type AgentListResult,
  type JobDone,
  type JobProgress,
  type NodeInfo,
  type PermissionRequest,
  type PermissionResolved,
  type Project,
  type RequestMethodName,
  type RequestMethods,
  type RequestOptions,
  type SessionChanged,
  type SessionEvent,
  type SessionListParams,
  type SessionSettings,
  type SessionSummary,
  type SessionWorkspace,
  type WorkspaceBranchesResult,
  type WorkspaceBrowseResult,
  type WorkspaceChanged,
  type SearchHit,
  type TrashEntry,
  type WorktreeInfo,
} from '@anycon/protocol'

import {
  downloadAttachment,
  raceAbort,
  resolveFileMime,
  resolveImageMime,
  uploadBlob,
  type DownloadOptions,
  type DownloadedAttachment,
  type UploadFileOptions,
  type UploadImageOptions,
} from './attachments.js'
import type { Client } from './client.js'
import { Emitter } from './emitter.js'
import {
  AnyconError,
  NodeOffline,
  NodeRejected,
  ProtocolMismatch,
  RpcError,
  mapRpcError,
} from './errors.js'
import type { LinkState, NodeLink } from './link.js'
import { decisionToParams, type PermissionPolicy } from './permissions.js'
import { Session, randomId } from './session.js'

export interface NodeEvents {
  /** 与节点的端到端通道建立（含每次重连后）。 */
  online: []
  /** 通道断开（节点下线、hub 断线、重新握手开始）。 */
  offline: []
  state: [state: LinkState]
  /**
   * 非致命的通道问题：hub 丢了我们的信封（`dropped`：分片后仍超限 / 配对关系不对）。进行中的请求已经
   * 失败，通道会重新握手；这里让调用方有机会记录。限速（`rate_limited`）和背压（`backpressure`）不走这里：
   * hub 不丢信封，SDK 暂停、等一会儿再按序重发，请求照常完成。
   */
  error: [error: Error]
  /** `node.close()` 或 client 关闭：进行中的事件流会因此失败。 */
  closed: []
  session_changed: [SessionChanged]
  permission: [PermissionRequest]
  permission_resolved: [PermissionResolved]
  /**
   * 权限策略没能答复这个请求：策略回调抛了异常，或它的决定无法执行（比如 `'allow'` 但请求里没有 `allow_once`）。
   * 请求照常走超时 deny（`permissionTimeoutMs`）；这里让调用方有机会记录 / 报警，而不是悄悄吞掉。
   */
  permission_error: [error: unknown, request: PermissionRequest]
  agent_changed: [AgentListResult]
  workspace_changed: [WorkspaceChanged]
}

export interface NodeOptions {
  onPermission?: PermissionPolicy
  /** SDK 代答 deny 前等多久。默认 10 分钟。 */
  permissionTimeoutMs?: number
}

export interface CreateSessionParams {
  agent: string
  /** 项目名；省略 = scratch 会话。 */
  project?: string
  /** 值来自 `agents.list()`：model / thinking / mode / options / env ……不传的取节点默认。 */
  settings?: SessionSettings
  /** 只对这个会话生效的权限策略，覆盖 `connect()` 的。 */
  onPermission?: PermissionPolicy
  /**
   * 在哪里工作（ADR-34）。不传 = 在项目目录本身工作（线路上缺省 `{ mode: 'local' }`；SDK 不替调用方读节点的
   * `worktree.default`）。`{ mode: 'worktree', base?, branch? }` 让 daemon 新建 worktree 和分支，`create()` 返回时
   * `session.worktree.state` 是 `creating`，建好之后才拉起 agent。项目不是 `worktree_ready` → `RpcError`
   * （`invalid`，`data.reason = 'not_worktree_ready'`），分支已存在 → `exists`，`base` 解析不了 → `invalid`（`bad_base`）。
   */
  workspace?: SessionWorkspace
  /**
   * 创建后立即发送的第一条消息。SDK 为它生成 `message_id`（uuid v4）；空串当作没有。
   * `create()` 不替你收这个 turn 的事件：要看它，创建后马上 `session.events({ fromSeq: 0 })`
   * （`events()` 会从历史补齐已经发生的），权限策略也要等会话有进行中的 `events()` / `send()` 才会代答。
   */
  prompt?: string
  /**
   * 第一条消息的附件（`node.uploadImage()` / `node.uploadFile()` 的结果，原样放进来）。有附件时 `prompt` 可以为空，照样发第一条消息。
   * daemon 的核对见 `Session.send` 的 `attachments`。
   */
  attachments?: AttachmentRef[]
  /**
   * 第一条消息的幂等键，默认随机 uuid。`create()` 超时后要重试时传同一个：daemon 按它返回第一次创建的会话，
   * 不会再开第二个（附件已经绑定到那个会话也不会得到 `blob_in_use`）。没有 `prompt` 也没有 `attachments` 时忽略。
   */
  messageId?: string
}

/** 长任务（克隆）。 */
export interface Job {
  readonly id: string
  /** 进度流；任务结束（`job.done`）后迭代完毕。 */
  progress(): AsyncIterable<JobProgress>
  /** 等任务结束。`ok = false` 时抛 `AnyconError('job_failed')`。 */
  wait(): Promise<JobDone>
}

interface JobRecord {
  progress: JobProgress[]
  done: JobDone | undefined
  waiters: Set<() => void>
}

const PERMISSION_TIMEOUT_MS = 10 * 60_000

export class Node extends Emitter<NodeEvents> {
  readonly id: string
  readonly #client: Client
  readonly #link: NodeLink
  readonly #policy: PermissionPolicy | undefined
  readonly #permissionTimeoutMs: number
  readonly #sessions = new Map<string, Session>()
  readonly #jobs = new Map<string, JobRecord>()
  readonly #permSeen = new Set<string>()
  readonly #permTimers = new Map<string, ReturnType<typeof setTimeout>>()
  #name: string | undefined
  /** 上传排队：每个节点同时只传一张（这张传完 / 失败之后才轮到下一张）。 */
  #uploadTail: Promise<void> = Promise.resolve()
  #info: NodeInfo | undefined
  #agents: AgentInfo[] | undefined
  #agentsEtag: string | undefined
  #ownsClient = false

  /** @internal 由 `Client.node()` 创建。 */
  constructor(client: Client, link: NodeLink, options: NodeOptions = {}) {
    super()
    this.#client = client
    this.#link = link
    this.id = link.node.id
    this.#name = link.node.name
    this.#policy = options.onPermission
    this.#permissionTimeoutMs = options.permissionTimeoutMs ?? PERMISSION_TIMEOUT_MS

    link.on('ready', () => {
      this.emit('online')
      void this.#resyncSessions()
    })
    link.on('offline', () => this.emit('offline'))
    link.on('state', (state) => this.emit('state', state))
    link.on('error', (error) => this.emit('error', error))

    const rpc = link.rpc
    rpc.onNotification<SessionEvent>(METHODS.SESSION_EVENT, (se) => {
      this.#sessions.get(se.session_id)?.dispatch(se)
    })
    rpc.onNotification<SessionChanged>(METHODS.SESSION_CHANGED, (c) => this.#onSessionChanged(c))
    rpc.onNotification<PermissionRequest>(METHODS.PERMISSION_REQUEST, (req) => {
      this.emit('permission', req)
      this.observePermission(req)
    })
    rpc.onNotification<PermissionResolved>(METHODS.PERMISSION_RESOLVED, (r) => this.#onResolved(r))
    rpc.onNotification<AgentListResult>(METHODS.AGENT_CHANGED, (r) => {
      this.#agents = r.agents
      this.#agentsEtag = r.etag
      this.emit('agent_changed', r)
    })
    rpc.onNotification<WorkspaceChanged>(METHODS.WORKSPACE_CHANGED, (c) => {
      this.emit('workspace_changed', c)
    })
    rpc.onNotification<JobProgress>(METHODS.JOB_PROGRESS, (p) => {
      const rec = this.#job(p.job_id)
      rec.progress.push(p)
      this.#wake(rec)
    })
    rpc.onNotification<JobDone>(METHODS.JOB_DONE, (d) => {
      const rec = this.#job(d.job_id)
      rec.done = d
      this.#wake(rec)
    })
  }

  /** 节点显示名（配对时未知，首次 `info()` 后补上）。 */
  get name(): string | undefined {
    return this.#name
  }

  /** 端到端通道是否可用（节点在线且 Noise 握手完成）。 */
  get online(): boolean {
    return this.#link.state === 'ready'
  }

  get state(): LinkState {
    return this.#link.state
  }

  /** 配置了 `onPermission`（`run()` 据此决定无策略时是否自动 deny）。 */
  get hasPermissionPolicy(): boolean {
    return this.#policy !== undefined
  }

  /** 等节点上线。超时抛 `NodeOffline`；握手被拒抛 `NodeRejected`。 */
  waitOnline(timeoutMs = 30_000): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const check = (): boolean => {
        const state = this.#link.state
        if (state === 'ready') {
          resolve()
          return true
        }
        if (state === 'noise_failed') {
          reject(this.#link.failure ?? new NodeRejected())
          return true
        }
        if (state === 'closed') {
          reject(new NodeOffline(this.#name ?? this.id, 'the client was closed'))
          return true
        }
        return false
      }
      if (check()) return
      const off = this.#link.on('state', () => {
        if (check()) done()
      })
      const timer =
        timeoutMs > 0
          ? setTimeout(() => {
              done()
              reject(
                new NodeOffline(this.#name ?? this.id, `node still offline after ${timeoutMs}ms`),
              )
            }, timeoutMs)
          : undefined
      const done = () => {
        off()
        if (timer !== undefined) clearTimeout(timer)
      }
    })
  }

  // ---------- RPC ----------

  /** 类型化的 RPC 调用。错误按稳定码映射成 `AgentUnavailable` / `SettingsInvalid` / … */
  async call<M extends RequestMethodName>(
    method: M,
    params: RequestMethods[M]['params'],
    options?: RequestOptions,
  ): Promise<RequestMethods[M]['result']> {
    return this.rpc<RequestMethods[M]['result']>(method, params, options)
  }

  /** 未类型化的 RPC（调用 SDK 还不认识的方法）。 */
  async rpc<T = unknown>(method: string, params?: unknown, options?: RequestOptions): Promise<T> {
    const fatal = this.#client.fatalError
    if (fatal) throw fatal
    try {
      return await this.#link.rpc.request<T>(method, params ?? {}, options)
    } catch (err) {
      throw mapRpcError(err, PROTOCOL_VERSION)
    }
  }

  /** `node.info`。首次成功时检查协议版本、记下节点名。 */
  async info(options: { refresh?: boolean } = {}): Promise<NodeInfo> {
    if (this.#info && !options.refresh) return this.#info
    const info = await this.call('node.info', {})
    if (Math.abs(info.protocol_version - PROTOCOL_VERSION) > 1) {
      throw new ProtocolMismatch(PROTOCOL_VERSION, info.protocol_version)
    }
    this.#info = info
    if (info.name && info.name !== this.#name) {
      this.#name = info.name
      await this.#client.rememberNodeName(this.id, info.name)
    }
    return info
  }

  readonly agents = {
    /** `agent.list`，带 etag 缓存；`refresh` 强制重探。 */
    list: async (options: { refresh?: boolean } = {}): Promise<AgentInfo[]> => {
      const useEtag =
        !options.refresh && this.#agents !== undefined && this.#agentsEtag !== undefined
      const result = await this.call('agent.list', {
        ...(options.refresh ? { refresh: true } : {}),
        ...(useEtag ? { if_none_match: this.#agentsEtag } : {}),
      })
      if (result.not_modified && this.#agents) return this.#agents
      this.#agents = result.agents
      this.#agentsEtag = result.etag
      return result.agents
    },
    diagnose: (id: string) => this.call('agent.diagnose', { id }),
  }

  readonly projects = {
    list: async (): Promise<Project[]> => (await this.call('workspace.list', {})).projects,
    create: (name: string, options: { gitInit?: boolean } = {}) =>
      this.call('workspace.create', { name, git_init: options.gitInit ?? false }),
    /** 克隆是长任务：返回 `Job`，`job.progress()` 看进度，`job.wait()` 等结束。 */
    clone: async (url: string, options: { name?: string } = {}): Promise<Job> => {
      const started = await this.call('workspace.clone', {
        url,
        ...(options.name !== undefined ? { name: options.name } : {}),
      })
      return this.#jobHandle(started.job_id)
    },
    /**
     * 关联本机已有目录为项目（`workspace.link`）：不移动、不复制。`path` 是 daemon 所在机器上的绝对路径
     * 或 `~/…`，必须在 daemon 的 `workspace.link_roots` 之内；`name` 缺省取目录名。
     * 越界 → `forbidden`，重名或已关联 → `exists`。
     */
    link: (path: string, options: { name?: string } = {}): Promise<Project> =>
      this.call('workspace.link', {
        path,
        ...(options.name !== undefined ? { name: options.name } : {}),
      }),
    /** 解除关联（`workspace.unlink`）：只从项目列表去掉，不动目录。对根目录下的项目 → `invalid`，有运行中的会话 → `busy`。 */
    unlink: (name: string) => this.call('workspace.unlink', { name }),
    /**
     * 列本机目录供选择（`workspace.browse`）：只列 `link_roots` 之内的子目录。`path` 缺省 = `roots` 的第一项；
     * `showHidden` 也列 `.` 开头的目录。`link_roots` 为空 → `forbidden`。
     */
    browse: (
      options: { path?: string; showHidden?: boolean } = {},
    ): Promise<WorkspaceBrowseResult> =>
      this.call('workspace.browse', {
        ...(options.path !== undefined ? { path: options.path } : {}),
        ...(options.showHidden !== undefined ? { show_hidden: options.showHidden } : {}),
      }),
    /**
     * `workspace.branches`：列项目的分支（给「基于哪个分支」下拉框用）。`default_base` 是新会话的默认 base，
     * `local` / `remote` 按最近提交时间倒序（各最多 200 个，`truncated` 为 true 表示被截断）。项目目录不是 git 仓库 → 报错。
     */
    branches: (project: string): Promise<WorkspaceBranchesResult> =>
      this.call('workspace.branches', { project }),
    rename: (from: string, to: string) => this.call('workspace.rename', { from, to }),
    trash: (name: string) => this.call('workspace.trash', { name }),
    trashList: async (): Promise<TrashEntry[]> =>
      (await this.call('workspace.trash_list', {})).entries,
    restore: (id: string) => this.call('workspace.restore', { id }),
    purge: (id: string) => this.call('workspace.purge', { id }),
  }

  readonly worktrees = {
    /**
     * `workspace.worktrees`：anycon 建的全部 worktree（含已保留的、会话已删除的），带状态、未提交文件数、
     * 自己的提交数、磁盘占用。`project` 缺省 = 全部项目。
     */
    list: async (options: { project?: string } = {}): Promise<WorktreeInfo[]> =>
      (
        await this.call(
          'workspace.worktrees',
          options.project !== undefined ? { project: options.project } : {},
        )
      ).worktrees,
    /**
     * `workspace.remove_worktree`：删掉一个已保留或没有会话的 worktree。所属会话还没归档 → `busy`；
     * 有未提交改动而没 `force` → `invalid`（`data.reason = 'uncommitted_changes'`）。`deleteBranch` 同时删分支
     * （`git branch -D`，没合并的提交会丢）。
     */
    remove: (id: string, options: { force?: boolean; deleteBranch?: boolean } = {}) =>
      this.call('workspace.remove_worktree', {
        id,
        ...(options.force !== undefined ? { force: options.force } : {}),
        ...(options.deleteBranch !== undefined ? { delete_branch: options.deleteBranch } : {}),
      }),
  }

  readonly sessions = {
    list: async (params: SessionListParams = {}): Promise<SessionSummary[]> =>
      (await this.call('session.list', params)).sessions,
    /** 拿到已有会话的句柄。 */
    get: async (id: string): Promise<Session> =>
      this.#handle(await this.call('session.get', { session_id: id })),
    create: async (params: CreateSessionParams): Promise<Session> => {
      const attachments = params.attachments ?? []
      // 带 prompt 或附件时 daemon 发第一条消息，要求 message_id（幂等键）；空 prompt 当作没有（daemon 也这样处理）。
      const hasMessage = Boolean(params.prompt) || attachments.length > 0
      const summary = await this.call('session.create', {
        agent: params.agent,
        ...(params.project !== undefined ? { project: params.project } : {}),
        settings: params.settings ?? {},
        ...(params.workspace !== undefined ? { workspace: params.workspace } : {}),
        ...(params.prompt ? { prompt: params.prompt } : {}),
        ...(attachments.length > 0 ? { attachments: [...attachments] } : {}),
        ...(hasMessage ? { message_id: params.messageId || randomId() } : {}),
      })
      const session = this.#handle(summary)
      if (params.onPermission) session.policy = params.onPermission
      return session
    },
    search: async (
      query: string,
      options: { project?: string; limit?: number } = {},
    ): Promise<SearchHit[]> => (await this.call('session.search', { query, ...options })).hits,
  }

  // ---------- 附件（ADR-35） ----------

  /**
   * 把一张图片传到节点（`blob.put_begin` → 512 KiB 一块、最多 2 块在途的 `blob.put_chunk` → `blob.put_end`），
   * 返回 daemon 的记录（`AttachmentRef`，含宽高），原样放进 `session.send(text, { attachments })` /
   * `sessions.create({ attachments })`。同一个节点上的上传（图片和文件）排队、一个一个传。
   *
   * 只收 `image/png` / `image/jpeg` / `image/gif` / `image/webp`：`mime` 缺省而文件头认不出，或者给的不是这四种，
   * 抛 `AnyconError('unsupported_image')`，什么都不发。别的文件用 `uploadFile()`。
   * SDK 不缩放、不重编码：调用方自己把图控制在长边 ≤ 2000 px、单张 ≤ 约 1.5 MiB（daemon 每个文件默认上限 10 MiB）。
   * 被拒绝时抛 `AttachmentError`（`reason`：`too_large` / `quota` / `disk_full` / `hash_mismatch` / `mime_mismatch` / …）
   * 或 `RpcError`（`busy`：这台设备同时进行的上传太多）。没传完的上传会被 `blob.discard`。
   * 没上传完就不要了用 `signal`；传完了但不发了用 `discardAttachment()`（不丢的话 daemon 24 小时后回收）。
   */
  async uploadImage(bytes: Uint8Array, options: UploadImageOptions): Promise<AttachmentRef> {
    const mime = resolveImageMime(bytes, options)
    return this.#queueUpload(options.signal, () => uploadBlob(this, bytes, mime, options))
  }

  /**
   * 把任意文件传到节点，和 `uploadImage()` 走同一套分块上传、同一个队列，返回 `AttachmentRef`（四种图片才有宽高）。
   *
   * `mime` 缺省时：文件头是四种图片之一就用它，否则按 `name` 的扩展名猜（`pdf`、`txt`、`md`、`csv`、`json`、`zip`、
   * `py`、`rs`……），猜不出用 `application/octet-stream`。给了 `mime` 就必须是成形的 `type/subtype`（不带参数，
   * 发出去前转小写），否则抛 `AnyconError('invalid_mime')`，什么都不发；真正的校验在 daemon。
   * 四种图片作为图片交给模型；其它文件以 daemon 所在机器上的路径交给 agent，由 agent 用自己的工具读
   * （Claude Code 可能先请求读取权限）。agent 收不收看 `acceptsAttachment(agent, ref.mime)`；不收的话
   * `send()` / `sessions.create()` 得到 `RpcError`（`unsupported`）。拒绝、中止、清理同 `uploadImage()`。
   */
  async uploadFile(bytes: Uint8Array, options: UploadFileOptions): Promise<AttachmentRef> {
    const mime = resolveFileMime(bytes, options)
    return this.#queueUpload(options.signal, () => uploadBlob(this, bytes, mime, options))
  }

  /** 同一个节点上的上传排队：前一个结束（成功或失败）才开始下一个；排队时被中止的什么都不发。 */
  async #queueUpload<T>(signal: AbortSignal | undefined, run: () => Promise<T>): Promise<T> {
    const prev = this.#uploadTail
    let release!: () => void
    this.#uploadTail = new Promise<void>((resolve) => (release = resolve))
    try {
      await raceAbort(prev, signal)
      return await run()
    } finally {
      // 排队时被中止：等前一个传完再放行下一个，保证同时只有一个在传
      void prev.then(release)
    }
  }

  /**
   * 下载一个附件（图片或任意文件，`blob.get` 按 512 KiB 分页拉）。任何已配对设备都能读任何 blob；不存在 / 文件不见了 →
   * `RpcError`（`not_found`，文件不见了时是 `AttachmentError`，`reason = 'blob'`）。
   */
  downloadAttachment(
    attachment: AttachmentRef | string,
    options: DownloadOptions = {},
  ): Promise<DownloadedAttachment> {
    const blobId = typeof attachment === 'string' ? attachment : attachment.blob_id
    return downloadAttachment(this, blobId, options)
  }

  /**
   * 丢掉一个已经上传、但还没被消息引用的附件（`blob.discard`，幂等：不存在也成功）。已被消息引用 →
   * `AttachmentError`（`invalid`，`reason = 'in_use'`）：它随会话删除。
   */
  async discardAttachment(attachment: AttachmentRef | string): Promise<void> {
    const blobId = typeof attachment === 'string' ? attachment : attachment.blob_id
    await this.call('blob.discard', { blob_id: blobId })
  }

  readonly permissions = {
    /** 未决的权限请求（重连后先拉一次）。 */
    list: async (sessionId?: string): Promise<PermissionRequest[]> =>
      (await this.call('permission.list', sessionId ? { session_id: sessionId } : {})).requests,
  }

  /** 关闭这个节点的视图。由 `connect()` 创建的 Node 会同时关闭底层 hub 连接。 */
  async close(): Promise<void> {
    await this.#client.release(this, this.#ownsClient)
  }

  /** @internal Client 丢弃这个 Node 时调用。 */
  markClosed(): void {
    for (const timer of this.#permTimers.values()) clearTimeout(timer)
    this.#permTimers.clear()
    this.emit('closed')
    this.#sessions.clear()
  }

  /** @internal */
  markOwnsClient(): void {
    this.#ownsClient = true
  }

  /** @internal Session.delete 之后。 */
  forgetSession(id: string): void {
    this.#sessions.delete(id)
  }

  // ---------- 权限 ----------

  /** @internal 来自 `permission.request` 通知或会话事件流里的 `permission` 事件。 */
  observePermission(request: PermissionRequest): void {
    if (this.#permSeen.has(request.request_id)) return
    const session = this.#sessions.get(request.session_id)
    // 只管自己正在用的会话：不替别的设备（比如用户在 Web 上开的会话）做决定。
    if (!session?.active) return
    this.#permSeen.add(request.request_id)
    if (this.#permSeen.size > 2000) {
      const oldest = this.#permSeen.values().next().value
      if (oldest !== undefined) this.#permSeen.delete(oldest)
    }
    void this.#decide(request, session)
  }

  async #decide(request: PermissionRequest, session: Session): Promise<void> {
    const policy = session.policy ?? this.#policy
    if (policy) {
      try {
        const decision = await policy(request, { session, node: this })
        const params = decisionToParams(request, decision)
        if (params) {
          await this.call('permission.respond', params)
          return
        }
        // 策略给了决定却无法执行（比如 'allow' 但没有 allow_once）：不悄悄升级，告诉调用方。
        if (decision !== undefined && decision !== null) {
          this.emit(
            'permission_error',
            new AnyconError(
              'no_matching_option',
              `the permission policy decided on request ${request.request_id}, but it has no matching option (an 'allow' needs an allow_once option; nothing is upgraded to allow_always)`,
            ),
            request,
          )
        }
      } catch (err) {
        // 策略出错：落到超时兜底，并告诉调用方。已被别的设备答复（already_resolved）不算错。
        if (!(err instanceof RpcError && err.code === 'already_resolved')) {
          this.emit('permission_error', err, request)
        }
      }
    }
    this.#armPermissionTimeout(request)
  }

  #armPermissionTimeout(request: PermissionRequest): void {
    if (this.#permissionTimeoutMs <= 0) return
    const timer = setTimeout(() => {
      this.#permTimers.delete(request.request_id)
      const params =
        decisionToParams(request, { deny: 'timeout' }) ?? decisionToParams(request, 'deny')
      if (params) void this.call('permission.respond', params).catch(() => undefined)
    }, this.#permissionTimeoutMs)
    ;(timer as { unref?: () => void }).unref?.()
    this.#permTimers.set(request.request_id, timer)
  }

  #onResolved(resolved: PermissionResolved): void {
    const timer = this.#permTimers.get(resolved.request_id)
    if (timer !== undefined) clearTimeout(timer)
    this.#permTimers.delete(resolved.request_id)
    this.emit('permission_resolved', resolved)
  }

  // ---------- 内部 ----------

  #handle(summary: SessionSummary): Session {
    const existing = this.#sessions.get(summary.id)
    if (existing) {
      existing.updateSummary(summary)
      return existing
    }
    const session = new Session(this, summary)
    this.#sessions.set(summary.id, session)
    return session
  }

  #onSessionChanged(change: SessionChanged): void {
    this.emit('session_changed', change)
    if (change.deleted) {
      this.#sessions.delete(change.session_id)
      return
    }
    const session = this.#sessions.get(change.session_id)
    if (!session) return
    session.updateSummary({
      ...session.summary,
      title: change.title,
      status: change.status,
      archived: change.archived ?? session.summary.archived ?? false,
      pending_permissions: change.pending_permissions,
      // 通知是后台任务数的完整快照；旧 daemon 不发这个字段，按 0 处理。
      background_tasks: change.background_tasks ?? 0,
      updated_at_ms: change.updated_at_ms,
      last_seq: Math.max(change.last_seq, session.summary.last_seq),
      // worktree 的状态变化（creating → active / kept / removed …）；通知里没带就保持原值。
      ...(change.worktree ? { worktree: change.worktree } : {}),
    })
  }

  async #resyncSessions(): Promise<void> {
    for (const session of Array.from(this.#sessions.values())) {
      try {
        await session.resync()
      } catch {
        // 补事件失败不影响连接；调用方下次 history() 会拿到。
      }
    }
  }

  #job(id: string): JobRecord {
    let rec = this.#jobs.get(id)
    if (!rec) {
      rec = { progress: [], done: undefined, waiters: new Set() }
      this.#jobs.set(id, rec)
      if (this.#jobs.size > 200) {
        const oldest = this.#jobs.keys().next().value
        if (oldest !== undefined) this.#jobs.delete(oldest)
      }
    }
    return rec
  }

  #wake(rec: JobRecord): void {
    for (const w of Array.from(rec.waiters)) w()
  }

  #jobHandle(id: string): Job {
    const rec = this.#job(id)
    const waitForChange = () =>
      new Promise<void>((resolve) => {
        const wake = () => {
          rec.waiters.delete(wake)
          resolve()
        }
        rec.waiters.add(wake)
      })
    return {
      id,
      progress: () => ({
        async *[Symbol.asyncIterator]() {
          let at = 0
          for (;;) {
            while (at < rec.progress.length) yield rec.progress[at++]!
            if (rec.done) return
            await waitForChange()
          }
        },
      }),
      wait: async () => {
        while (!rec.done) await waitForChange()
        if (!rec.done.ok) {
          throw new AnyconError('job_failed', rec.done.error ?? `job ${id} failed`)
        }
        return rec.done
      },
    }
  }
}
