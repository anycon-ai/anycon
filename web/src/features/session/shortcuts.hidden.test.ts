// 手机上藏起来的「待处理」标签页（`inert`、不可见）里还留着同一个请求的卡片，坐标照样在屏幕上：
// ⌘⇧A 的「看不看得见」只能认真正看得见的那一份（spec/web.md § 9）。
import { afterEach, describe, expect, it } from 'vitest'
import { isRequestCardVisible, requestCardElement } from './shortcuts'

const rect = (top: number): DOMRect => ({ top, bottom: top + 40, left: 0, right: 10, width: 10, height: 40, x: 0, y: top, toJSON: () => ({}) })

function card(parent: Element, id: string, top: number): HTMLElement {
  const el = document.createElement('div')
  el.setAttribute('data-request', id)
  el.getBoundingClientRect = () => rect(top)
  parent.append(el)
  return el
}

afterEach(() => {
  document.body.innerHTML = ''
})

describe('request card lookup', () => {
  it('skips a copy inside an inert (hidden tab) subtree, even when it is on screen and comes first in the document', () => {
    const hidden = document.createElement('div')
    hidden.setAttribute('inert', '')
    document.body.append(hidden)
    card(hidden, 'r1', 10)
    const live = card(document.body, 'r1', 5000)
    expect(requestCardElement('r1')).toBe(live)
    // 看见的那份在屏幕外：要先滚过去，不能拿藏着的那份的坐标当「看见了」
    expect(isRequestCardVisible('r1')).toBe(false)
  })

  it('finds nothing when the only copy is hidden', () => {
    const hidden = document.createElement('div')
    hidden.setAttribute('inert', '')
    document.body.append(hidden)
    card(hidden, 'r1', 10)
    expect(requestCardElement('r1')).toBeNull()
    expect(isRequestCardVisible('r1')).toBe(false)
  })

  it('skips an element that checkVisibility says is not visible', () => {
    const el = card(document.body, 'r1', 10)
    const seen: unknown[] = []
    ;(el as unknown as { checkVisibility: (o?: unknown) => boolean }).checkVisibility = (o) => {
      seen.push(o)
      return false
    }
    const live = card(document.body, 'r1', 20)
    expect(requestCardElement('r1')).toBe(live)
    expect(isRequestCardVisible('r1')).toBe(true)
    // 要求把 `visibility: hidden` 也算作看不见
    expect(seen).toContainEqual({ visibilityProperty: true })
  })

  it('without checkVisibility (older browsers, jsdom) it falls back to the plain lookup', () => {
    const el = card(document.body, 'r1', 10)
    expect('checkVisibility' in el).toBe(false)
    expect(requestCardElement('r1')).toBe(el)
    expect(isRequestCardVisible('r1')).toBe(true)
  })
})
