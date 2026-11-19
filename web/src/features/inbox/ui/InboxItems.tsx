// 「待处理」页里一个条目的两种样子：列表里的一行（`InboxRow`）和展开后的详情（`InboxDetail`）。
// 权限请求、计划、提问在详情里用的是会话页同一个 `PermissionCard`，按钮只在详情里：行上没有任何不看内容就能答复的入口。
import { useMemo, type ReactNode } from 'react'
import { Link } from '@tanstack/react-router'
import { AlertTriangle, ChevronRight, ClipboardList, ExternalLink, Flag, HelpCircle, ShieldQuestion, Smartphone } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { displayHint } from '@/lib/hint'
import { useDeviceLabel, useRespondPermission } from '@/features/session/hooks'
import { PermissionCard } from '@/features/session/ui/PermissionCard'
import { useNodes } from '@/stores/nodes'
import { useSessions, type SessionItem } from '@/stores/sessions'
import type { EnrollItem } from '@/stores/enroll'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Chip } from '@/ui/Misc'
import type { InboxItem, InboxItemKind, RequestItem } from '../derive'
import { permissionBlock } from '../block'
import { useItemInfo } from './info'

/** 行和详情里的操作（处理后的焦点转移由页面负责）。 */
export interface InboxActions {
  /** 清掉会话上的「待处理」标记。 */
  markDone: (s: SessionItem) => void
  archive: (s: SessionItem) => void
  /** 打开接入请求的批准对话框（spec/web.md § 4.3.2）。 */
  reviewEnroll: (e: EnrollItem) => void
}

const KIND_ICON: Record<InboxItemKind, typeof Flag> = {
  permission: ShieldQuestion,
  plan: ClipboardList,
  question: HelpCircle,
  enroll: Smartphone,
  follow_up: Flag,
  error: AlertTriangle,
}

/** 不靠颜色分辨：每种都有文字标签。 */
const KIND_TONE: Record<InboxItemKind, 'warning' | 'info' | 'danger'> = {
  permission: 'warning',
  plan: 'warning',
  question: 'warning',
  enroll: 'info',
  follow_up: 'info',
  error: 'danger',
}

export function CachedChip({ nodeName }: { nodeName: string }) {
  const { t } = useTranslation()
  return (
    <Chip tone="warning" title={t('inbox.cachedHint', { node: nodeName })} testId="inbox-cached">
      {t('inbox.cached')}
    </Chip>
  )
}

/** 行：主按钮（选中 / 展开）+ 不需要看详情的操作（已处理、归档、查看接入请求）。`children` 是手机上展开后的详情。 */
export function InboxRow({
  item,
  mode,
  selected,
  showNode,
  actions,
  onSelect,
  children,
}: {
  item: InboxItem
  mode: 'split' | 'overlay' | 'inline'
  selected: boolean
  /** 勾了多个节点时，行上标出节点。 */
  showNode: boolean
  actions: InboxActions
  onSelect: (key: string) => void
  children?: ReactNode
}) {
  const { t } = useTranslation()
  const info = useItemInfo(item)
  const Icon = KIND_ICON[item.kind]
  const meta = [info.sessionTitle, info.project, showNode ? info.nodeName : undefined, info.since].filter(Boolean).join(' · ')
  const expanded = mode === 'inline' && selected
  return (
    <li
      data-inbox-row={item.key}
      data-kind={item.kind}
      data-selected={selected ? 'true' : undefined}
      data-testid="inbox-row"
      className={cn('rounded-lg border bg-surface', selected && mode === 'split' ? 'border-accent/50 bg-accent-subtle' : 'border-border')}
    >
      <button
        type="button"
        data-inbox-focus
        data-testid="inbox-row-main"
        // 手机：展开 / 收起；桌面：当前显示在右边的那一条
        aria-expanded={mode === 'inline' ? expanded : undefined}
        aria-current={mode === 'split' && selected ? 'true' : undefined}
        aria-label={mode === 'inline' ? `${info.title}. ${expanded ? t('inbox.collapse') : t('inbox.expand')}` : undefined}
        onClick={() => onSelect(item.key)}
        className="flex min-h-11 w-full min-w-0 items-start gap-3 rounded-lg px-3 py-2.5 text-left hover:bg-bg-hover"
      >
        <Icon className="mt-0.5 size-4 shrink-0 text-fg-muted" aria-hidden />
        <span className="min-w-0 flex-1">
          <span className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span className="min-w-0 flex-1 truncate font-medium [overflow-wrap:anywhere]">{info.title}</span>
            <Chip tone={KIND_TONE[item.kind]}>{t(`inbox.kind.${item.kind}`)}</Chip>
            {info.online ? null : <CachedChip nodeName={info.nodeName} />}
          </span>
          {meta ? <span className="mt-0.5 block truncate text-xs text-fg-subtle">{meta}</span> : null}
        </span>
        {mode === 'inline' ? <ChevronRight className={cn('mt-1 size-4 shrink-0 text-fg-subtle transition-transform', expanded && 'rotate-90')} aria-hidden /> : null}
      </button>
      <RowActions item={item} actions={actions} online={info.online} sessionId={info.sessionId} nodeParam={info.nodeParam} />
      {children}
    </li>
  )
}

