import { lazy, Suspense, useEffect, useState, type ReactNode } from 'react'
import { AlertTriangle, Check, ChevronRight, CircleDashed, Clock, Copy, ListChecks, Loader2, Pencil, RefreshCw, RotateCw, Trash2 } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { ToolResultDetail } from '@/protocol'
import { Markdown } from '@/components/Markdown'
import { copyText } from '@/lib/clipboard'
import { compactNumber, formatCost, formatDuration } from '@/lib/format'
import type { PendingMessage } from '@/stores/drafts'
import { usePrefs } from '@/stores/prefs'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Chip, CopyButton } from '@/ui/Misc'
import { errorSeverity, isExplainedError } from '../errors'
import { gitEventText } from '@/features/worktree/eventText'
import { describeAttachmentFailure } from '@/features/attachments/errors'
import { MessageAttachments } from '@/features/attachments/ui/MessageAttachments'
import type { AssistantBlock, ErrorBlock, GitSystemBlock, PlanBlock, SystemBlock, ThinkingBlock, ToolBlock, TurnDoneBlock, UserBlock } from '../reducer'
import { diffStats, splitLines, syntheticDiff, toolCommand, toolDescription, toolFileChanges, toolLabel, toolOutputText, toolPath, toolQuery, toolUrl } from '../text'

const DiffView = lazy(() => import('./DiffView').then((m) => ({ default: m.DiffView })))

// ---- 用户消息 ----

export function UserBubble({ block, nodeId }: { block: UserBlock; nodeId?: string }) {
  return (
    <div className="flex flex-col items-end gap-1.5" data-testid="message-user">
      <MessageAttachments nodeId={nodeId} attachments={block.attachments} />
      {block.content || block.attachments.length === 0 ? (
        <div className="max-w-[min(85%,42rem)] rounded-2xl bg-bg-muted px-4 py-2.5 text-fg">
          <p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{block.content}</p>
        </div>
      ) : null}
    </div>
  )
}

/** 还没被事件流确认的用户消息：发送中 / 排队 / 未发送 / 失败。 */
export function PendingBubble({ msg, nodeId, onRetry, onEdit, onDelete }: { msg: PendingMessage; nodeId?: string; onRetry: () => void; onEdit: () => void; onDelete: () => void }) {
  const { t } = useTranslation()
  const unsent = msg.state === 'unsent' || msg.state === 'failed'
  return (
    <div className="flex flex-col items-end gap-1" data-testid="message-user" data-state={msg.state}>
      <MessageAttachments nodeId={nodeId} attachments={msg.attachments ?? []} />
      {msg.content || !msg.attachments?.length ? (
        <div className={cn('max-w-[min(85%,42rem)] rounded-2xl px-4 py-2.5 text-fg', unsent ? 'border border-dashed border-warning bg-warning-subtle' : 'bg-bg-muted/70')}>
          <p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{msg.content}</p>
        </div>
      ) : null}
      <div className="flex items-center gap-2 text-xs text-fg-subtle">
        {msg.state === 'sending' ? (
          <span className="inline-flex items-center gap-1">
            <Loader2 className="size-3 animate-spin" />
            {t('message.sending')}
          </span>
        ) : msg.state === 'accepted' ? (
          <span className="inline-flex items-center gap-1">
            <Check className="size-3" />
            {t('message.sent')}
          </span>
        ) : msg.state === 'queued' ? (
          <span className="inline-flex items-center gap-1 text-info" data-testid="message-queued">
            <Clock className="size-3" />
            {t('message.queued')}
          </span>
        ) : (
          <>
            <Chip tone={msg.state === 'failed' ? 'danger' : 'warning'} testId="message-unsent-badge">
              {msg.state === 'failed' ? t('message.failed') : t('message.unsent')}
            </Chip>
            {msg.error ? <span className="text-danger" data-testid="message-unsent-error" data-reason={msg.error}>{msg.error === 'archived' ? t('session.archivedHint') : (describeAttachmentFailure(msg.error, t) ?? t(`errors.rpc.${msg.error}`, { defaultValue: msg.error }))}</span> : null}
            <button type="button" className="inline-flex items-center gap-1 hover:text-fg" onClick={onRetry} data-testid="message-retry">
              <RotateCw className="size-3" />
              {t('message.retry')}
            </button>
            <button type="button" className="inline-flex items-center gap-1 hover:text-fg" onClick={onEdit}>
              <Pencil className="size-3" />
              {t('message.edit')}
            </button>
            <button type="button" className="inline-flex items-center gap-1 hover:text-danger" onClick={onDelete}>
              <Trash2 className="size-3" />
              {t('message.delete')}
            </button>
          </>
        )}
      </div>
    </div>
  )
}

