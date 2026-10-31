// 能不能给这个会话 / 新建表单里选中的 agent 发附件（spec/web.md § 4.6「输入区」）。
import type { AgentInfo, NodeAttachmentLimits, NodeInfo } from '@/protocol'
import { isImageMime, uploadableMimes } from './normalize'

/** 这个 agent 在这个节点上能收什么附件。 */
export interface AttachmentSupport {
  /** 作为图片交给模型的格式（agent 点名的 ∩ 四种图片格式），添加图片时规整成其中一种。可能为空（只收任意文件）。 */
  images: string[]
  /**
   * 节点能把任意文件交给任何会话的 agent（`node.info` 的 `capabilities.file_attachments`，ADR-44）：agent 不原生收的附件由 daemon 落到会话的
   * 工作目录，所以 agent 不收图片也行。其余文件原样上传。旧 daemon 没有这个字段（false）：只能传 agent 点名的图片。
   */
  files: boolean
  /** 节点报的附件上限（`node.info.attachment_limits`）；旧 daemon 没有。 */
  limits?: NodeAttachmentLimits
}

/**
 * 节点支持 blob 方法（`capabilities.blobs`），并且节点收任意文件（`file_attachments`）或 agent 点名了至少一种图片格式时，返回它能收什么；
 * 否则 null——输入区不显示「＋」。节点离线是另一回事（菜单项禁用并说明），不在这里判断。
 */
export function attachmentSupport(info: NodeInfo | undefined, agent: Pick<AgentInfo, 'capabilities'> | undefined): AttachmentSupport | null {
  if (!info?.capabilities.blobs) return null
  const images = uploadableMimes(agent?.capabilities?.attachments ?? [])
  const files = info.capabilities.file_attachments === true
  if (images.length === 0 && !files) return null
  return { images, files, ...(info.attachment_limits ? { limits: info.attachment_limits } : {}) }
}

/** 已经上传好的这个附件（daemon 记录的 mime）能不能发给这个节点上的这个 agent：节点收任意文件，或者是 agent 点名的图片格式。 */
export function acceptsAttachment(support: AttachmentSupport | null | undefined, mime: string): boolean {
  if (!support) return false
  return support.files || (isImageMime(mime) && support.images.includes(mime))
}
