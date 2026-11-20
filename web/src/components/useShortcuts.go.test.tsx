// `G` 然后 `I` / `S`（spec/web.md § 14.11）：去「待处理」/ 回到上次的会话；单字母键在输入框里、有对话框开着时都不生效，
// 隔太久或中间按了别的键就取消。
import { fireEvent, render } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { setNavigator } from '@/lib/navigate'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useUi } from '@/stores/ui'
import { useCapture } from '@/features/todos/capture'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, {}))

import { GO_TIMEOUT_MS, useShortcuts } from './useShortcuts'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)

function Harness() {
  useShortcuts()
  return <input aria-label="box" />
}

const press = (key: string, init: KeyboardEventInit = {}, target: Element = document.body): void => {
  fireEvent.keyDown(target, { key, ...init })
}

let go: ReturnType<typeof vi.fn<(to: string) => void>>
beforeEach(() => {
  navigate.mockReset()
  go = vi.fn<(to: string) => void>()
  setNavigator(go)
  useUi.setState({ lastSessionsPath: null })
  usePrefs.setState({ lastNode: null })
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A }), [B]: nodeEntry({ id: B }) }, order: [A, B], loaded: true })
})
afterEach(() => {
  setNavigator(null)
  vi.useRealTimers()
})

describe('G then I / S', () => {
  it('G I goes to the inbox', () => {
    render(<Harness />)
    press('g')
    press('i')
    expect(navigate).toHaveBeenCalledWith({ to: '/inbox' })
  })

  it('G S goes back to the last place in Sessions', () => {
    useUi.setState({ lastSessionsPath: '/n/aaaaaaaa/s/sess_1' })
    render(<Harness />)
    press('g')
    press('s')
    expect(go).toHaveBeenCalledWith('/n/aaaaaaaa/s/sess_1')
  })

  it('G S without a last place opens the node home of the node used last, or the first one', () => {
    usePrefs.setState({ lastNode: B })
    const { unmount } = render(<Harness />)
    press('g')
    press('s')
    expect(go).toHaveBeenLastCalledWith('/n/bbbbbbbb')
    unmount()
    usePrefs.setState({ lastNode: null })
    render(<Harness />)
    press('g')
    press('s')
    expect(go).toHaveBeenLastCalledWith('/n/aaaaaaaa')
  })

  it('I or S alone does nothing', () => {
    render(<Harness />)
    press('i')
    press('s')
    expect(navigate).not.toHaveBeenCalled()
    expect(go).not.toHaveBeenCalled()
  })

  it('is cancelled by any other key in between, and by waiting too long', () => {
    vi.useFakeTimers()
    render(<Harness />)
    press('g')
    press('x')
    press('i')
    expect(navigate).not.toHaveBeenCalled()
    press('g')
    vi.setSystemTime(Date.now() + GO_TIMEOUT_MS + 100)
    press('i')
    expect(navigate).not.toHaveBeenCalled()
  })

  it('does nothing while typing in an input', () => {
    const { getByLabelText } = render(<Harness />)
    const box = getByLabelText('box')
    press('g', {}, box)
    press('i', {}, box)
    expect(navigate).not.toHaveBeenCalled()
    // 在输入框里按过 G，之后在外面按 I 也不能算数
    press('i')
    expect(navigate).not.toHaveBeenCalled()
  })

  it('does nothing while a dialog or a menu is open, and with modifier keys', () => {
    render(<Harness />)
    const dialog = document.createElement('div')
    dialog.setAttribute('role', 'dialog')
    document.body.append(dialog)
    press('g')
    press('i')
    expect(navigate).not.toHaveBeenCalled()
    dialog.remove()
    press('g', { metaKey: true })
    press('i', { metaKey: true })
    expect(navigate).not.toHaveBeenCalled()
  })

  it('ignores key repeat, so holding G does not arm it forever', () => {
    render(<Harness />)
    press('g', { repeat: true })
    press('i')
    expect(navigate).not.toHaveBeenCalled()
  })
})

describe('to-dos: G T and C', () => {
  it('G T goes to the to-do list', () => {
    render(<Harness />)
    press('g')
    press('t')
    expect(navigate).toHaveBeenCalledWith({ to: '/todos' })
  })

  it('T on its own does nothing', () => {
    render(<Harness />)
    press('t')
    expect(navigate).not.toHaveBeenCalled()
  })

  it('C opens the capture bar without leaving the page, but not while typing, with a modifier, or over a dialog', () => {
    useCapture.setState({ open: false })
    const { getByLabelText } = render(<Harness />)
    press('c', { metaKey: true })
    press('c', {}, getByLabelText('box'))
    expect(useCapture.getState().open).toBe(false)
    const dialog = document.createElement('div')
    dialog.setAttribute('role', 'dialog')
    document.body.append(dialog)
    press('c')
    expect(useCapture.getState().open).toBe(false)
    dialog.remove()
    press('c')
    expect(useCapture.getState().open).toBe(true)
    expect(navigate).not.toHaveBeenCalled()
  })
})