// ---- assistant 文本 ----

export function AssistantMessage({ block }: { block: AssistantBlock }) {
  const { t } = useTranslation()
  const [copied, setCopied] = useState(false)
  return (
    <div className="group relative" data-testid="message-assistant" data-done={block.done}>
      <Markdown text={block.text} streaming={!block.done} />
      <button
        type="button"
        aria-label={t('message.copyMarkdown')}
        className="absolute -top-1 right-0 rounded-md border border-border bg-surface p-1.5 text-fg-subtle opacity-0 shadow-sm transition-opacity group-hover:opacity-100 focus:opacity-100"
        onClick={() => {
          copyText(block.text).then(
            () => {
              setCopied(true)
              setTimeout(() => setCopied(false), 1500)
            },
            () => toast.error(t('common.copyFailed')),
          )
        }}
      >
        {copied ? <Check className="size-3.5 text-success" /> : <Copy className="size-3.5" />}
      </button>
    </div>
  )
}

// ---- 思考 ----

export function ThinkingView({ block }: { block: ThinkingBlock }) {
  const { t } = useTranslation()
  const mode = usePrefs((s) => s.thinkingDisplay)
  const [open, setOpen] = useState(mode === 'expanded')
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (block.done) return
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [block.done])
  if (mode === 'hidden') return null
  const secs = block.done ? Math.round((block.durationMs ?? 0) / 1000) : Math.max(1, Math.round((now - block.ts) / 1000))
  const lastLines = splitLines(block.text).filter(Boolean).slice(-2).join('\n')
  const hasText = block.text.trim().length > 0
  return (
    // 一行灰字（不是卡片）：「思考了 3 秒 ›」，点开在下面缩进显示思考内容
    <div className="text-sm" data-testid="thinking-block" data-done={block.done}>
      <button type="button" className="inline-flex max-w-full items-center gap-1.5 py-0.5 text-left text-fg-subtle hover:text-fg-muted disabled:hover:text-fg-subtle" aria-expanded={open} onClick={() => setOpen((v) => !v)} disabled={!hasText}>
        <span className={cn('truncate', !block.done && 'animate-pulse-soft')}>{block.done ? t('thinking.done', { seconds: secs }) : t('thinking.running')}</span>
        {hasText ? <ChevronRight className={cn('size-3.5 shrink-0 transition-transform', open && 'rotate-90')} aria-hidden /> : null}
      </button>
      {open && hasText ? <div className="ml-1.5 mt-1 border-l-2 border-border pl-3 text-fg-muted"><Markdown text={block.text} streaming={!block.done} /></div> : null}
      {!open && !block.done && lastLines ? <p className="ml-1.5 line-clamp-2 whitespace-pre-wrap border-l-2 border-border pl-3 text-xs text-fg-subtle">{lastLines}</p> : null}
    </div>
  )
}

// ---- 工具 ----

const COLLAPSE_LINES = 20

