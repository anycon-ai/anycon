// 周期项的跨次状态（spec/web.md § 4.12，docs/spec/daemon.md § 26.11）：表单的开关（编辑只改别的字段、只改时间都不丢开关）、
// 用法说明取自节点且即时反映没保存的勾选、「复制到备注」「插入示例」、分段预览、模板详情里的查看和清理、「经过」里的用量。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo, PromptSegment, RepeatStateEntry, TodoEvent, TodoRepeatPreviewParams } from '@/protocol'
import * as db from '@/db'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTodoDaemon } from '@/test/fakeTodoDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { handleStateChanged, useStateCounts } from '../state'
import { resetTodosMemory, syncTodos } from '../service'
import { draftKey, useDrafts } from '../drafts'
import { useTodoHistory, historyKey } from '../history'
import { useTodoView } from '../store'
import { BODY_MAX_BYTES } from '../validate'
import { TodoDetail } from './TodoDetail'
import { TodoHistory } from './TodoHistory'
import { CaptureForm } from './CaptureForm'
import { useCapture } from '../capture'
import { PromptSegments } from './StateBits'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}))

const A = 'a'.repeat(32)
const T = '11111111-1111-4111-8111-111111111111'
const caps = (state: boolean) => ({ capabilities: { todo: true, todo_repeat: true, todo_repeat_interval: true, todo_repeat_state: state, rules: true } }) as unknown as NodeInfo

let rpc: FakeRpc
let daemon: FakeTodoDaemon
let entries: RepeatStateEntry[]

const flush = async (): Promise<void> => {
  await act(async () => {
    for (let i = 0; i < 30; i++) await new Promise((r) => setTimeout(r, 10))
  })
}

/** 假 daemon 的用法说明：真的文字在 daemon 里，这里只要认得出来。 */
const usageOf = (id: string, count?: number): string => `USAGE for ${id}${count === undefined ? '' : ` (${count})`}`

function previewOf(p: TodoRepeatPreviewParams): { first_message: string; segments: PromptSegment[]; rules_text: string; state_usage_text: string; state_count: number } {
  const stored = daemon.todos.get(p.id)
  const title = p.title ?? stored?.title ?? ''
  const body = p.body ?? stored?.body ?? ''
  const keep = p.keep_state ?? stored?.repeat?.keep_state ?? false
  const off = p.state_hint_off ?? stored?.repeat?.state_hint_off ?? false
  const segments: PromptSegment[] = [{ kind: 'title', text: title }]
  if (body) segments.push({ kind: 'body', text: body })
  if (keep && !off) segments.push({ kind: 'state_usage', text: usageOf(p.id, entries.length) })
  segments.push({ kind: 'closing_report', text: 'When you are done, report.' })
  if (p.no_push !== false) segments.push({ kind: 'no_push', text: 'Do not push.' })
  return { first_message: segments.map((s) => s.text).join('\n\n'), segments, rules_text: (p.project ?? stored?.project) === 'proj' ? '- Write tests first.' : '', state_usage_text: usageOf(p.id), state_count: entries.length }
}

const previewCalls = (): TodoRepeatPreviewParams[] => rpc.calls.filter((c) => c.method === 'todo.repeat_preview').map((c) => c.params as TodoRepeatPreviewParams)

const NODES = [A]
function Live({ id }: { id: string }) {
  const item = useTodoView(NODES).find((v) => v.id === id)
  return item ? <TodoDetail item={item} online /> : <p data-testid="gone">gone</p>
}
async function show(over: Parameters<typeof todoItem>[0]): Promise<void> {
  daemon.seed(todoItem(over))
  await syncTodos(A)
  render(<Live id={over.id} />)
  await flush()
}

