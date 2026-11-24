// 所有写操作在线执行；通知合并成一次重读。序号防止旧请求盖掉新快照，清理后在途请求失效。
import type { TwinAnswerParams, TwinGetResult, TwinQuestion, TwinQuestionNotice, TwinQuestionsResult, TwinRunsResult, TwinSetParams, TwinDryRunResult, TwinStatsResult } from '@/protocol'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { emptyTwin, supportsTwin, supportsTwinStats, useTwin } from './store'

const versions = new Map<string, number>()
const nextVersion = (id: string): number => {
  const v = (versions.get(id) ?? 0) + 1
  versions.set(id, v)
  return v
}
const lifetimes = new Map<string, number>()
const lifetime = (id: string): number => lifetimes.get(id) ?? 0
function checkLifetime(id: string, started: number): void {
  if (lifetime(id) !== started) throw new RpcCallError('offline', 'Twin node was forgotten')
}
/** 分身页上挂着的卡片数（按节点）。只有有人要看统计时才读 `twin.stats`；侧栏状态条、待办页只要状态。 */
const statsWanted = new Map<string, number>()
export function wantTwinStats(id: string): () => void {
  statsWanted.set(id, (statsWanted.get(id) ?? 0) + 1)
  let released = false
  return () => {
    if (released) return
    released = true
    const left = (statsWanted.get(id) ?? 1) - 1
    if (left > 0) statsWanted.set(id, left)
    else statsWanted.delete(id)
  }
}
const pending = new Map<string, Promise<void>>()
const dirty = new Set<string>()
export async function syncTwin(id: string): Promise<void> {
  if (!supportsTwin(id) || useNodes.getState().byId[id]?.conn !== 'ready') return
  const running = pending.get(id)
  if (running) {
    dirty.add(id)
    return running
  }
  const version = nextVersion(id)
  const task = (async () => {
    try {
      // 统计失败不拖累其余数据：记在 `statsFailed` 上，趋势区显示错误和重试，下次重读成功就清掉
      const days = useTwin.getState().nodes[id]?.statsDays ?? 7
      const wantStats = supportsTwinStats(id) && statsWanted.has(id)
      let statsFailed = false
      const [value, runs, questions, stats] = await Promise.all([
        rpc().request<TwinGetResult>(id, 'twin.get', {}),
        rpc().request<TwinRunsResult>(id, 'twin.runs', { limit: 50 }),
        rpc().request<TwinQuestionsResult>(id, 'twin.questions', {}),
        wantStats
          ? rpc().request<TwinStatsResult>(id, 'twin.stats', { days }).catch(() => {
              statsFailed = true
              return undefined
            })
          : Promise.resolve(undefined),
      ])
      if (versions.get(id) === version && supportsTwin(id)) {
        useTwin.getState().patch(id, { value, runs: runs.runs, questions: questions.questions, failed: false, ...(stats ? { stats: stats.days, statsFailed: false } : {}), ...(statsFailed ? { statsFailed: true } : {}) })
      }
    } catch {
      if (versions.get(id) === version) useTwin.getState().patch(id, { failed: true })
    }
  })()
  pending.set(id, task)
  await task
  if (pending.get(id) === task) {
    pending.delete(id)
    if (dirty.delete(id)) await syncTwin(id)
  }
}
function requireOnline(id: string): void {
  if (!supportsTwin(id) || useNodes.getState().byId[id]?.conn !== 'ready') throw new RpcCallError('offline', 'Twin node is offline')
}
export async function setTwin(id: string, params: TwinSetParams): Promise<void> {
  requireOnline(id)
  const started = lifetime(id)
  nextVersion(id)
  const value = await rpc().request<TwinGetResult>(id, 'twin.set', params)
  checkLifetime(id, started)
  if (supportsTwin(id)) useTwin.getState().patch(id, { value, failed: false })
  await syncTwin(id)
}
export async function dryRunTwin(id: string): Promise<TwinDryRunResult> {
  requireOnline(id)
  const started = lifetime(id)
  const result = await rpc().request<TwinDryRunResult>(id, 'twin.dry_run', {}, 16 * 60_000)
  checkLifetime(id, started)
  await syncTwin(id)
  return result
}
/** 切换趋势区间并按新区间重读；已有的旧区间数据先留着，读到之前不清空。 */
export async function setTwinStatsRange(id: string, days: number): Promise<void> {
  useTwin.getState().patch(id, { statsDays: days, statsFailed: false })
  await syncTwin(id)
}
/** 趋势区的「重试」：先回到读取中，再重读一次。 */
export async function retryTwinStats(id: string): Promise<void> {
  useTwin.getState().patch(id, { statsFailed: false })
  await syncTwin(id)
}
export async function wakeTwin(id: string): Promise<void> {
  requireOnline(id)
  const started = lifetime(id)
  await rpc().request(id, 'twin.wake', {})
  checkLifetime(id, started)
  await syncTwin(id)
}
export async function answerTwin(id: string, params: TwinAnswerParams): Promise<void> {
  requireOnline(id)
  const started = lifetime(id)
  const question = await rpc().request<TwinQuestion>(id, 'twin.answer', params)
  checkLifetime(id, started)
  handleTwinQuestion(id, { question })
  await syncTwin(id)
}
export function handleTwinQuestion(id: string, notice: TwinQuestionNotice): void {
  if (!supportsTwin(id) || !notice?.question?.id) return
  const cur = useTwin.getState().nodes[id] ?? emptyTwin()
  const q = notice.question
  const old = cur.questions.find((x) => x.id === q.id)
  if (old && old.status !== 'open' && q.status === 'open') return
  nextVersion(id)
  useTwin.getState().patch(id, { questions: [...cur.questions.filter((x) => x.id !== q.id), q] })
}
export function forgetTwin(id: string): void {
  lifetimes.set(id, lifetime(id) + 1)
  nextVersion(id)
  dirty.delete(id)
  pending.delete(id)
  useTwin.getState().remove(id)
}
export function resetTwin(): void {
  for (const id of new Set([...versions.keys(), ...Object.keys(useTwin.getState().nodes)])) forgetTwin(id)
}
