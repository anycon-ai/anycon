// `/` 的重定向规则（spec/web.md § 2）。
import { render, screen } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ApiDevice } from '@/api/types'
import { mockRouter } from '@/test/routerMock'
import { nodeEntry } from '@/test/factories'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'

const h = vi.hoisted(() => ({ navigate: vi.fn(), devices: { data: undefined as unknown, isError: false, refetch: vi.fn() }, enabled: [] as Array<boolean | undefined> }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, {}))
vi.mock('@/api/queries', () => ({
  useDevices: (o?: { enabled?: boolean }) => {
    h.enabled.push(o?.enabled)
    return h.devices
  },
}))

import { HomeRedirect } from './HomeRedirect'

const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const daemon = (id: string): ApiDevice => ({ id, name: 'n', kind: 'daemon', platform: 'linux', created_at_ms: 1, online: true, paired_with: [] })

beforeEach(() => {
  h.navigate.mockReset()
  h.enabled = []
  h.devices.data = undefined
  h.devices.isError = false
  usePrefs.setState({ lastNode: null })
  useNodes.setState({ byId: {}, order: [], loaded: true })
})

describe('"/"', () => {
  it('this browser has paired nodes → the one opened last', () => {
    useNodes.setState({ byId: { [A]: nodeEntry({ id: A }), [B]: nodeEntry({ id: B }) }, order: [A, B], loaded: true })
    usePrefs.setState({ lastNode: B })
    render(<HomeRedirect />)
    expect(h.navigate).toHaveBeenCalledWith({ to: '/n/$node', params: { node: 'bbbbbbbb' }, replace: true })
    // 有已配对的节点就不用去问 hub 账号下有什么
    expect(h.enabled.every((e) => e === false)).toBe(true)
  })

  it('no paired node here but the account has one → /unlock', () => {
    h.devices.data = [daemon(A)]
    render(<HomeRedirect />)
    expect(h.navigate).toHaveBeenCalledWith({ to: '/unlock', replace: true })
  })

  it('nothing on the account either → /setup', () => {
    h.devices.data = []
    render(<HomeRedirect />)
    expect(h.navigate).toHaveBeenCalledWith({ to: '/setup', replace: true })
  })

  it('waits while the local nodes or the device list are loading', () => {
    useNodes.setState({ byId: {}, order: [], loaded: false })
    const { unmount } = render(<HomeRedirect />)
    expect(h.navigate).not.toHaveBeenCalled()
    unmount()
    useNodes.setState({ byId: {}, order: [], loaded: true })
    render(<HomeRedirect />)
    expect(h.navigate).not.toHaveBeenCalled()
    expect(screen.getByTestId('home-redirect')).toHaveAttribute('data-target', 'wait')
  })

  it('falls back to /setup when the hub cannot be asked', () => {
    h.devices.isError = true
    render(<HomeRedirect />)
    expect(h.navigate).toHaveBeenCalledWith({ to: '/setup', replace: true })
  })
})
