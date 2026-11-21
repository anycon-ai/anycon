import { useMemo, useState, type HTMLAttributes } from 'react'
import { Link, useNavigate, useParams, useRouterState } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Bell, ChevronDown, ChevronRight, Flag, ListChecks, FolderGit2, FolderPlus, Inbox, LogOut, Pin, PinOff, Plus, ScanLine, ScrollText, Search, Settings, Shield, Trash2, User } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useMe } from '@/api/queries'
import { describeError } from '@/lib/errors'
import { resetClientState, signOut as signOutOfHub } from '@/app/session'
import { useCameraAvailable } from '@/features/approve/useCameraAvailable'
import { groupNodeSessions, isActiveStatus, type NodeProjectGroup } from '@/features/session/group'
import { useInboxNowCount } from '@/features/inbox/useInbox'
import { useCapture } from '@/features/todos/capture'
import { useRulesAvailable } from '@/features/rules/store'
import { useTodoAvailable } from '@/features/todos/store'
import { useOpenTodoCount } from '@/features/todos/useOpenTodoCount'
import { isFollowUp, isFollowUpReady, isUnread } from '@/features/session/marks'
import { useEnroll } from '@/stores/enroll'
import { shortNodeId } from '@/lib/ids'
import { useNodeFilter } from '@/lib/nodeFilter'
import { SessionDot, UnreadDot } from './StatusDot'
import { useHub } from '@/stores/hub'
import { useNewProject } from '@/stores/newProject'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions, type SessionItem } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Menu, MenuContent, MenuItem, MenuSeparator, MenuTrigger } from '@/ui/Menu'
import { Chip } from '@/ui/Misc'
import { Logo } from './Logo'
import { SpaceSwitcher } from './SpaceSwitcher'
import { RemoveDeviceDialog } from './RemoveDeviceDialog'
import { SignOutDialog } from './SignOutDialog'

const RECENT_PER_PROJECT = 5

/**
 * 「新建项目」默认建在哪个节点：页面当前所在的节点（在勾选里的话），否则第一个勾选的；对话框里还能换。
 * 没有可建的节点时返回 undefined。
 */
function useNewProjectAction(nodeId: string | undefined, selected: string[], onNavigate?: () => void): (() => void) | undefined {
  const target = nodeId && selected.includes(nodeId) ? nodeId : selected[0]
  if (!target) return undefined
  return () => {
    onNavigate?.()
    useNewProject.getState().open({ nodeId: target, mode: 'empty', then: 'project', pickNode: true })
  }
}

function isWaiting(s: SessionItem): boolean {
  return s.pending_permissions > 0 || s.status === 'waiting_permission'
}

/** 一行上和标注有关的两个字段：未读小红点、「待处理」状态（还在跑 = armed，停下来 = ready）。 */
function marksOf(s: SessionItem): { unread: boolean; followUp?: 'armed' | 'ready' } {
  return { unread: isUnread(s), ...(isFollowUp(s) ? { followUp: isFollowUpReady(s) ? ('ready' as const) : ('armed' as const) } : {}) }
}

/**
 * 进行中的会话（只算勾选的节点），最近的在前：左栏「Recent」分组，省得挨个项目去找。
 * 「进行中」和项目页的筛选同一个口径（`isActiveStatus`）；等审批的会话在「待处理」页里，这里不重复列。
 */
function useRecentSessions(selected: string[], enabled: boolean) {
  const list = useSessions((s) => s.list)
  return useMemo(
    () =>
      enabled
        ? Object.values(list)
            .filter((x) => selected.includes(x.node_id) && !x.archived && isActiveStatus(x.status) && !isWaiting(x))
            .sort((a, b) => b.updated_at_ms - a.updated_at_ms)
        : [],
    [list, selected, enabled],
  )
}

