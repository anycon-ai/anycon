import { lazy, Suspense, useState, type ReactNode } from 'react'
import { AlertTriangle, ChevronRight, Clock, FileEdit, FileText, Globe, HelpCircle, Lock, Puzzle, Search, ShieldAlert, ShieldQuestion, SquareTerminal, Users } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { PermissionOption, PermissionOptionKind, PermissionRespondParams, Question, ToolCategory } from '@/protocol'
import { Markdown } from '@/components/Markdown'
import { isRecord } from '@/lib/guards'
import { useNow } from '@/lib/useNow'
import { Button, type ButtonVariant } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Textarea } from '@/ui/Field'
import { Card, Chip } from '@/ui/Misc'
import type { PermissionBlock } from '../reducer'
import { diffStats, syntheticDiff, toolFileChanges } from '../text'

const DiffView = lazy(() => import('./DiffView').then((m) => ({ default: m.DiffView })))

const CATEGORY_ICON: Record<ToolCategory, typeof SquareTerminal> = {
  shell: SquareTerminal,
  file_edit: FileEdit,
  file_read: FileText,
  search: Search,
  web: Globe,
  subagent: Users,
  mcp: Puzzle,
  other: Puzzle,
}

export function optionVariant(kind: PermissionOptionKind): ButtonVariant {
  switch (kind) {
    case 'allow_once':
      return 'primary'
    case 'allow_always':
      return 'outline-success'
    case 'reject_once':
    case 'reject_always':
      return 'outline-danger'
  }
}

export function optionTone(kind: PermissionOptionKind): 'allow' | 'reject' {
  return kind.startsWith('allow') ? 'allow' : 'reject'
}

export type SettledReason = 'folder_changed' | 'gone'

export interface PermissionCardProps {
  block: PermissionBlock
  /** 把 device id 翻成显示名；本机返回「你」。 */
  deviceLabel: (deviceId: string) => string
  onRespond: (params: PermissionRespondParams) => void
  /** 已经发出答复、等 daemon 确认中。 */
  busy?: boolean
  /**
   * 答复时 daemon 回了 `not_found`，卡片定格成说明（不再给按钮、不重试）：
   * `folder_changed`（`data.reason = project_dir_changed`）：项目目录变了，请求已被拒绝、本轮已中止；
   * `gone`（别的 `not_found`）：这个请求已经不存在。
   */
  settled?: SettledReason
  /** kind = mode_change：目标模式的显示名，以及它是不是危险模式（危险的要醒目警示）。 */
  targetMode?: { label: string; dangerous: boolean }
}