beforeEach(async () => {
  await db.resetDb()
  rpc = new FakeRpc()
  daemon = new FakeTodoDaemon()
  daemon.install(rpc)
  entries = []
  rpc.handlers['todo.repeat_preview'] = (p) => previewOf(p as TodoRepeatPreviewParams)
  rpc.handlers['todo.history'] = () => ({ events: [], has_more: false })
  rpc.handlers['todo.state_list'] = (p) => {
    const q = p as { id: string; prefix?: string; after?: string; limit?: number }
    const matching = entries.filter((e) => !q.prefix || e.key.startsWith(q.prefix)).sort((a, b) => (a.key < b.key ? -1 : 1))
    const rest = matching.filter((e) => !q.after || e.key > q.after)
    const page = rest.slice(0, q.limit ?? 200)
    return { template_id: q.id, entries: page, total: entries.length, matched: matching.length, has_more: rest.length > page.length }
  }
  rpc.handlers['todo.state_delete'] = (p) => {
    const q = p as { id: string; key: string }
    const before = entries.length
    entries = entries.filter((e) => e.key !== q.key)
    return { template_id: q.id, deleted: entries.length < before, count: entries.length }
  }
  rpc.handlers['todo.state_clear'] = (p) => {
    const removed = entries.length
    entries = []
    return { template_id: (p as { id: string }).id, removed }
  }
  setRpc(rpc)
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: caps(true), conn: 'ready' }) }, order: [A], loaded: true })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  resetTodosMemory()
  useStateCounts.getState().reset()
  useDrafts.setState({ drafts: {} })
  useUi.setState({ toasts: [] })
  useCapture.setState({ open: false, text: '', focusTick: 0 })
})
afterEach(() => {
  setRpc(null)
  vi.restoreAllMocks()
})

const template = (repeat: Record<string, unknown>) => ({ id: T, title: 'Sweep the reviews', body: 'Look at the open merge requests.', project: 'proj', repeat: { kind: 'daily', at: '09:00', ...repeat } as never })

describe('saving keeps the two switches', () => {
  it('editing only the notes of a template with state on does not send the rule, so both switches stay', async () => {
    const user = userEvent.setup()
    await show(template({ keep_state: true, state_hint_off: true }))
    await user.click(screen.getByTestId('todo-edit'))
    await user.type(screen.getByTestId('todo-edit-body'), ' Also the drafts.')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    const update = daemon.log.find((l) => l.method === 'todo.update')?.params as Record<string, unknown>
    expect(update).toMatchObject({ body: 'Look at the open merge requests. Also the drafts.' })
    expect(update.repeat).toBeUndefined()
    expect(daemon.todos.get(T)?.repeat).toMatchObject({ keep_state: true, state_hint_off: true })
  })

  it('changing the time sends the rule with both switches still on', async () => {
    const user = userEvent.setup()
    await show(template({ keep_state: true, state_hint_off: true }))
    await user.click(screen.getByTestId('todo-edit'))
    const at = screen.getByTestId('todo-edit-repeat-at')
    await user.clear(at)
    await user.type(at, '10:30')
    await user.click(screen.getByTestId('todo-save'))
    await flush()
    expect(daemon.todos.get(T)?.repeat).toEqual({ kind: 'daily', at: '10:30', keep_state: true, state_hint_off: true })
  })
})