function ToolFrame({ block, summary, summaryTitle, children, defaultOpen, meta }: { block: ToolBlock; summary: ReactNode; summaryTitle?: string; children?: ReactNode; defaultOpen?: boolean; meta?: ReactNode }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(!!defaultOpen)
  const failed = block.result?.isError
  const running = !block.result
  return (
    // 收起时是一行灰字（失败的是红字），不画卡片；展开的输出 / diff 放在下面的框里
    <div className="text-sm" data-testid="tool-block" data-category={block.category} data-state={failed ? 'error' : running ? 'running' : 'done'}>
      <button type="button" className={cn('flex max-w-full items-center gap-1.5 py-0.5 text-left', failed ? 'text-danger' : 'text-fg-subtle hover:text-fg-muted disabled:hover:text-fg-subtle')} aria-expanded={open} onClick={() => setOpen((v) => !v)} disabled={!children}>
        <span className="min-w-0 truncate font-mono text-[0.8125rem]" title={summaryTitle}>
          {summary}
        </span>
        {/* 失败不只靠红色：后面跟一个词 */}
        {failed ? <span className="shrink-0 text-xs" data-testid="tool-failed">{t('tool.failed')}</span> : null}
        {running ? <Loader2 className="size-3.5 shrink-0 animate-spin" aria-label={t('status.session.running')} /> : null}
        {meta}
        <TruncationBadges detail={block.result?.detail} />
        {children ? <ChevronRight className={cn('size-3.5 shrink-0 transition-transform', open && 'rotate-90')} aria-hidden /> : null}
      </button>
      {open && children ? <div className={cn('mt-1 overflow-hidden rounded-lg border', failed ? 'border-danger/60' : 'border-border')}>{children}</div> : null}
    </div>
  )
}

/**
 * daemon 对大输出做了截断时的小角标（spec/daemon.md § 12）：`output` 保留头尾、丢掉中间 N 字节；
 * 实时输出累计超过上限后不再转发 delta（最终 `output` 仍是头尾截断的完整输出）。
 */
export function TruncationBadges({ detail }: { detail: ToolResultDetail | null | undefined }) {
  const { t, i18n } = useTranslation()
  const dropped = detail?.output_truncated_bytes
  const live = detail?.live_output_truncated
  if (!dropped && !live) return null
  return (
    <>
      {dropped ? (
        <Chip tone="warning" testId="tool-truncated" title={t('tool.truncatedHint')}>
          {t('tool.truncated', { bytes: dropped.toLocaleString(i18n.language) })}
        </Chip>
      ) : null}
      {live ? (
        <Chip tone="warning" testId="tool-live-truncated" title={t('tool.liveTruncatedHint')}>
          {t('tool.liveTruncated')}
        </Chip>
      ) : null}
    </>
  )
}

/** 展开后的完整命令：标题行放不下的部分在这里折行显示，右上角可以复制（复制的是命令本身，不带 `$ `）。 */
function CommandLine({ command }: { command: string }) {
  const { t } = useTranslation()
  return (
    <div className="flex items-start gap-2 bg-bg px-3 py-2" data-testid="tool-command">
      <pre className="min-w-0 flex-1 font-mono text-xs leading-relaxed whitespace-pre-wrap [overflow-wrap:anywhere]">
        <span className="select-none text-fg-subtle">$ </span>
        {command}
      </pre>
      <CopyButton text={command} label={t('tool.copyCommand')} iconOnly className="-mr-1.5 -mt-0.5 shrink-0" />
    </div>
  )
}

function Terminal({ text, error, running }: { text: string; error?: boolean; running?: boolean }) {
  const { t } = useTranslation()
  const lines = splitLines(text)
  const [all, setAll] = useState(false)
  const folded = !all && lines.length > COLLAPSE_LINES
  const shown = folded ? lines.slice(0, COLLAPSE_LINES) : lines
  // 还在跑、暂时没有输出（流式输出的 agent 如 Codex 一开始就是这样）：不能说「没有输出」
  if (!text) return running ? null : <p className="px-3 py-2 text-xs text-fg-subtle">{t('tool.noOutput')}</p>
  return (
    <div className="bg-bg">
      <pre className={cn('scroll-thin max-h-96 overflow-auto px-3 py-2 font-mono text-xs leading-relaxed whitespace-pre-wrap [overflow-wrap:anywhere]', error && 'text-danger')} data-testid="tool-output">
        {shown.join('\n')}
      </pre>
      {folded ? (
        <button type="button" className="w-full border-t border-border px-3 py-1.5 text-xs text-accent-text hover:bg-bg-hover" onClick={() => setAll(true)}>
          {t('tool.showMore', { count: lines.length - COLLAPSE_LINES })}
        </button>
      ) : null}
    </div>
  )
}

