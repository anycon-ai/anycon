// ADR-36 的三个 daemon→客户端通知：node.enroll_requested（→ 横幅）、node.enroll_resolved（→ 收起卡片）、
// node.security_event（→ toast，点「查看」去节点设置的「接入」标签）。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import { enrollInfo, nodeEntry } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { setNavigator } from '@/lib/navigate'
import { enrollKey, pendingApprovals, useEnroll } from '@/stores/enroll'
import { useHub } from '@/stores/hub'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import type { NodeSecurityEvent } from '@/protocol'
import { handleNotification } from './notifications'
import { securityText } from './securityEvent'

const NODE = 'n'.repeat(32)
const SELF = 's'.repeat(32)
let rpc: FakeRpc
const navigated: string[] = []

beforeEach(() => {
  rpc = new FakeRpc()
  setRpc(rpc)
  navigated.length = 0
  setNavigator((to) => navigated.push(to))
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: false, securityTick: {} })
  useUi.setState({ toasts: [] })
  useNodes.setState({
    byId: { [NODE]: nodeEntry({ id: NODE, name: 'mac-mini', devices: [{ device_id: 'phone', fingerprint: 'ab', kind: 'web', name: 'iPhone', online: true, paired_at_ms: 1 }] }) },
    order: [NODE],
    loaded: true,
  })
  useHub.setState((s) => ({ snapshot: { ...s.snapshot, deviceId: SELF } }))
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => {
  setRpc(null)
  setNavigator(null)
})

describe('node.enroll_requested', () => {
  it('puts the request where the banner finds it', () => {
    handleNotification(NODE, 'node.enroll_requested', enrollInfo({ enroll_id: 'a' }))
    expect(pendingApprovals(useEnroll.getState(), Date.now()).map((i) => [i.nodeId, i.info.enroll_id])).toEqual([[NODE, 'a']])
  })

  it('ignores malformed payloads instead of breaking the UI', () => {
    handleNotification(NODE, 'node.enroll_requested', { enroll_id: 5 })
    handleNotification(NODE, 'node.enroll_requested', null)
    expect(Object.keys(useEnroll.getState().items)).toEqual([])
  })
})

describe('node.enroll_resolved', () => {
  it('marks the request resolved (so the card collapses and the banner goes away)', () => {
    handleNotification(NODE, 'node.enroll_requested', enrollInfo({ enroll_id: 'a' }))
    handleNotification(NODE, 'node.enroll_resolved', { enroll_id: 'a', result: 'approved', by_device: 'phone' })
    expect(useEnroll.getState().resolved[enrollKey(NODE, 'a')]).toMatchObject({ result: 'approved', byDevice: 'phone' })
    expect(pendingApprovals(useEnroll.getState(), Date.now())).toEqual([])
  })

  it('works without by_device (the daemon decided: expired / failed / completed)', () => {
    handleNotification(NODE, 'node.enroll_requested', enrollInfo({ enroll_id: 'a' }))
    handleNotification(NODE, 'node.enroll_resolved', { enroll_id: 'a', result: 'expired' })
    expect(useEnroll.getState().resolved[enrollKey(NODE, 'a')]).toMatchObject({ result: 'expired', byDevice: null })
  })

  it('a result for a request we never heard of is not kept around', () => {
    handleNotification(NODE, 'node.enroll_resolved', { enroll_id: 'unknown', result: 'denied', by_device: 'phone' })
    expect(useEnroll.getState().resolved).toEqual({})
  })

  it('ignores malformed payloads', () => {
    handleNotification(NODE, 'node.enroll_resolved', { result: 'approved' })
    expect(useEnroll.getState().resolved).toEqual({})
  })
})

