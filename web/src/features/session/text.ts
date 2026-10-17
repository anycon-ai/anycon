// 工具输入 / 输出的文本化与摘要（渲染用，纯函数）。
import { isRecord, nonEmptyString } from '@/lib/guards'
import type { ToolBlock } from './reducer'

/** 只需要 input / detail 的最小形状，tool_call 块和权限请求都能满足。 */
type ToolLike = Pick<ToolBlock, 'input' | 'detail'>

/** tool_result.output 可能是字符串、对象或数组（协议里是 serde_json::Value，生成的类型是 `unknown`）。 */
export function outputToText(output: unknown): string {
  if (output == null) return ''
  if (typeof output === 'string') return output
  if (Array.isArray(output)) {
    // Claude Code 的 tool_result content: [{type:'text', text:'…'}]
    const parts = output.map((p: unknown) => (isRecord(p) && typeof p.text === 'string' ? p.text : null))
    if (parts.every((p) => p !== null)) return parts.join('\n')
  }
  if (isRecord(output) && typeof output.text === 'string') return output.text
  try {
    return JSON.stringify(output, null, 2)
  } catch {
    return String(output)
  }
}

/** 流式输出优先；工具结束后以结果为准（结果通常是完整输出）。 */
export function toolOutputText(b: ToolBlock): string {
  const finalText = b.result ? outputToText(b.result.output) : ''
  if (b.result && finalText) return finalText
  return b.streamed
}

export function splitLines(text: string): string[] {
  return text.replace(/\n$/, '').split('\n')
}

/** tool_call.input / 权限请求的 tool.input 是 `unknown`：只在它确实是对象且字段是非空字符串时取值。 */
function inputField(input: unknown, ...keys: string[]): string | undefined {
  if (!isRecord(input)) return undefined
  for (const k of keys) {
    const v = nonEmptyString(input[k])
    if (v) return v
  }
  return undefined
}

export function toolCommand(b: ToolBlock): string {
  return b.detail?.command ?? inputField(b.input, 'command', 'cmd') ?? b.title
}

/** agent 自己写的命令说明（Claude Code 的 Bash 带 `description`）：有就当命令块收起时的标题。 */
export function toolDescription(b: ToolBlock): string | undefined {
  return inputField(b.input, 'description')
}

export function toolPath(b: ToolLike): string | undefined {
  return b.detail?.path ?? inputField(b.input, 'file_path', 'path', 'notebook_path')
}

export function toolQuery(b: ToolBlock): string | undefined {
  return b.detail?.query ?? inputField(b.input, 'query', 'pattern')
}

export function toolUrl(b: ToolBlock): string | undefined {
  return b.detail?.url ?? inputField(b.input, 'url')
}

/**
 * 工具块 / 审批请求的一行标题。**只看 `title` 和 `detail`，不看工具名**：各 agent 的工具名不一样
 * （Claude Code 是 `Bash` / `Edit`，Codex 是 item 类型 `commandExecution` / `fileChange`），名字只在什么都没有时兜底。
 */
export function toolLabel(b: Pick<ToolBlock, 'name' | 'title' | 'detail'>): string {
  const d = b.detail
  return nonEmptyString(b.title) ?? (d?.server && d.tool ? `${d.server}/${d.tool}` : undefined) ?? nonEmptyString(d?.description) ?? b.name
}

/** 一次改多个文件的调用（Codex `fileChange`）的 `input.changes`：每项 `{ path, kind, diff, move_path? }`。 */
export interface FileChangeInput {
  path: string
  kind: string
  diff: string
  movePath?: string
}

export function toolFileChanges(b: ToolLike): FileChangeInput[] {
  if (!isRecord(b.input) || !Array.isArray(b.input.changes)) return []
  const out: FileChangeInput[] = []
  for (const c of b.input.changes as unknown[]) {
    if (!isRecord(c)) continue
    const path = nonEmptyString(c.path)
    if (!path) continue
    out.push({ path, kind: typeof c.kind === 'string' ? c.kind : 'update', diff: typeof c.diff === 'string' ? c.diff : '', movePath: nonEmptyString(c.move_path) })
  }
  return out
}