export function ToolView({ block }: { block: ToolBlock }) {
  const { t } = useTranslation()
  const output = toolOutputText(block)
  const failed = block.result?.isError
  const exit = block.result?.detail?.exit_code

  switch (block.category) {
    case 'shell': {
      const command = toolCommand(block)
      const description = toolDescription(block)
      return (
        <ToolFrame
          block={block}
          summary={description ? <span className="font-sans">{description}</span> : <>$ {command}</>}
          summaryTitle={description ?? command}
          meta={exit != null && exit !== 0 ? <Chip tone="danger">{t('tool.exitCode', { code: exit })}</Chip> : undefined}
        >
          <CommandLine command={command} />
          {/* 还在跑、没有输出时 Terminal 什么都不画：分隔线也别画 */}
          {output || block.result ? (
            <div className="border-t border-border">
              <Terminal text={output} error={failed} running={!block.result} />
            </div>
          ) : null}
        </ToolFrame>
      )
    }
    case 'file_edit': {
      const diff = block.result?.detail?.unified_diff || syntheticDiff(block)
      const stats = diff ? diffStats(diff) : null
      return (
        <ToolFrame
          block={block}
          // 一次改多个文件（Codex fileChange）时 detail.path 只是第一个：显示 title（「Edit 3 files」）
          summary={toolFileChanges(block).length > 1 ? block.title : (toolPath(block) ?? block.title)}
          meta={stats ? <span className="shrink-0 font-mono text-xs"><span className="text-success">+{stats.added}</span> <span className="text-danger">−{stats.removed}</span></span> : undefined}
        >
          {diff ? (
            <Suspense fallback={<p className="px-3 py-2 text-xs text-fg-subtle">…</p>}>
              <DiffView diff={diff} />
            </Suspense>
          ) : output ? (
            <Terminal text={output} error={failed} />
          ) : undefined}
        </ToolFrame>
      )
    }
    case 'file_read':
      return (
        <ToolFrame block={block} summary={t('tool.read', { path: toolPath(block) ?? block.title })}>
          {output ? <Terminal text={output} error={failed} /> : undefined}
        </ToolFrame>
      )
    case 'search': {
      const n = block.result?.detail?.match_count
      return (
        <ToolFrame block={block} summary={n != null ? t('tool.searchResults', { query: toolQuery(block) ?? block.title, count: n }) : t('tool.search', { query: toolQuery(block) ?? block.title })}>
          {output ? <Terminal text={output} error={failed} /> : undefined}
        </ToolFrame>
      )
    }
    case 'web':
      return (
        <ToolFrame block={block} summary={toolUrl(block) ?? block.title}>
          {output ? <Terminal text={output} error={failed} /> : undefined}
        </ToolFrame>
      )
    default:
      return (
        <ToolFrame block={block} summary={toolLabel(block)}>
          <div className="space-y-2 p-3">
            <pre className="scroll-thin max-h-60 overflow-auto font-mono text-xs">{safeJson(block.input)}</pre>
            {output ? <Terminal text={output} error={failed} /> : null}
          </div>
        </ToolFrame>
      )
  }
}

function safeJson(v: unknown): string {
  try {
    return JSON.stringify(v, null, 2)
  } catch {
    return String(v)
  }
}

// ---- plan ----

