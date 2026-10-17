import { beforeEach, describe, expect, it } from 'vitest'
import { enrollInfo } from '@/test/factories'
import { enrollKey, isPendingApproval, isPendingScan, parseEnrollInfo, pendingApprovals, useEnroll } from './enroll'

const NODE = 'n'.repeat(32)
const OTHER = 'o'.repeat(32)
const NOW = 1_700_000_000_000

beforeEach(() => {
  useEnroll.setState({ items: {}, resolved: {}, dialog: null, scannerOpen: false, securityTick: {} })
})

describe('parseEnrollInfo', () => {
  it('accepts what the daemon sends and drops anything malformed', () => {
    expect(parseEnrollInfo(enrollInfo())).not.toBeNull()
    expect(parseEnrollInfo(null)).toBeNull()
    expect(parseEnrollInfo({})).toBeNull()
    expect(parseEnrollInfo({ ...enrollInfo(), enroll_id: '' })).toBeNull()
    expect(parseEnrollInfo({ ...enrollInfo(), status: 'bogus' })).toBeNull()
    expect(parseEnrollInfo({ ...enrollInfo(), expires_at_ms: 'soon' })).toBeNull()
  })
})

describe('pending approvals', () => {
  it('are the code requests that wait for a paired device, oldest first', () => {
    const s = useEnroll.getState()
    s.upsert(NODE, enrollInfo({ enroll_id: 'b', expires_at_ms: NOW + 200_000 }), NOW + 10)
    s.upsert(NODE, enrollInfo({ enroll_id: 'a', expires_at_ms: NOW + 200_000 }), NOW)
    s.upsert(OTHER, enrollInfo({ enroll_id: 'c', expires_at_ms: NOW + 200_000 }), NOW + 20)
    expect(pendingApprovals(useEnroll.getState(), NOW + 100).map((i) => i.info.enroll_id)).toEqual(['a', 'b', 'c'])
    expect(pendingApprovals(useEnroll.getState(), NOW + 100, OTHER).map((i) => i.info.enroll_id)).toEqual(['c'])
  })

  it('leave out scan requests, the ones already approved (ready), and expired ones', () => {
    const s = useEnroll.getState()
    s.upsert(NODE, enrollInfo({ enroll_id: 'scan', method: 'scan', approval: undefined }), NOW)
    s.upsert(NODE, enrollInfo({ enroll_id: 'ready', status: 'ready' }), NOW)
    s.upsert(NODE, enrollInfo({ enroll_id: 'hello', status: 'waiting_hello', method: undefined }), NOW)
    s.upsert(NODE, enrollInfo({ enroll_id: 'noapproval', approval: false }), NOW)
    s.upsert(NODE, enrollInfo({ enroll_id: 'ok', expires_at_ms: NOW + 100_000 }), NOW)
    expect(pendingApprovals(useEnroll.getState(), NOW + 1_000).map((i) => i.info.enroll_id)).toEqual(['ok'])
    expect(pendingApprovals(useEnroll.getState(), NOW + 200_000)).toEqual([]) // 5 分钟过了
    const scan = useEnroll.getState().items[enrollKey(NODE, 'scan')]!
    expect(isPendingScan(scan, {}, NOW + 1_000)).toBe(true)
    expect(isPendingApproval(scan, {}, NOW + 1_000)).toBe(false)
  })

  it('go away once another device (or the daemon) has resolved them', () => {
    const s = useEnroll.getState()
    s.upsert(NODE, enrollInfo({ enroll_id: 'a' }), NOW)
    s.resolve(NODE, 'a', 'approved', 'phone', NOW + 5)
    expect(pendingApprovals(useEnroll.getState(), NOW + 10)).toEqual([])
    expect(useEnroll.getState().resolved[enrollKey(NODE, 'a')]).toMatchObject({ result: 'approved', byDevice: 'phone' })
  })

  it('use a plausible local deadline when the daemon clock is far off', () => {
    useEnroll.getState().upsert(NODE, enrollInfo({ enroll_id: 'a', expires_at_ms: NOW - 3_600_000 }), NOW)
    expect(useEnroll.getState().items[enrollKey(NODE, 'a')]!.expiresAt).toBe(NOW + 300_000)
  })
})

describe('replaceNode (node.enroll_list)', () => {
  it('replaces that node\'s requests only, dropping the ones that ended while we were away', () => {
    const s = useEnroll.getState()
    s.upsert(NODE, enrollInfo({ enroll_id: 'old' }), NOW)
    s.upsert(OTHER, enrollInfo({ enroll_id: 'keep' }), NOW)
    s.replaceNode(NODE, [enrollInfo({ enroll_id: 'new' })], NOW + 1)
    expect(Object.keys(useEnroll.getState().items).sort()).toEqual([enrollKey(NODE, 'new'), enrollKey(OTHER, 'keep')].sort())
  })

  it('keeps when a request was first received', () => {
    const s = useEnroll.getState()
    s.upsert(NODE, enrollInfo({ enroll_id: 'a' }), NOW)
    s.replaceNode(NODE, [enrollInfo({ enroll_id: 'a' })], NOW + 60_000)
    expect(useEnroll.getState().items[enrollKey(NODE, 'a')]!.receivedAt).toBe(NOW)
  })
})

describe('dialog', () => {
  it('closing it clears a request that has been resolved, and keeps one that is still waiting', () => {
    const s = useEnroll.getState()
    s.upsert(NODE, enrollInfo({ enroll_id: 'a' }), Date.now())
    s.upsert(NODE, enrollInfo({ enroll_id: 'b' }), Date.now())
    s.resolve(NODE, 'a', 'denied', 'phone')
    s.openDialog(NODE, 'a')
    useEnroll.getState().closeDialog()
    expect(Object.keys(useEnroll.getState().items)).toEqual([enrollKey(NODE, 'b')])
    useEnroll.getState().openDialog(NODE, 'b')
    useEnroll.getState().closeDialog()
    expect(Object.keys(useEnroll.getState().items)).toEqual([enrollKey(NODE, 'b')])
    expect(useEnroll.getState().dialog).toBeNull()
  })

  it('keeps the request the dialog is showing even when it is long expired', () => {
    useEnroll.getState().upsert(NODE, enrollInfo({ enroll_id: 'a' }), NOW)
    useEnroll.getState().openDialog(NODE, 'a')
    useEnroll.getState().upsert(NODE, enrollInfo({ enroll_id: 'b' }), NOW + 3_600_000)
    expect(useEnroll.getState().items[enrollKey(NODE, 'a')]).toBeDefined()
  })
})

describe('createdAt (for "2 minutes ago")', () => {
  it('uses the daemon\'s timestamp when its clock is close to ours, otherwise the time we received it', () => {
    const s = useEnroll.getState()
    s.upsert(NODE, enrollInfo({ enroll_id: 'a', created_at_ms: NOW - 120_000 }), NOW)
    s.upsert(NODE, enrollInfo({ enroll_id: 'b', created_at_ms: NOW - 3_600_000 }), NOW) // 时钟差一个小时
    s.upsert(NODE, enrollInfo({ enroll_id: 'c', created_at_ms: NOW + 3_600_000 }), NOW)
    const items = useEnroll.getState().items
    expect(items[enrollKey(NODE, 'a')]!.createdAt).toBe(NOW - 120_000)
    expect(items[enrollKey(NODE, 'b')]!.createdAt).toBe(NOW)
    expect(items[enrollKey(NODE, 'c')]!.createdAt).toBe(NOW)
  })
})
