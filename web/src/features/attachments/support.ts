// 能不能给这个会话 / 新建表单里选中的 agent 发图片（spec/web.md § 4.6「输入区」）。
import type { AgentInfo, NodeInfo } from '@/protocol'
import { uploadableMimes } from './normalize'

/**
 * 节点支持 blob 方法（`capabilities.blobs`）且 agent 收附件（`capabilities.attachments` 非空）时，返回能上传的 mime
 * （agent 收的 ∩ daemon 的上传白名单）；否则 null——输入区不显示回形针。节点离线是另一回事（按钮禁用并说明），不在这里判断。
 */
export function attachmentMimes(info: NodeInfo | undefined, agent: Pick<AgentInfo, 'capabilities'> | undefined): string[] | null {
  if (!info?.capabilities.blobs) return null
  const mimes = uploadableMimes(agent?.capabilities?.attachments ?? [])
  return mimes.length > 0 ? mimes : null
}
