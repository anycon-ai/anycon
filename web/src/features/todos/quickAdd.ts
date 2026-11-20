// 不经输入条直接记一项（命令面板的 `⌘Enter` 和「记到清单：…」那一行）：节点按缺省（勾选的、支持清单的节点里上次用过的，没有就第一个）。
import i18n from '@/i18n'
import { selectedNodes } from '@/lib/nodeFilter'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { toast } from '@/stores/ui'
import { useCapture } from './capture'
import { addTodo, deleteTodo } from './service'
import { nodeSupportsTodo } from './store'
import { fieldErrorText } from './ui/errors'
import { parseCapture } from './validate'

export function defaultTodoNode(): string | null {
  const supporting = selectedNodes(useNodes.getState().order, usePrefs.getState().sidebarNodes).filter(nodeSupportsTodo)
  const last = usePrefs.getState().lastTodoNode
  return (last && supporting.includes(last) ? last : supporting[0]) ?? null
}

/** 把这段文字记成一项。校验不过 / 没有节点支持时不记，改为打开输入条把文字放进去（用户能看到哪里不对）。 */
export async function quickAdd(text: string): Promise<boolean> {
  const nodeId = defaultTodoNode()
  const { title, body } = parseCapture(text)
  if (!nodeId || title === '') {
    useCapture.getState().openCapture({ text })
    return false
  }
  const r = await addTodo(nodeId, { title, body })
  if (!r.ok) {
    useCapture.getState().openCapture({ text })
    toast.error(fieldErrorText(i18n.t.bind(i18n), r.error))
    return false
  }
  usePrefs.getState().set({ lastTodoNode: nodeId })
  toast.info(i18n.t('todos.added', { title: title.length > 60 ? `${[...title].slice(0, 60).join('')}…` : title }), { label: i18n.t('todos.undo'), run: () => void deleteTodo(nodeId, r.id) })
  return true
}
