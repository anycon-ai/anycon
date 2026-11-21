// 规则页（spec/web.md § 4.14）：两个标签、每条规则的全文在卡片上才能确认、每个动作（带 `if_updated_ms`）、别处改过的规则不被覆盖、
// 每种拒绝的说法、字数预算、节点不在线时禁用、多选、不认识的状态和来源。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry, project, summary } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeRulesDaemon, ruleError, ruleItem } from '@/test/fakeRulesDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { cancelRuleRefreshes, handleRuleChanged, resetRulesMemory, syncRules } from '@/features/rules/service'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/rules' }))

import { RulesPage } from './RulesPage'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const info = (rules: boolean): NodeInfo => ({ capabilities: { rules } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeRulesDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 5))
  })
}
const cards = (): HTMLElement[] => screen.queryAllByTestId('rule-card')
const setNode = (over: Partial<Parameters<typeof nodeEntry>[0]> = {}): void => {
  useNodes.setState({
    byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(true), conn: 'ready', projects: [project({ name: 'web' }), project({ name: 'api' })], ...over }), [B]: nodeEntry({ id: B, name: 'beta', info: info(false), conn: 'ready' }) },
    order: [A, B],
    loaded: true,
  })
}
async function load(...rules: Array<ReturnType<typeof ruleItem>>): Promise<void> {
  daemon.seed(...rules)
  await syncRules(A)
  render(<RulesPage />)
  await flush()
}

beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeRulesDaemon()
  daemon.install(rpc)
  daemon.onChange = (id, rule) => handleRuleChanged(A, rule ? { id, rule, deleted: false } : { id, deleted: true })
  setRpc(rpc)
  resetRulesMemory()
  usePrefs.setState({ sidebarNodes: null })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.setState({ toasts: [] })
  setNode()
})
afterEach(() => {
  cancelRuleRefreshes()
  setRpc(null)
  vi.restoreAllMocks()
})

const agentRule = (id: string, text = `from the agent ${id}`) => ruleItem({ id, text, status: 'pending', source: { kind: 'agent', session_id: 's1' }, created_ms: 2_000 })

