// 详情里没存的编辑（drafts.ts）：关标签页 / 刷新前要问一声（应用级，不管当前在哪一页）；退出登录、移除设备、换账号之后不能留着。
import { render } from '@testing-library/react'
import { beforeEach, describe, expect, it } from 'vitest'
import { resetClientState } from '@/app/session'
import { dirtyFields, draftKey, hasDirtyDrafts, useDrafts, useUnsavedDraftsGuard } from './drafts'

const values = { title: 'old', body: '', project: '', due: '' }

function Guard() {
  useUnsavedDraftsGuard()
  return <p>some other page</p>
}
const unload = (): boolean => {
  const ev = new Event('beforeunload', { cancelable: true })
  window.dispatchEvent(ev)
  return ev.defaultPrevented
}

beforeEach(() => useDrafts.setState({ drafts: {} }))

describe('unsaved edit guard', () => {
  it('asks before the tab closes while a draft has unsaved text, wherever in the app you are, and stops when there is nothing to lose', () => {
    render(<Guard />)
    expect(unload()).toBe(false)
    useDrafts.getState().start(draftKey('n', 'a'), values)
    expect(unload()).toBe(false) // 开了编辑但没改
    useDrafts.getState().set(draftKey('n', 'a'), 'title', 'old!')
    expect(hasDirtyDrafts(useDrafts.getState().drafts)).toBe(true)
    expect(unload()).toBe(true)
    useDrafts.getState().clear(draftKey('n', 'a'))
    expect(unload()).toBe(false)
  })

  it('removes its listener when the app shell goes away', () => {
    const { unmount } = render(<Guard />)
    useDrafts.getState().start('n:a', values)
    useDrafts.getState().set('n:a', 'title', 'x')
    unmount()
    expect(unload()).toBe(false)
  })
})

describe('draft bookkeeping', () => {
  it('only fields that differ from where the edit started are dirty; rebasing a field makes it clean', () => {
    useDrafts.getState().start('n:a', values)
    useDrafts.getState().set('n:a', 'title', 'new')
    expect(dirtyFields(useDrafts.getState().drafts['n:a']!)).toEqual(['title'])
    useDrafts.getState().rebase('n:a', ['title'], { ...values, title: 'new' }, false)
    expect(dirtyFields(useDrafts.getState().drafts['n:a']!)).toEqual([])
  })
})

describe('sign-out, remove this device and switching accounts', () => {
  it('forget unsaved to-do edits (resetClientState)', () => {
    useDrafts.getState().start('n:a', values)
    useDrafts.getState().set('n:a', 'title', 'secret edit')
    resetClientState()
    expect(useDrafts.getState().drafts).toEqual({})
  })
})
