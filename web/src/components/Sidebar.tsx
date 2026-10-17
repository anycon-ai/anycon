import { useMemo, useState } from 'react'
import { Link, useNavigate, useParams } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Bell, Check, ChevronDown, ChevronRight, FolderGit2, Lock, LogOut, PanelLeftClose, Plus, ScanLine, Search, Server, Settings, Shield, Trash2, User } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices, useMe } from '@/api/queries'
import { describeError } from '@/lib/errors'
import { resetClientState, signOut as signOutOfHub } from '@/app/session'
import { useCameraAvailable } from '@/features/approve/useCameraAvailable'
import { accountNodes, lockedNodes, unlockParam } from '@/features/unlock/nodes'
import { groupSessions } from '@/features/session/group'
import { useEnroll } from '@/stores/enroll'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { connectionTone } from './useConnectionLabel'
import { SessionDot } from './StatusDot'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Menu, MenuContent, MenuItem, MenuLabel, MenuSeparator, MenuTrigger } from '@/ui/Menu'
import { Chip, Dot } from '@/ui/Misc'
import { Logo } from './Logo'
import { RemoveDeviceDialog } from './RemoveDeviceDialog'
import { SignOutDialog } from './SignOutDialog'

const RECENT_PER_PROJECT = 5

/** `onCollapse`：宽屏常驻的左栏才传，Logo 旁边多一个收起按钮（手机抽屉自己有关闭按钮）。 */
export function Sidebar({ nodeId, onNavigate, onSearch, onCollapse }: { nodeId: string | undefined; onNavigate?: () => void; onSearch?: () => void; onCollapse?: () => void }) {
  const { t } = useTranslation()
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const list = useSessions((s) => s.list)
  const params = useParams({ strict: false }) as { session?: string; project?: string }
  const navigate = useNavigate()
  const entry = nodeId ? nodes[nodeId] : undefined
  const param = nodeId ? shortNodeId(nodeId, order) : ''

  const grouped = useMemo(() => {
    if (!nodeId) return { projects: [], scratch: [] }
    return groupSessions(
      Object.values(list).filter((s) => s.node_id === nodeId),
      entry?.projects ?? [],
    )
  }, [list, nodeId, entry?.projects])

  // 待审批的会话在左栏置顶（spec/web.md § 7）
  const waiting = useMemo(
    () =>
      Object.values(list)
        .filter((x) => x.node_id === nodeId && !x.archived && (x.pending_permissions > 0 || x.status === 'waiting_permission'))
        .sort((a, b) => b.updated_at_ms - a.updated_at_ms),
    [list, nodeId],
  )

  return (
    <div className="flex h-full min-h-0 flex-col bg-bg-subtle" data-testid="sidebar">
      <div className="flex items-center gap-2 px-3 pb-2 pt-3">
        <Link to="/" onClick={onNavigate} className="mr-auto rounded-md px-1 py-1">
          <Logo />
        </Link>
        {onCollapse ? (
          <button type="button" aria-label={t('sidebar.collapseSidebar')} title={t('sidebar.collapseSidebar')} className="rounded-md p-1.5 text-fg-subtle hover:bg-bg-hover hover:text-fg" onClick={onCollapse} data-testid="sidebar-collapse">
            <PanelLeftClose className="size-4" />
          </button>
        ) : null}
      </div>

      <div className="px-3 pb-2">
        <NodeSwitcher nodeId={nodeId} onNavigate={onNavigate} />
      </div>

      {nodeId ? (
        <div className="flex gap-2 px-3 pb-2">
          <Button
            variant="primary"
            size="sm"
            className="flex-1"
            icon={<Plus className="size-4" />}
            onClick={() => {
              onNavigate?.()
              void navigate({ to: '/n/$node/new', params: { node: param }, search: { project: params.project } })
            }}
            data-testid="sidebar-new-session"
          >
            {t('sidebar.newSession')}
          </Button>
          {onSearch ? (
            <Button variant="outline" size="icon" className="!h-8 !w-8" aria-label={t('sidebar.search')} onClick={onSearch}>
              <Search className="size-4" />
            </Button>
          ) : null}
        </div>
      ) : null}

      <nav className="scroll-thin min-h-0 flex-1 overflow-y-auto px-2 pb-2" aria-label={t('sidebar.sessions')}>
        {nodeId ? (
          <>
            {waiting.length > 0 ? (
              <div className="mb-2" data-testid="sidebar-waiting">
                <p className="px-2 py-1 text-xs font-semibold tracking-wide text-warning uppercase">{t('sidebar.waiting')}</p>
                {waiting.map((s) => (
                  <SessionRow key={s.id} nodeParam={param} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={params.session === s.id} onNavigate={onNavigate} />
                ))}
              </div>
            ) : null}
            {grouped.projects.length > 0 ? (
              <div className="flex flex-col gap-2">
                {grouped.projects.map((g) => (
                  <ProjectBranch key={g.name} nodeParam={param} name={g.name} group={g} activeSession={params.session} activeProject={params.project} onNavigate={onNavigate} />
                ))}
              </div>
            ) : null}
            {grouped.scratch.length > 0 ? (
              <div className="mt-2">
                <p className="px-2 py-1 text-xs font-semibold tracking-wide text-fg-subtle uppercase">{t('sidebar.scratch')}</p>
                {grouped.scratch.map((s) => (
                  <SessionRow key={s.id} nodeParam={param} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={params.session === s.id} onNavigate={onNavigate} />
                ))}
              </div>
            ) : null}
            {grouped.projects.length === 0 && grouped.scratch.length === 0 ? <p className="px-2 py-6 text-center text-sm text-fg-subtle">{t('sidebar.empty')}</p> : null}
          </>
        ) : (
          <p className="px-2 py-6 text-center text-sm text-fg-subtle">{t('sidebar.noNode')}</p>
        )}
      </nav>

      <AccountFooter nodeParam={param || undefined} onNavigate={onNavigate} />
    </div>
  )
}