describe('what is on the page', () => {
  it('only shows nodes that support rules; with none it says so and offers no way to add', async () => {
    setNode({ info: info(false) })
    render(<RulesPage />)
    expect(screen.getByText(/No node here supports rules/)).toBeInTheDocument()
    expect(screen.getByTestId('rules-add')).toBeDisabled()
  })

  it('has two tabs with counts; the pending one opens first when something is waiting', async () => {
    await load(agentRule('p1'), ruleItem({ id: 'a1' }), ruleItem({ id: 'a2' }))
    expect(screen.getByTestId('rules-tab-pending')).toHaveTextContent('To review (1)')
    expect(screen.getByTestId('rules-tab-active')).toHaveTextContent('Active (2)')
    expect(screen.getByTestId('rules-pending')).toBeInTheDocument()
    expect(cards()).toHaveLength(1)
  })

  it('opens on the active tab when nothing is waiting', async () => {
    await load(ruleItem({ id: 'a1' }))
    expect(screen.getByTestId('rules-active')).toBeInTheDocument()
    expect(screen.queryByTestId('rules-pending')).toBeNull()
  })

  it('shows the whole text of every rule as plain text: line breaks kept, markup and HTML not interpreted', async () => {
    const text = 'line one\n\n# not a heading\n**not bold** <b>not html</b> <img src=x onerror=alert(1)>\n' + 'long'.repeat(80)
    await load(agentRule('p1', text))
    const el = within(cards()[0]!).getByTestId('rule-text')
    expect(el.textContent).toBe(text)
    expect(el.querySelector('b, img, h1, strong')).toBeNull()
    expect(el.className).toContain('whitespace-pre-wrap')
  })

  it('shows scope, source and last-used time; an agent rule links to the session that proposed it', async () => {
    useSessions.setState({ list: { s1: { ...summary({ id: 's1', title: 'proposer' }), node_id: A, cached_seq: 0 } as never } })
    await load(agentRule('p1'), ruleItem({ id: 'a1', project: 'web', last_injected_ms: Date.now() - 3 * 60_000 }), ruleItem({ id: 'a2' }))
    const pending = cards()[0]!
    expect(within(pending).getByTestId('rule-source')).toHaveAttribute('data-kind', 'agent')
    expect(within(pending).getByTestId('rule-source')).toHaveTextContent('Proposed by an agent')
    expect(within(pending).getByTestId('rule-source-session')).toHaveAttribute('href', expect.stringContaining('/s/s1'))
    expect(within(pending).getByTestId('rule-scope')).toHaveTextContent('All projects')
    await userEvent.click(screen.getByTestId('rules-tab-active'))
    const act1 = screen.getAllByTestId('rule-card').find((c) => c.getAttribute('data-rule') === 'a1')!
    expect(within(act1).getByTestId('rule-scope')).toHaveTextContent('Project: web')
    expect(within(act1).getByTestId('rule-last-used')).toHaveTextContent(/Last used/)
    const act2 = screen.getAllByTestId('rule-card').find((c) => c.getAttribute('data-rule') === 'a2')!
    expect(within(act2).getByTestId('rule-last-used')).toHaveTextContent('Not used in a session yet')
  })

  it('a missing session is said to be gone instead of a dead link', async () => {
    await load(agentRule('p1'))
    expect(within(cards()[0]!).getByTestId('rule-source-gone')).toBeInTheDocument()
    expect(within(cards()[0]!).queryByTestId('rule-source-session')).toBeNull()
  })

  it('a status or source this version does not know is shown as not active / not written by a person, with no status actions', async () => {
    const user = userEvent.setup()
    await load(ruleItem({ id: 'x', status: 'expired' as never, source: { kind: 'twin' as never } }), ruleItem({ id: 'y' }))
    await user.click(screen.getByTestId('rules-tab-active'))
    await user.click(screen.getByTestId('rules-inactive-toggle'))
    const card = screen.getAllByTestId('rule-card').find((c) => c.getAttribute('data-rule') === 'x')!
    expect(card).toHaveAttribute('data-status', 'unknown')
    expect(within(card).getByTestId('rule-status')).toHaveTextContent('Not active')
    expect(within(card).getByTestId('rule-source')).toHaveAttribute('data-kind', 'other')
    expect(within(card).getByTestId('rule-source')).toHaveTextContent('Not written by a person')
    for (const id of ['rule-confirm', 'rule-enable', 'rule-disable', 'rule-reject']) expect(within(card).queryByTestId(id)).toBeNull()
    expect(screen.getByTestId('rules-tab-active')).toHaveTextContent('Active (1)')
  })

  it('disabled and rejected rules are collapsed under the active tab', async () => {
    const user = userEvent.setup()
    await load(ruleItem({ id: 'on' }), ruleItem({ id: 'off', status: 'disabled' }), ruleItem({ id: 'no', status: 'rejected' }))
    expect(cards()).toHaveLength(1)
    expect(screen.getByTestId('rules-inactive-toggle')).toHaveTextContent('(2)')
    await user.click(screen.getByTestId('rules-inactive-toggle'))
    expect(cards()).toHaveLength(3)
  })
})

describe('the size budget', () => {
  it('shows used / cap for every project’s sessions and for the rest', async () => {
    daemon.budget = { cap: 4096, node_bytes: 300, projects: { web: 100 } }
    await load(ruleItem({ id: 'a1' }))
    expect(screen.getByTestId('rules-budget-node-used')).toHaveTextContent('300 / 4096 bytes')
    expect(screen.getByTestId('rules-budget-project-used')).toHaveTextContent('400 / 4096 bytes')
  })
})