describe('the switches on the form', () => {
  it('are not there when the node does not support state', async () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: caps(false), conn: 'ready' }) } })
    const user = userEvent.setup()
    await show(template({}))
    await user.click(screen.getByTestId('todo-edit'))
    expect(screen.queryByTestId('todo-edit-repeat-keep-state')).toBeNull()
    expect(screen.queryByTestId('todo-edit-repeat-preview')).toBeNull()
    expect(previewCalls()).toHaveLength(0)
  })

  it('ticking it shows the text from the node, with the unsaved form content, and labels it as added by anycon', async () => {
    const user = userEvent.setup()
    await show(template({}))
    await user.click(screen.getByTestId('todo-edit'))
    const keep = screen.getByTestId('todo-edit-repeat-keep-state')
    // 说明和勾选框关联起来
    const hintId = keep.getAttribute('aria-describedby')
    expect(hintId && document.getElementById(hintId)).toHaveTextContent(/read and write this state/)
    await user.click(keep)
    expect(screen.getByTestId('todo-edit-repeat-state-note')).toHaveAttribute('data-state', 'checked')
    await waitFor(() => expect(screen.getByTestId('todo-edit-repeat-state-usage')).toHaveTextContent(usageOf(T, 0)))
    expect(screen.getByText(/anycon adds this after the task description/)).toBeInTheDocument()
    expect(previewCalls().at(-1)).toMatchObject({ id: T, keep_state: true, state_hint_off: false, title: 'Sweep the reviews', body: 'Look at the open merge requests.', project: 'proj' })
    // 改了备注：带着表单里的新内容重新取
    await user.type(screen.getByTestId('todo-edit-body'), '!')
    await waitFor(() => expect(previewCalls().at(-1)).toMatchObject({ body: 'Look at the open merge requests.!' }))
  })

  it('without the usage notes: says so and copies the text to the end of the notes, after a blank line', async () => {
    const user = userEvent.setup()
    await show(template({ keep_state: true }))
    await user.click(screen.getByTestId('todo-edit'))
    await user.click(screen.getByTestId('todo-edit-repeat-state-note'))
    expect(screen.getByTestId('todo-edit-repeat-state-note-off')).toHaveTextContent(/describe the commands in the notes yourself/)
    expect(screen.queryByTestId('todo-edit-repeat-state-usage')).toBeNull()
    await waitFor(() => expect(screen.getByTestId('todo-edit-repeat-state-copy')).toBeEnabled())
    expect(previewCalls().at(-1)).toMatchObject({ keep_state: true, state_hint_off: true })
    await user.click(screen.getByTestId('todo-edit-repeat-state-copy'))
    expect(screen.getByTestId('todo-edit-body')).toHaveValue(`Look at the open merge requests.\n\n${usageOf(T)}`)
  })

  it('copying to notes that would go over the limit does not change them and shows the usual message', async () => {
    const user = userEvent.setup()
    await show(template({ keep_state: true, state_hint_off: true }))
    await user.click(screen.getByTestId('todo-edit'))
    act(() => useDrafts.getState().set(draftKey(A, T), 'body', 'x'.repeat(BODY_MAX_BYTES - 5)))
    await waitFor(() => expect(screen.getByTestId('todo-edit-repeat-state-copy')).toBeEnabled())
    await user.click(screen.getByTestId('todo-edit-repeat-state-copy'))
    expect(screen.getByTestId('todo-edit-repeat-state-append-error')).toBeInTheDocument()
    expect((screen.getByTestId('todo-edit-body') as HTMLTextAreaElement).value).toHaveLength(BODY_MAX_BYTES - 5)
  })

  it('an example is written into the notes, not shown as a placeholder', async () => {
    const user = userEvent.setup()
    await show(template({ keep_state: true }))
    await user.click(screen.getByTestId('todo-edit'))
    await user.click(screen.getByTestId('todo-edit-state-examples'))
    await user.click(await screen.findByTestId('todo-edit-state-examples-cursor'))
    expect((screen.getByTestId('todo-edit-body') as HTMLTextAreaElement).value).toMatch(/^Look at the open merge requests\.\n\nUse one key, cursor,/)
  })

  it('an older reply does not replace a newer one', async () => {
    const user = userEvent.setup()
    await show(template({ keep_state: true }))
    let releaseFirst: (() => void) | null = null
    let n = 0
    rpc.handlers['todo.repeat_preview'] = (p) => {
      const out = previewOf(p as TodoRepeatPreviewParams)
      if (n++ === 0) {
        return new Promise((resolve) => {
          releaseFirst = () => resolve({ ...out, segments: out.segments.map((s) => (s.kind === 'state_usage' ? { ...s, text: 'STALE' } : s)) })
        })
      }
      return out
    }
    await user.click(screen.getByTestId('todo-edit'))
    await waitFor(() => expect(releaseFirst).not.toBeNull())
    await user.type(screen.getByTestId('todo-edit-body'), '!')
    await waitFor(() => expect(screen.getByTestId('todo-edit-repeat-state-usage')).toHaveTextContent(usageOf(T, 0)))
    await act(async () => releaseFirst?.())
    await flush()
    expect(screen.getByTestId('todo-edit-repeat-state-usage')).toHaveTextContent(usageOf(T, 0))
  })
})

describe('a new repeating item', () => {
  it('the usage notes are previewed for the id it will be saved under, and the item is saved under that id with the switches', async () => {
    const user = userEvent.setup()
    render(<CaptureForm />)
    await user.type(screen.getByTestId('capture-input'), 'Sweep the queue')
    await user.selectOptions(screen.getByTestId('capture-repeat-kind'), 'daily')
    await user.click(screen.getByTestId('capture-repeat-keep-state'))
    const usage = await screen.findByTestId('capture-repeat-state-usage')
    const asked = previewCalls().at(-1)!
    expect(asked).toMatchObject({ title: 'Sweep the queue', keep_state: true, state_hint_off: false })
    expect(usage).toHaveTextContent(usageOf(asked.id, 0))
    // 插入示例：真的写进输入框（第一行仍是标题）
    await user.click(screen.getByTestId('capture-state-examples'))
    await user.click(await screen.findByTestId('capture-state-examples-mergeRequest'))
    expect((screen.getByTestId('capture-input') as HTMLTextAreaElement).value).toMatch(/^Sweep the queue\n\nUse keys of the form mr:/)
    await user.click(screen.getByTestId('capture-send'))
    await waitFor(() => expect([...daemon.todos.values()]).toHaveLength(1))
    const sent = rpc.calls.find((c) => c.method === 'todo.add')?.params as { id: string; repeat?: unknown }
    expect(sent.id).toBe(asked.id)
    expect(sent.repeat).toEqual({ kind: 'daily', at: '09:00', keep_state: true })
  })
})

