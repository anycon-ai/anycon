import type { PreviewLaunch } from '@anycon/protocol'
import { describe, expect, it, vi } from 'vitest'
import type { LaunchLookup } from '../shared/launch'
import { makeLaunch } from '../test/rig'
import { TunnelHost } from './host'
import { TunnelUnavailable, type PreviewTunnel } from './tunnel/link'
import type { TunnelSession } from './tunnel/session'

function fakeTunnel(launch: PreviewLaunch) {
  const t = {
    launch,
    state: { phase: 'idle' } as PreviewTunnel['state'],
    stopped: false,
    stop() {
      t.stopped = true
    },
    whenReady: () => Promise.resolve({} as TunnelSession),
  }
  return t
}

function setup(initial: LaunchLookup) {
  let lookup = initial
  const created: Array<ReturnType<typeof fakeTunnel>> = []
  const host = new TunnelHost({
    loadLaunch: () => Promise.resolve(lookup),
    createTunnel: (launch) => {
      const t = fakeTunnel(launch)
      created.push(t)
      return t as unknown as PreviewTunnel
    },
  })
  return { host, created, set: (l: LaunchLookup) => (lookup = l) }
}

const ok = (over: Partial<PreviewLaunch> = {}): LaunchLookup => ({ ok: true, launch: makeLaunch(over) })

describe('TunnelHost', () => {
  it('creates the tunnel from the stored launch on first use and reuses it', async () => {
    const { host, created } = setup(ok())
    const a = await host.current(false)
    const b = await host.current(false)
    expect(a).toBe(b)
    expect(created).toHaveLength(1)
  })

  it('does not read the launch again for sub-resource requests', async () => {
    const load = vi.fn(() => Promise.resolve(ok()))
    const host = new TunnelHost({ loadLaunch: load, createTunnel: (l) => fakeTunnel(l) as unknown as PreviewTunnel })
    await host.current(false)
    await host.current(false)
    await host.current(false)
    expect(load).toHaveBeenCalledTimes(1)
  })

  it('on navigations notices a new launch and replaces the tunnel', async () => {
    const { host, created, set } = setup(ok())
    await host.current(true)
    set(ok({ ticket_id: 'ff'.repeat(16), token: 'new-token' }))
    const next = await host.current(true)
    expect(created).toHaveLength(2)
    expect(created[0]!.stopped).toBe(true)
    expect(next).toBe(created[1])
  })

  it('keeps the tunnel when the same launch is read again', async () => {
    const { host, created } = setup(ok())
    await host.current(true)
    await host.current(true)
    expect(created).toHaveLength(1)
  })

  it('a terminal tunnel is revived by a new launch, and stays terminal for the old one', async () => {
    const { host, created, set } = setup(ok())
    await host.current(false)
    created[0]!.state = { phase: 'terminal', kind: 'expired' }
    expect(await host.current(false)).toBe(created[0])
    set(ok({ token: 'fresh', ticket_id: 'ab'.repeat(16) }))
    expect(await host.current(false)).toBe(created[1])
  })

  it('reports a missing, invalid or expired launch', async () => {
    for (const [reason, kind] of [
      ['none', 'no_launch'],
      ['invalid', 'no_launch'],
      ['expired', 'expired'],
    ] as const) {
      const { host } = setup({ ok: false, reason })
      await expect(host.current(false)).rejects.toMatchObject({ kind })
      await expect(host.whenReady(true)).rejects.toBeInstanceOf(TunnelUnavailable)
    }
  })

  it('keeps using a live tunnel when the stored launch can no longer be read', async () => {
    const { host, created, set } = setup(ok())
    await host.current(false)
    set({ ok: false, reason: 'none' })
    expect(await host.current(true)).toBe(created[0])
  })

  it('tells why the tunnel is terminal', async () => {
    const { host, created } = setup(ok())
    expect(host.terminalReason()).toBeUndefined()
    await host.current(false)
    expect(host.terminalReason()).toBeUndefined()
    created[0]!.state = { phase: 'terminal', kind: 'closed', detail: 'bye' }
    expect(host.terminalReason()).toMatchObject({ kind: 'closed', detail: 'bye', terminal: true })
  })

  it('whenReady waits for the tunnel', async () => {
    const { host } = setup(ok())
    await expect(host.whenReady(false)).resolves.toBeDefined()
  })
})