describe('node.security_event', () => {
  const event = (over: Partial<NodeSecurityEvent>): NodeSecurityEvent => ({ kind: 'device_enrolled', ts_ms: Date.now(), ...over })
  const toasts = () => useUi.getState().toasts

  it('shows a toast with a "review" action that opens the Access tab of that node', () => {
    handleNotification(NODE, 'node.security_event', event({ kind: 'password_locked', failures: 10 }))
    expect(toasts()).toHaveLength(1)
    expect(toasts()[0]!.text).toMatch(/mac-mini/)
    expect(toasts()[0]!.action).toBeDefined()
    toasts()[0]!.action!.run()
    expect(navigated).toEqual(['/n/nnnnnnnn/settings?tab=enroll'])
  })

  it('tells the "Access" tab to re-read its status', () => {
    handleNotification(NODE, 'node.security_event', event({ kind: 'password_failures', failures: 3 }))
    expect(useEnroll.getState().securityTick[NODE]).toBe(1)
  })

  it('a new device joining refreshes the node\'s device list', async () => {
    rpc.handlers['node.devices'] = () => ({ devices: [{ device_id: 'new', fingerprint: 'cd', kind: 'web', name: 'iPad', online: true, paired_at_ms: 2 }] })
    handleNotification(NODE, 'node.security_event', event({ kind: 'device_enrolled', device_name: 'iPad', device_id: 'new', method: 'password' }))
    await vi.waitFor(() => expect(useNodes.getState().byId[NODE]!.devices.map((d) => d.device_id)).toEqual(['new']))
  })

  it('does not nag about a change this device made itself', () => {
    handleNotification(NODE, 'node.security_event', event({ kind: 'password_changed', password_set: true, by_device: SELF }))
    handleNotification(NODE, 'node.security_event', event({ kind: 'settings_changed', by_device: SELF }))
    expect(toasts()).toHaveLength(0)
    expect(useEnroll.getState().securityTick[NODE]).toBe(2) // 状态还是要刷新
  })

  it('ignores malformed payloads', () => {
    handleNotification(NODE, 'node.security_event', { kind: 'nope', ts_ms: 1 })
    handleNotification(NODE, 'node.security_event', null)
    expect(toasts()).toHaveLength(0)
  })
})

describe('securityText', () => {
  const t = i18n.getFixedT('en')
  const ctx = { node: 'mac-mini', deviceName: (id: string) => (id === 'phone' ? 'iPhone' : undefined) }
  const ev = (over: Partial<NodeSecurityEvent>): NodeSecurityEvent => ({ kind: 'device_enrolled', ts_ms: 1, ...over })

  it('names the new device, the node and how it got in', () => {
    expect(securityText(t, ev({ device_name: 'iPad', method: 'scan' }), ctx)).toBe('New device “iPad” joined mac-mini (QR scan).')
    expect(securityText(t, ev({ device_name: 'iPad' }), ctx)).toBe('New device “iPad” joined mac-mini.')
    expect(securityText(t, ev({ device_name: undefined }), ctx)).toContain('a new device')
  })

  it('wrong password streak, lock', () => {
    expect(securityText(t, ev({ kind: 'password_failures', failures: 3 }), ctx)).toBe('The node password of mac-mini was entered wrongly 3 times in a row.')
    expect(securityText(t, ev({ kind: 'password_locked', failures: 10 }), ctx)).toBe('Password unlock of mac-mini is now locked.')
  })

  it('who changed the password: a known device, the node\'s own CLI, or nobody we know', () => {
    expect(securityText(t, ev({ kind: 'password_changed', password_set: true, by_device: 'phone' }), ctx)).toBe('The node password of mac-mini was changed by iPhone.')
    expect(securityText(t, ev({ kind: 'password_changed', password_set: false, by_device: 'local' }), ctx)).toBe('The node password of mac-mini was removed by the node’s own machine.')
    expect(securityText(t, ev({ kind: 'settings_changed' }), ctx)).toBe('The access settings of mac-mini were changed by someone.')
    expect(securityText(t, ev({ kind: 'settings_changed', by_device: 'abcdef0123456789' }), ctx)).toContain('abcdef01')
  })

  it('the Chinese texts exist for every kind too', () => {
    const zh = i18n.getFixedT('zh')
    for (const kind of ['device_enrolled', 'password_failures', 'password_locked', 'password_changed', 'settings_changed'] as const) {
      const text = securityText(zh, ev({ kind, device_name: 'iPad', failures: 3, by_device: 'phone' }), ctx)
      expect(text).toMatch(/mac-mini/)
      expect(text).toMatch(/[一-鿿]/u)
    }
  })
})
