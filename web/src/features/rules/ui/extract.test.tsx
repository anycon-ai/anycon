// 提炼界面用模拟节点验证：菜单能力、生命周期、结果、出处和按节点读取记录。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ExtractRun, JudgeErrorCode, NodeInfo, RuleExtractResult, RuleExtractRunsResult } from '@/protocol'
import { nodeEntry, nodeInfo, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { ruleItem } from '@/test/fakeRulesDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { handleNotification } from '@/features/sync/notifications'
import i18n from '@/i18n'
import { extractResultText, startExtract } from '../extract'
import { ExtractMenu } from './ExtractMenu'
import { ExtractHistory } from './ExtractHistory'
import { RuleSourceLine } from './RuleParts'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn()))
const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const run = (over: Partial<ExtractRun> = {}): ExtractRun => ({ id: 'run-1', session_id: 's1', status: 'running', trigger: 'manual', started_ms: 1000, points: 3, from_seq: 2, to_seq: 10, added: [], dropped: { duplicate: 0, invalid: 0, full: 0 }, backend: 'mock', ...over })
const info = (supported = true, configured = true): NodeInfo => nodeInfo({ capabilities: { ...nodeInfo().capabilities, rules: true, rule_extract: supported }, judge: { backends: [], daily_calls: 20, ...(configured ? { extract: 'mock' } : {}) } })
let fake: FakeRpc
const done = (value = run({ status: 'done' }), nodeId = A): void => { act(() => handleNotification(nodeId, 'rule.extract_done', { run: value })) }
const begin = async (): Promise<void> => {
  const user = userEvent.setup()
  await user.click(screen.getByTestId('session-more'))
  await user.click(screen.getByTestId('rule-extract'))
  await screen.findByTestId('extract-status')
}

beforeEach(() => {
  fake = new FakeRpc()
  fake.handlers['rule.extract'] = () => ({ points: 3, run: run() })
  fake.handlers['rule.extract_runs'] = () => ({ runs: [], used_today: 1, daily_calls: 20 })
  setRpc(fake)
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info() }), [B]: nodeEntry({ id: B, name: 'beta', info: info() }) }, order: [A, B] })
  useSessions.setState({ list: {} })
  useSessions.getState().upsertSummaries(A, [summary({ id: 's1', title: 'Source session' })])
})
afterEach(() => { setRpc(null); vi.useRealTimers() })

