// 预览入口什么时候出现（docs/spec/web.md § 13.1）：hub 配了预览（`/api/config.preview_origin_template` 不为空），
// 并且这个节点支持预览（`capabilities.preview = true`，daemon 的 `preview.enabled`）。两个条件缺一个，
// 会话页的「预览」分段和节点设置的「预览」标签都不出现。
import { useConfig } from '@/api/queries'
import type { NodeInfo } from '@/protocol'

/** hub 给的预览 origin 模板；没配预览、或模板里没有 `{label}`（用不了）时是 `null`。 */
export function usePreviewTemplate(): string | null {
  const template = useConfig().data?.preview_origin_template
  return typeof template === 'string' && template.includes('{label}') ? template : null
}

/** 模板可用且节点支持预览。 */
export function previewAvailable(template: string | null, info: NodeInfo | undefined): template is string {
  return template !== null && info?.capabilities.preview === true
}