describe('the preview of what the agent receives', () => {
  it('opens on demand, splits what you wrote from what anycon adds, lists the rules, and says what is left out', async () => {
    const user = userEvent.setup()
    await show(template({ keep_state: true }))
    expect(previewCalls()).toHaveLength(1) // 只有跨次状态那一块取了条数
    await user.click(screen.getByTestId('repeat-preview-toggle'))
    const box = await screen.findByTestId('repeat-preview-segments')
    const segs = within(box).getAllByTestId('prompt-segment')
    expect(segs.map((s) => s.dataset.kind)).toEqual(['title', 'body', 'state_usage', 'closing_report', 'no_push'])
    expect(segs.map((s) => s.dataset.authored)).toEqual(['true', 'true', 'false', 'false', 'false'])
    expect(segs[2]).toHaveTextContent('State usage notes')
    expect(segs[4]).toHaveTextContent('No-push line')
    expect(screen.getByTestId('repeat-preview-rules-text')).toHaveTextContent('- Write tests first.')
    expect(screen.getByTestId('repeat-preview-box')).toHaveTextContent(/reports from earlier runs are added too/)
  })

  it('shows why it failed', async () => {
    const user = userEvent.setup()
    await show(template({}))
    rpc.handlers['todo.repeat_preview'] = () => {
      throw rpcError('invalid', 'body', 'too_long')
    }
    await user.click(screen.getByTestId('repeat-preview-toggle'))
    expect(await screen.findByTestId('repeat-preview-error')).toHaveTextContent(/notes are too long/)
  })

  it('the segments render as plain text, never as markup', () => {
    render(<PromptSegments segments={[{ kind: 'body', text: '<b>bold</b> **x**' }, { kind: 'unknown', text: 'later' }]} />)
    const segs = screen.getAllByTestId('prompt-segment')
    expect(segs[0]!.querySelector('b')).toBeNull()
    expect(segs[0]).toHaveTextContent('<b>bold</b> **x**')
    expect(segs[1]).toHaveTextContent('Added by anycon')
  })
})

describe('looking at and clearing the saved state', () => {
  const seedEntries = (): void => {
    entries = [
      { key: 'mr:1@abc', value: 'done', written_ms: 1_700_000_000_000 },
      { key: 'mr:2@def', value: '', written_ms: 1_700_000_100_000 },
      { key: 'jira:X-1', value: 'line one\nline two‮', written_ms: 1_700_000_200_000 },
    ]
  }

  it('shows the count, the entries as plain text, filters by prefix and deletes one', async () => {
    seedEntries()
    const user = userEvent.setup()
    await show(template({ keep_state: true }))
    expect(screen.getByTestId('todo-state-count')).toHaveTextContent('Entries: 3')
    await user.click(screen.getByTestId('todo-state-toggle'))
    await waitFor(() => expect(screen.getAllByTestId('todo-state-entry')).toHaveLength(3))
    expect(screen.getAllByTestId('todo-state-key').map((k) => k.textContent)).toEqual(['jira:X-1', 'mr:1@abc', 'mr:2@def'])
    // 双向控制符不进界面
    expect(screen.getAllByTestId('todo-state-value')[0]!.textContent).toBe('line one\nline two ')
    await user.type(screen.getByTestId('todo-state-prefix'), 'mr:')
    await waitFor(() => expect(screen.getAllByTestId('todo-state-entry')).toHaveLength(2))
    await user.click(screen.getAllByTestId('todo-state-delete')[0]!)
    await waitFor(() => expect(screen.getAllByTestId('todo-state-entry')).toHaveLength(1))
    expect(entries.map((e) => e.key)).toEqual(['mr:2@def', 'jira:X-1'])
    expect(screen.getByTestId('todo-state-count')).toHaveTextContent('Entries: 2')
  })

  it('clearing asks first', async () => {
    seedEntries()
    const user = userEvent.setup()
    await show(template({ keep_state: true }))
    await user.click(screen.getByTestId('todo-state-clear'))
    expect(screen.getByTestId('todo-state-clear-dialog')).toHaveTextContent(/all 3 entries/)
    expect(rpc.methods()).not.toContain('todo.state_clear')
    await user.click(screen.getByTestId('todo-state-clear-confirm'))
    await flush()
    expect(rpc.methods()).toContain('todo.state_clear')
    expect(screen.getByTestId('todo-state-count')).toHaveTextContent('Entries: 0')
  })

  it('a change on the node refreshes the count and the open list', async () => {
    seedEntries()
    const user = userEvent.setup()
    await show(template({ keep_state: true }))
    await user.click(screen.getByTestId('todo-state-toggle'))
    await waitFor(() => expect(screen.getAllByTestId('todo-state-entry')).toHaveLength(3))
    entries.push({ key: 'mr:3@aaa', value: '', written_ms: 1_700_000_300_000 })
    act(() => handleStateChanged(A, { id: T, count: 4 }))
    expect(screen.getByTestId('todo-state-count')).toHaveTextContent('Entries: 4')
    await waitFor(() => expect(screen.getAllByTestId('todo-state-entry')).toHaveLength(4))
  })

  it('with the switch off: shown only while entries are left, and then only clearing', async () => {
    seedEntries()
    await show(template({}))
    expect(screen.getByTestId('todo-state-count')).toHaveTextContent('Entries: 3')
    expect(screen.queryByTestId('todo-state-toggle')).toBeNull()
    expect(screen.getByTestId('todo-state-clear')).toBeEnabled()
  })

  it('a template without state and without entries has no such block', async () => {
    await show(template({}))
    expect(screen.queryByTestId('todo-state-panel')).toBeNull()
  })

  it('is not available while the node is offline, and nothing is read', async () => {
    seedEntries()
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A, name: 'alpha', info: caps(true), conn: 'peer_offline' }) } })
    await show(template({ keep_state: true }))
    expect(screen.getByTestId('todo-state-offline')).toBeInTheDocument()
    expect(rpc.methods()).not.toContain('todo.state_list')
  })
})