describe('extraction menu and result', () => {
  it('hides unsupported nodes, explains missing configuration with a documentation link, and disables offline nodes', async () => {
    useNodes.getState().patch(A, { info: info(false) })
    const view = render(<ExtractMenu nodeId={A} sessionId="s1" />)
    expect(screen.queryByTestId('session-more')).toBeNull()
    act(() => useNodes.getState().patch(A, { info: info(true, false) }))
    const user = userEvent.setup()
    await user.click(screen.getByTestId('session-more'))
    expect(screen.getByTestId('rule-extract')).toHaveAttribute('data-disabled')
    expect(screen.getByText(/Configure a judgment backend/)).toBeInTheDocument()
    expect(screen.getByRole('link')).toHaveAttribute('href', expect.stringContaining('/docs/spec/daemon.md#25-'))
    await user.keyboard('{Escape}')
    act(() => useNodes.getState().patch(A, { info: info(), conn: 'peer_offline' }))
    await user.click(screen.getByTestId('session-more'))
    expect(screen.getByTestId('rule-extract')).toHaveAttribute('data-disabled')
    expect(screen.getByText('This node is offline.')).toBeInTheDocument()
    expect(fake.calls).toHaveLength(0)
    view.unmount()
  })

  it('shows zero new decision points without waiting for an event', async () => {
    fake.handlers['rule.extract'] = () => ({ points: 0 })
    render(<ExtractMenu nodeId={A} sessionId="s1" />)
    await begin()
    expect(screen.getByRole('status')).toHaveTextContent('There are no new decision points')
  })

  it('matches node, session and run.id, then links specifically to the pending tab', async () => {
    render(<ExtractMenu nodeId={A} sessionId="s1" />)
    await begin()
    expect(fake.calls[0]).toEqual({ nodeId: A, method: 'rule.extract', params: { session_id: 's1' } })
    done(run({ status: 'done', id: 'other' }))
    done(run({ status: 'done' }), B)
    done(run({ status: 'done', session_id: 'other' }))
    expect(screen.getByRole('status')).toHaveTextContent('Extracting…')
    done(run({ status: 'done', added: ['r1', 'r2'] }))
    expect(screen.getByRole('status')).toHaveTextContent('Extracted 2 rules to review.')
    expect(screen.getByRole('link', { name: 'Review them' })).toHaveAttribute('href', '/rules?tab=pending')
  })

  it('handles a completion notification arriving before the RPC response', async () => {
    let resolve!: (result: RuleExtractResult) => void
    fake.handlers['rule.extract'] = () => new Promise<RuleExtractResult>((r) => { resolve = r })
    render(<ExtractMenu nodeId={A} sessionId="s1" />)
    await begin()
    done(run({ id: 'other', status: 'failed', error: { code: 'auth' } }))
    done(run({ status: 'done', dropped: { duplicate: 2, invalid: 3, full: 4 } }))
    await act(async () => resolve({ points: 3, run: run() }))
    expect(screen.getByRole('status')).toHaveTextContent('No new rules were extracted. Dropped: 2 duplicates, 3 invalid rules, 4 rules because there are too many pending rules.')
    expect(screen.queryByRole('link')).toBeNull()
  })

  it.each<[JudgeErrorCode, string]>([
    ['auth', 'API key'], ['rate_limited', 'rate limiting'], ['timeout', 'timed out'], ['http', 'HTTP 503'],
    ['network', 'Could not connect'], ['bad_output', 'invalid response format'], ['agent_failed', 'agent failed'], ['cancelled', 'interrupted'], ['internal', 'Try again later'], ['unknown', 'Try again later'],
  ])('explains %s without backend content', async (code, message) => {
    render(<ExtractMenu nodeId={A} sessionId="s1" />)
    await begin()
    done(run({ status: 'failed', error: { code, status: 503 } }))
    expect(screen.getByRole('status')).toHaveTextContent(message)
  })

  it.each(['budget', 'busy', 'not_configured'])('explains a rejected %s request', async (reason) => {
    const data = { code: 'invalid' as const, reason, used: 20, limit: 20 }
    fake.handlers['rule.extract'] = () => { throw new RpcCallError('rpc', 'secret backend content', { code: -32000, message: 'secret backend content', data }) }
    render(<ExtractMenu nodeId={A} sessionId="s1" />)
    await begin()
    const status = screen.getByRole('status')
    expect(status).toHaveTextContent(reason === 'budget' ? '20 / 20' : reason === 'busy' ? 'Another session' : 'config.toml')
    expect(status).not.toHaveTextContent('secret backend content')
  })

  it('treats an unfamiliar run status as failed', async () => {
    render(<ExtractMenu nodeId={A} sessionId="s1" />)
    await begin()
    done(run({ status: 'unknown' }))
    expect(screen.getByRole('status')).toHaveTextContent('The extraction failed.')
  })

  it('stops waiting on disconnect and ignores late completion', async () => {
    render(<ExtractMenu nodeId={A} sessionId="s1" />)
    await begin()
    act(() => useNodes.getState().setConn(A, 'peer_offline'))
    expect(screen.getByRole('status')).toHaveTextContent('Stopped waiting')
    done(run({ status: 'done' }))
    expect(screen.getByRole('status')).toHaveTextContent('Stopped waiting')
  })

  it('cleans up on leaving, including when the response is still pending', async () => {
    let resolve!: (result: RuleExtractResult) => void
    fake.handlers['rule.extract'] = () => new Promise<RuleExtractResult>((r) => { resolve = r })
    const view = render(<ExtractMenu nodeId={A} sessionId="s1" />)
    await begin()
    view.unmount()
    done()
    await act(async () => resolve({ points: 3, run: run() }))
    expect(screen.queryByRole('status')).toBeNull()
  })

  it('limits waiting when the notification is lost, and cleanup detaches every callback', async () => {
    vi.useFakeTimers()
    const update = vi.fn()
    const stop = startExtract(A, 's1', update)
    await vi.advanceTimersByTimeAsync(16 * 60_000)
    expect(update.mock.calls.at(-1)).toEqual([{ kind: 'interrupted' }])
    stop()
    update.mockClear()
    done()
    useNodes.getState().setConn(A, 'peer_offline')
    expect(update).not.toHaveBeenCalled()
  })
})

