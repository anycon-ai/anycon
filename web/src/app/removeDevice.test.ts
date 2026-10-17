import { describe, expect, it, vi } from 'vitest'
import { removeThisDevice, RemoveDeviceError, type RemoveDeviceDeps } from './removeDevice'

function deps(over: Partial<RemoveDeviceDeps> = {}) {
  const calls: string[] = []
  const d: RemoveDeviceDeps = {
    deviceId: 'dev1',
    nodeIds: ['n1', 'n2', 'n3'],
    isReady: (id) => id !== 'n3',
    revokeOnNode: async (n, dev) => void calls.push(`node:${n}:${dev}`),
    revokeAtHub: async (dev) => void calls.push(`hub:${dev}`),
    logout: async () => void calls.push('logout'),
    wipe: async () => void calls.push('wipe'),
    ...over,
  }
  return { d, calls }
}

describe('removeThisDevice (sign out and remove this device)', () => {
  it('revokes on every node, then at the hub, then signs out, then wipes - in that order', async () => {
    const { d, calls } = deps({ isReady: () => true })
    const r = await removeThisDevice(d)
    expect(calls).toEqual(['node:n1:dev1', 'node:n2:dev1', 'node:n3:dev1', 'hub:dev1', 'logout', 'wipe'])
    expect(r).toEqual({ revokedOnNodes: ['n1', 'n2', 'n3'], skippedNodes: [] })
  })

  it('an offline node stops everything before the hub is touched and nothing is deleted (so it can be retried)', async () => {
    const { d, calls } = deps() // n3 离线
    const err = await removeThisDevice(d).catch((e: unknown) => e)
    expect(err).toBeInstanceOf(RemoveDeviceError)
    expect(err).toMatchObject({ step: 'nodes', failedNodes: ['n3'] })
    expect(calls).toEqual(['node:n1:dev1', 'node:n2:dev1']) // 能撤销的已经撤销了；hub / 退出 / 清库都没动
  })

  it('a node that refuses to revoke is named in the error and nothing else happens', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const { d, calls } = deps({
      isReady: () => true,
      revokeOnNode: async (n) => {
        if (n === 'n2') throw new Error('boom')
        calls.push(`node:${n}`)
      },
    })
    const err = await removeThisDevice(d).catch((e: unknown) => e)
    expect(err).toMatchObject({ step: 'nodes', failedNodes: ['n2'] })
    expect(calls).toEqual(['node:n1', 'node:n3'])
  })

  it('force carries on past unreachable nodes and reports them', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const { d, calls } = deps()
    const r = await removeThisDevice(d, { force: true })
    expect(r).toEqual({ revokedOnNodes: ['n1', 'n2'], skippedNodes: ['n3'] })
    expect(calls.slice(-3)).toEqual(['hub:dev1', 'logout', 'wipe'])
  })

  it('if the hub cannot revoke, nothing is signed out or wiped', async () => {
    const { d, calls } = deps({
      isReady: () => true,
      revokeAtHub: async () => {
        throw new Error('network')
      },
    })
    await expect(removeThisDevice(d)).rejects.toMatchObject({ step: 'hub' })
    expect(calls).not.toContain('logout')
    expect(calls).not.toContain('wipe')
  })

  it('…unless the user insists: force wipes the local data anyway', async () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const { d, calls } = deps({
      isReady: () => true,
      revokeAtHub: async () => {
        throw new Error('network')
      },
      logout: async () => {
        throw new Error('network')
      },
    })
    await removeThisDevice(d, { force: true })
    expect(calls.at(-1)).toBe('wipe')
  })

  it('a failed sign-out request aborts before wiping and says which step failed', async () => {
    const { d, calls } = deps({
      isReady: () => true,
      logout: async () => {
        throw new Error('network')
      },
    })
    const err = await removeThisDevice(d).catch((e: unknown) => e)
    expect(err).toBeInstanceOf(RemoveDeviceError)
    expect(err).toMatchObject({ step: 'logout' })
    expect(calls).not.toContain('wipe')
  })

  it('a browser that never paired just signs out and wipes', async () => {
    const { d, calls } = deps({ deviceId: null })
    await removeThisDevice(d)
    expect(calls).toEqual(['logout', 'wipe'])
  })
})