describe('confirming', () => {
  it('a pending rule is confirmed from the same card that shows its whole text, with the version it was read at', async () => {
    const user = userEvent.setup()
    await load(agentRule('p1', 'Never push to main without asking me first.'))
    const card = cards()[0]!
    expect(within(card).getByTestId('rule-text')).toHaveTextContent('Never push to main without asking me first.')
    await user.click(within(card).getByTestId('rule-confirm'))
    await flush()
    expect(daemon.calls('rule.update')[0]).toEqual({ id: 'p1', status: 'active', if_updated_ms: 1_000 })
    expect(daemon.rules.get('p1')!.status).toBe('active')
    await waitFor(() => expect(screen.getByTestId('rules-pending-empty')).toBeInTheDocument())
  })

  it('reject keeps the rule but it is no longer waiting', async () => {
    const user = userEvent.setup()
    await load(agentRule('p1'))
    await user.click(within(cards()[0]!).getByTestId('rule-reject'))
    await flush()
    expect(daemon.rules.get('p1')!.status).toBe('rejected')
  })

  it('a rule edited elsewhere is not confirmed: the page says so and shows the new text', async () => {
    const user = userEvent.setup()
    await load(agentRule('p1', 'original'))
    // the page has not heard about the change yet when the button is pressed
    daemon.onChange = null
    daemon.changeElsewhere('p1', { text: 'edited elsewhere' })
    await user.click(within(cards()[0]!).getByTestId('rule-confirm'))
    await flush()
    expect(daemon.rules.get('p1')!.status).toBe('pending')
    expect(useUi.getState().toasts.some((t) => /changed elsewhere/.test(t.text))).toBe(true)
    expect(within(cards()[0]!).getByTestId('rule-text')).toHaveTextContent('edited elsewhere')
  })

  it('an over-budget refusal explains the numbers and what to do', async () => {
    const user = userEvent.setup()
    await load(agentRule('p1'))
    daemon.failNext('rule.update', ruleError('invalid', { field: 'status', reason: 'over_budget', bytes: 4300, cap: 4096 }))
    await user.click(within(cards()[0]!).getByTestId('rule-confirm'))
    await flush()
    const msg = useUi.getState().toasts.at(-1)!.text
    expect(msg).toContain('4300')
    expect(msg).toContain('4096')
    expect(msg).toMatch(/Merge some rules or disable some/)
    expect(daemon.rules.get('p1')!.status).toBe('pending')
  })

  it('edit-then-confirm saves the edited text and activates it in one step', async () => {
    const user = userEvent.setup()
    await load(agentRule('p1', 'draft'))
    await user.click(within(cards()[0]!).getByTestId('rule-edit-confirm'))
    const text = screen.getByTestId('rule-editor-text')
    expect(text).toHaveValue('draft')
    await user.clear(text)
    await user.type(text, 'better wording')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(daemon.calls('rule.update')[0]).toEqual({ id: 'p1', text: 'better wording', status: 'active', if_updated_ms: 1_000 })
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })
})

describe('other actions', () => {
  it('disable and enable', async () => {
    const user = userEvent.setup()
    await load(ruleItem({ id: 'a1' }))
    await user.click(within(cards()[0]!).getByTestId('rule-disable'))
    await flush()
    expect(daemon.rules.get('a1')!.status).toBe('disabled')
    await user.click(screen.getByTestId('rules-inactive-toggle'))
    await user.click(within(cards()[0]!).getByTestId('rule-enable'))
    await flush()
    expect(daemon.rules.get('a1')!.status).toBe('active')
    expect(daemon.calls('rule.update').map((c) => c.if_updated_ms)).toEqual([1_000, expect.any(Number)])
  })

  it('delete asks first, shows the text, and sends the version', async () => {
    const user = userEvent.setup()
    await load(ruleItem({ id: 'a1', text: 'about to be deleted' }))
    await user.click(within(cards()[0]!).getByTestId('rule-delete'))
    expect(screen.getByTestId('rule-delete-text')).toHaveTextContent('about to be deleted')
    expect(daemon.calls('rule.delete')).toHaveLength(0)
    await user.click(screen.getByTestId('rule-delete-confirm'))
    await flush()
    expect(daemon.calls('rule.delete')[0]).toEqual({ id: 'a1', if_updated_ms: 1_000 })
    expect(daemon.rules.has('a1')).toBe(false)
  })

  it('delete of a rule changed elsewhere does not delete; the dialog shows the new version and a second click does', async () => {
    const user = userEvent.setup()
    await load(ruleItem({ id: 'a1', text: 'old text' }))
    await user.click(within(cards()[0]!).getByTestId('rule-delete'))
    daemon.onChange = null
    daemon.changeElsewhere('a1', { text: 'new text' })
    await user.click(screen.getByTestId('rule-delete-confirm'))
    await flush()
    expect(daemon.rules.has('a1')).toBe(true)
    expect(screen.getByTestId('rule-delete-error')).toHaveTextContent(/changed elsewhere/)
    expect(screen.getByTestId('rule-delete-text')).toHaveTextContent('new text')
    await user.click(screen.getByTestId('rule-delete-confirm'))
    await flush()
    expect(daemon.rules.has('a1')).toBe(false)
  })
})