function ProjectBranch({ nodeParam, name, group, activeSession, activeProject, onNavigate }: { nodeParam: string; name: string; group: ReturnType<typeof groupSessions>['projects'][number]; activeSession?: string; activeProject?: string; onNavigate?: () => void }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(true)
  const [all, setAll] = useState(false)
  const shown = all ? group.sessions : group.sessions.slice(0, RECENT_PER_PROJECT)
  return (
    <div>
      <div className={cn('group flex items-center rounded-md', activeProject === name && !activeSession && 'bg-bg-hover')}>
        <button type="button" aria-label={open ? t('sidebar.collapse') : t('sidebar.expand')} aria-expanded={open} className="rounded p-1 text-fg-subtle hover:text-fg" onClick={() => setOpen((v) => !v)}>
          {open ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
        </button>
        <Link to="/n/$node/p/$project" params={{ node: nodeParam, project: name }} onClick={onNavigate} className="flex min-w-0 flex-1 items-center gap-1.5 py-1.5 pr-2 text-sm font-medium">
          <FolderGit2 className="size-3.5 shrink-0 text-fg-subtle" />
          <span className="truncate">{name}</span>
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
        <div className="ml-3 border-l border-border pl-1">
          {shown.map((s) => (
            <SessionRow key={s.id} nodeParam={nodeParam} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={activeSession === s.id} onNavigate={onNavigate} />
          ))}
          {!all && group.sessions.length > RECENT_PER_PROJECT ? (
            <button type="button" className="px-2 py-1 text-xs text-fg-subtle hover:text-fg" onClick={() => setAll(true)}>
              {t('sidebar.showMore', { count: group.sessions.length - RECENT_PER_PROJECT })}
            </button>
          ) : null}
        </div>
      ) : null}
    </div>
  )
}

