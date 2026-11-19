// ⌘⇧A / ⌘⇧D 发出的答复（会话页和「待处理」页共用）：发出去，把 daemon 回的 `not_found` 说明一下（不是失败，也不重试），别的错误弹 toast。
import type { TFunction } from 'i18next'
import type { PermissionRespondParams } from '@/protocol'
import { describeError } from '@/lib/errors'
import { toast } from '@/stores/ui'
import { respondPermission } from './actions'

export function answerFromShortcut(nodeId: string, params: PermissionRespondParams, t: TFunction): Promise<void> {
  return respondPermission(nodeId, params).then(
    (outcome) => {
      // not_found：目录变了（请求已被拒绝、本轮已中止）或请求已不存在
      if (outcome === 'denied_folder_changed') toast.info(t('permission.deniedFolderChanged'))
      else if (outcome === 'request_gone') toast.info(t('permission.requestGone'))
    },
    (err: unknown) => toast.error(describeError(err, t)),
  )
}