/** 钉住 / 取消钉住宽屏左栏的按钮：钉住时常驻占位，没钉住时收成窄条、悬停浮出（见 AppShell）。 */
function PinButton({ pinned, onToggle, className }: { pinned: boolean; onToggle: () => void; className?: string }) {
  const { t } = useTranslation()
  const label = pinned ? t('sidebar.unpinSidebar') : t('sidebar.pinSidebar')
  return (
    <button type="button" aria-label={label} title={label} aria-pressed={pinned} className={cn('rounded-md p-1.5 text-fg-subtle hover:bg-bg-hover hover:text-fg', className)} onClick={onToggle} data-testid="sidebar-pin">
      {pinned ? <PinOff className="size-4" /> : <Pin className="size-4" />}
    </button>
  )
}

/**
 * `nodeId`：页面当前所在的节点，只用于下拉里的「节点设置」；左栏显示哪些节点由下拉里的勾选决定。
 * `pin`：宽屏左栏才传，Logo 旁边多一个钉住 / 取消钉住按钮。
 * 自上而下：Logo、导航（「待处理」带「现在」的个数）、节点过滤和搜索、会话树、账号菜单。手机没有左栏，用底部标签栏（`TabBar`）。
 */
export function Sidebar({ nodeId, onNavigate, onSearch, pin }: { nodeId: string | undefined; onNavigate?: () => void; onSearch?: () => void; pin?: { pinned: boolean; onToggle: () => void } }) {
  return (
    <div className="flex h-full min-h-0 flex-col bg-bg-subtle" data-testid="sidebar">
      <div className="flex items-center gap-2 px-3 pb-2 pt-3">
        <Link to="/" onClick={onNavigate} className="mr-auto rounded-md px-1 py-1">
          <Logo />
        </Link>
        {pin ? <PinButton pinned={pin.pinned} onToggle={pin.onToggle} /> : null}
      </div>
      <SidebarNav onNavigate={onNavigate} />
      <SessionsPanel nodeId={nodeId} onNavigate={onNavigate} onSearch={onSearch} />
      <AccountFooter onNavigate={onNavigate} />
    </div>
  )
}

/** 顶层导航。目前只有「待处理」；之后的去处（清单…）加在这里。 */
function SidebarNav({ onNavigate }: { onNavigate?: () => void }) {
  const { t } = useTranslation()
  const count = useInboxNowCount()
  const todoCount = useOpenTodoCount()
  const todoAvailable = useTodoAvailable()
  const rulesAvailable = useRulesAvailable()
  const openCapture = useCapture((s) => s.openCapture)
  const here = useRouterState({ select: (s) => s.location.pathname })
  const current = here === '/inbox'
  const todosHere = here === '/todos' || here.startsWith('/todos/')
  const rulesHere = here === '/rules'
  const row = 'flex items-center gap-2 rounded-md px-2 py-1.5 text-sm font-medium hover:bg-bg-hover'
  return (
    <nav className="space-y-0.5 px-2 pb-2" aria-label={t('sidebar.nav')} data-testid="sidebar-nav">
      <Link
        to="/inbox"
        onClick={onNavigate}
        aria-current={current ? 'page' : undefined}
        aria-label={count > 0 ? t('inbox.navLabel', { count }) : t('inbox.title')}
        data-testid="sidebar-inbox"
        className={cn(row, current && 'bg-accent-subtle')}
      >
        <Inbox className="size-4 shrink-0 text-fg-muted" aria-hidden />
        <span className="min-w-0 flex-1 truncate">{t('inbox.title')}</span>
        {count > 0 ? (
          <span className="rounded-full bg-warning-subtle px-2 py-0.5 text-xs font-medium text-warning" data-testid="sidebar-inbox-count" aria-hidden>
            {count}
          </span>
        ) : null}
      </Link>
      {todoAvailable ? (
      <div className="flex items-center gap-1">
        <Link
          to="/todos"
          onClick={onNavigate}
          aria-current={todosHere ? 'page' : undefined}
          aria-label={todoCount > 0 ? t('todos.navLabel', { count: todoCount }) : t('todos.title')}
          data-testid="sidebar-todos"
          className={cn(row, 'min-w-0 flex-1', todosHere && 'bg-accent-subtle')}
        >
          <ListChecks className="size-4 shrink-0 text-fg-muted" aria-hidden />
          <span className="min-w-0 flex-1 truncate">{t('todos.title')}</span>
          {todoCount > 0 ? (
            <span className="rounded-full bg-bg-muted px-2 py-0.5 text-xs font-medium text-fg-muted" data-testid="sidebar-todos-count" aria-hidden>
              {todoCount}
            </span>
          ) : null}
        </Link>
        <button type="button" onClick={() => openCapture()} aria-label={t('todos.addTitle')} title={`${t('todos.addTitle')} (C)`} className="grid place-items-center rounded-md p-1.5 text-fg-subtle hover:bg-bg-hover hover:text-fg [@media(pointer:coarse)]:size-11" data-testid="sidebar-capture">
          <Plus className="size-4" aria-hidden />
        </button>
      </div>
      ) : null}
      {rulesAvailable ? (
        <Link to="/rules" onClick={onNavigate} aria-current={rulesHere ? 'page' : undefined} data-testid="sidebar-rules" className={cn(row, rulesHere && 'bg-accent-subtle')}>
          <ScrollText className="size-4 shrink-0 text-fg-muted" aria-hidden />
          <span className="min-w-0 flex-1 truncate">{t('rules.title')}</span>
        </Link>
      ) : null}
    </nav>
  )
}

