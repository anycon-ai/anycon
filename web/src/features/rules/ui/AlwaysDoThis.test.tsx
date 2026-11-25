// 「以后都这样」（docs/spec/daemon.md § 23.6，spec/web.md § 14.6）：回答 agent 的提问和 Bot 的问题时的勾选。勾上并且答复成功之后才打开
// 「记成规则」的同一个编辑框；答复失败、没勾都不打开；预填原样不能保存；勾选不改答复的参数，也不碰按钮的锁。
import { act, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import type { NodeInfo } from '@/protocol'
import type { PermissionBlock } from '@/features/session/reducer'
import { useRespondPermission } from '@/features/session/hooks'
import { PermissionCard } from '@/features/session/ui/PermissionCard'
import { TwinQuestionForm } from '@/features/twin/Question'
import { nodeEntry, project, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeRulesDaemon } from '@/test/fakeRulesDaemon'
import { twinQuestion } from '@/test/fakeTwinDaemon'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { cancelRuleRefreshes, resetRulesMemory } from '../service'
import { useFollowUp } from '../followUp'
import { FollowUpRuleHost } from './AlwaysDoThis'

const N = 'n'.repeat(32)
const info = (rules: boolean): NodeInfo => ({ capabilities: { rules } }) as unknown as NodeInfo

const question: PermissionBlock = {
  type: 'permission',
  key: 'perm-q1',
  seq: 1,
  ts: 1,
  requestId: 'q1',
  kind: 'question',
  title: 'Which package manager?',
  detail: { questions: [{ id: 'pm', text: 'Which package manager should I use?', choices: [{ id: 'pnpm', label: 'pnpm' }, { id: 'npm', label: 'npm' }] }] },
  options: [{ id: 'submit', label: 'Submit answers', kind: 'allow_once' }],
}
const tool: PermissionBlock = {
  ...question,
  requestId: 'r1',
  key: 'perm-r1',
  kind: 'tool',
  title: 'Run the migration',
  detail: { tool: { tool: 'Bash', category: 'shell', input: { command: 'pnpm db:migrate' }, detail: { command: 'pnpm db:migrate' } } },
  options: [{ id: 'allow', label: 'Allow once', kind: 'allow_once' }, { id: 'deny', label: 'Deny', kind: 'reject_once' }],
}

let rpc: FakeRpc
let daemon: FakeRulesDaemon
const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 8; i++) await new Promise((r) => setTimeout(r, 5))
  })
}
const setNode = (rules = true): void => {
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'alpha', info: info(rules), conn: 'ready', projects: [project({ name: 'web' })] }) }, order: [N], loaded: true })
}

/** 真正的答复路径（`useRespondPermission`）：成功才会调用 `onAnswered`。 */
function Harness({ block, busy }: { block: PermissionBlock; busy?: boolean }) {
  const { respond, responding } = useRespondPermission(N)
  return (
    <>
      <PermissionCard block={block} deviceLabel={() => 'you'} onRespond={respond} busy={busy || responding.has(block.requestId)} recordAsRule={{ nodeId: N, sessionId: 's1' }} />
      <FollowUpRuleHost />
    </>
  )
}
const respondCalls = (): Array<Record<string, unknown>> => rpc.calls.filter((c) => c.method === 'permission.respond').map((c) => c.params as Record<string, unknown>)

beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeRulesDaemon()
  daemon.install(rpc)
  rpc.handlers['permission.respond'] = () => ({})
  setRpc(rpc)
  resetRulesMemory()
  useFollowUp.getState().close()
  useUi.setState({ toasts: [] })
  useSessions.setState({ list: { s1: { ...summary({ id: 's1', project: 'web' }), node_id: N, cached_seq: 0 } as never }, pending: {}, open: {} })
  setNode()
})
afterEach(() => {
  cancelRuleRefreshes()
  setRpc(null)
})

async function answerQuestion(user: ReturnType<typeof userEvent.setup>, always: boolean): Promise<void> {
  await user.click(within(screen.getByTestId('permission-questions')).getByLabelText('pnpm'))
  if (always) await user.click(screen.getByTestId('always-do-this-box'))
  await user.click(screen.getByTestId('permission-option-submit'))
  await flush()
}

