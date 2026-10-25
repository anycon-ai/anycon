import { describe, expect, it } from 'vitest'
import type { ApiDevice } from '@/api/types'
import { accountNodes, findAccountNode, homeTarget, lockedNodes, removedNodes, unlockParam } from './nodes'

const dev = (id: string, kind: ApiDevice['kind'] = 'daemon', extra: Partial<ApiDevice> = {}): ApiDevice => ({ id, name: id.slice(0, 4), kind, platform: 'linux-x86_64', created_at_ms: 1, online: true, paired_with: [], ...extra })
const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const W = 'w'.repeat(32)

describe('account nodes', () => {
  const devices = [dev(A), dev(B), dev(W, 'web'), dev('c'.repeat(32), 'cli')]

  it('only daemons count as nodes', () => {
    expect(accountNodes(devices).map((d) => d.id)).toEqual([A, B])
    expect(accountNodes(undefined)).toEqual([])
  })

  it('locked = the account\'s nodes this browser has no pairing with', () => {
    expect(lockedNodes(devices, [A]).map((d) => d.id)).toEqual([B])
    expect(lockedNodes(devices, [A, B])).toEqual([])
    expect(lockedNodes(devices, []).map((d) => d.id)).toEqual([A, B])
  })

  it('removed = paired here, but no longer a node of the account, judged only by a list fetched after the pairing', () => {
    const local = [
      { id: A, pairedAt: 100 },
      { id: 'd'.repeat(32), pairedAt: 100 },
      { id: 'e'.repeat(32), pairedAt: 300 },
    ]
    // d 不在账号里了；e 也不在，但它是在这份列表之后配对的，不算
    expect(removedNodes(devices, 200, local)).toEqual(['d'.repeat(32)])
    // 同 id 的 web 设备不算节点
    expect(removedNodes([dev('d'.repeat(32), 'web')], 200, local)).toEqual([A, 'd'.repeat(32)])
    expect(removedNodes(undefined, 200, local)).toEqual([]) // 列表没拉到：什么都不算
  })

  it('resolves the URL parameter: full id, or an unambiguous prefix', () => {
    const nodes = accountNodes(devices)
    expect(findAccountNode(A, nodes)?.id).toBe(A)
    expect(findAccountNode('aaaaaaaa', nodes)?.id).toBe(A)
    expect(findAccountNode('zzzz', nodes)).toBeUndefined()
    const twins = [dev('abcd1234' + '0'.repeat(24)), dev('abcd1234' + '1'.repeat(24))]
    expect(findAccountNode('abcd1234', twins)).toBeUndefined() // 有歧义：不猜
    expect(findAccountNode(twins[1]!.id, twins)?.id).toBe(twins[1]!.id)
    expect(unlockParam(twins[0]!, twins)).toBe(twins[0]!.id) // 冲突时回退全长
    expect(unlockParam(dev(A), [dev(A), dev(B)])).toBe('aaaaaaaa')
  })
})

describe('where "/" goes (spec/web.md § 2)', () => {
  const base = { nodesLoaded: true, localOrder: [] as string[], lastNode: null as string | null, devices: [dev(A)] as ApiDevice[] | undefined, devicesFailed: false }

  it('waits until the local nodes are loaded', () => {
    expect(homeTarget({ ...base, nodesLoaded: false })).toEqual({ to: 'wait' })
  })

  it('this browser has paired nodes → the one opened last (or the first)', () => {
    expect(homeTarget({ ...base, localOrder: [A, B], lastNode: B })).toEqual({ to: 'node', node: B })
    expect(homeTarget({ ...base, localOrder: [A, B], lastNode: 'gone' })).toEqual({ to: 'node', node: A })
    expect(homeTarget({ ...base, localOrder: [A], lastNode: null })).toEqual({ to: 'node', node: A })
    // 有已配对的节点时根本不需要设备列表
    expect(homeTarget({ ...base, localOrder: [A], devices: undefined })).toEqual({ to: 'node', node: A })
  })

  it('no local nodes but the account has some → /unlock', () => {
    expect(homeTarget(base)).toEqual({ to: 'unlock' })
    expect(homeTarget({ ...base, devices: [dev(W, 'web'), dev(A)] })).toEqual({ to: 'unlock' })
  })

  it('the account has no node either → /setup (other kinds of devices do not count)', () => {
    expect(homeTarget({ ...base, devices: [] })).toEqual({ to: 'setup' })
    expect(homeTarget({ ...base, devices: [dev(W, 'web')] })).toEqual({ to: 'setup' })
  })

  it('waits for the device list, and falls back to /setup when the hub cannot be asked', () => {
    expect(homeTarget({ ...base, devices: undefined })).toEqual({ to: 'wait' })
    expect(homeTarget({ ...base, devices: undefined, devicesFailed: true })).toEqual({ to: 'setup' })
  })
})
