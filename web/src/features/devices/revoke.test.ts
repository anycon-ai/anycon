import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ApiError } from '@/api/client'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { revokeDevice, revokeDeviceOnNode } from './revoke'

let rpc: FakeRpc
let fetchMock: ReturnType<typeof vi.fn>

function json(status: number, body?: unknown): Response {
  return new Response(body === undefined ? null : JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
}

beforeEach(() => {
  rpc = new FakeRpc()
  rpc.handlers['node.revoke_device'] = () => ({})
  setRpc(rpc)
  fetchMock = vi.fn().mockResolvedValue(json(204))
  vi.stubGlobal('fetch', fetchMock)
})
afterEach(() => {
  setRpc(null)
  vi.unstubAllGlobals()
})

describe('revokeDevice', () => {
  it('asks the node to drop the key, then tells the hub', async () => {
    const out = await revokeDevice('dev1', ['n1'])
    expect(rpc.calls).toEqual([{ nodeId: 'n1', method: 'node.revoke_device', params: { device_id: 'dev1' } }])
    expect(fetchMock).toHaveBeenCalledWith('/api/devices/dev1', expect.objectContaining({ method: 'DELETE' }))
    expect(out.offlineNodes).toEqual([])
  })

  it('surfaces a node failure and does not touch the hub (so the device is not half-revoked)', async () => {
    rpc.handlers['node.revoke_device'] = () => {
      throw rpcError('internal')
    }
    await expect(revokeDevice('dev1', ['n1'])).rejects.toMatchObject({ kind: 'rpc', code: 'internal' })
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('surfaces a hub failure instead of swallowing it', async () => {
    fetchMock.mockResolvedValue(json(500, { error: { code: 'internal', message: 'boom' } }))
    await expect(revokeDevice('dev1', ['n1'])).rejects.toBeInstanceOf(ApiError)
  })

  it('a retry works: the node saying not_found (already removed) counts as done, and a hub 404 too', async () => {
    rpc.handlers['node.revoke_device'] = () => {
      throw rpcError('not_found')
    }
    fetchMock.mockResolvedValue(json(404, { error: { code: 'not_found', message: 'nope' } }))
    await expect(revokeDevice('dev1', ['n1'])).resolves.toEqual({ offlineNodes: [] })
  })

  it('skips nodes that are offline but reports them, and still revokes at the hub', async () => {
    rpc.ready = false
    const out = await revokeDevice('dev1', ['n1', 'n2'])
    expect(rpc.calls).toEqual([])
    expect(out.offlineNodes).toEqual(['n1', 'n2'])
    expect(fetchMock).toHaveBeenCalled()
  })
})

describe('revokeDeviceOnNode', () => {
  it('treats not_found as already done, but passes every other failure on', async () => {
    rpc.handlers['node.revoke_device'] = () => {
      throw rpcError('not_found')
    }
    await expect(revokeDeviceOnNode('n1', 'dev1')).resolves.toBeUndefined()
    rpc.handlers['node.revoke_device'] = () => {
      throw rpcError('internal')
    }
    await expect(revokeDeviceOnNode('n1', 'dev1')).rejects.toMatchObject({ kind: 'rpc', code: 'internal' })
    rpc.ready = false
    await expect(revokeDeviceOnNode('n1', 'dev1')).rejects.toMatchObject({ kind: 'offline' })
  })
})
