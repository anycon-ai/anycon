import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { ArrowDown, Loader2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { PermissionRespondParams } from '@/protocol'
import type { PendingMessage } from '@/stores/drafts'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { errorSeverity } from '../errors'
import { latestSyncBlock, type ActivityInfo, type Block, type UserBlock } from '../reducer'
import { toolTree } from '../subagents'
import { AssistantMessage, ErrorView, PendingBubble, PlanCard, SystemLine, ThinkingView, ToolNode, TurnDivider, TurnUsage, UserBubble, type ConflictActions } from './Messages'
import { ActivityLine, NoticeLine } from './Notices'
import { PermissionCard, type SettledReason } from './PermissionCard'
import { TaskLiveContext, TaskPanel } from './Tasks'

/** 超过这个行数才启用虚拟化；更短的会话全部渲染（更稳，E2E 也更好查）。 */
export const VIRTUALIZE_THRESHOLD = 80
const FOLLOW_THRESHOLD_PX = 96

export interface MessageListProps {
  /** 拉历史里图片附件用的节点（`blob.get`）；不给就只能显示缓存里有的。 */
  nodeId?: string
  blocks: Block[]
  pending: PendingMessage[]
  hasMoreOlder: boolean
  loadingOlder: boolean
  onLoadOlder: () => void
  deviceLabel: (deviceId: string) => string
  respondingIds: ReadonlySet<string>
  /** 答复时 daemon 回了 `not_found` 的请求（id → 原因）：卡片显示说明、不再给按钮。 */
  settledIds?: ReadonlyMap<string, SettledReason>
  onRespond: (params: PermissionRespondParams) => void
  onRetryPending: (messageId: string) => void
  onEditPending: (messageId: string) => void
  onDeletePending: (messageId: string) => void
  modelLabel: (id: string) => string
  modeLabel: (id: string) => string
  /** 权限模式 id 是不是危险模式（mode_change 请求据此醒目警示）。 */
  isDangerousMode?: (id: string) => boolean
  onRetryTurn?: () => void
  /**
   * 「可能没有送达」的用户消息上的「重发」：用新的 `message_id` 再发一遍同样的内容和附件引用。不给（离线、已归档）就只显示提示。
   * 不会自动重发，由用户决定。
   */
  onResendMessage?: (block: UserBlock) => void
  /** 已经重发过的原消息 id（按钮换成「已重发」，免得点两次）。 */
  resentMessageIds?: ReadonlySet<string>
  /** 消息流最底下的临时状态（重试 API、压缩上下文）。调用方只在会话还在跑时给。 */
  activity?: ActivityInfo | null
  /** 会话里的后台任务还可能有进展（主 agent 在跑，或 daemon 报还有后台任务）。不可能时，停在「运行中」的任务显示成「已停止」。默认 true。 */
  tasksLive?: boolean
  /** 滚动到某个权限请求（信息面板里点「待审批」）。 */
  scrollToRequest?: string | null
  /**
   * 搜索命中：滚到这个 seq 所在的那条消息并短暂高亮（命中的 seq 可以落在一个块的中间，见 `focusRowIndex`）。
   * 会话已经加载到那里了才给；处理完（不管找没找到）调 `onFocused`，调用方据此清掉它。
   */
  focusSeq?: number | null
  onFocused?: () => void
  /** 正在为 `focusSeq` 往前加载历史：盖一层转圈，别让人以为卡住了。 */
  finding?: boolean
  /**
   * 冲突那条 `git` 事件上的按钮（ADR-34）。只给最近一次同步的冲突，而且 `merging` 不是 false（合并确实还停在中途，
   * 或者还不知道）时才显示。
   */
  git?: { merging?: boolean; busy?: boolean; onResolve: () => void; onAbort: () => void }
}

/** `line`：一行灰字的块（思考、工具、系统提示、答复过的审批）。连着的几行之间只留很小的间距，像一张列表。`seq`：产生这一行的块的第一个事件的 seq。 */
type Row = { key: string; node: ReactNode; line?: boolean; seq?: number }

/** 定位后高亮多久（和 index.css 里 `.search-flash` 的动画时长一致）。 */
const FLASH_MS = 2200

/**
 * 搜索命中的 seq 落在哪一行：起点不晚于它的最后一个块。`session.search` 给的 seq 是被索引那段文字开始处的事件：
 * 用户消息就是那条消息的 seq，assistant 文字是一个块里某一段的第一个 `text_delta`（长文字每 32 KiB 切一段索引），
 * 所以不一定恰好等于某个块的起点。找不到（它比已加载的最早的块还早）返回 -1。
 */
export function focusRowIndex(rows: ReadonlyArray<{ seq?: number }>, seq: number): number {
  let best = -1
  rows.forEach((r, i) => {
    if (r.seq !== undefined && r.seq <= seq && (best < 0 || r.seq >= (rows[best]!.seq ?? 0))) best = i
  })
  return best
}

/** 这一行下面的间距：后面紧跟着另一行灰字时收紧，其余用消息间距。 */
function rowGap(rows: Row[], i: number): string {
  return rows[i]?.line && rows[i + 1]?.line ? 'pb-1' : 'pb-[var(--msg-gap)]'
}

/** 「重试」只给最后一个 recoverable 错误，且它之后还没有新的用户消息（更早的错误已经过去了）。 */
export function retryableErrorKey(blocks: Block[]): string | null {
  for (let i = blocks.length - 1; i >= 0; i--) {
    const b = blocks[i]!
    if (b.type === 'user') return null
    if (b.type === 'error') {
      // 重连中 / 被拒绝的请求只是提示，不是「这一轮失败了」：不挡住更早的错误，也自己不给重试
      if (errorSeverity(b.code) !== 'error') continue
      return b.recoverable ? b.key : null
    }
  }
  return null
}

function buildRows(p: MessageListProps): Row[] {
  const rows: Row[] = []
  const retryKey = p.onRetryTurn ? retryableErrorKey(p.blocks) : null
  const latestSync = p.git ? latestSyncBlock(p.blocks) : null
  // 子 agent 内部的调用收进它的块里，不单独成行（几个子 agent 并行时不再交错成一串）
  const tree = toolTree(p.blocks)
  // 这一轮 agent 说的全部文字：turn_done 那一行的「复制」用
  let turnText: string[] = []
  for (const b of p.blocks) {
    const start = rows.length
    switch (b.type) {
      case 'user':
        turnText = []
        rows.push({
          key: b.key,
          node: <UserBubble block={b} nodeId={p.nodeId} onResend={p.onResendMessage ? () => p.onResendMessage?.(b) : undefined} resent={p.resentMessageIds?.has(b.messageId)} />,
        })
        break
      case 'turn_start':
        turnText = []
        rows.push({ key: b.key, node: <TurnDivider turnId={b.turnId} ts={b.ts} agentStarted={b.agentStarted} /> })
        break
      case 'thinking':
        rows.push({ key: b.key, node: <ThinkingView block={b} />, line: true })
        break
      case 'assistant':
        if (b.text.trim()) turnText.push(b.text.trim())
        rows.push({ key: b.key, node: <AssistantMessage block={b} /> })
        break
      case 'tool':
        if (tree.nested.has(b.key)) break
        rows.push({ key: b.key, node: <ToolNode block={b} tree={tree} />, line: true })
        break
      case 'task':
        rows.push({ key: b.key, node: <TaskPanel task={b.task} /> })
        break
      case 'notice':
        // 命令输出是一个块，其余是一行灰字 / 黄字
        rows.push({ key: b.key, node: <NoticeLine block={b} modelLabel={p.modelLabel} />, line: b.code !== 'command_output' })
        break
      case 'plan':
        rows.push({ key: b.key, node: <PlanCard block={b} /> })
        break
      case 'permission':
        rows.push({
          key: b.key,
          node: (
            <PermissionCard
              block={b}
              deviceLabel={p.deviceLabel}
              onRespond={p.onRespond}
              busy={p.respondingIds.has(b.requestId)}
              settled={p.settledIds?.get(b.requestId)}
              targetMode={b.kind === 'mode_change' && b.detail.mode ? { label: p.modeLabel(b.detail.mode), dangerous: p.isDangerousMode?.(b.detail.mode) ?? false } : undefined}
            />
          ),
          // 带着问题和回答的不是一行字，和消息一样留正常间距
          line: (!!b.resolved && !(b.kind === 'question' && b.resolved.answers)) || !!p.settledIds?.get(b.requestId),
        })
        break
      case 'turn_done':
        rows.push({ key: b.key, node: <TurnUsage block={b} copyText={turnText.length > 0 ? turnText.join('\n\n') : undefined} /> })
        turnText = []
        break
      case 'error':
        rows.push({ key: b.key, node: <ErrorView block={b} onRetry={b.key === retryKey ? p.onRetryTurn : undefined} /> })
        break
      case 'system': {
        const git = p.git
        const conflict: ConflictActions | undefined =
          git && b.kind === 'git' && b === latestSync && b.action === 'sync_conflicts' && git.merging !== false
            ? { busy: git.busy, onResolve: git.onResolve, onAbort: git.onAbort }
            : undefined
        rows.push({ key: b.key, node: <SystemLine block={b} modelLabel={p.modelLabel} modeLabel={p.modeLabel} conflict={conflict} />, line: !conflict })
        break
      }
    }
    for (let i = start; i < rows.length; i++) rows[i]!.seq = b.seq
  }
  if (p.activity) rows.push({ key: 'activity', node: <ActivityLine activity={p.activity} />, line: true })
  for (const m of p.pending) {
    rows.push({
      key: `pending-${m.messageId}`,
      node: <PendingBubble msg={m} nodeId={p.nodeId} onRetry={() => p.onRetryPending(m.messageId)} onEdit={() => p.onEditPending(m.messageId)} onDelete={() => p.onDeletePending(m.messageId)} />,
    })
  }
  return rows
}

/** 消息流：自动跟随底部、向上翻页拉历史、「↓ 新消息」浮标；超长会话虚拟化。 */
export function MessageList(props: MessageListProps) {
  return (
    <TaskLiveContext.Provider value={props.tasksLive ?? true}>
      <MessageListView {...props} />
    </TaskLiveContext.Provider>
  )
}

function MessageListView(props: MessageListProps) {
  const { t } = useTranslation()
  const rows = buildRows(props)
  const scrollRef = useRef<HTMLDivElement>(null)
  const [following, setFollowing] = useState(true)
  const [unseen, setUnseen] = useState(false)
  const followingRef = useRef(true)
  const lastCount = useRef(0)
  const prevHeight = useRef<number | null>(null)
  const virtual = rows.length > VIRTUALIZE_THRESHOLD
  const rowsRef = useRef(rows)
  rowsRef.current = rows
  /** 刚被定位到的那一行（key），高亮一会儿。 */
  const [flash, setFlash] = useState<string | null>(null)

  const virtualizer = useVirtualizer({
    count: virtual ? rows.length : 0,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 96,
    overscan: 10,
    getItemKey: (i) => rows[i]?.key ?? i,
  })

  const scrollToBottom = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    if (virtual) virtualizer.scrollToIndex(rows.length - 1, { align: 'end' })
    el.scrollTop = el.scrollHeight
  }, [virtual, virtualizer, rows.length])

  const onScroll = (): void => {
    const el = scrollRef.current
    if (!el) return
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < FOLLOW_THRESHOLD_PX
    followingRef.current = atBottom
    setFollowing(atBottom)
    if (atBottom) setUnseen(false)
    if (el.scrollTop < 120 && props.hasMoreOlder && !props.loadingOlder) {
      prevHeight.current = el.scrollHeight
      props.onLoadOlder()
    }
  }

  // 新内容到达：跟随则贴底，否则亮「新消息」浮标；向上翻页后保持视口位置。
  useLayoutEffect(() => {
    const el = scrollRef.current
    if (!el) return
    if (prevHeight.current !== null && !followingRef.current) {
      el.scrollTop += el.scrollHeight - prevHeight.current
      prevHeight.current = null
    } else if (followingRef.current) {
      scrollToBottom()
    } else if (rows.length > lastCount.current) {
      setUnseen(true)
    }
    lastCount.current = rows.length
  }, [props.blocks, props.pending, props.hasMoreOlder, rows.length, scrollToBottom])

  // 内容高度变化（图片 / 代码高亮 / 流式增长）时，跟随状态下继续贴底
  useEffect(() => {
    const el = scrollRef.current?.firstElementChild
    if (!el || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(() => {
      if (followingRef.current) scrollToBottom()
    })
    ro.observe(el)
    return () => ro.disconnect()
  }, [scrollToBottom])

  // 搜索命中的定位：不再跟随底部，把那一行滚到中间并高亮。虚拟化时目标行可能还没渲染，交给 virtualizer 按下标滚
  const { focusSeq, onFocused } = props
  useEffect(() => {
    if (focusSeq == null) return
    const rs = rowsRef.current
    const idx = focusRowIndex(rs, focusSeq)
    onFocused?.()
    if (idx < 0) return
    followingRef.current = false
    setFollowing(false)
    if (virtual) virtualizer.scrollToIndex(idx, { align: 'center' })
    else scrollRef.current?.querySelector(`[data-index="${idx}"]`)?.scrollIntoView({ block: 'center' })
    setFlash(rs[idx]!.key)
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 只在要定位的 seq 变化时跑一次
  }, [focusSeq])
  useEffect(() => {
    if (flash === null) return
    const timer = setTimeout(() => setFlash(null), FLASH_MS)
    return () => clearTimeout(timer)
  }, [flash])

  useEffect(() => {
    if (!props.scrollToRequest) return
    const el = scrollRef.current?.querySelector(`[data-request="${CSS.escape(props.scrollToRequest)}"]`)
    el?.scrollIntoView({ block: 'center', behavior: 'smooth' })
  }, [props.scrollToRequest])

  return (
    <div className="relative min-h-0 flex-1">
      <div ref={scrollRef} onScroll={onScroll} className="scroll-thin h-full overflow-y-auto" data-testid="message-list">
        <div className="chat-column py-4" style={virtual ? { height: virtualizer.getTotalSize(), position: 'relative' } : undefined}>
          {props.hasMoreOlder ? (
            <div className="flex justify-center pb-3">
              <Button variant="ghost" size="sm" onClick={props.onLoadOlder} disabled={props.loadingOlder} data-testid="load-older">
                {props.loadingOlder ? <Loader2 className="size-3.5 animate-spin" /> : null}
                {t('session.loadOlder')}
              </Button>
            </div>
          ) : null}
          {virtual
            ? virtualizer.getVirtualItems().map((v) => {
                const row = rows[v.index]
                if (!row) return null
                // 绝对定位不吃父级 padding，左右偏移用 chat-column 同一个 --chat-pad，否则内容会贴边、也对不上输入框
                return (
                  <div key={row.key} data-index={v.index} ref={virtualizer.measureElement} className={cn('absolute left-[var(--chat-pad)] right-[var(--chat-pad)] top-0', rowGap(rows, v.index), flash === row.key && 'search-flash')} style={{ transform: `translateY(${v.start}px)` }}>
                    {row.node}
                  </div>
                )
              })
            : rows.map((row, i) => (
                <div key={row.key} data-index={i} className={cn(rowGap(rows, i), flash === row.key && 'search-flash')}>
                  {row.node}
                </div>
              ))}
        </div>
      </div>
      {props.finding ? (
        <div className="absolute inset-0 z-10 flex items-center justify-center bg-bg/60" role="status" aria-busy="true" data-testid="message-list-finding">
          <span className="inline-flex items-center gap-2 rounded-md bg-surface px-3 py-2 text-sm text-fg-muted shadow-pop">
            <Loader2 className="size-4 animate-spin" aria-hidden />
            {t('session.findingMessage')}
          </span>
        </div>
      ) : null}
      {!following && unseen ? (
        // 只有一个圆形向下箭头，文字放进 aria-label / title
        <Button variant="outline" size="icon" className="absolute bottom-3 left-1/2 -translate-x-1/2 rounded-full bg-surface shadow-pop" aria-label={t('session.newMessages')} title={t('session.newMessages')} onClick={() => { followingRef.current = true; setFollowing(true); setUnseen(false); scrollToBottom() }} data-testid="new-messages">
          <ArrowDown className="size-4" />
        </Button>
      ) : null}
    </div>
  )
}
