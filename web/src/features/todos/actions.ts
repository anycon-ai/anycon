// 清单项的常用动作（行上、详情里、长按菜单里、键盘、多选共用）。
import i18n from '@/i18n'
import { toast } from '@/stores/ui'
import { updateTodo } from './service'
import type { ViewTodo } from './types'
import type { TodoState } from '@/protocol'
import { cleanOneLine } from './validate'

/** 改状态；`undo` 时弹一条带「撤销」的提示（撤销 = 改回原来的状态，离线时也照常，走待发队列）。 */
export async function setTodoState(t: Pick<ViewTodo, 'node_id' | 'id' | 'state' | 'title'>, state: TodoState, opts: { undo?: boolean } = {}): Promise<void> {
  if (t.state === state) return
  const r = await updateTodo(t.node_id, t.id, { state })
  if (!r.ok) {
    toast.error(i18n.t('todos.errors.rejected'))
    return
  }
  if (opts.undo) {
    const key = state === 'done' ? 'todos.markedDone' : state === 'dropped' ? 'todos.markedDropped' : 'todos.moved'
    toast.info(i18n.t(key, { title: cleanOneLine(t.title, 60) }), { label: i18n.t('todos.undo'), run: () => void updateTodo(t.node_id, t.id, { state: t.state }) }, UNDO_MS)
  }
}

/** 撤销提示留多久（毫秒）：手机上滑一下完成之后的 5 秒（spec/web.md § 4.12）。 */
export const UNDO_MS = 5000
