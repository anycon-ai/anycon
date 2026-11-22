// 范围选择、确认、项目限制和卡片同步标记；对齐的推送与失败记录操作。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import * as db from '@/db'
import type { NodeInfo } from '@/protocol'
import { setNavigator } from '@/lib/navigate'
import { nodeEntry, project } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeRulesDaemon, ruleItem } from '@/test/fakeRulesDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { resetRulesMemory } from '../service'
import { useRuleSync, replayRuleSync, resetRuleSync, enqueueRuleSync } from '../groupSync'
import { RuleEditor } from './RuleEditor'
import { RuleCard } from './RuleCard'
import { GroupAlignment } from './GroupAlignment'
import { useRuleOps } from './useRuleOps'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/rules' }))
const A = 'a'.repeat(32), B = 'b'.repeat(32)
const info = { group: 'work', capabilities: { rules: true } } as unknown as NodeInfo
const navigate = vi.fn()
let rpc: FakeRpc
let daemon: FakeRulesDaemon
beforeEach(async () => {
  resetRulesMemory()
  navigate.mockReset()
  setNavigator(navigate)
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeRulesDaemon()
  daemon.install(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: {
    [A]: nodeEntry({ id: A, name: 'alpha', info, conn: 'ready', projects: [project({ name: 'app' })] }),
    [B]: nodeEntry({ id: B, name: 'beta', info, conn: 'peer_offline' }),
  }, order: [A, B] })
})
afterEach(async () => {
  await replayRuleSync(B)
  resetRulesMemory()
  setRpc(null)
  setNavigator(null)
})

it('新建缺省本节点，选同组后参数与持久化期望都是 group', async () => {
  const user = userEvent.setup()
  render(<RuleEditor open onOpenChange={vi.fn()} mode={{ kind: 'add', nodeIds: [A] }} />)
  expect(screen.getByTestId('rule-editor-sync')).toHaveValue('node')
  await user.type(screen.getByTestId('rule-editor-text'), 'ask first')
  await user.selectOptions(screen.getByTestId('rule-editor-sync'), 'group')
  await user.click(screen.getByTestId('rule-editor-save'))
  await waitFor(() => expect(daemon.calls('rule.add')[0]).toMatchObject({ sync: 'group' }))
  await waitFor(async () => expect(await db.listRuleSync()).toMatchObject([{ node: B, desired: { text: 'ask first', sync: 'group' } }]))
})

it('确认待确认规则打开范围编辑器，一次 update 同时 active 与 group', async () => {
  const rule = ruleItem({ id: 'one', status: 'pending', source: { kind: 'extracted' } })
  daemon.seed(rule)
  function Card() {
    const { ops, dialogs } = useRuleOps()
    return <><RuleCard rule={{ ...rule, node_id: A }} online showNode={false} ops={ops} />{dialogs}</>
  }
  const user = userEvent.setup()
  render(<Card />)
  await user.click(screen.getByTestId('rule-confirm'))
  expect(screen.getByTestId('rule-editor-sync')).toHaveValue('node')
  expect(daemon.calls('rule.update')).toEqual([])
  await user.selectOptions(screen.getByTestId('rule-editor-sync'), 'group')
  await user.click(screen.getByTestId('rule-editor-save'))
  await waitFor(() => expect(daemon.calls('rule.update')).toEqual([{ id: 'one', status: 'active', sync: 'group', if_updated_ms: rule.updated_ms }]))
})