describe('writing a rule', () => {
  it('adds an active rule for every project, or for one; the id is made here', async () => {
    const user = userEvent.setup()
    await load()
    await user.click(screen.getByTestId('rules-add'))
    await user.type(screen.getByTestId('rule-editor-text'), 'Write the failing test first.')
    await user.selectOptions(screen.getByTestId('rule-editor-scope'), 'web')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    const call = daemon.calls('rule.add')[0]!
    expect(call).toMatchObject({ text: 'Write the failing test first.', project: 'web', status: 'active' })
    expect(call.id).toMatch(/^[0-9a-f-]{36}$/)
    expect(call).not.toHaveProperty('source')
  })

  it('checks the text before sending: empty, too long, control characters and invisible characters', async () => {
    const user = userEvent.setup()
    await load()
    await user.click(screen.getByTestId('rules-add'))
    const text = screen.getByTestId('rule-editor-text')
    const save = screen.getByTestId('rule-editor-save')
    expect(save).toBeDisabled()
    await user.click(text)
    await user.paste('a'.repeat(601))
    expect(save).toBeDisabled()
    expect(screen.getByText('A rule can be at most 600 characters.')).toBeInTheDocument()
    await user.clear(text)
    await user.paste(`sneaky${String.fromCodePoint(0x202e)}text`)
    expect(save).toBeDisabled()
    expect(screen.getByText(/invisible characters/)).toBeInTheDocument()
    await user.clear(text)
    await user.paste('fine\nwith a line break')
    expect(save).toBeEnabled()
    expect(daemon.calls('rule.add')).toHaveLength(0)
  })

  it.each([
    ['too_many', ruleError('invalid', { field: 'rules', reason: 'too_many' }), /already 300 rules/],
    ['too_many_pending', ruleError('invalid', { field: 'rules', reason: 'too_many_pending' }), /proposed by agents are waiting/],
    ['invisible', ruleError('invalid', { field: 'text', reason: 'invisible' }), /invisible characters/],
    ['over_budget', ruleError('invalid', { field: 'text', reason: 'over_budget', bytes: 5000, cap: 4096 }), /5000 bytes.*at most 4096/],
  ])('shows the node’s refusal in plain words (%s) and keeps what you wrote', async (_n, error, text) => {
    const user = userEvent.setup()
    await load()
    await user.click(screen.getByTestId('rules-add'))
    await user.type(screen.getByTestId('rule-editor-text'), 'my rule')
    daemon.failNext('rule.add', error)
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(screen.getByTestId('rule-editor-error')).toHaveTextContent(text)
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('my rule')
  })

  it('a retry after a lost reply uses the same id (the node answers with the rule it already has)', async () => {
    const user = userEvent.setup()
    await load()
    await user.click(screen.getByTestId('rules-add'))
    await user.type(screen.getByTestId('rule-editor-text'), 'once only')
    daemon.failNext('rule.add', new (await import('@/transport/rpcClient')).RpcCallError('timeout', 'lost'))
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(screen.getByTestId('rule-editor-error')).toHaveAttribute('data-reason', 'link')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    const ids = daemon.calls('rule.add').map((c) => c.id)
    expect(ids).toHaveLength(2)
    expect(ids[0]).toBe(ids[1])
  })

  it('editing a rule changed elsewhere shows the new version and saves against it only after you look', async () => {
    const user = userEvent.setup()
    await load(ruleItem({ id: 'a1', text: 'mine' }))
    await user.click(within(cards()[0]!).getByTestId('rule-edit'))
    daemon.onChange = null
    daemon.changeElsewhere('a1', { text: 'theirs' })
    await user.clear(screen.getByTestId('rule-editor-text'))
    await user.type(screen.getByTestId('rule-editor-text'), 'my edit')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(daemon.rules.get('a1')!.text).toBe('theirs')
    expect(screen.getByTestId('rule-editor-conflict')).toBeInTheDocument()
    expect(screen.getByTestId('rule-editor-current')).toHaveTextContent('theirs')
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('my edit')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(daemon.rules.get('a1')!.text).toBe('my edit')
  })
})

