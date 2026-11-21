// 页面在前台时一项变成「待验收」：应用内 toast，点「去验收」落在验收页（和推送通知同样的话、同一个落点）。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry, todoItem } from '@/test/factories'
import { setNavigator } from '@/lib/navigate'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { toastIfReady } from './reviewToast'
import { handleTodoChanged, resetTodosMemory } from './service'

const N = 'n'.repeat(32)
const item = (activity?: string, over: object = {}) => todoItem({ id: 't1', title: 'Fix the redirect', ...(activity ? { activity: activity as never } : {}), ...over })

beforeEach(() => {
  useNodes.setState({ byId: { [N]: nodeEntry({ id: N }) }, order: [N], loaded: true })
  useUi.setState({ toasts: [] })
})
afterEach(() => {
  setNavigator(null)
  window.history.pushState({}, '', '/')
  vi.restoreAllMocks()
})

describe('in-page toast when an item becomes ready to review', () => {
  it('says “<title> is ready to review” and its action opens the review page', () => {
    const go = vi.fn()
    setNavigator(go)
    toastIfReady(N, item('working'), item('review'))
    const t = useUi.getState().toasts[0]!
    expect(t.text).toBe('“Fix the redirect” is ready to review.')
    t.action!.run()
    expect(go).toHaveBeenCalledWith('/todos/nnnnnnnn/t1/review')
  })

  it('only on the change into review: not when it was already in review, not for other activities, not for a closed item', () => {
    toastIfReady(N, item('review'), item('review'))
    toastIfReady(N, item('open'), item('working'))
    toastIfReady(N, item('working'), item('review', { state: 'done' }))
    toastIfReady(N, undefined, item('waiting'))
    expect(useUi.getState().toasts).toEqual([])
  })

  it('a first sighting of an item that is already in review (a new item from another device) also counts', () => {
    toastIfReady(N, undefined, item('review'))
    expect(useUi.getState().toasts).toHaveLength(1)
  })

  it('does not disturb someone who is already looking at that item (its detail or its review page)', () => {
    window.history.pushState({}, '', '/todos/nnnnnnnn/t1')
    toastIfReady(N, item('working'), item('review'))
    window.history.pushState({}, '', '/todos/nnnnnnnn/t1/review')
    toastIfReady(N, item('working'), item('review'))
    expect(useUi.getState().toasts).toEqual([])
  })

  it('the title is cleaned; a page that is hidden shows nothing (the system notification covers it)', () => {
    toastIfReady(N, item('working'), item('review', { title: 'a\u202Eb' }))
    expect(useUi.getState().toasts[0]!.text).toContain('a b')
    useUi.setState({ toasts: [] })
    vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('hidden')
    toastIfReady(N, item('working'), item('review'))
    expect(useUi.getState().toasts).toEqual([])
  })
})

describe('wired to todo.changed (not to the sync)', () => {
  it('a todo.changed that moves an item into review toasts once; the same change again does not', () => {
    useNodes.setState({ byId: { [N]: nodeEntry({ id: N, info: { capabilities: { todo: true } } as never }) }, order: [N], loaded: true })
    resetTodosMemory()
    handleTodoChanged(N, { id: 't1', todo: item('working') })
    expect(useUi.getState().toasts).toEqual([])
    handleTodoChanged(N, { id: 't1', todo: item('review') })
    expect(useUi.getState().toasts).toHaveLength(1)
    handleTodoChanged(N, { id: 't1', todo: item('review') })
    expect(useUi.getState().toasts).toHaveLength(1)
  })
})