it('编辑保留同组范围，改回本节点说明副本保留；项目规则禁用同组', async () => {
  const user = userEvent.setup()
  const view = { ...ruleItem({ id: 'one', sync: 'group' }), node_id: A }
  daemon.seed(view)
  const { unmount } = render(<RuleEditor open onOpenChange={vi.fn()} mode={{ kind: 'edit', rule: view, confirm: false }} />)
  expect(screen.getByTestId('rule-editor-sync')).toHaveValue('group')
  expect(screen.getByText(/leaves other nodes/)).toBeInTheDocument()
  await user.selectOptions(screen.getByTestId('rule-editor-sync'), 'node')
  await user.click(screen.getByTestId('rule-editor-save'))
  await waitFor(() => expect(daemon.calls('rule.update')[0]).toMatchObject({ sync: 'node' }))
  unmount()
  render(<RuleEditor open onOpenChange={vi.fn()} mode={{ kind: 'edit', rule: { ...view, project: 'app', sync: 'node' }, confirm: true }} />)
  expect(within(screen.getByTestId('rule-editor-sync')).getByRole('option', { name: 'Group nodes' })).toBeDisabled()
  expect(screen.getByText('Project rules cannot sync to the group.')).toBeInTheDocument()
})

it('没有其他同组节点时不显示范围选择', () => {
  useNodes.getState().patch(B, { info: { ...info, group: '' } })
  render(<RuleEditor open onOpenChange={vi.fn()} mode={{ kind: 'add', nodeIds: [A] }} />)
  expect(screen.queryByTestId('rule-editor-sync')).not.toBeInTheDocument()
})

it('卡片标出同组、待发数量、失败与对齐入口', async () => {
  const rule = { ...ruleItem({ id: 'one', sync: 'group' }), node_id: A }
  function Card() {
    const { ops } = useRuleOps()
    return <RuleCard rule={rule} online showNode={false} ops={ops} />
  }
  render(<Card />)
  expect(screen.getByTestId('rule-group')).toHaveTextContent('Group')
  await act(async () => enqueueRuleSync([B], 'one', { text: rule.text, status: 'active', sync: 'group' }))
  expect(screen.getByTestId('rule-sync-status')).toHaveTextContent('Syncing to 1 node')
  act(() => useRuleSync.setState((s) => ({ records: s.records.map((r) => ({ ...r, state: 'failed', error: { code: 'forbidden' } })) })))
  expect(screen.getByTestId('rule-sync-status')).toHaveTextContent('Sync failed')
  await userEvent.setup().click(screen.getByTestId('rule-sync-status'))
  expect(navigate).toHaveBeenCalledWith('/rules')
  expect(useRuleSync.getState().review).toBe(A)
})

it('对齐显示离线未知，选版本只给其余节点排队；失败记录可以放弃', async () => {
  daemon.seed(ruleItem({ id: 'one', sync: 'group' }))
  const nodes = [A, B]
  const user = userEvent.setup()
  render(<GroupAlignment nodes={nodes} open onOpenChange={vi.fn()} />)
  const diff = await screen.findByTestId('rule-difference')
  expect(within(diff).getByText('Offline or unreadable · Unknown')).toBeInTheDocument()
  await user.click(within(diff).getByText('Use this version'))
  await waitFor(async () => expect(await db.listRuleSync()).toMatchObject([{ node: B, rule_id: 'one' }]))
  const rec = (await db.listRuleSync())[0]!
  await act(async () => {
    await db.finishRuleSync(rec.seq!, { code: 'invalid', reason: 'over_budget' })
    useRuleSync.setState({ records: await db.listRuleSync() })
  })
  expect(screen.getByTestId('rule-sync-failed')).toHaveTextContent('over_budget')
  await user.click(within(screen.getByTestId('rule-sync-failed')).getByText('Discard'))
  await waitFor(async () => expect(await db.listRuleSync()).toEqual([]))
  resetRuleSync()
})

it('没有读到同组规则时，离线节点仍为未知，不能显示一致', async () => {
  useNodes.getState().patch(B, { conn: 'ready' })
  render(<GroupAlignment nodes={[A, B]} open onOpenChange={vi.fn()} />)
  await screen.findByText('All known group rules match.')
  act(() => useNodes.getState().patch(B, { conn: 'peer_offline' }))
  expect(screen.getByText(/beta: Offline or unreadable/)).toBeInTheDocument()
  expect(screen.queryByText('All known group rules match.')).not.toBeInTheDocument()
})