describe('extraction history and source', () => {
  it('links an extracted source to its exact seq; missing or other-node sessions have no link', () => {
    const rule = { ...ruleItem({ id: 'extracted-rule', source: { kind: 'extracted', session_id: 's1', seq: 7 } }), node_id: A }
    const view = render(<RuleSourceLine rule={rule} />)
    expect(screen.getByTestId('rule-source-session')).toHaveAttribute('href', '/n/aaaaaaaa/s/s1?seq=7')
    act(() => useSessions.setState({ list: {} }))
    expect(screen.getByTestId('rule-source-gone')).toBeInTheDocument()
    expect(screen.queryByRole('link')).toBeNull()
    act(() => useSessions.getState().upsertSummaries(B, [summary({ id: 's1' })]))
    view.rerender(<RuleSourceLine rule={rule} />)
    expect(screen.queryByRole('link')).toBeNull()
  })

  it('loads only selected supporting nodes on expansion and rereads only the notified node', async () => {
    fake.handlers['rule.extract_runs'] = () => ({ runs: [run({ status: 'done', added: ['r1'] }), run({ id: 'failed', status: 'failed', trigger: 'auto', error: { code: 'http', status: 502 }, dropped: { invalid: 1, duplicate: 2, full: 3 } })], used_today: 7, daily_calls: 20 })
    const user = userEvent.setup()
    const view = render(<ExtractHistory nodeIds={[A]} />)
    expect(fake.calls).toHaveLength(0)
    await user.click(screen.getByRole('button', { name: 'Extraction history' }))
    await screen.findByText('Used 7 / 20 calls today')
    expect(fake.calls).toEqual([{ nodeId: A, method: 'rule.extract_runs', params: { limit: 20 } }])
    expect(screen.getAllByTestId('extract-run')).toHaveLength(2)
    expect(screen.getByText('Manual')).toBeInTheDocument()
    expect(screen.getByText('Automatic')).toBeInTheDocument()
    expect(screen.getByText('Added 0 · Dropped 6')).toBeInTheDocument()
    expect(screen.getByText(/HTTP 502/)).toBeInTheDocument()
    expect(screen.getAllByRole('link')[0]).toHaveAttribute('href', '/n/aaaaaaaa/s/s1')
    done(undefined, B)
    expect(fake.calls).toHaveLength(1)
    done()
    await waitFor(() => expect(fake.calls).toHaveLength(2))
    await user.click(screen.getByRole('button', { name: 'Extraction history' }))
    done()
    expect(fake.calls).toHaveLength(2)
    act(() => useNodes.getState().patch(B, { info: info(false) }))
    view.rerender(<ExtractHistory nodeIds={[B]} />)
    expect(screen.queryByTestId('extract-history')).toBeNull()
  })

  it('keeps the latest read when a completion races with the first request, retries failures and refreshes on reconnect', async () => {
    let resolve!: (result: RuleExtractRunsResult) => void
    fake.handlers['rule.extract_runs'] = (_params, nodeId) => nodeId === A ? new Promise<RuleExtractRunsResult>((r) => { resolve = r }) : { runs: [], used_today: 0, daily_calls: 20 }
    const user = userEvent.setup()
    render(<ExtractHistory nodeIds={[A, B]} />)
    await user.click(screen.getByRole('button', { name: 'Extraction history' }))
    // alpha 的初次读取挂起；完成通知触发的新一轮读取先返回。
    fake.handlers['rule.extract_runs'] = () => ({ runs: [run({ status: 'done' })], used_today: 9, daily_calls: 20 })
    done()
    await screen.findByText('Used 9 / 20 calls today')
    await act(async () => resolve({ runs: [], used_today: 0, daily_calls: 20 }))
    expect(within(screen.getAllByTestId('extract-node')[0]!).getByText('Used 9 / 20 calls today')).toBeInTheDocument()
    fake.handlers['rule.extract_runs'] = () => { throw rpcError('internal') }
    done()
    await screen.findByText('Could not load extraction history.')
    fake.handlers['rule.extract_runs'] = () => ({ runs: [], used_today: 10, daily_calls: 20 })
    await user.click(screen.getByRole('button', { name: 'Retry' }))
    await screen.findByText('Used 10 / 20 calls today')
    act(() => useNodes.getState().setConn(A, 'peer_offline'))
    expect(screen.getByText('This node is offline.')).toBeInTheDocument()
    act(() => useNodes.getState().setConn(A, 'ready'))
    await waitFor(() => expect(fake.calls.filter((c) => c.nodeId === A)).toHaveLength(5))
  })
})


