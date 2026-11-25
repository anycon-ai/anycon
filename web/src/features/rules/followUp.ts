// 「以后都这样」（docs/spec/daemon.md § 23.6、docs/spec/web.md § 14.6）：回答 Bot 的问题或 agent 的提问时勾上，答复**成功之后**
// 打开「记成规则」的同一个编辑框，由用户改成自己的话再存。编辑框的宿主在应用外壳里（`FollowUpRuleHost`）：答复成功后问题卡片
// 常常随之消失（「待处理」里的条目被移走），编辑框不能挂在卡片上。这里只放「要打开什么」，不做任何请求，也没有一键存下的路径。
import { create } from 'zustand'
import type { PermissionBlock } from '@/features/session/reducer'
import { useSessions } from '@/stores/sessions'
import { agentLines, chosenAnswers, twinAnswerContext } from './recordAsRule'
import type { NewRule } from './service'

export interface FollowUp {
  nodeId: string
  project?: string
  /** 出处：agent 的提问带会话和请求；Bot 的问题没有会话，`request_id` 记问题的 id。 */
  source: NonNullable<NewRule['source']>
  /** 提问的原文（agent / 模型写的，单独显示成纯文字）。 */
  asked: string[]
  /** 用户的回答，每题一行。 */
  answers: Array<{ question: string; answer: string }>
}

interface FollowUpState {
  current: FollowUp | null
  open: (f: FollowUp) => void
  close: () => void
}

export const useFollowUp = create<FollowUpState>()((set) => ({
  current: null,
  open: (f) => set({ current: f }),
  close: () => set({ current: null }),
}))

/** agent 的提问答复成功之后：用提交的回答打开编辑框。 */
export function openAfterAgentAnswer(nodeId: string, sessionId: string, block: PermissionBlock, submitted: Readonly<Record<string, string | string[]>>): void {
  const project = useSessions.getState().list[sessionId]?.project
  useFollowUp.getState().open({
    nodeId,
    ...(project ? { project } : {}),
    source: { kind: 'answer', session_id: sessionId, request_id: block.requestId },
    asked: agentLines(block),
    answers: chosenAnswers(block, submitted),
  })
}

/** Bot 的问题答复成功之后：没有会话，出处只记问题的 id。 */
export function openAfterTwinAnswer(nodeId: string, questionId: string, question: string, answer: string): void {
  useFollowUp.getState().open({ nodeId, source: { kind: 'answer', request_id: questionId }, ...twinAnswerContext(question, answer) })
}
