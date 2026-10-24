import { describe, expect, it, vi } from 'vitest'
import { createConfigLoader } from './config'

const json = (body: unknown, status = 200): Response => new Response(JSON.stringify(body), { status })

describe('config loader', () => {
  it('reads hub_origin once and caches it', async () => {
    const fetchFn = vi.fn(() => Promise.resolve(json({ label: 'x', hub_origin: 'https://hub.example', protocol_version: 1 })))
    const load = createConfigLoader(fetchFn)
    expect(await load()).toEqual({ hubOrigin: 'https://hub.example' })
    expect(await load()).toEqual({ hubOrigin: 'https://hub.example' })
    expect(fetchFn).toHaveBeenCalledTimes(1)
    expect(fetchFn).toHaveBeenCalledWith('/__anycon/config.json')
  })

  it('shares one request between concurrent callers', async () => {
    const fetchFn = vi.fn(() => Promise.resolve(json({ hub_origin: 'https://hub.example' })))
    const load = createConfigLoader(fetchFn)
    await Promise.all([load(), load(), load()])
    expect(fetchFn).toHaveBeenCalledTimes(1)
  })

  it('does not cache a failure for long: tries again after 30 seconds', async () => {
    let now = 0
    let fail = true
    const fetchFn = vi.fn(() => (fail ? Promise.reject(new Error('offline')) : Promise.resolve(json({ hub_origin: 'https://hub.example' }))))
    const load = createConfigLoader(fetchFn, () => now)
    expect(await load()).toEqual({})
    fail = false
    now = 29_999
    expect(await load()).toEqual({})
    expect(fetchFn).toHaveBeenCalledTimes(1)
    now = 30_000
    expect(await load()).toEqual({ hubOrigin: 'https://hub.example' })
  })

  it.each([
    ['not json', new Response('<html>', { status: 200 })],
    ['http error', json({ hub_origin: 'https://hub.example' }, 500)],
    ['no hub_origin', json({ label: 'x' })],
    ['a path in the origin', json({ hub_origin: 'https://hub.example/app' })],
    ['an injected policy', json({ hub_origin: "https://hub.example; script-src *" })],
    ['not a url', json({ hub_origin: 'javascript:alert(1)' })],
  ])('ignores a bad answer (%s)', async (_name, res) => {
    const load = createConfigLoader(() => Promise.resolve(res))
    expect(await load()).toEqual({})
  })
})
