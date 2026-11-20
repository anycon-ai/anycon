import { useNavigate } from '@tanstack/react-router'
import { Check, ChevronDown, Home, Lock, Plus, Server, ServerOff, Settings, Users } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useDevices } from '@/api/queries'
import { useInboxOutsideWhere } from '@/features/inbox/useInbox'
import { accountNodes, lockedNodes, removedNodes, unlockParam } from '@/features/unlock/nodes'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { useNodeFilter } from '@/lib/nodeFilter'
import { spaceLabelOf } from '@/lib/spaces'
import { useManageSpaces } from '@/stores/manageSpaces'
import { useNodes } from '@/stores/nodes'
import { cn } from '@/ui/cn'
import { Menu, MenuCheckboxItem, MenuContent, MenuItem, MenuLabel, MenuSeparator, MenuTrigger } from '@/ui/Menu'
import { Dot } from '@/ui/Misc'
import { connectionTone } from './useConnectionLabel'

/**
 * 空间切换（spec/web.md § 4.13）。没有任何节点带分组时就是原来的节点过滤器（一模一样：勾选哪些节点，左栏就显示这些节点上的所有项目；
 * 多选，点一下只切换勾选，菜单不关）。有分组之后菜单最上面多出「全部」和各个空间，选一个空间就选中它的所有节点；下面照旧可以单独勾选节点。
 * `variant`：`sidebar` 是桌面左栏 / 手机「会话」页里的整行按钮；`topbar` 是手机各个标签页根页面顶栏里的紧凑按钮（只在有分组时出现）。
 */
