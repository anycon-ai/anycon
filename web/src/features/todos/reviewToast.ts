// 页面在前台时，一项变成「待验收」（`activity` 变成 `review`）：弹一条应用内 toast「<标题> 做完了，等你验收」，点「去验收」落在验收页。
// 页面在后台时由 daemon 的 `turn_finished` 通知变成系统通知（同样的文案，落在同一处，见 `features/sync/pushText.ts`）。
// 只在收到 `todo.changed` 时看：同步（`todo.list`）带来的不弹，免得刚打开页面就一串提示；已经在看那一项（详情或验收页）时也不打扰。
import i18n from '@/i18n'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { navigateTo } from '@/lib/navigate'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { activityOf } from './activity'
import type { Todo } from '@/protocol'

export function toastIfReady(nodeId: string, before: Todo | undefined, after: Todo): void {
  if (activityOf(after) !== 'review' || (before && activityOf(before) === 'review')) return
  if (typeof document !== 'undefined' && document.visibilityState !== 'visible') return
  const order = useNodes.getState().order
  const node = shortNodeId(nodeId, order)
  const path = typeof location !== 'undefined' ? location.pathname : ''
  // 已经在这一项的详情或验收页上：不打扰
  if (path === `/todos/${node}/${after.id}` || path === `/todos/${node}/${after.id}/review` || path === `/todos/${nodeId}/${after.id}` || path === `/todos/${nodeId}/${after.id}/review`) return
  const title = cleanHintText(after.title, 80)
  toast.info(i18n.t('toast.todoReview', { title: title ? `“${title}”` : i18n.t('toast.aTodo') }), { label: i18n.t('toast.review'), run: () => navigateTo(`/todos/${node}/${after.id}/review`) })
}