/** 权限卡片：三种 kind（tool / plan / question）+ mode_change / other；按钮完全由 options[] 渲染。 */
export function PermissionCard({ block, deviceLabel, onRespond, busy, targetMode, settled }: PermissionCardProps) {
  const { t } = useTranslation()
  const [note, setNote] = useState('')
  const [answers, setAnswers] = useState<Record<string, string | string[]>>({})
  const needsNote = block.options.some((o) => o.needs_message)

  if (block.resolved) {
    const { answers, message } = block.resolved
    const opt = block.options.find((o) => o.id === block.resolved?.optionId)
    const allowed = opt ? optionTone(opt.kind) === 'allow' : true
    const who = deviceLabel(block.resolved.byDevice)
    // daemon 自己答复的（目录变了自动拒绝、超时、取消）：不是某台设备，也不能把原始的 "daemon" 当设备名显示
    const auto = block.resolved.byDevice === 'daemon'
    const text = auto
      ? block.kind === 'question' || !opt
        ? t('permission.resolvedAuto')
        : allowed
          ? t('permission.allowedAuto')
          : t('permission.deniedAuto')
      : block.kind === 'question'
        ? t(allowed ? 'permission.answeredBy' : 'permission.skippedBy', { device: who })
        : opt
          ? t(allowed ? 'permission.allowedBy' : 'permission.deniedBy', { device: who })
          : t('permission.resolvedBy', { device: who })
    // 问题和回答留在对话里（旧事件没记回答，只剩一行）；答复附的话（拒绝理由、跳过说明）跟在下面
    const qa = block.kind === 'question' && answers ? <AnsweredQuestions questions={block.detail.questions ?? []} answers={answers} /> : null
    const note = message ? (
      <p className="mt-1 border-l-2 border-border pl-3 text-sm whitespace-pre-wrap text-fg-muted [overflow-wrap:anywhere]" data-testid="permission-message">
        {message}
      </p>
    ) : null
    return (
      <ResolvedLine block={block} testState="resolved" extra={{ 'data-option': block.resolved.optionId ?? '', 'data-by': auto ? 'daemon' : undefined }} below={qa || note ? <>{qa}{note}</> : undefined}>
        {block.title} — {text}
        {opt && block.kind !== 'question' ? ` (${opt.label})` : null}
      </ResolvedLine>
    )
  }

  // 答复时 daemon 回了 not_found：说明原因，不再给按钮（不能重试同一个请求）
  if (settled) {
    return (
      <ResolvedLine block={block} testState="settled" danger={settled === 'folder_changed'} extra={{ 'data-settled': settled, role: 'status' }}>
        {block.title}
        <span data-testid="permission-settled"> — {settled === 'folder_changed' ? t('permission.deniedFolderChanged') : t('permission.requestGone')}</span>
      </ResolvedLine>
    )
  }

  const isQuestion = block.kind === 'question'
  // 问答卡片上 allow 类选项（adapter 给的「Submit answers」）就是提交按钮：带上 answers，答完才能点
  const submitsAnswers = (opt: PermissionOption): boolean => isQuestion && optionTone(opt.kind) === 'allow'

  const respond = (opt: PermissionOption): void => {
    onRespond({
      request_id: block.requestId,
      option_id: opt.id,
      message: opt.needs_message && note.trim() ? note.trim() : undefined,
      answers: submitsAnswers(opt) ? answers : undefined,
    })
  }

  const questions = block.detail.questions ?? []
  const complete = questions.every((q) => {
    const a = answers[q.id]
    return Array.isArray(a) ? a.length > 0 : !!a?.trim()
  })
  // adapter 没给 allow 类选项时才用自带的提交按钮，否则会出现两个「Submit answers」
  const ownSubmit = isQuestion && !block.options.some(submitsAnswers)

  const danger = block.kind === 'mode_change' && !!targetMode?.dangerous
  // 补丁内容未知（Codex 的审批请求先于对应的 item 到达）：看不到它会改什么，批准要格外小心
  const unknown = block.kind === 'tool' && isRecord(block.detail.tool?.input) && block.detail.tool.input.contentUnknown === true
  const caution = danger || unknown
  const HeaderIcon = danger ? ShieldAlert : block.kind === 'tool' && block.detail.tool ? CATEGORY_ICON[block.detail.tool.category] : block.kind === 'question' ? HelpCircle : block.kind === 'plan' ? ShieldQuestion : Lock

  return (
    <div data-testid="permission-card" data-state="pending" data-kind={block.kind} data-request={block.requestId} data-danger={danger ? 'true' : undefined} data-unknown-content={unknown ? 'true' : undefined}>
      <Card tone={caution ? 'danger' : 'warning'} className={cn('overflow-hidden', caution && 'bg-danger-subtle/40')}>
        <div className="space-y-3 p-3">
        <div className="flex items-start gap-2">
          <HeaderIcon className={cn('mt-0.5 size-4 shrink-0', danger ? 'text-danger' : 'text-warning')} aria-hidden />
          <div className="min-w-0 flex-1">
            {/* 标题可能很长（多文件补丁带着几个绝对路径）：窄屏上折行，不撑出屏幕 */}
            <p className="text-sm font-semibold [overflow-wrap:anywhere]">{block.title}</p>
            {block.kind === 'tool' ? <ToolSummary block={block} unknownContent={unknown} /> : null}
          </div>
          <Chip tone={danger ? 'danger' : 'warning'}>{danger ? t('permission.dangerous') : t('permission.waiting')}</Chip>
        </div>
        {block.expiresAtMs ? <ExpiryNote at={block.expiresAtMs} /> : null}
        {unknown ? (
          <p className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger-subtle px-2.5 py-2 text-sm font-medium text-danger" role="alert" data-testid="permission-unknown-content">
            <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
            {t('permission.contentUnknown')}
          </p>
        ) : null}

        {block.kind === 'plan' && block.detail.text ? (
          <div className="max-h-96 overflow-y-auto rounded-md border border-border bg-bg p-3" data-testid="permission-plan">
            <Markdown text={block.detail.text} />
          </div>
        ) : null}
        {block.kind === 'other' || block.kind === 'mode_change' ? (
          <p className="text-sm text-fg-muted">{block.detail.text ?? (block.detail.mode ? t('permission.modeChange', { mode: targetMode?.label ?? block.detail.mode }) : '')}</p>
        ) : null}
        {danger ? (
          <p className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger-subtle px-2.5 py-2 text-sm font-medium text-danger" role="alert" data-testid="permission-danger">
            <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
            {t('permission.dangerousWarning', { mode: targetMode?.label ?? block.detail.mode ?? '' })}
          </p>
        ) : null}

        {block.kind === 'question' ? (
          <div className="space-y-4" data-testid="permission-questions">
            {questions.map((q) => (
              <fieldset key={q.id} className="space-y-2">
                <legend className="text-sm font-medium">{q.text}</legend>
                {(q.choices ?? []).map((c) => {
                  const cur = answers[q.id]
                  const checked = q.multi ? Array.isArray(cur) && cur.includes(c.id) : cur === c.id
                  return (
                    <label key={c.id} className="flex cursor-pointer items-center gap-2 text-sm">
                      <input
                        type={q.multi ? 'checkbox' : 'radio'}
                        name={`${block.requestId}-${q.id}`}
                        checked={checked}
                        onChange={() =>
                          setAnswers((a) => {
                            if (!q.multi) return { ...a, [q.id]: c.id }
                            const arr = Array.isArray(a[q.id]) ? (a[q.id] as string[]) : []
                            return { ...a, [q.id]: arr.includes(c.id) ? arr.filter((x) => x !== c.id) : [...arr, c.id] }
                          })
                        }
                        className="size-4 accent-[var(--accent)]"
                      />
                      {c.label}
                    </label>
                  )
                })}
                {q.free_text ? (
                  <Textarea
                    rows={2}
                    aria-label={q.text}
                    placeholder={t('permission.freeText')}
                    value={typeof answers[q.id] === 'string' && !(q.choices ?? []).some((c) => c.id === answers[q.id]) ? (answers[q.id] as string) : ''}
                    onChange={(e) => setAnswers((a) => ({ ...a, [q.id]: e.target.value }))}
                  />
                ) : null}
              </fieldset>
            ))}
          </div>
        ) : null}

        {needsNote ? <Textarea rows={2} value={note} onChange={(e) => setNote(e.target.value)} placeholder={t('permission.notePlaceholder')} aria-label={t('permission.note')} data-testid="permission-note" /> : null}

        <div className="flex flex-wrap gap-2">
          {ownSubmit ? (
            <Button variant="primary" size="sm" disabled={busy || !complete} onClick={() => onRespond({ request_id: block.requestId, answers })} data-testid="permission-submit-answers">
              {t('permission.submitAnswers')}
            </Button>
          ) : null}
          {block.options.map((o) => (
            <Button
              key={o.id}
              size="sm"
              variant={caution && optionTone(o.kind) === 'allow' ? 'danger' : optionVariant(o.kind)}
              disabled={busy || (submitsAnswers(o) && !complete)}
              onClick={() => respond(o)}
              data-testid={`permission-option-${o.id}`}
              data-kind={o.kind}
            >
              {o.label}
            </Button>
          ))}
        </div>
        </div>
      </Card>
    </div>
  )
}

