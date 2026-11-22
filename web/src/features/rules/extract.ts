// 规则提炼的通知订阅和一次手动请求的生命周期；只在内存里保留当前页面所需的数据。
import type { ExtractRun, JudgeError, RuleExtractDone, RuleExtractResult } from '@/protocol'
import type { TFunction } from 'i18next'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'

type Listener = (nodeId: string, run: ExtractRun) => void
const listeners = new Set<Listener>()

export function onExtractDone(listener: Listener): () => void {
  listeners.add(listener)
  return () => { listeners.delete(listener) }
}

export function handleExtractDone(nodeId: string, raw: unknown): void {
  if (!raw || typeof raw !== 'object') return
  const run = (raw as Partial<RuleExtractDone>).run
  if (!run || typeof run.id !== 'string' || typeof run.session_id !== 'string' || run.status === 'running') return
  for (const listener of listeners) listener(nodeId, run)
}

export type ExtractState =
  | { kind: 'running' }
  | { kind: 'empty' }
  | { kind: 'done'; run: ExtractRun }
  | { kind: 'error'; error: unknown }
  | { kind: 'interrupted' }

/** 先订阅再发请求，容忍完成通知早于 RPC 响应；只认返回的 run.id。清理只停止等结果，不取消节点上的调用。 */
export function startExtract(nodeId: string, sessionId: string, update: (state: ExtractState) => void): () => void {
  let alive = true
  let runId: string | undefined
  const early = new Map<string, ExtractRun>()
  const cleanups: Array<() => void> = []
  const cleanup = (): void => {
    alive = false
    early.clear()
    for (const off of cleanups) off()
  }
  const finish = (state: ExtractState): void => {
    if (!alive) return
    cleanup()
    update(state)
  }
  if (useNodes.getState().byId[nodeId]?.conn !== 'ready') {
    finish({ kind: 'interrupted' })
    return cleanup
  }
  update({ kind: 'running' })
  cleanups.push(onExtractDone((id, run) => {
    if (id !== nodeId || run.session_id !== sessionId) return
    if (runId === run.id) finish({ kind: 'done', run })
    else if (!runId) early.set(run.id, run)
  }))
  cleanups.push(useNodes.subscribe((s) => {
    if (s.byId[nodeId]?.conn !== 'ready') finish({ kind: 'interrupted' })
  }))
  // 后端最长 15 分钟；通知丢失时也不能无限等待，记录页仍可查询最终结果。
  const timer = setTimeout(() => finish({ kind: 'interrupted' }), 16 * 60_000)
  cleanups.push(() => clearTimeout(timer))
  void rpc().request<RuleExtractResult>(nodeId, 'rule.extract', { session_id: sessionId }).then((result) => {
    if (!alive) return
    if (result.points === 0) return finish({ kind: 'empty' })
    if (!result.run) return finish({ kind: 'error', error: null })
    runId = result.run.id
    const done = early.get(runId)
    early.clear()
    if (done || result.run.status !== 'running') finish({ kind: 'done', run: done ?? result.run })
  }).catch((error: unknown) => finish({ kind: 'error', error }))
  return cleanup
}

/** 未知码按 internal 显示，绝不把后端回应或 RPC message 带到界面或日志。 */
export function extractErrorText(error: JudgeError | null | undefined, t: TFunction): string {
  const code = error?.code
  switch (code) {
    case 'auth': case 'rate_limited': case 'timeout': case 'network': case 'bad_output': case 'agent_failed': case 'cancelled':
      return t(`rules.extract.errors.${code}`)
    case 'http': return t('rules.extract.errors.http', { status: error?.status ?? '?' })
    default: return t('rules.extract.errors.internal')
  }
}

export function extractRejectedText(error: unknown, t: TFunction): string {
  if (error instanceof RpcCallError) {
    if (error.kind !== 'rpc') return t('rules.extract.interrupted')
    if (error.reason === 'not_configured' || error.reason === 'busy') return t(`rules.extract.${error.reason}`)
    if (error.reason === 'budget') {
      // 通用错误类型可能还不认识较新节点带的预算字段，按数值收窄，不能显示任意内容。
      const data: unknown = error.rpc?.data
      const number = (key: string): number | string => {
        if (typeof data !== 'object' || data === null || !(key in data)) return '?'
        const value: unknown = Reflect.get(data, key)
        return typeof value === 'number' && Number.isFinite(value) ? value : '?'
      }
      return t('rules.extract.budget', { used: number('used'), limit: number('limit') })
    }
    if (error.code === 'not_found') return t('rules.source.sessionGone')
  }
  return t('rules.extract.errors.internal')
}

export function extractResultText(run: ExtractRun, t: TFunction): string {
  if (run.status === 'running') return t('rules.extract.running')
  if (run.status !== 'done') return extractErrorText(run.error, t)
  const result = run.added.length > 0 ? t('rules.extract.added', { count: run.added.length }) : t('rules.extract.noRules')
  const reasons = (['duplicate', 'invalid', 'full'] as const)
    .filter((reason) => run.dropped[reason] > 0)
    .map((reason) => t(`rules.extract.droppedReasons.${reason}`, { count: run.dropped[reason] }))
    .join(t('rules.extract.droppedSeparator'))
  return reasons ? `${result} ${t('rules.extract.dropped', { reasons })}` : result
}