/** 会话类和接入类条目的行内操作；权限请求、计划、提问没有（要先展开看详情）。 */
function RowActions({ item, actions, online, sessionId, nodeParam }: { item: InboxItem; actions: InboxActions; online: boolean; sessionId?: string; nodeParam: string }) {
  const { t } = useTranslation()
  const small = 'max-md:min-h-11 max-md:px-4'
  switch (item.kind) {
    case 'permission':
    case 'plan':
    case 'question':
      return null
    case 'enroll':
      return (
        <div className="flex gap-2 px-3 pb-2.5 pl-10">
          <Button size="sm" variant="primary" className={small} onClick={() => actions.reviewEnroll(item.enroll)} data-testid="inbox-review">
            {t('inbox.review')}
          </Button>
        </div>
      )
    case 'follow_up':
    case 'error':
      return (
        <div className="flex flex-wrap gap-2 px-3 pb-2.5 pl-10">
          {sessionId ? (
            <Link to="/n/$node/s/$session" params={{ node: nodeParam, session: sessionId }} className={cn('inline-flex h-8 select-none items-center rounded-md border border-border-strong px-3 text-[0.8125rem] font-medium text-fg hover:bg-bg-hover', small)} data-testid="inbox-open">
              {t('inbox.open')}
            </Link>
          ) : null}
          {item.kind === 'follow_up' ? (
            <Button size="sm" variant="primary" className={small} disabled={!online} onClick={() => actions.markDone(item.session)} data-testid="inbox-done">
              {t('inbox.markDone')}
            </Button>
          ) : (
            <Button size="sm" variant="secondary" className={small} disabled={!online} onClick={() => actions.archive(item.session)} data-testid="inbox-archive">
              {t('inbox.archive')}
            </Button>
          )}
        </div>
      )
  }
}

/** 详情。`data-inbox-detail`：Enter 把焦点移进来时找它。 */
export function InboxDetail({ item }: { item: InboxItem }) {
  switch (item.kind) {
    case 'permission':
    case 'plan':
    case 'question':
      return <RequestDetail item={item} />
    default:
      return <PlainDetail item={item} />
  }
}