describe('an agent’s question', () => {
  it('has the box next to the answer buttons; the other kinds of request and nodes without rules do not', () => {
    const { unmount } = render(<Harness block={question} />)
    expect(screen.getByTestId('always-do-this')).toHaveTextContent('Always do this')
    unmount()
    render(<Harness block={tool} />)
    expect(screen.queryByTestId('always-do-this')).toBeNull()
  })

  it('is not there when the node has no rules', () => {
    setNode(false)
    render(<Harness block={question} />)
    expect(screen.queryByTestId('always-do-this')).toBeNull()
  })

  it('ticked and answered: the same editor opens, with the question and your answer kept apart and a prefilled sentence', async () => {
    const user = userEvent.setup()
    render(<Harness block={question} />)
    await answerQuestion(user, true)
    expect(respondCalls()).toEqual([{ request_id: 'q1', answers: { pm: 'pnpm' }, option_id: 'submit' }])
    expect(screen.getByTestId('rule-editor')).toBeInTheDocument()
    expect(screen.getByTestId('record-agent')).toHaveTextContent('Which package manager should I use?')
    expect(screen.getByTestId('record-choice')).toHaveTextContent('pnpm')
    expect(screen.getByTestId('record-choice')).not.toHaveTextContent('Which package manager should I use?')
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Which package manager should I use?”, my answer is: pnpm.')
    expect(screen.getByTestId('rule-editor-scope')).toHaveValue('web')
    expect(screen.getByTestId('rule-editor')).toHaveTextContent('A rule does not approve anything')
  })

  it('cannot be saved as prefilled; nothing is saved until you change the text and press save', async () => {
    const user = userEvent.setup()
    render(<Harness block={question} />)
    await answerQuestion(user, true)
    expect(screen.getByTestId('rule-editor-save')).toBeDisabled()
    expect(screen.getByTestId('rule-editor-edit-first')).toBeInTheDocument()
    expect(daemon.calls('rule.add')).toHaveLength(0)
    await user.type(screen.getByTestId('rule-editor-text'), ' Use it in every repo.')
    expect(screen.getByTestId('rule-editor-save')).toBeEnabled()
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(daemon.calls('rule.add')).toHaveLength(1)
    expect(daemon.calls('rule.add')[0]).toMatchObject({
      text: 'When asked “Which package manager should I use?”, my answer is: pnpm. Use it in every repo.',
      project: 'web',
      status: 'active',
      source: { kind: 'answer', session_id: 's1', request_id: 'q1' },
    })
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })

  it('closing the editor saves nothing', async () => {
    const user = userEvent.setup()
    render(<Harness block={question} />)
    await answerQuestion(user, true)
    await user.click(screen.getByTestId('rule-editor-cancel'))
    expect(daemon.calls('rule.add')).toHaveLength(0)
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })

  it('not ticked: the answer goes out and no editor opens', async () => {
    const user = userEvent.setup()
    render(<Harness block={question} />)
    await answerQuestion(user, false)
    expect(respondCalls()).toHaveLength(1)
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })

  it('the answer fails: no editor opens', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    render(<Harness block={question} />)
    await answerQuestion(user, true)
    expect(respondCalls()).toHaveLength(1)
    expect(screen.queryByTestId('rule-editor')).toBeNull()
    expect(useUi.getState().toasts.map((t) => t.kind)).toEqual(['error'])
  })

  it('the request is already gone (not_found): that is not an answer, so no editor opens', async () => {
    rpc.handlers['permission.respond'] = () => {
      throw rpcError('not_found')
    }
    const user = userEvent.setup()
    render(<Harness block={question} />)
    await answerQuestion(user, true)
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })

  it('does not change the answer: the same parameters go out with or without the tick', async () => {
    const user = userEvent.setup()
    const { unmount } = render(<Harness block={question} />)
    await answerQuestion(user, false)
    const without = respondCalls()[0]
    unmount()
    rpc.calls.length = 0
    render(<Harness block={{ ...question, requestId: 'q1' }} />)
    await answerQuestion(user, true)
    expect(respondCalls()[0]).toEqual(without)
  })

  it('does not unlock the answer buttons: while they are locked a tick leaves them locked and nothing is sent', async () => {
    const user = userEvent.setup()
    render(<Harness block={question} busy />)
    await user.click(within(screen.getByTestId('permission-questions')).getByLabelText('pnpm'))
    await user.click(screen.getByTestId('always-do-this-box'))
    expect(screen.getByTestId('permission-option-submit')).toBeDisabled()
    await user.click(screen.getByTestId('permission-option-submit'))
    expect(respondCalls()).toEqual([])
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })

  it('a refusal on saving (the rules are full) is shown in the editor and the answer stays answered', async () => {
    const user = userEvent.setup()
    render(<Harness block={question} />)
    await answerQuestion(user, true)
    await user.type(screen.getByTestId('rule-editor-text'), ' x')
    daemon.failNext('rule.add', (await import('@/test/fakeRulesDaemon')).ruleError('invalid', { field: 'text', reason: 'over_budget', bytes: 4200, cap: 4096 }))
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    expect(screen.getByTestId('rule-editor-error')).toHaveTextContent(/4200 bytes/)
    expect(respondCalls()).toHaveLength(1)
  })
})