/**
 * 会话树和它上面的一排（节点过滤、新建项目、搜索）：桌面左栏的中间部分，也是手机「会话」标签页的内容。
 * Recent、项目树（带「待处理」小旗和未读小红点）、临时会话照旧。
 */
export function SessionsPanel({ nodeId, onNavigate, onSearch }: { nodeId: string | undefined; onNavigate?: () => void; onSearch?: () => void }) {
  const { t } = useTranslation()
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const list = useSessions((s) => s.list)
  const params = useParams({ strict: false }) as { node?: string; session?: string; project?: string }
  const { selected } = useNodeFilter()
  const newProject = useNewProjectAction(nodeId, selected, onNavigate)
  const paramOf = (id: string): string => shortNodeId(id, order)
  // 勾了多个节点时，项目行和 Recent 行要标出各自属于哪个节点
  const nodeLabel = (id: string): string | undefined => (selected.length > 1 ? nodes[id]?.name : undefined)

  const grouped = useMemo(
    () =>
      groupNodeSessions(
        selected.map((id) => ({ id, projects: nodes[id]?.projects ?? [], projectsLoaded: nodes[id]?.projectsLoaded })),
        Object.values(list),
      ),
    [list, selected, nodes],
  )

  const showRecent = usePrefs((s) => s.sidebarRecent)
  const recent = useRecentSessions(selected, showRecent)

  return (
    <>
      <div className="flex items-stretch gap-2 px-3 pb-2">
        <div className="min-w-0 flex-1">
          <SpaceSwitcher nodeId={nodeId} onNavigate={onNavigate} />
        </div>
        {newProject ? (
          <Button variant="outline" size="icon" className="!h-auto !w-10" aria-label={t('project.new')} title={t('project.new')} onClick={newProject} data-testid="sidebar-new-project-button">
            <FolderPlus className="size-4" />
          </Button>
        ) : null}
        {onSearch && order.length > 0 ? (
          <Button variant="outline" size="icon" className="!h-auto !w-10" aria-label={t('sidebar.search')} onClick={onSearch}>
            <Search className="size-4" />
          </Button>
        ) : null}
      </div>

      <nav className="scroll-thin min-h-0 flex-1 overflow-y-auto px-2 pb-2" aria-label={t('sidebar.sessions')}>
        {selected.length > 0 ? (
          <>
            {recent.length > 0 ? (
              <div className="mb-2" data-testid="sidebar-recent">
                <p className="px-2 py-1 text-xs font-semibold tracking-wide text-fg-subtle uppercase">{t('sidebar.recent')}</p>
                {recent.map((s) => (
                  <SessionRow key={s.id} nodeParam={paramOf(s.node_id)} nodeName={nodeLabel(s.node_id)} project={s.project ?? undefined} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={params.session === s.id} onNavigate={onNavigate} {...marksOf(s)} />
                ))}
              </div>
            ) : null}
            {grouped.projects.length > 0 ? (
              <div className="flex flex-col gap-2">
                {grouped.projects.map((g) => (
                  <ProjectBranch
                    key={`${g.nodeId}:${g.name}`}
                    nodeParam={paramOf(g.nodeId)}
                    nodeName={nodeLabel(g.nodeId)}
                    group={g}
                    activeNode={params.node}
                    activeSession={params.session}
                    activeProject={params.project}
                    onNavigate={onNavigate}
                  />
                ))}
              </div>
            ) : null}
            {grouped.scratch.length > 0 ? (
              <div className="mt-2">
                <p className="px-2 py-1 text-xs font-semibold tracking-wide text-fg-subtle uppercase">{t('sidebar.scratch')}</p>
                {grouped.scratch.map((s) => (
                  <SessionRow key={s.id} nodeParam={paramOf(s.node_id)} nodeName={nodeLabel(s.node_id)} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={params.session === s.id} onNavigate={onNavigate} {...marksOf(s)} />
                ))}
              </div>
            ) : null}
            {grouped.projects.length === 0 && grouped.scratch.length === 0 ? (
              <div className="px-2 py-6 text-center text-sm text-fg-subtle" data-testid="sidebar-empty">
                <p>{t('sidebar.empty')}</p>
                {/* 左栏没有单独的「新会话」按钮，没有项目就没有「＋」可点：直接在这里打开新建项目对话框 */}
                {newProject ? (
                  <button type="button" onClick={newProject} className="mt-2 inline-flex items-center gap-1.5 rounded-md px-2 py-1 font-medium text-accent-text hover:bg-bg-hover" data-testid="sidebar-new-project">
                    <FolderPlus className="size-4" />
                    {t('project.new')}
                  </button>
                ) : null}
              </div>
            ) : null}
          </>
        ) : (
          <p className="px-2 py-6 text-center text-sm text-fg-subtle">{t('sidebar.noNode')}</p>
        )}
      </nav>
    </>
  )
}

