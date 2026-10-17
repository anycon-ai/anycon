// 快捷消息的发送（让 agent 提交 / 创建 PR / 解决冲突）：经 `session.send`，和用户自己打的消息走同一条路
// （turn 在跑时进入排队，节点离线时留在本地标「未发送」）。推送和 `gh` 由 agent 做，走它的权限审批。
import type { TFunction } from 'i18next'
import { sendMessage } from '@/features/session/actions'
import { toast } from '@/stores/ui'

export async function sendQuickMessage(nodeId: string, sessionId: string, text: string, t: TFunction): Promise<void> {
  const r = await sendMessage(nodeId, sessionId, text)
  if (r.state === 'failed') toast.error(r.reason === 'archived' ? t('session.archivedHint') : t('git.messageFailed'))
  else if (r.state === 'unsent') toast.info(t('git.messageUnsent'))
  else toast.info(r.state === 'queued' ? t('git.messageQueued') : t('git.messageSent'))
}
