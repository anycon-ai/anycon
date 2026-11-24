// 分身界面的假节点：可控制状态、运行和问题，记录协议参数及拒绝。
import type { TwinAnswerParams, TwinGetParams, TwinGetResult, TwinLink, TwinLinkParams, TwinPeerStatus, TwinQuestion, TwinRun, TwinSetParams, TwinStatsParams, TwinDayStats, TwinUnlinkParams } from '@/protocol'
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
/** 一天的统计；`date` 缺省是 2026-01-01，计数缺省为 0（与守护进程的「缺省 = 0」一致）。 */
export const twinDay = (over: Partial<TwinDayStats> = {}): TwinDayStats => ({ date: '2026-01-01', start_ms: 0, ...over })
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
export const peerStatus = (over: Partial<TwinPeerStatus> = {}): TwinPeerStatus => ({
  name: 'beta',
  projects: ['api'],
  agents: [{ id: 'mock', label: 'Mock', ready: true }],
  remaining_sessions: 2,
  remaining_dispatches: 7,
  paused: false,
  accept_links: true,
  ...over,
})
export const twinLink = (over: Partial<TwinLink> = {}): TwinLink => ({
  node_id: 'b'.repeat(32),
  name: 'beta',
  fingerprint: 'abcd1234',
  paired_ms: 1000,
  online: true,
  status: peerStatus(),
  ...over,
})
/** 一次短码，测试里断言它从没出现在界面、日志和存储里。 */
export const LINK_CODE = 'QZXW-7391-MNBV-4826'
export class FakeTwinDaemon {
  values = new Map<string, TwinGetResult>()
  runs: TwinRun[] = []
  questions: TwinQuestion[] = []
  dry = twinRun({ id: 'dry-1', dry_run: true })
  failSet = false
  /** `twin.stats` 的结果：按日期升序，最后一项是今天。缺省按请求的天数补零。 */
  statsDays: TwinDayStats[] | null = null
  failStats = false
  /** 分身设备（`requester`）的范围，键是 `<节点 id>:<设备 id>`。 */
  scopes = new Map<string, TwinGetResult>()
  /** 使用方节点上的分身连接。 */
  links: TwinLink[] = []
  /** 下一次 `node.twin_link_code` / `twin.link` 抛的错误。 */
  failCode: Error | null = null
  failLink: Error | null = null
  constructor(readonly rpc: FakeRpc) {
    const slot = (id: string, requester?: string | null): string => (requester && requester !== 'local' ? `${id}:${requester}` : id)
    const book = (requester?: string | null) => (requester && requester !== 'local' ? this.scopes : this.values)
    rpc.handlers['twin.get'] = (raw, id) => {
      const { requester } = (raw ?? {}) as TwinGetParams
      const fallback = requester && requester !== 'local' ? twinValue({ configured: false, state: 'not_configured', scope: { paused: false, projects: [], max_sessions: 1, daily_dispatches: 0 }, today: { dispatches: 0, judge_calls: 0 } }) : twinValue()
      return book(requester).get(slot(id, requester)) ?? fallback
    }
    rpc.handlers['node.twin_link_code'] = () => {
      if (this.failCode) throw this.failCode
      return { code: LINK_CODE, expires_ms: Date.now() + 600_000 }
    }
    rpc.handlers['twin.links'] = () => ({ links: this.links })
    rpc.handlers['twin.link'] = (raw) => {
      if (this.failLink) throw this.failLink
      const p = raw as TwinLinkParams
      const link = twinLink({ node_id: p.node_id })
      this.links = [...this.links.filter((l) => l.node_id !== p.node_id), link]
      return link
    }
    rpc.handlers['twin.unlink'] = (raw) => {
      const p = raw as TwinUnlinkParams
      this.links = this.links.filter((l) => l.node_id !== p.node_id)
      return {}
    }
    rpc.handlers['twin.set'] = (raw, id) => {
      if (this.failSet) throw rpcError('invalid', 'max_sessions', 'range')
      const p = raw as TwinSetParams
      const { clear, requester, ...patch } = p
      const cur = book(requester).get(slot(id, requester)) ?? (requester && requester !== 'local' ? twinValue({ configured: false, state: 'not_configured', scope: { paused: false, projects: [], max_sessions: 1, daily_dispatches: 0 }, today: { dispatches: 0, judge_calls: 0 } }) : twinValue())
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
      book(requester).set(slot(id, requester), value)
      return value
    }
    rpc.handlers['twin.runs'] = (raw) => {
      const p = raw as { before_ms?: number; limit?: number }
      return { runs: this.runs.filter((r) => p.before_ms === undefined || r.started_ms < p.before_ms).slice(0, p.limit ?? 50) }
    }
    rpc.handlers['twin.stats'] = (raw) => {
      if (this.failStats) throw rpcError('internal')
      const days = (raw as TwinStatsParams).days ?? 7
      const all = this.statsDays ?? [twinDay({ date: '2026-01-01' })]
      // 不够的往前补零日，多的取最近的
      const first = new Date(`${all[0]!.date}T00:00:00Z`)
      const pad = Array.from({ length: Math.max(0, days - all.length) }, (_, i) => {
        const d = new Date(first)
        d.setUTCDate(d.getUTCDate() - (days - all.length - i))
        return twinDay({ date: d.toISOString().slice(0, 10) })
      })
      return { days: [...pad, ...all].slice(-days) }
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