/**
 * 宽屏左栏没钉住时留在左边的窄条（像 VS Code 的活动栏）：Logo、搜索、新建项目、「待处理」（带「现在」的个数），底部是钉住按钮。
 * 悬停 / 点空白处浮出整个左栏的事件由调用方挂上（`useSidebarPeek` 的 `railProps`）。
 */
export function SidebarRail({ nodeId, onSearch, onPin, ...rest }: { nodeId: string | undefined; onSearch: () => void; onPin: () => void } & Omit<HTMLAttributes<HTMLDivElement>, 'children'>) {
  const { t } = useTranslation()
  const todoAvailable = useTodoAvailable()
  const hasNodes = useNodes((s) => s.order.length > 0)
  const { selected } = useNodeFilter()
  const newProject = useNewProjectAction(nodeId, selected)
  const count = useInboxNowCount()
  const here = useRouterState({ select: (s) => s.location.pathname })
  const icon = 'rounded-md p-2 text-fg-subtle hover:bg-bg-hover hover:text-fg'
  return (
    <div {...rest} className="relative z-10 flex w-12 shrink-0 flex-col items-center gap-1 border-r border-border bg-bg-subtle pt-3 pb-[max(0.5rem,env(safe-area-inset-bottom))]" data-testid="sidebar-rail">
      <Link to="/" className="mb-2 rounded-md p-1" aria-label="Anycon">
        <Logo withWord={false} />
      </Link>
      {hasNodes ? (
        <button type="button" aria-label={t('sidebar.search')} title={t('sidebar.search')} className={icon} onClick={onSearch}>
          <Search className="size-4" />
        </button>
      ) : null}
      {newProject ? (
        <button type="button" aria-label={t('project.new')} title={t('project.new')} className={icon} onClick={newProject}>
          <FolderPlus className="size-4" />
        </button>
      ) : null}
      {hasNodes ? (
        <Link
          to="/inbox"
          aria-label={count > 0 ? t('inbox.navLabel', { count }) : t('inbox.title')}
          aria-current={here === '/inbox' ? 'page' : undefined}
          title={t('inbox.title')}
          className={cn('relative', icon, here === '/inbox' && 'bg-accent-subtle')}
          data-testid="sidebar-rail-inbox"
        >
          <Inbox className="size-4" aria-hidden />
          {count > 0 ? (
            <span className="absolute -right-0.5 -top-0.5 min-w-4 rounded-full bg-warning px-1 text-center text-[0.625rem] leading-4 font-semibold text-accent-fg" data-testid="sidebar-rail-inbox-count" aria-hidden>
              {count > 99 ? '99+' : count}
            </span>
          ) : null}
        </Link>
      ) : null}
      {hasNodes && todoAvailable ? (
        <>
          <Link to="/todos" aria-label={t('todos.title')} title={t('todos.title')} className={icon} data-testid="sidebar-rail-todos">
            <ListChecks className="size-4" aria-hidden />
          </Link>
          <button type="button" aria-label={t('todos.addTitle')} title={t('todos.addTitle')} className={icon} onClick={() => useCapture.getState().openCapture()} data-testid="sidebar-rail-capture">
            <Plus className="size-4" aria-hidden />
          </button>
        </>
      ) : null}
      <PinButton pinned={false} onToggle={onPin} className="mt-auto p-2" />
    </div>
  )
}

