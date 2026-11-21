// 会话信息面板里「交给 agent 的规则」的来源核对（docs/spec/daemon.md § 23.5）：系统提示开头那一段是不是规则，靠固定的开头一句认；
// 调用方自己给的 `system_prompt_append` 如果也以同样的话起头，会被当成规则显示，会话设置之后也能改。所以清单派发出来的会话，
// 再拿「经过」里 `dispatched` 记的规则条数核对一次：对不上就说明「没法确认」，不替它背书。手动开的会话没有这条记录，只显示、不核对。
import { useEffect } from 'react'
import { loadHistory, useHistoryEntry } from '@/features/todos/history'
import { injectedRuleCount, injectedRulesOf } from './injected'

export type InjectedVerdict = 'ok' | 'checking' | 'mismatch'

export interface InjectedRules {
  text: string
  /** `ok`：和派发记下的一致；`checking`：还在读「经过」，或者不是清单派发的（没有可核对的）；`mismatch`：和派发记下的不一样。 */
  verdict: InjectedVerdict
}

export function useInjectedRules(nodeId: string | undefined, sessionId: string, todoId: string | null | undefined, append: string | null | undefined): InjectedRules | null {
  const text = injectedRulesOf(append)
  const history = useHistoryEntry(nodeId ?? '', todoId ?? '')
  const check = !!text && !!nodeId && !!todoId
  useEffect(() => {
    if (check && !history.loaded && !history.loading) void loadHistory(nodeId!, todoId!)
    // 只在需要核对、还没读过的时候读一次
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [check, history.loaded, nodeId, todoId])
  if (!text) return null
  if (!check) return { text, verdict: 'checking' }
  if (!history.loaded) return { text, verdict: 'checking' }
  const dispatched = history.events.find((e) => e.kind === 'dispatched' && e.session_id === sessionId)
  const recorded = dispatched && dispatched.kind === 'dispatched' ? (dispatched.rules ?? []).length : null
  return { text, verdict: recorded !== null && recorded === injectedRuleCount(text) ? 'ok' : 'mismatch' }
}