/** Codex 的新增 / 删除给的是整个文件内容，修改给的是 `@@` 开头的 hunk（带 move_path 时 hunk 后面追加 "Moved to: …"）。 */
function changeDiff(c: FileChangeInput): string {
  const rel = c.path.replace(/^\/+/, '')
  const lines = (p: string): string[] => c.diff.replace(/\n$/, '').split('\n').map((l) => p + l)
  if (c.kind === 'add') {
    const n = lines('+')
    return [`diff --git a/${rel} b/${rel}`, 'new file mode 100644', '--- /dev/null', `+++ b/${rel}`, `@@ -0,0 +1,${n.length} @@`, ...n].join('\n')
  }
  if (c.kind === 'delete') {
    const n = lines('-')
    return [`diff --git a/${rel} b/${rel}`, 'deleted file mode 100644', `--- a/${rel}`, '+++ /dev/null', `@@ -1,${n.length} +0,0 @@`, ...n].join('\n')
  }
  const newRel = c.movePath ? c.movePath.replace(/^\/+/, '') : rel
  const moved = c.diff.indexOf('\n\nMoved to: ')
  const body = (moved >= 0 ? c.diff.slice(0, moved) : c.diff).replace(/\n$/, '')
  return [`diff --git a/${rel} b/${newRel}`, `--- a/${rel}`, `+++ b/${newRel}`, body].join('\n')
}

/**
 * 保证 unified diff 里每个文件都有 `diff --git` 头：Codex adapter 给的 `unified_diff` 是几个文件首尾相连的 `---` / `+++` + hunk，
 * 没有 `diff --git` 行，react-diff-view 会把它们当成同一个文件。已经有 `diff --git` 的原样返回。
 * 用 hunk 头里的行数判断「还在 hunk 里」，所以内容里以 `--- ` 开头的行不会被当成文件头。
 */
export function normalizeDiff(diff: string): string {
  if (/^diff --git /m.test(diff)) return diff
  const lines = diff.split('\n')
  const out: string[] = []
  let oldLeft = 0
  let newLeft = 0
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!
    const next = lines[i + 1]
    if (oldLeft <= 0 && newLeft <= 0 && line.startsWith('--- ') && next?.startsWith('+++ ')) {
      const from = line.slice(4).split('\t')[0]!
      const to = next.slice(4).split('\t')[0]!
      const strip = (p: string): string => p.replace(/^[ab]\//, '')
      const a = from === '/dev/null' ? strip(to) : strip(from)
      const b = to === '/dev/null' ? strip(from) : strip(to)
      out.push(`diff --git a/${a} b/${b}`)
      if (from === '/dev/null') out.push('new file mode 100644')
      else if (to === '/dev/null') out.push('deleted file mode 100644')
      out.push(line, next)
      i++
      continue
    }
    const h = /^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@/.exec(line)
    if (h) {
      oldLeft = h[1] === undefined ? 1 : Number(h[1])
      newLeft = h[2] === undefined ? 1 : Number(h[2])
    } else if (oldLeft > 0 || newLeft > 0) {
      if (line.startsWith('+')) newLeft--
      else if (line.startsWith('-')) oldLeft--
      else if (!line.startsWith('\\')) {
        oldLeft--
        newLeft--
      }
    }
    out.push(line)
  }
  return out.join('\n')
}

/** 没有 unified_diff 时，用 Edit / Write 的入参拼一个简易 diff 来显示。 */
export function syntheticDiff(b: ToolLike): string | null {
  const changes = toolFileChanges(b).filter((c) => c.diff)
  if (changes.length > 0) return changes.map(changeDiff).join('\n')
  const path = toolPath(b)
  if (!path) return null
  const oldS = inputField(b.input, 'old_string')
  const newS = inputField(b.input, 'new_string')
  const content = inputField(b.input, 'content')
  const lines = (s: string, p: string): string[] => s.replace(/\n$/, '').split('\n').map((l) => p + l)
  if (oldS !== undefined || newS !== undefined) {
    const o = lines(oldS ?? '', '-')
    const n = lines(newS ?? '', '+')
    return [`diff --git a/${path} b/${path}`, `--- a/${path}`, `+++ b/${path}`, `@@ -1,${o.length} +1,${n.length} @@`, ...o, ...n].join('\n')
  }
  if (content !== undefined) {
    const n = lines(content, '+')
    return [`diff --git a/${path} b/${path}`, 'new file mode 100644', '--- /dev/null', `+++ b/${path}`, `@@ -0,0 +1,${n.length} @@`, ...n].join('\n')
  }
  return null
}

export function diffStats(diff: string): { added: number; removed: number } {
  let added = 0
  let removed = 0
  for (const line of diff.split('\n')) {
    if (line.startsWith('+') && !line.startsWith('+++')) added++
    else if (line.startsWith('-') && !line.startsWith('---')) removed++
  }
  return { added, removed }
}