function DetailHeader({ item }: { item: InboxItem }) {
  const { t } = useTranslation()
  const info = useItemInfo(item)
  return (
    <div className="space-y-1.5">
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs text-fg-subtle" data-testid="inbox-detail-meta">
        <dt>{t('inbox.node')}</dt>
        <dd className="min-w-0 truncate text-fg-muted" data-testid="inbox-detail-node">
          {info.nodeName}
        </dd>
        {info.sessionId ? (
          <>
            <dt>{t('inbox.session')}</dt>
            <dd className="min-w-0 truncate text-fg-muted [overflow-wrap:anywhere]" data-testid="inbox-detail-session">
              {info.sessionTitle ?? info.title}
            </dd>
          </>
        ) : null}
        {info.project ? (
          <>
            <dt>{t('inbox.project')}</dt>
            <dd className="min-w-0 truncate text-fg-muted">{info.project}</dd>
          </>
        ) : null}
      </dl>
      <p className="text-xs text-fg-subtle">{info.since}</p>
      {info.online ? null : (
        <p className="flex items-start gap-1.5 rounded-md border border-warning/40 bg-warning-subtle px-2.5 py-1.5 text-sm text-warning" role="status" data-testid="inbox-cached-note">
          <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
          {t('inbox.cachedBody', { node: info.nodeName })}
        </p>
      )}
      {info.sessionId ? (
        <Link to="/n/$node/s/$session" params={{ node: info.nodeParam, session: info.sessionId }} className="inline-flex min-h-8 items-center gap-1 text-sm font-medium text-accent-text hover:underline max-md:min-h-11" data-testid="inbox-open-session">
          {t('inbox.openSession')}
          <ExternalLink className="size-3.5" aria-hidden />
        </Link>
      ) : null}
    </div>
  )
}

function RequestDetail({ item }: { item: RequestItem }) {
  const { respond, responding, settled } = useRespondPermission(item.nodeId)
  const deviceLabel = useDeviceLabel(item.nodeId)
  const info = useItemInfo(item)
  const sessionAgent = useSessions((s) => s.list[item.sessionId]?.agent)
  const agent = useNodes((s) => s.byId[item.nodeId]?.agents.find((a) => a.id === sessionAgent))
  const block = useMemo(() => permissionBlock(item.request), [item.request])
  const mode = block.kind === 'mode_change' && block.detail.mode ? agent?.modes?.find((m) => m.id === block.detail.mode) : undefined
  const targetMode = block.kind === 'mode_change' && block.detail.mode ? { label: mode?.label ?? block.detail.mode, dangerous: mode?.risk === 'dangerous' } : undefined
  return (
    <div className="space-y-3" data-inbox-detail data-testid="inbox-detail" data-kind={item.kind}>
      <DetailHeader item={item} />
      {/* 和会话页同一个卡片：命令、工作目录、全部选项。节点连不上时按钮禁用（答复发不出去） */}
      <PermissionCard block={block} deviceLabel={deviceLabel} onRespond={respond} busy={responding.has(item.request.request_id) || !info.online} settled={settled.get(item.request.request_id)} targetMode={targetMode} />
    </div>
  )
}

function PlainDetail({ item }: { item: Exclude<InboxItem, RequestItem> }) {
  const { t } = useTranslation()
  const info = useItemInfo(item)
  return (
    <div className="space-y-3" data-inbox-detail data-testid="inbox-detail" data-kind={item.kind}>
      <h3 className="text-base font-semibold [overflow-wrap:anywhere]">{info.title}</h3>
      <DetailHeader item={item} />
      {item.kind === 'follow_up' ? (
        <p className="text-sm text-fg-muted">
          {t('followUp.barReady')}
          {item.session.status === 'error' ? ` ${t('inbox.alsoError')}` : ''}
        </p>
      ) : null}
      {item.kind === 'error' ? <p className="text-sm text-fg-muted">{t('inbox.errorBody')}</p> : null}
      {item.kind === 'enroll' ? <EnrollSummary item={item.enroll} nodeName={info.nodeName} /> : null}
    </div>
  )
}

function EnrollSummary({ item, nodeName }: { item: EnrollItem; nodeName: string }) {
  const { t } = useTranslation()
  const hint = displayHint(item.info.hint)
  return (
    <div className="space-y-1 text-sm text-fg-muted">
      <p>{t('inbox.enrollBody', { node: nodeName })}</p>
      <p className="text-xs text-fg-subtle">{[hint.name ?? t('inbox.enrollUnknown'), hint.platform].filter(Boolean).join(' · ')}</p>
    </div>
  )
}
