import { useMemo, useState } from 'react'
import { Link, useNavigate, useParams } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Bell, ChevronDown, ChevronRight, FolderGit2, FolderPlus, Lock, LogOut, PanelLeftClose, Plus, ScanLine, Search, Server, Settings, Shield, Trash2, User } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices, useMe } from '@/api/queries'
import { describeError } from '@/lib/errors'
import { resetClientState, signOut as signOutOfHub } from '@/app/session'
import { useCameraAvailable } from '@/features/approve/useCameraAvailable'
import { accountNodes, lockedNodes, unlockParam } from '@/features/unlock/nodes'
import { groupNodeSessions, type NodeProjectGroup } from '@/features/session/group'
import { useEnroll } from '@/stores/enroll'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { connectionTone } from './useConnectionLabel'
import { SessionDot } from './StatusDot'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Menu, MenuCheckboxItem, MenuContent, MenuItem, MenuLabel, MenuSeparator, MenuTrigger } from '@/ui/Menu'
import { Chip, Dot } from '@/ui/Misc'
import { Logo } from './Logo'
import { RemoveDeviceDialog } from './RemoveDeviceDialog'
import { SignOutDialog } from './SignOutDialog'

const RECENT_PER_PROJECT = 5

/**
 * 左栏显示哪些节点：偏好里存的是用户勾选的节点（null = 全部）。勾选里已经没有存在的节点（节点被移除）时退回全部，
 * 免得左栏一片空白；至少留一个，取消最后一个勾选不生效。
 */
function useNodeFilter(): { selected: string[]; toggle: (id: string, on: boolean) => void } {
  const order = useNodes((s) => s.order)
  const saved = usePrefs((s) => s.sidebarNodes)
  const setPrefs = usePrefs((s) => s.set)
  const selected = useMemo(() => {
    const picked = saved ? order.filter((id) => saved.includes(id)) : order
    return picked.length > 0 ? picked : order
  }, [saved, order])
  const toggle = (id: string, on: boolean): void => {
    const next = order.filter((x) => (x === id ? on : selected.includes(x)))
    if (next.length === 0) return
    setPrefs({ sidebarNodes: next.length === order.length ? null : next })
  }
  return { selected, toggle }
}

/**
 * `nodeId`：页面当前所在的节点，只用于下拉里的「节点设置」；左栏显示哪些节点由下拉里的勾选决定。
 * `onCollapse`：宽屏常驻的左栏才传，Logo 旁边多一个收起按钮（手机抽屉自己有关闭按钮）。
 */
export function Sidebar({ nodeId, onNavigate, onSearch, onCollapse }: { nodeId: string | undefined; onNavigate?: () => void; onSearch?: () => void; onCollapse?: () => void }) {
  const { t } = useTranslation()
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const list = useSessions((s) => s.list)
  const params = useParams({ strict: false }) as { node?: string; session?: string; project?: string }
  const { selected } = useNodeFilter()
  // 空状态里「新建项目」去哪个节点：页面当前所在的节点（在勾选里的话），否则第一个勾选的
  const emptyTarget = nodeId && selected.includes(nodeId) ? nodeId : selected[0]
  const paramOf = (id: string): string => shortNodeId(id, order)
  // 勾了多个节点时，项目行和置顶行要标出各自属于哪个节点
  const nodeLabel = (id: string): string | undefined => (selected.length > 1 ? nodes[id]?.name : undefined)

  const grouped = useMemo(
    () =>
      groupNodeSessions(
        selected.map((id) => ({ id, projects: nodes[id]?.projects ?? [] })),
        Object.values(list),
      ),
    [list, selected, nodes],
  )

  // 待审批的会话在左栏置顶（spec/web.md § 7）
  const waiting = useMemo(
    () =>
      Object.values(list)
        .filter((x) => selected.includes(x.node_id) && !x.archived && (x.pending_permissions > 0 || x.status === 'waiting_permission'))
        .sort((a, b) => b.updated_at_ms - a.updated_at_ms),
    [list, selected],
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

      <div className="flex items-stretch gap-2 px-3 pb-2">
        <div className="min-w-0 flex-1">
          <NodeSwitcher nodeId={nodeId} onNavigate={onNavigate} />
        </div>
        {onSearch && order.length > 0 ? (
          <Button variant="outline" size="icon" className="!h-auto !w-10" aria-label={t('sidebar.search')} onClick={onSearch}>
            <Search className="size-4" />
          </Button>
        ) : null}
      </div>

      <nav className="scroll-thin min-h-0 flex-1 overflow-y-auto px-2 pb-2" aria-label={t('sidebar.sessions')}>
        {selected.length > 0 ? (
          <>
            {waiting.length > 0 ? (
              <div className="mb-2" data-testid="sidebar-waiting">
                <p className="px-2 py-1 text-xs font-semibold tracking-wide text-warning uppercase">{t('sidebar.waiting')}</p>
                {waiting.map((s) => (
                  <SessionRow key={s.id} nodeParam={paramOf(s.node_id)} nodeName={nodeLabel(s.node_id)} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={params.session === s.id} onNavigate={onNavigate} />
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
                  <SessionRow key={s.id} nodeParam={paramOf(s.node_id)} nodeName={nodeLabel(s.node_id)} id={s.id} title={s.title} status={s.status} pending={s.pending_permissions} branch={s.worktree?.branch} active={params.session === s.id} onNavigate={onNavigate} />
                ))}
              </div>
            ) : null}
            {grouped.projects.length === 0 && grouped.scratch.length === 0 ? (
              <div className="px-2 py-6 text-center text-sm text-fg-subtle" data-testid="sidebar-empty">
                <p>{t('sidebar.empty')}</p>
                {/* 左栏没有单独的「新会话」按钮，没有项目就没有「＋」可点：给一个去建项目的入口（节点首页的空状态里有新建 / 克隆 / 打开已有目录） */}
                {emptyTarget ? (
                  <Link to="/n/$node" params={{ node: paramOf(emptyTarget) }} onClick={onNavigate} className="mt-2 inline-flex items-center gap-1.5 rounded-md px-2 py-1 font-medium text-accent-text hover:bg-bg-hover" data-testid="sidebar-new-project">
                    <FolderPlus className="size-4" />
                    {t('project.new')}
                  </Link>
                ) : null}
              </div>
            ) : null}
          </>
        ) : (
          <p className="px-2 py-6 text-center text-sm text-fg-subtle">{t('sidebar.noNode')}</p>
        )}
      </nav>

      <AccountFooter onNavigate={onNavigate} />
    </div>
  )
}