describe('extraction result translations', () => {
  it.each(['en', 'zh'])('omits dropped reasons when all counts are zero (%s)', (language) => {
    const t = i18n.getFixedT(language)
    expect(extractResultText(run({ status: 'done' }), t)).toBe(t('rules.extract.noRules'))
  })

  it.each<[keyof ExtractRun['dropped'], number, string, string]>([
    ['duplicate', 1, '1 duplicate', '1 条重复'],
    ['duplicate', 2, '2 duplicates', '2 条重复'],
    ['invalid', 1, '1 invalid rule', '1 条不合格'],
    ['invalid', 2, '2 invalid rules', '2 条不合格'],
    ['full', 1, '1 rule because there are too many pending rules', '1 条因为待确认的规则太多没加'],
    ['full', 2, '2 rules because there are too many pending rules', '2 条因为待确认的规则太多没加'],
  ])('shows only the nonzero reason with the correct plural: %s = %i', (reason, count, en, zh) => {
    const value = run({ status: 'done', dropped: { duplicate: 0, invalid: 0, full: 0, [reason]: count } })
    expect(extractResultText(value, i18n.getFixedT('en'))).toBe(`No new rules were extracted. Dropped: ${en}.`)
    expect(extractResultText(value, i18n.getFixedT('zh'))).toBe(`没有提炼出新的规则。 丢掉了 ${zh}。`)
  })

  it('joins nonzero reasons and omits the zero category in both languages', () => {
    const value = run({ status: 'done', added: ['r1'], dropped: { duplicate: 1, invalid: 0, full: 2 } })
    expect(extractResultText(value, i18n.getFixedT('en'))).toBe('Extracted 1 rule to review. Dropped: 1 duplicate, 2 rules because there are too many pending rules.')
    expect(extractResultText(value, i18n.getFixedT('zh'))).toBe('提炼出 1 条待确认的规则。 丢掉了 1 条重复、2 条因为待确认的规则太多没加。')
  })

  it.each([1, 2])('uses the Chinese plural key for %i added rules', (count) => {
    expect(extractResultText(run({ status: 'done', added: Array.from({ length: count }, (_, i) => `r${i}`) }), i18n.getFixedT('zh')))
      .toBe(`提炼出 ${count} 条待确认的规则。`)
  })
})
