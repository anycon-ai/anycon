// Bot 新问题的前台提示与推送落点：session 的 todo_ 前缀表示清单项。
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import i18n from '@/i18n'
import { nodeEntry, todoItem } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTwinDaemon, twinQuestion } from '@/test/fakeTwinDaemon'
import { setRpc } from '@/transport/instance'
import { setNavigator } from '@/lib/navigate'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { useTodos } from '@/features/todos/store'
import { useTwin } from '@/features/twin/store'
import { handleNotification } from './notifications'
import { notifyTodoId, pushText, twinInboxUrl } from './pushText'
import { textForNotify } from './systemNotify'
import { notificationFor } from '@/sw/push'
import { parsePushPayload } from '@/sw/payload'
const A = 'a'.repeat(32)
beforeEach(() => {
  const rpc = new FakeRpc()
  new FakeTwinDaemon(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, info: { capabilities: { twin: true } } as unknown as NodeInfo }) }, order: [A] })
  useTwin.getState().reset()
  useUi.setState({ toasts: [] })
  useTodos.setState({ nodes: {}, outbox: [] })
  useTodos.getState().setCache(A, [todoItem({ id: 't1', title: 'Fix login' })])
})
afterEach(() => setNavigator(null))
it('foreground toast names the item and locates it; duplicates and answered notices do not toast', () => {
  const go = vi.fn()
  setNavigator(go)
  const q = twinQuestion()
  handleNotification(A, 'twin.question', { question: q })
  const toast = useUi.getState().toasts[0]!
  expect(toast.text).toBe('Bot has a question: Fix login')
  toast.action!.run()
  expect(go).toHaveBeenCalledWith('/inbox?node=aaaaaaaa&todo=t1')
  handleNotification(A, 'twin.question', { question: q })
  handleNotification(A, 'twin.question', { question: { ...q, status: 'answered', answer: 'Web' } })
  expect(useUi.getState().toasts).toHaveLength(1)
})
it('push and page system notifications share en/zh text, escaping and a todo landing', () => {
  const node = { id: A, param: 'aaaaaaaa', name: 'alpha' }
  const input = { category: 'twin_question' as const, node, session: { id: 'todo_t1', title: 'Fix login' } }
  expect(pushText(i18n.getFixedT('zh'), input).title).toBe('Bot 有个问题：Fix login')
  const expected = pushText(i18n.getFixedT('en'), input)
  expect(expected.url).toBe('/inbox?node=aaaaaaaa&todo=t1')
  expect(pushText(i18n.getFixedT('en'), { ...input, node: { ...node, group: 'Work' } }).body).toContain('Work')
  const payload = parsePushPayload({ category: 'twin_question', node: A, session: 'todo_t1' })
  expect(payload?.category).toBe('twin_question')
  expect(notificationFor(i18n.getFixedT('en'), payload, { nodeName: 'alpha', sessionTitle: 'Fix login', nodeIds: [A] })).toEqual(expected)
  const todoTitle = vi.fn(() => 'Fix login')
  const sessionTitle = vi.fn(() => undefined)
  expect(
    textForNotify({ t: i18n.getFixedT('en'), nodeName: () => 'alpha', sessionTitle, todoTitle, nodeIds: () => [A] }, A, {
      category: 'twin_question',
      session_id: 'todo_t1',
    }),
  ).toEqual(expected)
  expect(todoTitle).toHaveBeenCalledWith(A, 't1')
  expect(sessionTitle).not.toHaveBeenCalled()
  expect(twinInboxUrl('a&b', 'x/y?z')).toBe('/inbox?node=a%26b&todo=x%2Fy%3Fz')
})
it('unsupported nodes have no toast or live Bot entry', () => {
  useNodes.getState().patch(A, { info: { capabilities: { twin: false } } as unknown as NodeInfo })
  handleNotification(A, 'twin.question', { question: twinQuestion() })
  expect(useUi.getState().toasts).toHaveLength(0)
  expect(useTwin.getState().nodes[A]).toBeUndefined()
})

it.each(['sess_abc', 't1', 'todo_'])('non-prefixed or empty-suffix target %s remains an ordinary session', (sessionId) => {
  const todoTitle = vi.fn(() => 'Wrong item')
  const sessionTitle = vi.fn(() => 'Ordinary session')
  const env = { t: i18n.getFixedT('en'), nodeName: () => 'alpha', nodeIds: () => [A], sessionTitle, todoTitle }
  const expected = textForNotify(env, A, { category: 'twin_question', session_id: sessionId, detail: null })
  expect(expected.url).toBe(`/n/aaaaaaaa/s/${sessionId}`)
  expect(expected.tag).toBe(`session:${sessionId}`)
  expect(expected.title).toContain('Ordinary session')
  expect(todoTitle).not.toHaveBeenCalled()
  expect(sessionTitle).toHaveBeenCalledWith(A, sessionId)
  expect(notificationFor(env.t, parsePushPayload({ category: 'twin_question', node: A, session: sessionId }), { nodeName: 'alpha', nodeIds: [A], sessionTitle: 'Ordinary session' })).toEqual(expected)
})
it('decodes exactly one prefix and keeps the todo destination without a cached title', () => {
  expect(notifyTodoId('todo_todo_t1')).toBe('todo_t1')
  expect(notifyTodoId(null)).toBeNull()
  const env = { t: i18n.getFixedT('en'), nodeName: () => 'alpha', nodeIds: () => [A], sessionTitle: () => undefined }
  const result = textForNotify(env, A, { category: 'twin_question', session_id: 'todo_t1', detail: null })
  expect(result.title).toBe('Bot has a question: a to-do')
  expect(result.url).toBe('/inbox?node=aaaaaaaa&todo=t1')
  expect(textForNotify(env, A, { category: 'follow_up', session_id: 'todo_t1', detail: null }).url).toBe('/todos/aaaaaaaa/t1')
})