describe('a twin question', () => {
  const nodeWithTwin = (rules = true): void => {
    useNodes.setState({ byId: { [N]: nodeEntry({ id: N, name: 'alpha', info: { capabilities: { rules, twin: true } } as unknown as NodeInfo, conn: 'ready', projects: [project({ name: 'web' })] }) }, order: [N], loaded: true })
  }
  const form = (armed = true) => (
    <>
      <TwinQuestionForm nodeId={N} question={twinQuestion({ id: 'tq1', question: 'Which project?', options: ['Web', 'API'] })} armed={armed} />
      <FollowUpRuleHost />
    </>
  )
  beforeEach(() => {
    nodeWithTwin()
    rpc.handlers['twin.answer'] = (p) => ({ ...twinQuestion({ id: 'tq1' }), status: 'answered', answer: (p as { answer: string }).answer })
  })
  const answers = (): Array<Record<string, unknown>> => rpc.calls.filter((c) => c.method === 'twin.answer').map((c) => c.params as Record<string, unknown>)

  it('ticked and answered with an option: the editor opens; the model’s question is shown apart, the source has no session', async () => {
    const user = userEvent.setup()
    render(form())
    await user.click(screen.getByTestId('always-do-this-box'))
    await user.click(screen.getByRole('button', { name: 'Web' }))
    await flush()
    expect(answers()).toEqual([{ id: 'tq1', answer: 'Web' }])
    expect(screen.getByTestId('record-agent')).toHaveTextContent('Which project?')
    expect(screen.getByTestId('record-choice')).toHaveTextContent('Web')
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Which project?”, my answer is: Web.')
    expect(screen.getByTestId('rule-editor-save')).toBeDisabled()
    await user.type(screen.getByTestId('rule-editor-text'), ' Start with the web app.')
    await user.click(screen.getByTestId('rule-editor-save'))
    await flush()
    const added = daemon.calls('rule.add')
    expect(added).toHaveLength(1)
    expect(added[0]).toMatchObject({ status: 'active', source: { kind: 'answer', request_id: 'tq1' } })
    expect(added[0]!.source).not.toHaveProperty('session_id')
  })

  it('ticked and answered in the text box', async () => {
    const user = userEvent.setup()
    render(form())
    await user.click(screen.getByTestId('always-do-this-box'))
    await user.type(screen.getByRole('textbox', { name: 'Answer' }), 'Ask me first')
    await user.click(screen.getByRole('button', { name: 'Answer' }))
    await flush()
    expect(screen.getByTestId('rule-editor-text')).toHaveValue('When asked “Which project?”, my answer is: Ask me first.')
  })

  it('not ticked: no editor', async () => {
    const user = userEvent.setup()
    render(form())
    await user.click(screen.getByRole('button', { name: 'Web' }))
    await flush()
    expect(answers()).toHaveLength(1)
    expect(screen.queryByTestId('rule-editor')).toBeNull()
  })

  it('the answer fails: no editor, and the tick is kept for the retry', async () => {
    rpc.handlers['twin.answer'] = () => {
      throw rpcError('internal')
    }
    const user = userEvent.setup()
    render(form())
    await user.click(screen.getByTestId('always-do-this-box'))
    await user.click(screen.getByRole('button', { name: 'Web' }))
    await flush()
    expect(screen.queryByTestId('rule-editor')).toBeNull()
    expect(screen.getByRole('alert')).toBeInTheDocument()
    expect(screen.getByTestId('always-do-this-box')).toBeChecked()
  })

  it('while the answer buttons are locked a tick unlocks nothing', async () => {
    const user = userEvent.setup()
    render(form(false))
    await user.click(screen.getByTestId('always-do-this-box'))
    expect(screen.getByRole('button', { name: 'Web' })).toBeDisabled()
    expect(answers()).toEqual([])
  })

  it('is not there when the node has no rules', () => {
    nodeWithTwin(false)
    render(form())
    expect(screen.queryByTestId('always-do-this')).toBeNull()
  })
})