function SessionRow({ nodeParam, id, title, status, pending, branch, active, onNavigate }: { nodeParam: string; id: string; title: string; status: Parameters<typeof SessionDot>[0]['status']; pending: number; branch?: string; active: boolean; onNavigate?: () => void }) {
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
      <span className="min-w-0 flex-1 truncate">{title || id}</span>
      {pending > 0 ? <Chip tone="warning">{pending}</Chip> : null}
    </Link>
  )
}

function NodeSwitcher({ nodeId, onNavigate }: { nodeId: string | undefined; onNavigate?: () => void }) {
  const { t } = useTranslation()
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const navigate = useNavigate()
  const current = nodeId ? nodes[nodeId] : undefined
  // 账号下、这个浏览器还没解锁的节点（ADR-36）：列在下拉底部，带锁图标，点了去 /unlock/:node
  const devices = useDevices()
  const account = accountNodes(devices.data)
  const locked = lockedNodes(devices.data, order)
  return (
    <Menu>
      <MenuTrigger asChild>
        <button type="button" data-testid="node-switcher" className="flex w-full items-center gap-2 rounded-md border border-border-strong bg-bg px-2.5 py-2 text-left text-sm hover:bg-bg-hover">
          <Server className="size-4 shrink-0 text-fg-subtle" />
          <span className="min-w-0 flex-1 truncate font-medium">{current?.name ?? t('sidebar.chooseNode')}</span>
          {current ? <Dot tone={connectionTone(current.conn)} /> : null}
          <ChevronDown className="size-4 shrink-0 text-fg-subtle" />
        </button>
      </MenuTrigger>
      <MenuContent className="w-[var(--radix-dropdown-menu-trigger-width)]">
        <MenuLabel>{t('sidebar.nodes')}</MenuLabel>
        {order.map((id) => {
          const n = nodes[id]
          if (!n) return null
          return (
            <MenuItem
              key={id}
              onSelect={() => {
                onNavigate?.()
                void navigate({ to: '/n/$node', params: { node: shortNodeId(id, order) } })
              }}
              icon={<Dot tone={connectionTone(n.conn)} className="mt-1.5" />}
            >
              <span className="flex items-center gap-1">
                <span className="truncate">{n.name}</span>
                {id === nodeId ? <Check className="size-3.5 text-accent-text" /> : null}
              </span>
            </MenuItem>
          )
        })}
        {locked.length > 0 ? (
          <>
            <MenuSeparator />
            <MenuLabel>{t('sidebar.locked')}</MenuLabel>
            {locked.map((n) => (
              <MenuItem
                key={n.id}
                testId="node-locked"
                icon={<Lock className="mt-0.5 size-4 text-fg-subtle" aria-hidden />}
                hint={t('sidebar.lockedHint')}
                onSelect={() => {
                  onNavigate?.()
                  void navigate({ to: '/unlock/$node', params: { node: unlockParam(n, account) } })
                }}
              >
                <span className="truncate">{cleanHintText(n.name) || n.id.slice(0, 8)}</span>
              </MenuItem>
            ))}
          </>
        ) : null}
        <MenuSeparator />
        {nodeId ? (
          <MenuItem
            icon={<Settings className="size-4" />}
            onSelect={() => {
              onNavigate?.()
              void navigate({ to: '/n/$node/settings', params: { node: shortNodeId(nodeId, order) } })
            }}
          >
            {t('sidebar.nodeSettings')}
          </MenuItem>
        ) : null}
        <MenuItem
          icon={<Plus className="size-4" />}
          onSelect={() => {
            onNavigate?.()
            void navigate({ to: '/pair' })
          }}
          testId="node-add"
        >
          {t('sidebar.addNode')}
        </MenuItem>
      </MenuContent>
    </Menu>
  )
}

function AccountFooter({ nodeParam, onNavigate }: { nodeParam?: string; onNavigate?: () => void }) {
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
  void nodeParam

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
