import { describe, expect, it, vi } from 'vitest'
import type { PermissionBlock } from './reducer'
import { handleApproveShortcut, shortcutTarget } from './shortcuts'

const block = (over: Partial<PermissionBlock> = {}): PermissionBlock => ({
  type: 'permission',
  key: 'k',
  seq: 1,
  ts: 1,
  requestId: 'r1',
  kind: 'tool',
  title: 'Run',
  detail: {},
  options: [
    { id: 'allow', label: 'Allow', kind: 'allow_once' },
    { id: 'allow_session', label: 'Always', kind: 'allow_always' },
    { id: 'deny', label: 'Deny', kind: 'reject_once' },
  ],
  ...over,
})

function key(k: string, over: Partial<{ target: EventTarget | null; shiftKey: boolean; metaKey: boolean }> = {}) {
  return { key: k, metaKey: true, ctrlKey: false, shiftKey: true, target: document.body as EventTarget | null, preventDefault: vi.fn(), ...over }
}

describe('shortcutTarget', () => {
  it('picks the latest pending tool request and only allow_once / reject_once', () => {
    const a = block({ requestId: 'a' })
    const b = block({ requestId: 'b' })
    expect(shortcutTarget([a, b], 'allow')).toEqual({ block: b, optionId: 'allow' })
    expect(shortcutTarget([a, b], 'deny')).toEqual({ block: b, optionId: 'deny' })
  })

  it('never upgrades to allow_always / reject_always when the once variant is missing', () => {
    const onlyAlways = block({ options: [{ id: 'a', label: 'Always', kind: 'allow_always' }, { id: 'n', label: 'Never', kind: 'reject_always' }] })
    expect(shortcutTarget([onlyAlways], 'allow')).toBeNull()
    expect(shortcutTarget([onlyAlways], 'deny')).toBeNull()
  })

  it('ignores mode changes, plans, questions and already resolved requests', () => {
    for (const kind of ['mode_change', 'plan', 'question', 'other'] as const) expect(shortcutTarget([block({ kind })], 'allow')).toBeNull()
    expect(shortcutTarget([block({ resolved: { optionId: 'allow', byDevice: 'x', ts: 2 } })], 'allow')).toBeNull()
  })
})

describe('handleApproveShortcut', () => {
  const deps = (over: Partial<Parameters<typeof handleApproveShortcut>[1]> = {}) => ({ pending: [block()], respond: vi.fn(), isVisible: () => true, reveal: vi.fn(), ...over })

  it('allows with ⌘⇧A and denies with ⌘⇧D when the card is on screen', () => {
    const d = deps()
    expect(handleApproveShortcut(key('A'), d)).toBe(true)
    expect(d.respond).toHaveBeenLastCalledWith({ request_id: 'r1', option_id: 'allow' })
    expect(handleApproveShortcut(key('d'), d)).toBe(true)
    expect(d.respond).toHaveBeenLastCalledWith({ request_id: 'r1', option_id: 'deny' })
  })

  it('does nothing while the user is typing in an input, textarea, select or contenteditable', () => {
    const d = deps()
    for (const tag of ['input', 'textarea', 'select']) {
      const el = document.createElement(tag)
      const ev = key('a', { target: el })
      expect(handleApproveShortcut(ev, d)).toBe(false)
      expect(ev.preventDefault).not.toHaveBeenCalled()
    }
    const div = document.createElement('div')
    Object.defineProperty(div, 'isContentEditable', { value: true })
    expect(handleApproveShortcut(key('a', { target: div }), d)).toBe(false)
    expect(d.respond).not.toHaveBeenCalled()
  })

  it('a held-down key (auto-repeat) does not answer more than once: repeats are swallowed', () => {
    const d = deps()
    const first = key('a')
    expect(handleApproveShortcut(first, d)).toBe(true)
    expect(d.respond).toHaveBeenCalledTimes(1)
    for (let i = 0; i < 5; i++) {
      const again = { ...key('a'), repeat: true }
      expect(handleApproveShortcut(again, d)).toBe(true) // 吞掉，也别让浏览器自己处理
      expect(again.preventDefault).toHaveBeenCalled()
    }
    expect(d.respond).toHaveBeenCalledTimes(1)
    // 长按也不会触发 reveal
    const off = deps({ isVisible: () => false })
    handleApproveShortcut({ ...key('d'), repeat: true }, off)
    expect(off.reveal).not.toHaveBeenCalled()
    expect(off.respond).not.toHaveBeenCalled()
  })

  it('only reveals an off-screen request instead of answering it', () => {
    const d = deps({ isVisible: () => false })
    expect(handleApproveShortcut(key('a'), d)).toBe(true)
    expect(d.reveal).toHaveBeenCalledWith('r1')
    expect(d.respond).not.toHaveBeenCalled()
  })

  it('ignores other keys and a missing shift', () => {
    const d = deps()
    expect(handleApproveShortcut(key('a', { shiftKey: false }), d)).toBe(false)
    expect(handleApproveShortcut(key('x'), d)).toBe(false)
    expect(d.respond).not.toHaveBeenCalled()
  })
})