/** 请求的截止时间（`expires_at_ms`）：倒计时，过期后提示（daemon 会按超时处理）。 */
function ExpiryNote({ at }: { at: number }) {
  const { t } = useTranslation()
  const now = useNow(1000)
  const left = Math.max(0, Math.ceil((at - now) / 1000))
  return (
    <p className={cn('flex items-center gap-1.5 text-xs', left > 0 ? 'text-fg-subtle' : 'text-danger')} data-testid="permission-expiry" data-expired={left === 0 ? 'true' : undefined}>
      <Clock className="size-3" aria-hidden />
      {left > 0 ? t('permission.expiresIn', { time: `${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}` }) : t('permission.expired')}
    </p>
  )
}

/**
 * 已经答复（或作废）的请求：收成一行灰字「标题 — 已由 X 允许（选项）›」，不再是卡片；有工具详情时点开看命令 / diff。
 * 允许 / 拒绝写在文字里，不靠颜色和图标；项目目录变了被拒绝的（turn 被中止）用红字。
 * 作废的（`settled`）只是一行字，没有任何按钮（同一个请求不能再答复）。
 * `below`：一直显示在这一行下面的内容（问题和回答、答复附的话）。
 */
function ResolvedLine({ block, testState, danger, extra, below, children }: { block: PermissionBlock; testState: 'resolved' | 'settled'; danger?: boolean; extra: Record<string, string | undefined>; below?: ReactNode; children: ReactNode }) {
  const [open, setOpen] = useState(false)
  const hasDetail = testState === 'resolved' && !!block.detail.tool
  if (!hasDetail) {
    const line = cn('py-0.5 text-sm [overflow-wrap:anywhere]', danger ? 'text-danger' : 'text-fg-subtle')
    if (!below) {
      return (
        <p className={line} data-testid="permission-card" data-state={testState} {...extra}>
          {children}
        </p>
      )
    }
    return (
      <div className="text-sm" data-testid="permission-card" data-state={testState} {...extra}>
        <p className={line}>{children}</p>
        {below}
      </div>
    )
  }
  return (
    <div className="text-sm" data-testid="permission-card" data-state={testState} {...extra}>
      <button type="button" className={cn('flex max-w-full items-center gap-1.5 py-0.5 text-left', danger ? 'text-danger' : 'text-fg-subtle hover:text-fg-muted')} aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span className="min-w-0 truncate">{children}</span>
        <ChevronRight className={cn('size-3.5 shrink-0 transition-transform', open && 'rotate-90')} aria-hidden />
      </button>
      {below}
      {open ? (
        <div className="mt-1 rounded-lg border border-border p-2.5">
          <ToolSummary block={block} />
        </div>
      ) : null}
    </div>
  )
}

