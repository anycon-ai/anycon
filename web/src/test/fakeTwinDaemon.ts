// 分身界面的假节点：可控制状态、运行和问题，记录协议参数及拒绝。
import type { TwinAnswerParams, TwinGetResult, TwinQuestion, TwinRun, TwinSetParams } from '@/protocol'
import { type FakeRpc, rpcError } from './fakeRpc'
export const twinValue = (over: Partial<TwinGetResult> = {}): TwinGetResult => ({
  enabled: true,
  configured: true,
  state: 'idle',
  scope: { paused: false, projects: ['web'], max_sessions: 2, daily_dispatches: 10 },
  today: { dispatches: 2, judge_calls: 3 },
  working: [],
  waiting: [],
  ...over,
})
export const twinRun = (over: Partial<TwinRun> = {}): TwinRun => ({
  id: 'run-1',
  requester: 'local',
  started_ms: 2000,
  finished_ms: 2100,
  dry_run: false,
  result: 'done',
  triggers: ['manual'],
  items: ['t1'],
  judge_calls: ['j1'],
  decisions: [{ todo_id: 't1', action: 'dispatch', reason: 'Ready to build', rules: [], outcome: 'executed' }],
  ...over,
})
export const twinQuestion = (over: Partial<TwinQuestion> = {}): TwinQuestion => ({
  id: 'q1',
  todo_id: 't1',
  question: 'Which project? <script>test</script>',
  options: ['Web', 'API'],
  created_ms: 1000,
  status: 'open',
  ...over,
})
export class FakeTwinDaemon {
  values = new Map<string, TwinGetResult>()
  runs: TwinRun[] = []
  questions: TwinQuestion[] = []
  dry = twinRun({ id: 'dry-1', dry_run: true })
  failSet = false
  constructor(readonly rpc: FakeRpc) {
    rpc.handlers['twin.get'] = (_p, id) => this.values.get(id) ?? twinValue()
    rpc.handlers['twin.set'] = (raw, id) => {
      if (this.failSet) throw rpcError('invalid', 'max_sessions', 'range')
      const p = raw as TwinSetParams
      const { clear, ...patch } = p
      const cur = this.values.get(id) ?? twinValue()
      const scope = {
        ...cur.scope,
        ...patch,
        projects: p.projects ?? cur.scope.projects,
        paused: p.paused ?? cur.scope.paused,
        max_sessions: p.max_sessions ?? cur.scope.max_sessions,
        daily_dispatches: p.daily_dispatches ?? cur.scope.daily_dispatches,
      }
      for (const key of clear ?? []) if (key === 'agent' || key === 'model' || key === 'mode') delete scope[key]
      const value: TwinGetResult = { ...cur, scope, configured: !!scope.projects.length, state: scope.paused ? 'paused' : scope.projects.length ? 'idle' : 'not_configured' }
      this.values.set(id, value)
      return value
    }
    rpc.handlers['twin.runs'] = (raw) => {
      const p = raw as { before_ms?: number; limit?: number }
      return { runs: this.runs.filter((r) => p.before_ms === undefined || r.started_ms < p.before_ms).slice(0, p.limit ?? 50) }
    }
    rpc.handlers['twin.questions'] = () => ({ questions: this.questions })
    rpc.handlers['twin.dry_run'] = () => ({ run: this.dry })
    rpc.handlers['twin.wake'] = () => ({})
    rpc.handlers['twin.answer'] = (raw) => {
      const p = raw as TwinAnswerParams
      const q = this.questions.find((q) => q.id === p.id)
      if (!q) throw rpcError('not_found')
      const answered: TwinQuestion = { ...q, answer: p.answer, answered_ms: 5000, status: 'answered' }
      this.questions = this.questions.map((q) => (q.id === p.id ? answered : q))
      return answered
    }
    rpc.handlers['node.get_config'] = () => ({ items: [{ key: 'sessions.max_concurrent', value: 4 }] })
  }
}
