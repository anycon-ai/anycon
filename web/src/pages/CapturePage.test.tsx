// `/capture`（spec/web.md § 14.5）：分享的内容取一次就删、StrictMode 里 effect 跑两遍也不丢；页面自己带表单时别的入口不再另开一个。
import { StrictMode } from 'react'
import { act, render, screen } from '@testing-library/react'
import { deleteDB } from 'idb'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { useCapture } from '@/features/todos/capture'
import { SHARE_DB, storeShared, takeShared } from '@/sw/shareStore'
import { nodeEntry } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'

vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn(), {}, { pathname: '/capture' }))

import { CapturePage } from './CapturePage'

beforeEach(async () => {
  await deleteDB(SHARE_DB)
  useNodes.setState({ byId: { n1: nodeEntry({ id: 'n1', info: { capabilities: { todo: true } } as unknown as NodeInfo }) }, order: ['n1'], loaded: true })
  useCapture.setState({ open: false, text: '', inline: false })
})
afterEach(async () => deleteDB(SHARE_DB))

describe('CapturePage', () => {
  it('under StrictMode (effects run twice) the shared text still lands in the box, and the stash is taken once', async () => {
    await storeShared({ title: 'Page', text: 'quote', url: 'https://a.test', at: Date.now() })
    render(
      <StrictMode>
        <CapturePage />
      </StrictMode>,
    )
    expect(await screen.findByTestId('capture-input')).toHaveValue('Page\nquote\nhttps://a.test')
    expect(await takeShared()).toBeNull()
  })

  it('nothing is added by itself and an empty stash gives an empty box', async () => {
    render(<CapturePage />)
    expect(await screen.findByTestId('capture-input')).toHaveValue('')
  })

  it('while it is showing, the other entry points do not open a second form bound to the same text; afterwards they work again', async () => {
    const { unmount } = render(<CapturePage />)
    await screen.findByTestId('capture-input')
    act(() => useCapture.getState().openCapture())
    expect(useCapture.getState().open).toBe(false)
    expect(document.querySelectorAll('#capture-error')).toHaveLength(0)
    unmount()
    act(() => useCapture.getState().openCapture())
    expect(useCapture.getState().open).toBe(true)
  })
})
