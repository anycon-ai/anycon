import type { Block } from './reducer'
import type { ToolCategory } from '@/protocol'
import { toolCommand, toolLabel, toolOutputText, toolPath } from './text'

const CATEGORY_LABEL: Record<ToolCategory, string> = {
  shell: 'Shell',
  file_edit: 'Edit',
  file_read: 'Read',
  search: 'Search',
  web: 'Web',
  subagent: 'Agent',
  mcp: 'MCP',
  other: 'Tool',
}

/** 导出里一张图片附件的写法。 */
export type ImageLabel = (name: string) => string
const defaultImageLabel: ImageLabel = (name) => `[image: ${name}]`

/**
 * 把已加载的会话导出成 Markdown（客户端本地生成，不经过 daemon）。
 * 图片附件只写文件名（`[image: shot.png]`），不嵌入内容；`imageLabel` 让界面按当前语言写（`[图片：shot.png]`）。
 */
export function exportMarkdown(title: string, blocks: readonly Block[], imageLabel: ImageLabel = defaultImageLabel): string {
  const out: string[] = [`# ${title}`, '']
  for (const b of blocks) {
    switch (b.type) {
      case 'user':
        out.push(`## User`, '')
        if (b.content || b.attachments.length === 0) out.push(b.content, '')
        for (const a of b.attachments) out.push(imageLabel(a.name), '')
        break
      case 'assistant':
        out.push(`## Assistant`, '', b.text, '')
        break
      case 'tool': {
        const label = b.category === 'shell' ? `$ ${toolCommand(b)}` : (toolPath(b) ?? toolLabel(b))
        const output = toolOutputText(b)
        // 标题按 category 取，不用工具名（各 agent 的工具名不同）
        out.push(`> **${CATEGORY_LABEL[b.category]}** — \`${label}\``)
        if (output) out.push('', '```', output.replace(/\n$/, ''), '```')
        out.push('')
        break
      }
      case 'error':
        out.push(`> **Error** (${b.code}): ${b.message}`, '')
        break
      default:
        break
    }
  }
  return out.join('\n')
}

export function downloadText(filename: string, text: string, type = 'text/markdown'): void {
  const url = URL.createObjectURL(new Blob([text], { type: `${type};charset=utf-8` }))
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  document.body.appendChild(a)
  a.click()
  a.remove()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
