import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { ArrowDown, Loader2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { PermissionRespondParams } from '@/protocol'
import type { PendingMessage } from '@/stores/drafts'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { errorSeverity } from '../errors'
import { latestSyncBlock, type Block } from '../reducer'
import { AssistantMessage, ErrorView, PendingBubble, PlanCard, SystemLine, ThinkingView, ToolView, TurnDivider, TurnUsage, UserBubble, type ConflictActions } from './Messages'
import { PermissionCard, type SettledReason } from './PermissionCard'

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
  /** 滚动到某个权限请求（信息面板里点「待审批」）。 */
  scrollToRequest?: string | null
  /**
   * 冲突那条 `git` 事件上的按钮（ADR-34）。只给最近一次同步的冲突，而且 `merging` 不是 false（合并确实还停在中途，
   * 或者还不知道）时才显示。
   */
  git?: { merging?: boolean; busy?: boolean; onResolve: () => void; onAbort: () => void }
}

/** `line`：一行灰字的块（思考、工具、系统提示、答复过的审批）。连着的几行之间只留很小的间距，像一张列表。 */
type Row = { key: string; node: ReactNode; line?: boolean }

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
  // 这一轮 agent 说的全部文字：turn_done 那一行的「复制」用
  let turnText: string[] = []
  for (const b of p.blocks) {
    switch (b.type) {
      case 'user':
        turnText = []
        rows.push({ key: b.key, node: <UserBubble block={b} nodeId={p.nodeId} /> })
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
        rows.push({ key: b.key, node: <ToolView block={b} />, line: true })
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
          line: !!b.resolved || !!p.settledIds?.get(b.requestId),
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
  }
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
  const { t } = useTranslation()
  const rows = buildRows(props)
  const scrollRef = useRef<HTMLDivElement>(null)
  const [following, setFollowing] = useState(true)
  const [unseen, setUnseen] = useState(false)
  const followingRef = useRef(true)
  const lastCount = useRef(0)
  const prevHeight = useRef<number | null>(null)
  const virtual = rows.length > VIRTUALIZE_THRESHOLD

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

  useEffect(() => {
    if (!props.scrollToRequest) return
    const el = scrollRef.current?.querySelector(`[data-request="${CSS.escape(props.scrollToRequest)}"]`)
    el?.scrollIntoView({ block: 'center', behavior: 'smooth' })
  }, [props.scrollToRequest])

  return (
    <div className="relative min-h-0 flex-1">
      <div ref={scrollRef} onScroll={onScroll} className="scroll-thin h-full overflow-y-auto" data-testid="message-list">
        <div className="mx-auto w-full max-w-3xl px-3 py-4 md:px-4" style={virtual ? { height: virtualizer.getTotalSize(), position: 'relative' } : undefined}>
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
                // 绝对定位不吃父级 padding，左右偏移要和外层 px-3 / md:px-4 一致，否则窄屏时内容贴边
                return (
                  <div key={row.key} data-index={v.index} ref={virtualizer.measureElement} className={cn('absolute left-3 right-3 top-0 md:left-4 md:right-4', rowGap(rows, v.index))} style={{ transform: `translateY(${v.start}px)` }}>
                    {row.node}
                  </div>
                )
              })
            : rows.map((row, i) => (
                <div key={row.key} className={rowGap(rows, i)}>
                  {row.node}
                </div>
              ))}
        </div>
      </div>
      {!following && unseen ? (
        // 只有一个圆形向下箭头，文字放进 aria-label / title
        <Button variant="outline" size="icon" className="absolute bottom-3 left-1/2 -translate-x-1/2 rounded-full bg-surface shadow-pop" aria-label={t('session.newMessages')} title={t('session.newMessages')} onClick={() => { followingRef.current = true; setFollowing(true); setUnseen(false); scrollToBottom() }} data-testid="new-messages">
          <ArrowDown className="size-4" />
        </Button>
      ) : null}
    </div>
  )
}