export function PlanCard({ block }: { block: PlanBlock }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(true)
  const done = block.entries.filter((e) => e.status === 'completed').length
  return (
    <div className="rounded-lg border border-border bg-bg-subtle text-sm" data-testid="plan-card">
      <button type="button" className="flex w-full items-center gap-2 px-3 py-2 text-left" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <ListChecks className="size-4 text-fg-subtle" aria-hidden />
        <span className="flex-1 font-medium">{t('plan.title')}</span>
        <span className="text-xs text-fg-subtle">
          {done}/{block.entries.length}
        </span>
        <ChevronRight className={cn('size-4 text-fg-subtle transition-transform', open && 'rotate-90')} aria-hidden />
      </button>
      {open ? (
        <ul className="space-y-1 border-t border-border px-3 py-2">
          {block.entries.map((e, i) => (
            <li key={i} className="flex items-start gap-2" data-status={e.status}>
              {e.status === 'completed' ? <Check className="mt-0.5 size-4 shrink-0 text-success" aria-label={t('plan.completed')} /> : e.status === 'in_progress' ? <Loader2 className="mt-0.5 size-4 shrink-0 animate-spin text-accent-text" aria-label={t('plan.inProgress')} /> : <CircleDashed className="mt-0.5 size-4 shrink-0 text-fg-subtle" aria-label={t('plan.pending')} />}
              <span className={cn(e.status === 'completed' && 'text-fg-subtle line-through')}>{e.content}</span>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}

// ---- 用量 ----

/** 每轮结束的一行：「复制这一轮的全部回复」+ 用量灰字。`copyText` 是这一轮 agent 的全部文字（Markdown）。 */
export function TurnUsage({ block, copyText }: { block: TurnDoneBlock; copyText?: string }) {
  const { t } = useTranslation()
  const u = block.usage
  const parts: string[] = []
  if (u?.model) parts.push(u.model)
  if (u && u.duration_ms > 0) parts.push(formatDuration(u.duration_ms))
  if (u && (u.input_tokens != null || u.output_tokens != null)) {
    const tokens: string[] = []
    if (u.input_tokens != null) tokens.push(t('usage.input', { n: compactNumber(u.input_tokens) }))
    if (u.output_tokens != null) tokens.push(t('usage.output', { n: compactNumber(u.output_tokens) }))
    parts.push(tokens.join(' / '))
  }
  if (u?.cost_usd != null) parts.push(formatCost(u.cost_usd))
  const reason = block.reason !== 'completed' ? t(`turn.reason.${block.reason}`) : null
  if (parts.length === 0 && !reason && !copyText) return null
  return (
    <div className="flex min-w-0 items-center gap-2 text-xs text-fg-subtle">
      {copyText ? <CopyButton text={copyText} label={t('message.copyTurn')} iconOnly className="-ml-1.5 p-1.5" testId="turn-copy" /> : null}
      {parts.length > 0 || reason ? (
        <p className="min-w-0 truncate" data-testid="turn-usage">
          {reason ? <span className="mr-2 font-medium text-warning">{reason}</span> : null}
          {parts.join(' · ')}
        </p>
      ) : null}
    </div>
  )
}

// ---- 错误 / 系统行 ----

export function ErrorView({ block, onRetry }: { block: ErrorBlock; onRetry?: () => void }) {
  const { t } = useTranslation()
  const severity = errorSeverity(block.code)
  const explained = isExplainedError(block.code) ? t(`agentError.${block.code}`) : null
  if (severity === 'notice') {
    // 重连中、上下文被重置之类：淡色提示，不是错误，也不打断阅读（不用 alert）。窄屏上文字多了会换行，不会撑出屏幕
    return (
      <div role="status" className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5 px-1 text-xs text-fg-subtle" data-testid="error-notice" data-code={block.code}>
        <span className="flex min-w-0 items-center gap-2">
          <RefreshCw className="size-3.5 shrink-0" aria-hidden />
          <span>{explained ?? block.message}</span>
        </span>
        {explained && block.message ? (
          <span className="min-w-0 max-w-full truncate opacity-70" title={block.message}>
            {block.message}
          </span>
        ) : null}
      </div>
    )
  }
  const warning = severity === 'warning'
  return (
    <div
      role={warning ? 'status' : 'alert'}
      className={cn('flex items-start gap-2 rounded-lg border px-3 py-2 text-sm', warning ? 'border-warning/50 bg-warning-subtle text-warning' : 'border-danger/50 bg-danger-subtle text-danger')}
      data-testid="error-block"
      data-code={block.code}
      data-severity={severity}
    >
      <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
      <div className="min-w-0 flex-1">
        <p className="font-medium">{block.message}</p>
        {explained ? <p className="mt-0.5 text-xs" data-testid="error-explained">{explained}</p> : null}
        <p className="font-mono text-xs opacity-70">{block.code}</p>
      </div>
      {!warning && block.recoverable && onRetry ? (
        <Button size="sm" variant="outline" onClick={onRetry}>
          {t('message.retry')}
        </Button>
      ) : null}
    </div>
  )
}

/** 冲突那条消息上的两个按钮（ADR-34）：让 agent 解决 / 放弃合并。 */
export interface ConflictActions {
  busy?: boolean
  onResolve: () => void
  onAbort: () => void
}

/** `git` 事件：daemon 自己做的 git 操作，一行居中的系统提示。 */
export function GitLine({ block, conflict }: { block: GitSystemBlock; conflict?: ConflictActions }) {
  const { t } = useTranslation()
  const failed = block.action === 'worktree_failed' || block.action === 'worktree_missing'
  // 一行灰字；失败 / 要处理的（冲突、worktree 被保留）用状态色，文字本身说明了状态
  return (
    <div className="flex flex-col items-start gap-1.5" data-testid="git-event" data-action={block.action}>
      <p className={cn('max-w-full py-0.5 text-sm [overflow-wrap:anywhere]', failed ? 'text-danger' : block.action === 'sync_conflicts' || block.action === 'worktree_kept' ? 'text-warning' : 'text-fg-subtle')}>
        {gitEventText(t, block)}
      </p>
      {conflict ? (
        <div className="flex flex-wrap gap-2" data-testid="git-event-conflict-actions">
          <Button size="md" variant="outline" onClick={conflict.onResolve} disabled={conflict.busy} data-testid="git-event-resolve">
            {t('git.resolveWithAgent')}
          </Button>
          <Button size="md" variant="outline-danger" onClick={conflict.onAbort} disabled={conflict.busy} data-testid="git-event-abort">
            {t('git.abortMerge')}
          </Button>
        </div>
      ) : null}
    </div>
  )
}

export function SystemLine({ block, modelLabel, modeLabel, conflict }: { block: SystemBlock; modelLabel: (id: string) => string; modeLabel: (id: string) => string; conflict?: ConflictActions }) {
  const { t } = useTranslation()
  if (block.kind === 'git') return <GitLine block={block} conflict={conflict} />
  // 不知道上一份设置时（缓存窗口不含会话开头）不能说「已切换」，只列出当前值
  const lines = block.initial
    ? [
        t('system.current', {
          value: block.changes
            .filter((c) => c.field !== 'option')
            .map((c) => (c.field === 'model' ? modelLabel(c.to) : c.field === 'mode' ? modeLabel(c.to) : c.to))
            .join(' · '),
        }),
      ]
    : block.changes.map((c) => {
        if (c.field === 'model') return t('system.model', { value: modelLabel(c.to) })
        if (c.field === 'thinking') return t('system.thinking', { value: c.to })
        if (c.field === 'mode') return t('system.mode', { value: modeLabel(c.to) })
        return t('system.option', { id: c.id, value: c.to })
      })
  return (
    <p className="py-0.5 text-sm text-fg-subtle [overflow-wrap:anywhere]" data-testid="system-line">
      {lines.join(' · ')}
    </p>
  )
}

/**
 * turn 之间的分隔线。agent 自己开始的 turn（后台任务结束后 CLI 自己再跑的一轮，没有用户消息）
 * 在线中间加一行系统提示，让人知道这段回复不是对上一条消息的答复。
 */
export function TurnDivider({ turnId, ts, agentStarted }: { turnId?: number; ts: number; agentStarted?: boolean }) {
  const { t, i18n } = useTranslation()
  const title = `#${turnId ?? ''} · ${new Date(ts).toLocaleTimeString(i18n.language)}`
  if (agentStarted) {
    return (
      <div className="flex items-center gap-3 py-0.5 text-sm text-fg-subtle" title={title} data-testid="agent-turn-start">
        <span aria-hidden className="h-px flex-1 bg-border" />
        <span className="min-w-0 max-w-[80%] text-center [overflow-wrap:anywhere]">{t('system.agentTurn')}</span>
        <span aria-hidden className="h-px flex-1 bg-border" />
      </div>
    )
  }
  return (
    <div className="flex items-center gap-3 text-[0.6875rem] text-fg-subtle" title={title} aria-hidden>
      <span className="h-px flex-1 bg-border" />
    </div>
  )
}

