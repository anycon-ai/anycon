// 「记成规则」（docs/spec/daemon.md § 23.6）要用的纯函数：从一个权限请求 / 提问里取出「agent 写的话」和「用户的选择」，各自单独显示，
// 再合成编辑框里预填的那句话。预填的文字里有 agent 写的东西，所以只会放进编辑框、由用户看过再存，不提供一键记下。
import type { PermissionBlock } from '@/features/session/reducer'
import { cleanOneLine } from './validate'

/** 显示 agent 写的话时每一块最多这么多个字符（计划、命令可能很长；全文在请求卡片上）。 */
export const AGENT_TEXT_MAX = 600
/** 预填的那句话里引用 agent 写的内容，最多这么多个字符。 */
export const WHAT_MAX = 120

const clip = (s: string, max: number): string => (([...s].length > max ? `${[...s].slice(0, max - 1).join('')}…` : s))
const str = (v: unknown): string | undefined => (typeof v === 'string' && v.trim() !== '' ? v : undefined)

/** 这次请求里 agent 写的东西：标题、命令 / 计划 / 提问和选项。每一块是纯文字（保留换行）。 */
export function agentLines(block: PermissionBlock): string[] {
  const out: string[] = []
  if (block.title.trim()) out.push(clip(block.title.trim(), AGENT_TEXT_MAX))
  const d = block.detail
  if (block.kind === 'tool' && d.tool) {
    const input = typeof d.tool.input === 'object' && d.tool.input !== null ? (d.tool.input as Record<string, unknown>) : {}
    const command = str(d.tool.detail?.command) ?? str(input.command) ?? str(input.cmd)
    if (command && command.trim() !== block.title.trim()) out.push(clip(command.trim(), AGENT_TEXT_MAX))
  }
  if ((block.kind === 'plan' || block.kind === 'other' || block.kind === 'mode_change') && d.text) out.push(clip(d.text.trim(), AGENT_TEXT_MAX))
  if (block.kind === 'question') {
    for (const q of d.questions ?? []) {
      out.push(q.text)
      const choices = (q.choices ?? []).map((c) => c.label)
      if (choices.length > 0) out.push(choices.map((c) => `- ${c}`).join('\n'))
    }
  }
  return out
}

/** 预填的那句话里引用的「这次问了什么」：单行、截短。提问用第一个问题，别的用标题。 */
export function whatWasAsked(block: PermissionBlock): string {
  const q = block.kind === 'question' ? block.detail.questions?.[0]?.text : undefined
  return clip(cleanOneLine(q ?? block.title), WHAT_MAX)
}

/** 用户在提问卡片里已经选 / 填的答案，每题一行：`题目 → 答案`（单选 / 多选的 id 换成选项的文字）。 */
export function chosenAnswers(block: PermissionBlock, answers: Readonly<Record<string, string | string[]>>): Array<{ question: string; answer: string }> {
  const out: Array<{ question: string; answer: string }> = []
  for (const q of block.detail.questions ?? []) {
    const a = answers[q.id]
    const ids = Array.isArray(a) ? a : a ? [a] : []
    const labels = ids.map((id) => (q.choices ?? []).find((c) => c.id === id)?.label ?? id).filter((s) => s.trim() !== '')
    if (labels.length > 0) out.push({ question: clip(cleanOneLine(q.text), WHAT_MAX), answer: clip(cleanOneLine(labels.join(', ')), WHAT_MAX) })
  }
  return out
}
