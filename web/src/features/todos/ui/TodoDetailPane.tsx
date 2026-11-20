import { useMemo } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { cleanHintText } from '@/lib/hint'
import { resolveNodeParam } from '@/lib/ids'
import { useNodeFilter } from '@/lib/nodeFilter'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Spinner } from '@/ui/Misc'
import { nodeSupportsTodo, useTodoView, useTodos } from '../store'
import { TodoDetail } from './TodoDetail'

/** 地址里的 `:node`（短 id 或全长）解析成节点 id；`:todo` 是项的 id。桌面右半边和手机二级页共用这一套。 */
export function resolveTodoRoute(node: string, order: readonly string[]): string | undefined {
  return resolveNodeParam(node, order)
}

/**
 * 单项详情的内容，按地址里的 `:node/:todo` 解析，说清每一种「看不到」的原因：外壳还没读完节点、节点还没读完缓存（加载中）、
 * 不认识这个节点、这个节点不带清单、这一项不存在。节点不在左栏勾选里时照样显示，写明不在勾选里并给「显示这个节点」
 * （和「待处理」里同样的说明）。没收下的 agent 项显示「收下 / 丢掉」，不给编辑控件。
 */
export function TodoDetailPane({ node, todo }: { node: string; todo: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const loaded = useNodes((s) => s.loaded)
  const order = useNodes((s) => s.order)
  const nodeId = resolveTodoRoute(node, order)
  const entry = useNodes((s) => (nodeId ? s.byId[nodeId] : undefined))
  const supported = useNodes((s) => (nodeId ? nodeSupportsTodo(nodeId) : false) && !!s.byId[nodeId ?? ''])
  const hydrated = useTodos((s) => (nodeId ? s.nodes[nodeId]?.hydrated === true : false))
  const { selected, toggle } = useNodeFilter()
  const ids = useMemo(() => (nodeId ? [nodeId] : []), [nodeId])
  const item = useTodoView(ids).find((v) => v.id === todo)
  const online = entry?.conn === 'ready'

  if (!loaded) return <Loading />
  if (!nodeId || !entry) return <Note testId="todo-unknown-node">{t('todos.unknownNode')}</Note>
  // 还没拿到过 `node.info`（刚连上）时不能断言「不支持」：等同步
  if (!entry.info) return <Loading />
  if (!supported) return <Note testId="todo-unsupported-node">{t('todos.nodeUnsupported', { node: cleanHintText(entry.name) || nodeId.slice(0, 8) })}</Note>
  if (!hydrated) return <Loading />
  if (!item) return <Note testId="todo-gone">{t('todos.gone')}</Note>
  return (
    <div className="space-y-3">
      {!selected.includes(nodeId) ? (
        <div className="flex flex-wrap items-center gap-2 rounded-md border border-border bg-bg-subtle px-2.5 py-1.5 text-sm" data-testid="todo-outside-note">
          <span className="min-w-0 flex-1">{t('todos.outsideFilter', { node: cleanHintText(entry.name) || nodeId.slice(0, 8) })}</span>
          <Button size="sm" variant="outline" className="max-md:min-h-11" onClick={() => toggle(nodeId, true)} data-testid="todo-show-node">
            {t('inbox.showNode')}
          </Button>
        </div>
      ) : null}
      <TodoDetail key={`${item.node_id}:${item.id}`} item={item} online={online} onDeleted={() => void navigate({ to: '/todos', replace: true })} />
    </div>
  )
}

function Loading() {
  return (
    <div className="flex justify-center py-8" role="status" data-testid="todo-loading">
      <Spinner className="size-6" />
    </div>
  )
}

function Note({ children, testId }: { children: string; testId: string }) {
  return (
    <p className="text-sm text-fg-subtle" role="status" data-testid={testId}>
      {children}
    </p>
  )
}
