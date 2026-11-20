import { useEffect, useMemo, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { ChevronRight, ListChecks, Plus } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { setTodoState } from '@/features/todos/actions'
import { useCapture } from '@/features/todos/capture'
import { flatOrder, groupTodos, NO_FILTERS, projectsOf, type SourceFilter, type TodoFilters } from '@/features/todos/list'
import { deleteTodo, loadClosed, setClosedExpanded } from '@/features/todos/service'
import { useSupportingNodes, useTodoView, useTodos } from '@/features/todos/store'
import { TodoDetail } from '@/features/todos/ui/TodoDetail'
import { TodoRow } from '@/features/todos/ui/TodoRow'
import { isTypingTarget } from '@/features/session/shortcuts'
import { overlayOpen } from '@/features/inbox/useInboxKeys'
import { shortNodeId } from '@/lib/ids'
import { useNodeFilter } from '@/lib/nodeFilter'
import { useIsWide } from '@/lib/useMediaQuery'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Select } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { EmptyState, Spinner } from '@/ui/Misc'
import { cn } from '@/ui/cn'
import { cleanOneLine } from '@/features/todos/validate'
import type { ViewTodo } from '@/features/todos/types'

const key = (t: Pick<ViewTodo, 'node_id' | 'id'>): string => `${t.node_id}:${t.id}`

/**
 * 清单页（`/todos`、`/todos/:node/:todo`，spec/web.md § 4.12）：勾选的、支持清单的节点合在一个列表里，分三组——处理中、还没开始、
 * 已关闭（折叠，展开才向节点要，一页页往下翻）。桌面左边列表右边详情；手机上是列表，点一行进二级页。
 * `selected`：地址里的那一项。`active`：页面此刻在眼前（手机上标签页常驻、藏起来时不处理键）。
 */