export function SpaceSwitcher({ nodeId, onNavigate, variant = 'sidebar' }: { nodeId: string | undefined; onNavigate?: () => void; variant?: 'sidebar' | 'topbar' }) {
  const { t } = useTranslation()
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  const navigate = useNavigate()
  const { selected, space, spaces, toggle, selectSpace, selectAll } = useNodeFilter()
  const openManage = useManageSpaces((s) => s.show)
  // 账号下、这个浏览器还没解锁的节点（ADR-36）：列在下拉底部，带锁图标，点了去 /unlock/:node
  const devices = useDevices()
  const account = accountNodes(devices.data)
  const locked = lockedNodes(devices.data, order)
  // 这个浏览器里有、账号下已经没有的节点（在别的设备上被移除了）：点了去它的页面，那里可以忘记它
  const removed = removedNodes(
    devices.data,
    devices.dataUpdatedAt ?? 0,
    order.map((id) => ({ id, pairedAt: nodes[id]?.pairedAt ?? Number.POSITIVE_INFINITY })),
  )
  // 没选中的节点上有等你的事：按钮上一个小点（带文字说明，不只靠颜色）；有分组时说明是哪些空间里有几件
  const where = useInboxOutsideWhere()
  const outside = where.reduce((n, w) => n + w.count, 0)
  const only = selected.length === 1 ? nodes[selected[0]!] : undefined
  const allSelected = selected.length === order.length && selected.length > 0
  const currentSpace = space !== null ? spaces.spaces.find((x) => x.key === space) : undefined
  const label = currentSpace
    ? currentSpace.label
    : only
      ? cleanHintText(only.name) || only.id.slice(0, 8)
      : allSelected
        ? t(spaces.hasGroups ? 'spaces.all' : 'sidebar.allNodes')
        : selected.length > 0
          ? t('sidebar.nodesSelected', { count: selected.length, total: order.length })
          : t('sidebar.chooseNode')
  const dotText = spaces.hasGroups ? t('spaces.otherWaiting', { count: outside, where: where.map((w) => `${w.label}: ${w.count}`).join(', ') }) : t('sidebar.otherNodesWaiting', { count: outside })
  const dot = outside > 0 ? <span role="img" aria-label={dotText} title={dotText} className="size-2.5 shrink-0 rounded-full bg-warning ring-2 ring-bg" data-testid="node-filter-dot" /> : null
  return (
    <Menu>
      <MenuTrigger asChild>
        {variant === 'topbar' ? (
          <button type="button" data-testid="space-topbar" aria-label={t('spaces.switchLabel', { space: label })} className="flex min-h-11 max-w-[45%] shrink-0 items-center gap-1.5 rounded-md px-2 text-sm font-medium hover:bg-bg-hover md:hidden">
            <Users className="size-4 shrink-0 text-fg-subtle" aria-hidden />
            <span className="min-w-0 truncate" data-testid="space-topbar-label">
              {label}
            </span>
            {dot}
            <ChevronDown className="size-4 shrink-0 text-fg-subtle" aria-hidden />
          </button>
        ) : (
          <button type="button" data-testid="node-switcher" className="flex w-full items-center gap-2 rounded-md border border-border-strong bg-bg px-2.5 py-2 text-left text-sm hover:bg-bg-hover">
            {spaces.hasGroups ? <Users className="size-4 shrink-0 text-fg-subtle" /> : <Server className="size-4 shrink-0 text-fg-subtle" />}
            <span className="min-w-0 flex-1 truncate font-medium" data-testid="space-label">
              {label}
            </span>
            {only && !currentSpace ? <Dot tone={connectionTone(only.conn)} /> : null}
            {dot}
            <ChevronDown className="size-4 shrink-0 text-fg-subtle" />
          </button>
        )}
      </MenuTrigger>
      <MenuContent className={cn(variant === 'topbar' ? 'w-72' : 'w-[var(--radix-dropdown-menu-trigger-width)]')}>
        {spaces.hasGroups ? (
          <>
            <MenuLabel>{t('spaces.title')}</MenuLabel>
            <MenuItem testId="space-all" icon={allSelected && space === null ? <Check className="size-4" aria-hidden /> : <span className="size-4" />} onSelect={selectAll}>
              {t('spaces.all')}
            </MenuItem>
            {spaces.spaces.map((sp) => (
              <MenuItem key={sp.key} testId="space-item" icon={space === sp.key ? <Check className="size-4" aria-hidden /> : <span className="size-4" />} hint={t('spaces.nodeCount', { count: sp.nodeIds.length })} onSelect={() => selectSpace(sp.key)}>
                <span className="truncate">{sp.label}</span>
              </MenuItem>
            ))}
            <MenuSeparator />
          </>
        ) : null}
        <MenuLabel>{t('sidebar.nodes')}</MenuLabel>
        {order.map((id) => {
          const n = nodes[id]
          if (!n) return null
          const sp = spaceLabelOf(spaces, id)
          return (
            <MenuCheckboxItem key={id} testId="node-filter-item" checked={selected.includes(id)} onCheckedChange={(on) => toggle(id, on)} icon={<Dot tone={connectionTone(n.conn)} className="mt-1.5" />}>
              <span className="truncate">{cleanHintText(n.name) || id.slice(0, 8)}</span>
              {sp ? <span className="ml-2 shrink-0 text-xs text-fg-subtle">{sp}</span> : null}
            </MenuCheckboxItem>
          )
        })}
        {spaces.hasGroups ? (
          <MenuItem testId="space-manage" icon={<Users className="size-4" />} onSelect={openManage}>
            {t('spaces.manage')}
          </MenuItem>
        ) : null}
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
        {removed.length > 0 ? (
          <>
            <MenuSeparator />
            <MenuLabel>{t('sidebar.removed')}</MenuLabel>
            {removed.map((id) => (
              <MenuItem
                key={id}
                testId="node-removed"
                icon={<ServerOff className="mt-0.5 size-4 text-fg-subtle" aria-hidden />}
                hint={t('sidebar.removedHint')}
                onSelect={() => {
                  onNavigate?.()
                  void navigate({ to: '/n/$node', params: { node: shortNodeId(id, order) } })
                }}
              >
                <span className="truncate">{cleanHintText(nodes[id]?.name) || id.slice(0, 8)}</span>
              </MenuItem>
            ))}
          </>
        ) : null}
        <MenuSeparator />
        {nodeId ? (
          <MenuItem
            icon={<Home className="size-4" />}
            testId="node-home"
            // 节点首页（项目、临时会话、最近会话）：首页换成「待处理」之后从这里进
            hint={order.length > 1 ? cleanHintText(nodes[nodeId]?.name) : undefined}
            onSelect={() => {
              onNavigate?.()
              void navigate({ to: '/n/$node', params: { node: shortNodeId(nodeId, order) } })
            }}
          >
            {t('sidebar.nodeHome')}
          </MenuItem>
        ) : null}
        {nodeId ? (
          <MenuItem
            icon={<Settings className="size-4" />}
            // 设置属于某一个节点：勾了几个节点时写明是哪个（页面当前所在的那个）
            hint={order.length > 1 ? cleanHintText(nodes[nodeId]?.name) : undefined}
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