describe('multi-select', () => {
  it('confirms or rejects several waiting rules at once, each with its own version; a failure is reported and the rest still go through', async () => {
    const user = userEvent.setup()
    await load(agentRule('p1'), agentRule('p2'), agentRule('p3'))
    daemon.onChange = null
    daemon.changeElsewhere('p2', { text: 'edited' })
    await user.click(screen.getByTestId('rules-select-all'))
    expect(screen.getByTestId('rules-bulk')).toHaveTextContent('3 selected')
    await user.click(screen.getByTestId('rules-bulk-confirm'))
    await flush()
    expect(daemon.rules.get('p1')!.status).toBe('active')
    expect(daemon.rules.get('p2')!.status).toBe('pending')
    expect(daemon.rules.get('p3')!.status).toBe('active')
    expect(useUi.getState().toasts.at(-1)!.text).toMatch(/Done: 2\. Not done: 1/)
  })

  it('only the ticked rules are confirmed or rejected; the others are not touched', async () => {
    const user = userEvent.setup()
    await load(agentRule('p1'), agentRule('p2'), agentRule('p3'))
    const box = (id: string): HTMLElement => within(cards().find((c) => c.getAttribute('data-rule') === id)!).getByTestId('rule-select')
    await user.click(box('p1'))
    await user.click(box('p3'))
    expect(screen.getByTestId('rules-bulk')).toHaveTextContent('2 selected')
    await user.click(screen.getByTestId('rules-bulk-confirm'))
    await flush()
    expect(daemon.calls('rule.update').map((c) => c.id).sort()).toEqual(['p1', 'p3'])
    expect(daemon.rules.get('p2')!.status).toBe('pending')
    // reject: just the one that is ticked now
    await user.click(box('p2'))
    await user.click(screen.getByTestId('rules-bulk-reject'))
    await flush()
    expect(daemon.rules.get('p2')!.status).toBe('rejected')
    expect(daemon.calls('rule.update')).toHaveLength(3)
  })

  it('a rule that changes after it was ticked is unticked and marked; the bulk action does not send it, and ticking again sends the new version', async () => {
    const user = userEvent.setup()
    await load(agentRule('p1'), agentRule('p2'))
    const card = (id: string): HTMLElement => cards().find((c) => c.getAttribute('data-rule') === id)!
    await user.click(screen.getByTestId('rules-select-all'))
    expect(screen.getByTestId('rules-bulk')).toHaveTextContent('2 selected')
    const changed = daemon.changeElsewhere('p2', { text: 'now it says something else' })
    await flush()
    expect(within(card('p2')).getByTestId('rule-select')).not.toBeChecked()
    expect(within(card('p2')).getByTestId('rule-stale')).toBeInTheDocument()
    expect(within(card('p1')).queryByTestId('rule-stale')).toBeNull()
    expect(screen.getByTestId('rules-bulk')).toHaveTextContent('1 selected')
    await user.click(screen.getByTestId('rules-bulk-confirm'))
    await flush()
    expect(daemon.calls('rule.update').map((c) => c.id)).toEqual(['p1'])
    expect(daemon.rules.get('p2')!.status).toBe('pending')
    // read again, tick again: the new version goes out
    await user.click(within(card('p2')).getByTestId('rule-select'))
    expect(within(card('p2')).queryByTestId('rule-stale')).toBeNull()
    await user.click(screen.getByTestId('rules-bulk-confirm'))
    await flush()
    expect(daemon.calls('rule.update')[1]).toMatchObject({ id: 'p2', if_updated_ms: changed.updated_ms })
    expect(daemon.rules.get('p2')!.status).toBe('active')
  })

  it('the bulk buttons need a selection', async () => {
    await load(agentRule('p1'))
    expect(screen.getByTestId('rules-bulk-confirm')).toBeDisabled()
    expect(screen.getByTestId('rules-bulk-reject')).toBeDisabled()
  })
})

describe('offline', () => {
  it('every action is disabled and says why; nothing is sent', async () => {
    await load(agentRule('p1'), ruleItem({ id: 'a1' }))
    act(() => setNode({ conn: 'peer_offline' }))
    expect(screen.getByTestId('rules-offline')).toHaveTextContent('alpha')
    for (const id of ['rule-confirm', 'rule-edit-confirm', 'rule-reject', 'rule-delete']) expect(within(cards()[0]!).getByTestId(id)).toBeDisabled()
    expect(screen.getByTestId('rules-add')).toBeDisabled()
    expect(rpc.calls.filter((c) => c.method.startsWith('rule.') && c.method !== 'rule.list')).toEqual([])
  })
})

describe('several nodes', () => {
  it('labels rules with their node when more than one supporting node is selected', async () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: info(true), conn: 'ready' }), [B]: nodeEntry({ id: B, name: 'beta', info: info(true), conn: 'ready' }) }, order: [A, B], loaded: true })
    daemon.seed(agentRule('p1'))
    await syncRules(A)
    await syncRules(B)
    render(<RulesPage />)
    await flush()
    expect(within(cards()[0]!).getByTestId('rule-node')).toHaveTextContent('alpha')
  })
})