export function TodosPage({ selected, active = true }: { selected?: { node: string; todo: string }; active?: boolean }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const wide = useIsWide()
  const { selected: pickedNodes } = useNodeFilter()
  const supporting = useSupportingNodes(pickedNodes)
  const order = useNodes((s) => s.order)
  const nodes = useNodes((s) => s.byId)
  const view = useTodoView(supporting)
  const nodeState = useTodos((s) => s.nodes)
  const [filters, setFilters] = useState<TodoFilters>(NO_FILTERS)
  const [multi, setMulti] = useState(false)
  const [checked, setChecked] = useState<ReadonlySet<string>>(new Set())
  const [cursor, setCursor] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<ViewTodo | null>(null)

  const groups = useMemo(() => groupTodos(view, filters), [view, filters])
  const closedExpanded = supporting.some((id) => nodeState[id]?.closed.expanded)
  const flat = useMemo(() => flatOrder(groups, closedExpanded), [groups, closedExpanded])
  const projects = useMemo(() => projectsOf(view), [view])
  const showNode = supporting.length > 1
  const hydrated = supporting.every((id) => nodeState[id]?.hydrated)
  const cachedNodes = supporting.filter((id) => nodes[id]?.conn !== 'ready').map((id) => nodes[id]?.name ?? id.slice(0, 8))
  const detail = selected ? view.find((v) => v.id === selected.todo && shortNodeId(v.node_id, order) === selected.node) : undefined
  const online = (id: string): boolean => nodes[id]?.conn === 'ready'

  const open = (item: ViewTodo): void => void navigate({ to: '/todos/$node/$todo', params: { node: shortNodeId(item.node_id, order), todo: item.id } })
  const targets = (item: ViewTodo): ViewTodo[] => (multi && checked.size > 0 ? flat.filter((x) => checked.has(key(x))) : [item])

  // 键盘（桌面，spec/web.md § 14.11）：J / K 移动，E 完成，Enter 打开，X 多选
  useEffect(() => {
    if (!active) return
    const onKey = (e: KeyboardEvent): void => {
      if (e.metaKey || e.ctrlKey || e.altKey || isTypingTarget(e.target) || overlayOpen()) return
      const k = e.key.toLowerCase()
      const i = Math.max(0, flat.findIndex((x) => key(x) === cursor))
      const cur = flat[i]
      if (k === 'j' || k === 'k') {
        e.preventDefault()
        const next = flat[Math.min(flat.length - 1, Math.max(0, cursor === null ? 0 : i + (k === 'j' ? 1 : -1)))]
        if (next) {
          setCursor(key(next))
          document.querySelector<HTMLElement>(`[data-id="${CSS.escape(next.id)}"] [data-todo-focus]`)?.focus()
        }
      } else if (k === 'e' && cur) {
        e.preventDefault()
        for (const x of targets(cur)) void setTodoState(x, 'done', { undo: true })
        setChecked(new Set())
      } else if (k === 'x' && cur) {
        e.preventDefault()
        setMulti(true)
        setChecked((c) => {
          const n = new Set(c)
          if (n.has(key(cur))) n.delete(key(cur))
          else n.add(key(cur))
          return n
        })
      } else if (k === 'escape' && multi) {
        setMulti(false)
        setChecked(new Set())
      } else if (k === 'enter' && cur && !(e.target as HTMLElement | null)?.closest('button, a, input, select, textarea')) {
        e.preventDefault()
        open(cur)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, flat, cursor, multi, checked])

  const header = (
    <PageHeader
      root
      crumbs={[{ label: t('todos.title') }]}
      showNodeStatus={false}
      actions={
        <Button size="sm" onClick={() => useCapture.getState().openCapture()} icon={<Plus className="size-4" aria-hidden />} data-testid="todos-add">
          {t('todos.addTitle')}
        </Button>
      }
    />
  )

  if (supporting.length === 0) {
    return (
      <div className="flex min-h-0 flex-1 flex-col" data-testid="todos-page">
        {header}
        <EmptyState icon={<ListChecks className="size-8" aria-hidden />} title={t('todos.unsupportedTitle')}>
          {t('todos.noSupport')}
        </EmptyState>
      </div>
    )
  }

  const row = (item: ViewTodo) => (
    <TodoRow
      key={key(item)}
      item={item}
      online={online(item.node_id)}
      {...(showNode ? { nodeName: nodes[item.node_id]?.name } : {})}
      selected={detail ? key(detail) === key(item) : cursor === key(item)}
      multi={multi}
      checked={checked.has(key(item))}
      onOpen={() => {
        setCursor(key(item))
        open(item)
      }}
      onToggleCheck={() =>
        setChecked((c) => {
          const n = new Set(c)
          if (n.has(key(item))) n.delete(key(item))
          else n.add(key(item))
          return n
        })
      }
      onDone={(undo) => void setTodoState(item, 'done', { undo })}
      onReopen={() => void setTodoState(item, 'open')}
      onDrop={() => void setTodoState(item, 'dropped', { undo: true })}
      onDelete={() => setDeleting(item)}
    />
  )

  const section = (id: 'working' | 'open', label: string) =>
    groups[id].length > 0 ? (
      <section aria-labelledby={`todos-${id}`} data-testid={`todos-group-${id}`}>
        <h2 id={`todos-${id}`} className="mb-2 flex items-center gap-2 px-1 text-xs font-semibold tracking-wide text-fg-muted uppercase">
          {label}
          <span className="rounded-full bg-bg-muted px-1.5 py-0.5 text-[0.6875rem] font-medium normal-case">{groups[id].length}</span>
        </h2>
        <ul className="space-y-2">{groups[id].map(row)}</ul>
      </section>
    ) : null

  const closedMore = supporting.some((id) => nodeState[id]?.closed.hasMore)
  const closedLoading = supporting.some((id) => nodeState[id]?.closed.loading)
  const empty = flat.length === 0 && groups.closed.length === 0
  const list = (
    <div className="mx-auto max-w-3xl space-y-4 p-3 sm:p-4">
      {cachedNodes.length > 0 ? (
        <p className="rounded-md border border-border bg-bg-subtle px-3 py-2 text-sm text-fg-muted" role="status" data-testid="todos-cached">
          {t('todos.cached', { nodes: cachedNodes.join(', ') })}
        </p>
      ) : null}
      <div className="flex flex-wrap items-center gap-2" data-testid="todos-filters">
        <Select aria-label={t('todos.filterProject')} value={filters.project ?? ''} onChange={(e) => setFilters((f) => ({ ...f, project: e.target.value || null }))} className="w-auto max-w-44" data-testid="todos-filter-project">
          <option value="">{t('todos.allProjects')}</option>
          {projects.map((p) => (
            <option key={p} value={p}>
              {cleanOneLine(p)}
            </option>
          ))}
        </Select>
        <Select aria-label={t('todos.filterSource')} value={filters.source} onChange={(e) => setFilters((f) => ({ ...f, source: e.target.value as SourceFilter }))} className="w-auto" data-testid="todos-filter-source">
          <option value="all">{t('todos.sourceAll')}</option>
          <option value="mine">{t('todos.sourceMine')}</option>
          <option value="agent">{t('todos.sourceAgent')}</option>
        </Select>
        <Button size="sm" variant={multi ? 'primary' : 'outline'} aria-pressed={multi} onClick={() => { setMulti((m) => !m); setChecked(new Set()) }} data-testid="todos-multi">
          {t('todos.select_')}
        </Button>
      </div>
      {multi ? (
        <div className="flex flex-wrap items-center gap-2 rounded-md border border-border bg-bg-subtle px-3 py-2 text-sm" role="toolbar" aria-label={t('todos.bulk')} data-testid="todos-bulk">
          <span>{t('todos.selectedCount', { count: checked.size })}</span>
          {(['open', 'working', 'done', 'dropped'] as const).map((s) => (
            <Button
              key={s}
              size="sm"
              disabled={checked.size === 0}
              onClick={() => {
                for (const x of flat.filter((v) => checked.has(key(v)))) void setTodoState(x, s)
                setChecked(new Set())
              }}
              data-testid={`todos-bulk-${s}`}
            >
              {t(`todos.states.${s}`)}
            </Button>
          ))}
        </div>
      ) : null}
      {!hydrated ? <Spinner className="mx-auto my-8 size-6" /> : null}
      {hydrated && empty ? (
        <EmptyState icon={<ListChecks className="size-8" aria-hidden />} title={t('todos.empty')}>
          {t('todos.emptyBody')}
        </EmptyState>
      ) : null}
      {section('working', t('todos.groups.working'))}
      {section('open', t('todos.groups.open'))}
      <section data-testid="todos-group-closed">
        <h2 className="text-sm">
          <button
            type="button"
            aria-expanded={closedExpanded}
            onClick={() => supporting.forEach((id) => setClosedExpanded(id, !closedExpanded))}
            className="flex min-h-11 w-full items-center gap-2 rounded-lg px-2 text-left text-sm text-fg-muted hover:bg-bg-hover"
            data-testid="todos-closed-toggle"
          >
            <ChevronRight className={cn('size-4 transition-transform', closedExpanded && 'rotate-90')} aria-hidden />
            <span className="font-medium">{t('todos.groups.closed')}</span>
            {closedExpanded ? <span className="rounded-full bg-bg-muted px-1.5 py-0.5 text-xs">{groups.closed.length}</span> : null}
          </button>
        </h2>
        {closedExpanded ? (
          <div className="mt-1 space-y-2">
            <ul className="space-y-2">{groups.closed.map(row)}</ul>
            {closedLoading ? <Spinner className="mx-auto size-5" /> : null}
            {closedMore && !closedLoading ? (
              <Button size="sm" onClick={() => supporting.forEach((id) => void loadClosed(id, { more: true }))} data-testid="todos-closed-more">
                {t('todos.loadMore')}
              </Button>
            ) : null}
          </div>
        ) : null}
      </section>
    </div>
  )

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="todos-page" data-mode={wide ? 'split' : 'single'}>
      {header}
      <div className="flex min-h-0 flex-1">
        <div className={cn('scroll-thin min-h-0 min-w-0 flex-1 overflow-y-auto', wide && 'max-w-[26rem] shrink-0 border-r border-border xl:w-[380px] xl:flex-none')} data-testid="todos-list">
          {list}
        </div>
        {wide ? (
          <div className="scroll-thin min-w-0 flex-1 overflow-y-auto p-6" data-testid="todos-detail-pane">
            <div className="mx-auto max-w-2xl">{detail ? <TodoDetail key={key(detail)} item={detail} online={online(detail.node_id)} onDeleted={() => void navigate({ to: '/todos' })} /> : <p className="text-sm text-fg-subtle">{selected ? t('todos.gone') : t('todos.pickOne')}</p>}</div>
          </div>
        ) : null}
      </div>
      <Dialog open={!!deleting} onOpenChange={(o) => !o && setDeleting(null)} title={t('todos.deleteTitle')} description={deleting ? t('todos.deleteBody', { title: cleanOneLine(deleting.title, 80) }) : ''} closeLabel={t('common.close')} testId="todo-delete-dialog">
        <div className="flex justify-end gap-2">
          <Button onClick={() => setDeleting(null)}>{t('common.cancel')}</Button>
          <Button
            variant="danger"
            onClick={() => {
              if (deleting) void deleteTodo(deleting.node_id, deleting.id)
              setDeleting(null)
            }}
            data-testid="todo-delete-confirm"
          >
            {t('todos.delete')}
          </Button>
        </div>
      </Dialog>
    </div>
  )
}
