// 节点设置的标签由 URL 的 `?tab=` 决定：安全提醒 / 接入请求的推送点进来直达「接入」（ADR-36）。
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enrollInfo, nodeEntry } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'

const h = vi.hoisted(() => ({ navigate: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, { node: 'nnnnnnnn' }))

import { Route } from '@/routes/_app/_shell/n.$node.settings'
import { parseSettingsTab } from '@/lib/settingsTabs'
import { NodeSettingsPage } from './NodeSettingsPage'

const NODE = 'n'.repeat(32)
let rpc: FakeRpc

beforeEach(() => {
  h.navigate.mockReset()
  rpc = new FakeRpc()
  rpc.handlers['node.enroll_status'] = () => ({ approval: true, password: { set: false, state: 'enabled', failures: 0 } })
  rpc.handlers['node.enroll_list'] = () => ({ enrolls: [] })
  rpc.handlers['node.audit'] = () => ({ entries: [] })
  rpc.handlers['node.devices'] = () => ({ devices: [] })
  setRpc(rpc)
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, name: 'mac-mini' }) }, order: [NODE], loaded: true })
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: false, securityTick: {} })
})
afterEach(() => setRpc(null))

describe('?tab=', () => {
  it('has an "Access" tab between Devices and Workspace', () => {
    render(<NodeSettingsPage tab="enroll" />)
    const tabs = screen.getAllByRole('tab').map((t) => t.getAttribute('data-testid'))
    expect(tabs).toEqual(['tab-agent', 'tab-devices', 'tab-enroll', 'tab-workspace', 'tab-limits', 'tab-about'])
    expect(screen.getByTestId('tab-enroll')).toHaveAttribute('aria-selected', 'true')
    expect(screen.getByTestId('enroll-tab')).toBeInTheDocument()
  })

  it('opens the first tab by default', () => {
    render(<NodeSettingsPage />)
    expect(screen.getByTestId('tab-agent')).toHaveAttribute('aria-selected', 'true')
    expect(screen.queryByTestId('enroll-tab')).toBeNull()
  })

  it('picking a tab puts it in the URL (so a reload or a shared link lands there)', async () => {
    const user = userEvent.setup()
    render(<NodeSettingsPage />)
    await user.click(screen.getByTestId('tab-enroll'))
    expect(h.navigate).toHaveBeenCalledWith({ to: '/n/$node/settings', params: { node: 'nnnnnnnn' }, search: { tab: 'enroll' }, replace: true })
  })

  it('a push with review=1 pops the approval dialog for the waiting request', async () => {
    rpc.handlers['node.enroll_list'] = () => ({ enrolls: [enrollInfo({ enroll_id: 'a' })] })
    render(<NodeSettingsPage tab="enroll" review />)
    await vi.waitFor(() => expect(useEnroll.getState().dialog).toEqual({ nodeId: NODE, enrollId: 'a' }))
  })

  it('only knows the real tabs', () => {
    expect(parseSettingsTab('enroll')).toBe('enroll')
    expect(parseSettingsTab('devices')).toBe('devices')
    expect(parseSettingsTab('nope')).toBeUndefined()
    expect(parseSettingsTab(3)).toBeUndefined()
  })

  it('the route reads ?tab= and ?review=1 from the URL', () => {
    const validate = Route.options.validateSearch as (s: Record<string, unknown>) => { tab?: string; review?: boolean }
    expect(validate({ tab: 'enroll', review: 1 })).toEqual({ tab: 'enroll', review: true })
    expect(validate({ tab: 'enroll', review: '1' })).toEqual({ tab: 'enroll', review: true })
    expect(validate({ tab: 'bogus' })).toEqual({ tab: undefined, review: undefined })
    expect(validate({})).toEqual({ tab: undefined, review: undefined })
  })
})
