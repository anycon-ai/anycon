// 能不能给这个会话 / 新建表单里选中的 agent 发附件（spec/web.md § 4.6「输入区」）。
import { ATTACHMENT_ANY_FILE, type AgentInfo, type NodeInfo } from '@/protocol'
import { isImageMime, uploadableMimes } from './normalize'

/** 这个 agent 在这个节点上能收什么附件。 */
export interface AttachmentSupport {
  /** 作为图片交给模型的格式（agent 点名的 ∩ 四种图片格式），添加图片时规整成其中一种。可能为空（只收任意文件）。 */
  images: string[]
  /** agent 收任意文件（`capabilities.attachments` 里有通配项 `ATTACHMENT_ANY_FILE`）：其余文件原样上传，由 agent 自己读。 */
  files: boolean
}

/**
 * 节点支持 blob 方法（`capabilities.blobs`）、agent 收图片或任意文件时返回它能收什么；否则 null——输入区不显示「＋」。
 * 节点离线是另一回事（菜单项禁用并说明），不在这里判断。
 */
export function attachmentSupport(info: NodeInfo | undefined, agent: Pick<AgentInfo, 'capabilities'> | undefined): AttachmentSupport | null {
  if (!info?.capabilities.blobs) return null
  const listed = agent?.capabilities?.attachments ?? []
  const images = uploadableMimes(listed)
  const files = listed.includes(ATTACHMENT_ANY_FILE)
  return images.length > 0 || files ? { images, files } : null
}

/** 已经上传好的这个附件（daemon 记录的 mime）能不能发给这个 agent：点名的图片格式，或者 agent 收任意文件。 */
export function acceptsAttachment(support: AttachmentSupport | null | undefined, mime: string): boolean {
  if (!support) return false
  return support.files || (isImageMime(mime) && support.images.includes(mime))
}
