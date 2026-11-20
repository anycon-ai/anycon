// ⌘⇧A / ⌘⇧D 发出的答复（会话页和「待处理」页共用）：发出去，把 daemon 回的 `not_found` 说明一下（不是失败，也不重试），别的错误弹 toast。
// 和卡片上的按钮共用「正在答复」记录：同一个请求已经在答复时（连按、刚点过按钮）直接丢掉。
import type { TFunction } from 'i18next'
import type { PermissionRespondParams } from '@/protocol'
import { describeError } from '@/lib/errors'
import { toast } from '@/stores/ui'
import { respondPermission } from './actions'
import { beginAnswer, endAnswer } from './inflight'

/** `handledHere`：「待处理」页在眼前时发起的（见 `beginAnswer`）。 */
export function answerFromShortcut(nodeId: string, params: PermissionRespondParams, t: TFunction, handledHere = false): Promise<void> {
  if (!beginAnswer(params.request_id, handledHere)) return Promise.resolve()
  let ok = false
  return respondPermission(nodeId, params)
    .then(
      (outcome) => {
        ok = true
        // not_found：目录变了（请求已被拒绝、本轮已中止）或请求已不存在
        if (outcome === 'denied_folder_changed') toast.info(t('permission.deniedFolderChanged'))
        else if (outcome === 'request_gone') toast.info(t('permission.requestGone'))
      },
      (err: unknown) => toast.error(describeError(err, t)),
    )
    .finally(() => endAnswer(params.request_id, ok))
}