function ProjectBranch({ nodeParam, nodeName, group, activeNode, activeSession, activeProject, onNavigate }: { nodeParam: string; nodeName?: string; group: NodeProjectGroup; activeNode?: string; activeSession?: string; activeProject?: string; onNavigate?: () => void }) {
  const { t } = useTranslation()
  const { name } = group
  const [open, setOpen] = useState(true)
  const [all, setAll] = useState(false)
  const shown = all ? group.sessions : group.sessions.slice(0, RECENT_PER_PROJECT)
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

function SessionRow({ nodeParam, nodeName, id, title, status, pending, branch, active, onNavigate }: { nodeParam: string; nodeName?: string; id: string; title: string; status: Parameters<typeof SessionDot>[0]['status']; pending: number; branch?: string; active: boolean; onNavigate?: () => void }) {
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
      {nodeName ? <span className="max-w-[40%] shrink-0 truncate text-xs text-fg-subtle">{nodeName}</span> : null}
      {pending > 0 ? <Chip tone="warning">{pending}</Chip> : null}
    </Link>
  )
}

/** 节点过滤：勾选哪些节点，左栏就显示这些节点上的所有项目；多选，点一下只切换勾选，菜单不关。 */
function NodeSwitcher({ nodeId, onNavigate }: { nodeId: string | undefined; onNavigate?: () => void }) {
  const { t } = useTranslation()
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const navigate = useNavigate()
  const { selected, toggle } = useNodeFilter()
  // 账号下、这个浏览器还没解锁的节点（ADR-36）：列在下拉底部，带锁图标，点了去 /unlock/:node
  const devices = useDevices()
  const account = accountNodes(devices.data)
  const locked = lockedNodes(devices.data, order)
  const only = selected.length === 1 ? nodes[selected[0]!] : undefined
  const label = only ? only.name : selected.length === order.length && selected.length > 0 ? t('sidebar.allNodes') : selected.length > 0 ? t('sidebar.nodesSelected', { count: selected.length, total: order.length }) : t('sidebar.chooseNode')
  return (
    <Menu>
      <MenuTrigger asChild>
        <button type="button" data-testid="node-switcher" className="flex w-full items-center gap-2 rounded-md border border-border-strong bg-bg px-2.5 py-2 text-left text-sm hover:bg-bg-hover">
          <Server className="size-4 shrink-0 text-fg-subtle" />
          <span className="min-w-0 flex-1 truncate font-medium">{label}</span>
          {only ? <Dot tone={connectionTone(only.conn)} /> : null}
          <ChevronDown className="size-4 shrink-0 text-fg-subtle" />
        </button>
      </MenuTrigger>
      <MenuContent className="w-[var(--radix-dropdown-menu-trigger-width)]">
        <MenuLabel>{t('sidebar.nodes')}</MenuLabel>
        {order.map((id) => {
          const n = nodes[id]
          if (!n) return null
          return (
            <MenuCheckboxItem key={id} testId="node-filter-item" checked={selected.includes(id)} onCheckedChange={(on) => toggle(id, on)} icon={<Dot tone={connectionTone(n.conn)} className="mt-1.5" />}>
              <span className="truncate">{n.name}</span>
            </MenuCheckboxItem>
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
            // 设置属于某一个节点：勾了几个节点时写明是哪个（页面当前所在的那个）
            hint={order.length > 1 ? nodes[nodeId]?.name : undefined}
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

function AccountFooter({ onNavigate }: { onNavigate?: () => void }) {
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