/**
 * 已回答的提问：每道题下面是设备给的回答（选项 id 换成显示名，多选用逗号连接，自由输入的文字原样）。
 * 没答的题写「未回答」。
 */
function AnsweredQuestions({ questions, answers }: { questions: Question[]; answers: Record<string, string | string[]> }) {
  const { t } = useTranslation()
  const label = (q: Question, id: string): string => q.choices?.find((c) => c.id === id)?.label ?? id
  return (
    <dl className="mt-1 space-y-2 border-l-2 border-border pl-3" data-testid="permission-answers">
      {questions.map((q) => {
        const a = answers[q.id]
        const text = (Array.isArray(a) ? a.map((x) => label(q, x)).join(', ') : a ? label(q, a) : '').trim()
        return (
          <div key={q.id} data-testid="permission-answer" data-question={q.id}>
            <dt className="text-fg-subtle [overflow-wrap:anywhere]">{q.text}</dt>
            <dd className={cn('whitespace-pre-wrap [overflow-wrap:anywhere]', text ? 'text-fg' : 'text-fg-subtle')}>{text || t('permission.noAnswer')}</dd>
          </div>
        )
      })}
    </dl>
  )
}

function ToolSummary({ block, unknownContent }: { block: PermissionBlock; unknownContent?: boolean }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  const tool = block.detail.tool
  if (!tool) return null
  const d = tool.detail
  // 一次改多个文件（Codex fileChange）：列出全部路径，不只第一个
  const files = toolFileChanges({ input: tool.input, detail: d })
  // 补丁内容未知时没有命令 / 路径可显示：不拿说明（多段文字）当「命令」，说明单独作为 reason 显示
  const headline = unknownContent ? null : files.length > 1 ? files.map((f) => f.path).join('\n') : (d?.command ?? d?.path ?? d?.url ?? d?.query ?? d?.description ?? tool.tool)
  const reason = block.detail.text ?? (d?.description && d.description !== headline ? d.description : null)
  const diff = tool.category === 'file_edit' ? syntheticDiff({ input: tool.input, detail: d }) : null
  const stats = diff ? diffStats(diff) : null
  return (
    <div className="mt-1 space-y-1.5">
      {headline !== null ? (
        <pre className="scroll-thin max-h-40 overflow-auto rounded-md bg-bg-muted px-2.5 py-1.5 font-mono text-[0.8125rem] whitespace-pre-wrap [overflow-wrap:anywhere]" data-testid="permission-command">
          {tool.category === 'shell' ? '$ ' : ''}
          {headline}
        </pre>
      ) : null}
      {/* 为什么要这次操作（Codex 的审批请求带 reason；Bash 的 description 同理） */}
      {reason ? (
        // 说明可能是多段多行（reason、路径清单 `- edit …`、grantRoot 说明，空行隔开）：保留换行，长行在窄屏上折行
        <p className="text-xs whitespace-pre-wrap text-fg-muted [overflow-wrap:anywhere]" data-testid="permission-reason">
          {reason}
        </p>
      ) : null}
      {tool.cwd ? <p className="font-mono text-xs text-fg-subtle">{t('permission.cwd', { cwd: tool.cwd })}</p> : null}
      <button type="button" className="inline-flex items-center gap-1 text-xs text-fg-subtle hover:text-fg" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <ChevronRight className={cn('size-3 transition-transform', open && 'rotate-90')} />
        {t('permission.details')}
        {stats ? (
          <span className="ml-1 font-mono">
            <span className="text-success">+{stats.added}</span> <span className="text-danger">−{stats.removed}</span>
          </span>
        ) : null}
      </button>
      {open ? (
        <div className="overflow-hidden rounded-md border border-border bg-bg">
          {diff ? (
            <Suspense fallback={null}>
              <DiffView diff={diff} />
            </Suspense>
          ) : null}
          <pre className="scroll-thin max-h-60 overflow-auto p-2 font-mono text-xs">{JSON.stringify(tool.input, null, 2)}</pre>
        </div>
      ) : null}
    </div>
  )
}