describe('state use in the history', () => {
  const C = '22222222-2222-4222-8222-222222222222'
  const ev = (seq: number, e: Record<string, unknown>): TodoEvent => ({ seq, ts_ms: 1_700_000_000_000 + seq, ...e }) as unknown as TodoEvent
  const events = [
    ev(1, { kind: 'dispatched', session_id: 's1', agent: 'claude', workspace_mode: 'worktree', no_push: true, by: { kind: 'local' } }),
    ev(2, { kind: 'result', session_id: 's1', turn: 1, text: 'first', truncated: false }),
    ev(3, { kind: 'state_usage', session_id: 's1', turn: 1, reads: 2, writes: 3 }),
    ev(4, { kind: 'result', session_id: 's1', turn: 2, text: 'second', truncated: false }),
    ev(5, { kind: 'result', session_id: 's1', turn: 3, text: 'third', truncated: false }),
    ev(6, { kind: 'state_usage', session_id: 's1', turn: 3, reads: 1, writes: 0 }),
  ].reverse()

  async function showCopy(templateRepeat: Record<string, unknown> | null): Promise<void> {
    if (templateRepeat) daemon.seed(todoItem(template(templateRepeat)))
    daemon.seed(todoItem({ id: C, title: 'Sweep the reviews', source: { kind: 'repeat', todo_id: T } as never, sessions: ['s1'] as never }))
    rpc.handlers['todo.history'] = (p) => ({ events: (p as { id: string }).id === C ? events : [], has_more: false })
    await syncTodos(A)
    useTodoHistory.getState().patch(historyKey(A, C), () => ({ events, hasMore: false, loading: false, loaded: true }))
    function View() {
      const item = useTodoView(NODES).find((v) => v.id === C)
      return item ? <TodoHistory item={item} /> : null
    }
    render(<View />)
    await flush()
  }

  it('reads and writes are one line, and a turn that wrote nothing gets a quiet note', async () => {
    await showCopy({ keep_state: true })
    const usage = screen.getAllByTestId('history-row').filter((r) => r.dataset.kind === 'state_usage')
    expect(usage.map((r) => r.textContent)).toEqual([expect.stringContaining('State: 2 reads, 3 writes'), expect.stringContaining('State: 1 reads, 0 writes')])
    const results = screen.getAllByTestId('history-row').filter((r) => r.dataset.kind === 'result')
    expect(results.map((r) => within(r).queryByTestId('history-no-state-write') !== null)).toEqual([false, true, true])
  })

  it('no note when the template does not keep state, or its switches are not known', async () => {
    await showCopy({})
    expect(screen.queryByTestId('history-no-state-write')).toBeNull()
  })

  it('no note when the template is gone', async () => {
    rpc.handlers['todo.get'] = () => {
      throw rpcError('not_found')
    }
    await showCopy(null)
    expect(screen.queryByTestId('history-no-state-write')).toBeNull()
  })
})
