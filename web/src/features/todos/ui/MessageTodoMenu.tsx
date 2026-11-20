import { MoreHorizontal, ListPlus } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { resolveNodeParam } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { Menu, MenuContent, MenuItem, MenuTrigger } from '@/ui/Menu'
import { cn } from '@/ui/cn'
import { captureFromQuote, useCapture } from '../capture'
import { nodeSupportsTodo } from '../store'

/** 回到这条消息的链接（`?seq=` 会话页据此定位那条消息，spec/web.md § 4.6）。 */
export function messageLink(node: string, session: string, seq: number): string {
  return `${location.origin}/n/${node}/s/${session}?seq=${seq}`
}

/** 当前地址是不是某个会话页：返回节点短 id、会话 id。不用路由 hook：消息组件在没有路由的测试和预览里也要能渲染。 */
export function sessionRoute(): { node: string; session: string } | null {
  const m = /^\/n\/([^/]+)\/s\/([^/?#]+)/.exec(location.pathname)
  return m ? { node: m[1]!, session: decodeURIComponent(m[2]!) } : null
}

/** 把一段话记成清单项：打开输入条，预填标题（第一行）和正文（原文 + 回到那条消息的链接）；用户按了发送才记下。 */
export function openCaptureFromMessage(text: string, node: string | undefined, session: string | undefined, seq: number | undefined): void {
  const link = node && session && seq ? messageLink(node, session, seq) : undefined
  useCapture.getState().openCapture({ text: captureFromQuote(text, link) })
}

/** 消息的「⋯」菜单里的「记到清单」。节点不支持清单时不显示。 */
export function MessageTodoMenu({ text, seq, className }: { text: string; seq: number; className?: string }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const route = sessionRoute()
  const nodeId = route ? resolveNodeParam(route.node, order) : undefined
  if (!route || !nodeId || !nodeSupportsTodo(nodeId) || text.trim() === '') return null
  return (
    <Menu>
      <MenuTrigger asChild>
        <button type="button" aria-label={t('todos.messageActions')} className={cn('rounded-md border border-border bg-surface p-1.5 text-fg-subtle opacity-0 shadow-sm transition-opacity group-hover:opacity-100 focus:opacity-100 data-[state=open]:opacity-100', className)} data-testid="message-menu">
          <MoreHorizontal className="size-3.5" aria-hidden />
        </button>
      </MenuTrigger>
      <MenuContent align="end" className="w-48">
        <MenuItem icon={<ListPlus className="size-4" />} onSelect={() => openCaptureFromMessage(text, route.node, route.session, seq)} testId="message-add-todo">
          {t('todos.addToTodos')}
        </MenuItem>
      </MenuContent>
    </Menu>
  )
}