function ProjectBranch({ nodeParam, nodeName, group, activeNode, activeSession, activeProject, onNavigate }: { nodeParam: string; nodeName?: string; group: NodeProjectGroup; activeNode?: string; activeSession?: string; activeProject?: string; onNavigate?: () => void }) {
  const { t } = useTranslation()
  const { name } = group
  const [open, setOpen] = useState(true)
  const [all, setAll] = useState(false)
  // 从清单项派出的会话收成一行「来自清单 N」（可展开）；外观设置里可以关掉。手动开的会话照旧逐条列
  const fold = usePrefs((s) => s.sidebarFoldTodo)
  const fromTodo = fold ? group.sessions.filter((s) => !!s.todo_id) : []
  const plain = fold ? group.sessions.filter((s) => !s.todo_id) : group.sessions
  const [foldOpen, setFoldOpen] = useState(false)
  // 正看着的会话在折叠里时展开它：不能让当前会话从列表里消失
  const activeInFold = !!activeSession && fromTodo.some((s) => s.id === activeSession)
  const shown = all ? plain : plain.slice(0, RECENT_PER_PROJECT)
  return (
    <div>
      <div className={cn('group flex items-center rounded-md', activeNode === nodeParam && activeProject === name && !activeSession && 'bg-bg-hover')}>
        <button type="button" aria-label={open ? t('sidebar.collapse') : t('sidebar.expand')} aria-expanded={open} className="rounded p-1 text-fg-subtle hover:text-fg" onClick={() => setOpen((v) => !v)}>
          {open ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
        </button>
        <Link to="/n/$node/p/$project" params={{ node: nodeParam, project: name }} onClick={onNavigate} className="flex min-w-0 flex-1 items-center gap-1.5 py-1.5 pr-2 text-sm font-medium">
          <FolderGit2 className="size-3.5 shrink-0 text-fg-subtle" />
          <span className="truncate">{name}</span>
          {nodeName ? <span className="max-w-[45%] shrink-0 truncate text-xs font-normal text-fg-subtle">{nodeName}</span> : null}
          {group.pending > 0 ? <Chip tone="warning">{group.pending}</Chip> : null}
        </Link>
        <Link
          to="/n/$node/new"
          params={{ node: nodeParam }}
          search={{ project: name }}
          onClick={onNavigate}
          aria-label={t('sidebar.newInProject', { project: name })}
          title={t('sidebar.newInProject', { project: name })}
          className="mr-1 shrink-0 rounded p-1 text-fg-subtle hover:bg-bg-hover hover:text-fg"
          data-testid="sidebar-project-new"
          data-project={name}
        >
          <Plus className="size-4" />
        </Link>
      </div>
      {open ? (
        <div className="ml-3 pl-1">
          {fromTodo.length > 0 ? (
            <div data-testid="sidebar-todo-fold" data-count={fromTodo.length}>
              <button type="button" aria-expanded={foldOpen || activeInFold} onClick={() => setFoldOpen((v) => !v)} className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm text-fg-muted hover:bg-bg-hover" data-testid="sidebar-todo-fold-toggle">
                {foldOpen || activeInFold ? <ChevronDown className="size-3.5 shrink-0" aria-hidden /> : <ChevronRight className="size-3.5 shrink-0" aria-hidden />}
                <span className="min-w-0 flex-1 truncate">{t('sidebar.fromTodo', { count: fromTodo.length })}</span>
                {fromTodo.some((s) => isActiveStatus(s.status)) ? <span className="size-2 shrink-0 rounded-full bg-accent" role="img" aria-label={t('sidebar.fromTodoRunning')} /> : null}
                {fromTodo.reduce((n, s) => n + s.pending_permissions, 0) > 0 ? <Chip tone="warning">{fromTodo.reduce((n, s) => n + s.pending_permissions, 0)}</Chip> : null}
              </button>
              {foldOpen || activeInFold ? (
                <div className="ml-3 pl-1">
                  {fromTodo.map((s) => (
                    <SessionRow key={s.id} nodeParam={nodeParam} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={activeSession === s.id} onNavigate={onNavigate} {...marksOf(s)} />
                  ))}
                </div>
              ) : null}
            </div>
          ) : null}
          {shown.map((s) => (
            <SessionRow key={s.id} nodeParam={nodeParam} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={activeSession === s.id} onNavigate={onNavigate} {...marksOf(s)} />
          ))}
          {!all && plain.length > RECENT_PER_PROJECT ? (
            <button type="button" className="px-2 py-1 text-xs text-fg-subtle hover:text-fg" onClick={() => setAll(true)}>
              {t('sidebar.showMore', { count: plain.length - RECENT_PER_PROJECT })}
            </button>
          ) : null}
        </div>
      ) : null}
    </div>
  )
}

/**
 * `project`：不在项目树里的行（Recent）带上所属项目，不然看不出这个会话是哪个项目的。
 * `unread`：未读小红点。`followUp`：标了「待处理」（空心 = 还在跑，实心 = 在等你）。
 */
function SessionRow({ nodeParam, nodeName, project, id, title, status, pending, branch, active, unread, followUp, onNavigate }: { nodeParam: string; nodeName?: string; project?: string; id: string; title: string; status: Parameters<typeof SessionDot>[0]['status']; pending: number; branch?: string; active: boolean; unread?: boolean; followUp?: 'armed' | 'ready'; onNavigate?: () => void }) {
  const { t } = useTranslation()
  const meta = [project, nodeName].filter(Boolean).join(' · ')
  return (
    <Link
      // 分支名放在悬停提示里（左栏窄，不占位置）
      title={branch ? `${title || id}\n⑂ ${branch}` : undefined}
      data-branch={branch}
      to="/n/$node/s/$session"
      params={{ node: nodeParam, session: id }}
      onClick={onNavigate}
      aria-current={active ? 'page' : undefined}
      className={cn('flex items-center gap-2 rounded-md px-2 py-1.5 text-sm hover:bg-bg-hover', active && 'bg-accent-subtle font-medium')}
    >
      <SessionDot status={status} />
      <span className={cn('min-w-0 flex-1 truncate', unread && !active && 'font-semibold')}>{title || id}</span>
      {meta ? <span className="max-w-[40%] shrink-0 truncate text-xs text-fg-subtle">{meta}</span> : null}
      {followUp ? <Flag className={cn('size-3 shrink-0 text-info', followUp === 'ready' && 'fill-current')} aria-label={followUp === 'ready' ? t('followUp.chipReady') : t('followUp.chipArmed')} data-testid="sidebar-follow-up-flag" /> : null}
      {unread && !active ? <UnreadDot /> : null}
      {pending > 0 ? <Chip tone="warning">{pending}</Chip> : null}
    </Link>
  )
}

export function AccountFooter({ onNavigate }: { onNavigate?: () => void }) {
  const { t } = useTranslation()
  const me = useMe()
  const navigate = useNavigate()
  const qc = useQueryClient()
  const hub = useHub((s) => s.snapshot)
  const hasNodes = useNodes((s) => s.order.length > 0)
  // 应用内扫码需要摄像头：没有（或不是安全上下文）就不显示入口
  const camera = useCameraAvailable()
  const [removeOpen, setRemoveOpen] = useState(false)
  const [signOutOpen, setSignOutOpen] = useState(false)

  // 普通退出：结束服务器会话、清内存、退订本浏览器的推送。设备 token、私钥、会话缓存都留在这个浏览器里（所以菜单里写明，退出后再提示一次）。
  const signOut = async (): Promise<void> => {
    try {
      await signOutOfHub()
    } catch (e) {
      // 没退成就别假装退了：cookie 还有效
      setSignOutOpen(false)
      toast.error(t('sidebar.signOutFailed', { error: describeError(e, t) }))
      return
    }
    setSignOutOpen(false)
    await resetClientState()
    qc.clear()
    await navigate({ to: '/login', search: {} })
    toast.info(t('sidebar.signedOutKept'))
  }

  // 底边距取 0.5rem 和安全区的较大值：单写 `pb-safe` 会把 `p-2` 的底边距盖成 0，账户按钮贴着侧栏底边
  return (
    <div className="border-t border-border p-2 pb-[max(0.5rem,env(safe-area-inset-bottom))]">
      <Menu>
        <MenuTrigger asChild>
          <button type="button" data-testid="account-menu" className="flex w-full items-center gap-2 rounded-md px-2 py-2 text-left text-sm hover:bg-bg-hover">
            <span className="grid size-7 shrink-0 place-items-center rounded-full bg-accent-subtle text-accent-text">
              <User className="size-4" />
            </span>
            <span className="min-w-0 flex-1">
              <span className="block truncate font-medium">{me.data?.display_name ?? '…'}</span>
              <span className="block truncate text-xs text-fg-subtle">{hub.deviceName}</span>
            </span>
            <ChevronDown className="size-4 text-fg-subtle" />
          </button>
        </MenuTrigger>
        <MenuContent align="start" className="w-64">
          <MenuItem icon={<Settings className="size-4" />} onSelect={() => { onNavigate?.(); void navigate({ to: '/settings' }) }}>
            {t('sidebar.accountSettings')}
          </MenuItem>
          <MenuItem icon={<Bell className="size-4" />} testId="notification-settings" onSelect={() => { onNavigate?.(); void navigate({ to: '/settings/notifications' }) }}>
            {t('sidebar.notificationSettings')}
          </MenuItem>
          {camera && hasNodes ? (
            <MenuItem
              icon={<ScanLine className="size-4" />}
              testId="scan-approve"
              onSelect={() => {
                onNavigate?.()
                useEnroll.getState().openScanner()
              }}
            >
              {t('sidebar.scanApprove')}
            </MenuItem>
          ) : null}
          {me.data?.is_admin ? (
            <MenuItem icon={<Shield className="size-4" />} onSelect={() => { onNavigate?.(); void navigate({ to: '/admin' }) }}>
              {t('sidebar.admin')}
            </MenuItem>
          ) : null}
          <MenuSeparator />
          <MenuItem icon={<LogOut className="size-4" />} testId="logout" hint={t('sidebar.signOutHint')} onSelect={() => setSignOutOpen(true)}>
            {t('sidebar.signOut')}
          </MenuItem>
          <MenuItem icon={<Trash2 className="size-4" />} danger testId="logout-remove" hint={t('sidebar.signOutRemoveHint')} onSelect={() => setRemoveOpen(true)}>
            {t('sidebar.signOutRemove')}
          </MenuItem>
        </MenuContent>
      </Menu>
      <SignOutDialog
        open={signOutOpen}
        onOpenChange={setSignOutOpen}
        onConfirm={signOut}
        onRemoveInstead={() => {
          setSignOutOpen(false)
          setRemoveOpen(true)
        }}
      />
      <RemoveDeviceDialog
        open={removeOpen}
        onOpenChange={setRemoveOpen}
        onDone={() => {
          // 整页跳转：内存里的 store、React Query 缓存、WebSocket 全部一起丢掉
          location.replace('/login')
        }}
      />
    </div>
  )
}
