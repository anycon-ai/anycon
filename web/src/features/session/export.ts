import type { Block, NoticeBlock, TaskSnapshot } from './reducer'
import type { AttachmentRef, ToolCategory } from '@/protocol'
import { isImageMime } from '@/features/attachments/normalize'
import { formatDuration } from '@/lib/format'
import { agentCounts } from './tasks'
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

/** 导出里一个附件的写法（`image` = 四种栅格图片之一，其余是文件）。 */
export type AttachmentLabel = (name: string, kind: 'image' | 'file') => string
const defaultAttachmentLabel: AttachmentLabel = (name, kind) => `[${kind}: ${name}]`
const attachmentKind = (a: Pick<AttachmentRef, 'mime'>): 'image' | 'file' => (isImageMime(a.mime) ? 'image' : 'file')

/** 一条提示在导出里最多写这么多字符（原文可能很长，导出里它只是一行）。 */
const NOTICE_MAX = 500

/** 一行文字：折叠空白（原文可能多行）。 */
const oneLine = (s: string): string => s.replace(/\s+/g, ' ').trim()

/**
 * 一条 notice 的一行文字。和其它系统行一样用固定的英文（导出里的标题都是英文）；agent 的原文 `message` 优先，
 * 没有原文时按 code 带的字段拼一句。
 */
function noticeLine(b: NoticeBlock): string {
  const message = oneLine(b.message)
  if (message) return message.length > NOTICE_MAX ? `${message.slice(0, NOTICE_MAX)}…` : message
  if (b.code === 'model_fallback' && b.modelTo) return b.modelFrom ? `Model switched from ${b.modelFrom} to ${b.modelTo}` : `Model switched to ${b.modelTo}`
  if (b.code === 'rate_limit' && b.resetsAtMs != null) return `Usage limit reached, resets ${new Date(b.resetsAtMs).toISOString()}`
  return b.code
}

/** 一个后台任务导出时的最终状态：名字、种类、状态，以及 agent 完成数和用量（有什么写什么）。 */
function taskLine(task: TaskSnapshot): string {
  const facts: string[] = [task.status]
  if (task.agents.length > 0) facts.push(`${agentCounts(task.agents).done}/${task.agents.length} agents done`)
  const u = task.usage
  if (u?.total_tokens != null) facts.push(`${u.total_tokens} tokens`)
  if (u?.tool_calls != null) facts.push(`${u.tool_calls} tool calls`)
  if (u?.duration_ms != null && u.duration_ms > 0) facts.push(formatDuration(u.duration_ms))
  return `> **Task** (${task.kind}) — \`${oneLine(task.title) || 'Background task'}\`: ${facts.join(', ')}`
}

/**
 * 把已加载的会话导出成 Markdown（客户端本地生成，不经过 daemon）。
 * 附件只写文件名（图片 `[image: shot.png]`、其它文件 `[file: notes.pdf]`），不嵌入内容；
 * `attachmentLabel` 让界面按当前语言写（`[图片：shot.png]`、`[文件：notes.pdf]`）。
 */
export function exportMarkdown(title: string, blocks: readonly Block[], attachmentLabel: AttachmentLabel = defaultAttachmentLabel): string {
  const out: string[] = [`# ${title}`, '']
  for (const b of blocks) {
    switch (b.type) {
      case 'user':
        out.push(`## User`, '')
        if (b.content || b.attachments.length === 0) out.push(b.content, '')
        for (const a of b.attachments) out.push(attachmentLabel(a.name, attachmentKind(a)), '')
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
        // 这次调用启动的后台任务：每个一行，写导出时的状态
        for (const task of b.tasks ?? []) out.push(taskLine(task), '')
        break
      }
      case 'task':
        out.push(taskLine(b.task), '')
        break
      case 'notice':
        // `/context` 之类本地命令的输出是多行原文，当作输出块；其余的提示一行一条
        if (b.code === 'command_output') out.push('> **Command output**', '', '```', b.message.replace(/\n+$/, ''), '```', '')
        else out.push(`> **${b.level === 'warning' ? 'Warning' : 'Notice'}** (${b.code}): ${noticeLine(b)}`, '')
        break
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
