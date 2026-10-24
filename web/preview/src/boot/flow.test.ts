import { PREVIEW_ENTER_PATH, decodePreviewLaunch, encodePreviewLaunch, type PreviewLaunch } from '@anycon/protocol'
import { describe, expect, it } from 'vitest'
import type { LaunchLookup } from '../shared/launch'
import { makeLaunch } from '../test/rig'
import { RELOAD_LIMIT, createReloadGuard, runBoot, safeLaunchPath, waitForActivation, type BootEnv, type BootView } from './flow'

const HOST = 'abcdefghijklmnopqrstuvwxyz.aiplace.app'
const ORIGIN = `https://${HOST}`
const NOW = 1_700_000_000_000

function setup(opts: { path?: string; hash?: string; launch?: LaunchLookup; register?: BootEnv['registerServiceWorker'] | 'unsupported'; saveFails?: boolean; allow?: boolean } = {}) {
  const calls: string[] = []
  const views: BootView[] = []
  let stored: PreviewLaunch | undefined
  const env: BootEnv = {
    location: { origin: ORIGIN, hostname: HOST, pathname: opts.path ?? PREVIEW_ENTER_PATH, search: '', hash: opts.hash ?? '' },
    now: () => NOW,
    replaceState: (url) => calls.push(`replaceState ${url}`),
    navigate: (url) => calls.push(`navigate ${url}`),
    reload: () => calls.push('reload'),
    registerServiceWorker:
      opts.register === 'unsupported'
        ? undefined
        : (opts.register ??
          (async () => {
            calls.push('register')
          })),
    saveLaunch: async (l) => {
      if (opts.saveFails) throw new Error('blocked')
      calls.push('save')
      stored = l
    },
    loadLaunch: async () => opts.launch ?? { ok: false, reason: 'none' },
    allowReload: () => opts.allow ?? true,
    show: (v) => {
      calls.push(`show ${v.kind}`)
      views.push(v)
    },
  }
  return { env, calls, views, stored: () => stored }
}

const fragment = (over: Partial<PreviewLaunch> = {}): string => `#${encodePreviewLaunch(makeLaunch({ expires_at_ms: NOW + 3_600_000, ...over }))}`

describe('/__anycon/enter with a launch', () => {
  it('erases the fragment first, stores the launch, registers the service worker, then goes to the path', async () => {
    const t = setup({ hash: fragment({ path: '/app?x=1' }) })
    await runBoot(t.env)
    expect(t.calls).toEqual(['replaceState /__anycon/enter', 'show starting', 'save', 'register', 'navigate /app?x=1'])
    expect(t.stored()?.token).toBe('preview-token')
  })

  it('erases the fragment even when it is garbage, and shows that the link is not valid', async () => {
    const t = setup({ hash: '#not-a-launch' })
    await runBoot(t.env)
    expect(t.calls).toEqual(['replaceState /__anycon/enter', 'show invalid'])
    expect(t.stored()).toBeUndefined()
  })

  it('keeps the query string when erasing the fragment', async () => {
    const t = setup({ hash: fragment() })
    t.env.location.search = '?utm=1'
    await runBoot(t.env)
    expect(t.calls[0]).toBe('replaceState /__anycon/enter?utm=1')
  })

  it('refuses a launch meant for another preview origin', async () => {
    const t = setup({ hash: fragment({ label: 'zzzzzzzzzzzzzzzzzzzzzzzzzz' }) })
    await runBoot(t.env)
    expect(t.views).toEqual([{ kind: 'invalid' }])
    expect(t.calls).not.toContain('save')
  })

  it('refuses an expired launch', async () => {
    const t = setup({ hash: fragment({ expires_at_ms: NOW }) })
    await runBoot(t.env)
    expect(t.views).toEqual([{ kind: 'expired' }])
    expect(t.calls).not.toContain('save')
  })

  it.each([
    ['/\\evil.example/x', '/'],
    ['/\t/evil.example', '/'],
    ['//evil.example', '/'],
    ['/__anycon/enter', '/'],
    ['/__anycon/sw.js', '/'],
  ])('only navigates to a safe path (%j → %s)', async (path, expected) => {
    // 这些写法在 decodePreviewLaunch 里多半已经被拒绝；这里直接测引导页自己的第二道防线
    expect(safeLaunchPath(path, ORIGIN)).toBe(expected)
  })

  it('without service worker support shows a way to continue in a new tab, with the launch in the link', async () => {
    const t = setup({ hash: fragment({ path: '/deep/link?q=1' }), register: 'unsupported' })
    await runBoot(t.env)
    const view = t.views.at(-1)
    expect(view).toMatchObject({ kind: 'unsupported' })
    const url = new URL((view as { newTabUrl: string }).newTabUrl)
    expect(url.origin).toBe(ORIGIN)
    expect(url.pathname).toBe(PREVIEW_ENTER_PATH)
    expect(decodePreviewLaunch(url.hash)).toMatchObject({ token: 'preview-token', path: '/deep/link?q=1' })
    expect(t.calls).not.toContain('navigate /deep/link?q=1')
  })

  it('shows the same when registration is refused (e.g. inside a third-party iframe) or storage is blocked', async () => {
    const refused = setup({ hash: fragment(), register: () => Promise.reject(new DOMException('denied', 'SecurityError')) })
    await runBoot(refused.env)
    expect(refused.views.at(-1)).toMatchObject({ kind: 'unsupported' })
    const blocked = setup({ hash: fragment(), saveFails: true })
    await runBoot(blocked.env)
    expect(blocked.views.at(-1)).toMatchObject({ kind: 'unsupported' })
    expect(blocked.calls).not.toContain('register')
  })
})

describe('other paths', () => {
  const stored = (over: Partial<PreviewLaunch> = {}): LaunchLookup => ({ ok: true, launch: makeLaunch({ expires_at_ms: NOW + 3_600_000, ...over }) })

  it('with a valid stored launch registers the service worker and reloads', async () => {
    const t = setup({ path: '/some/page', launch: stored() })
    await runBoot(t.env)
    expect(t.calls).toEqual(['show starting', 'register', 'reload'])
  })

  it('stops with an error when the page keeps being served by the hub', async () => {
    const t = setup({ path: '/some/page', launch: stored(), allow: false })
    await runBoot(t.env)
    expect(t.views.at(-1)).toEqual({ kind: 'loop' })
    expect(t.calls).not.toContain('reload')
  })

  it.each([
    [{ ok: false, reason: 'none' } as const, 'needs_launch'],
    [{ ok: false, reason: 'expired' } as const, 'expired'],
    [{ ok: false, reason: 'invalid' } as const, 'invalid'],
  ])('explains a missing, expired or broken stored launch (%j)', async (launch, kind) => {
    const t = setup({ path: '/x', launch })
    await runBoot(t.env)
    expect(t.views).toEqual([{ kind }])
    expect(t.calls).toEqual([`show ${kind}`])
  })

  it('rejects a stored launch that belongs to another label', async () => {
    const t = setup({ path: '/x', launch: stored({ label: 'zzzzzzzzzzzzzzzzzzzzzzzzzz' }) })
    await runBoot(t.env)
    expect(t.views).toEqual([{ kind: 'invalid' }])
  })

  it('offers the new tab fallback with the current path when service workers are unavailable', async () => {
    const t = setup({ path: '/page', launch: stored(), register: 'unsupported' })
    t.env.location.search = '?a=1'
    await runBoot(t.env)
    const view = t.views.at(-1) as { kind: string; newTabUrl: string }
    expect(view.kind).toBe('unsupported')
    expect(decodePreviewLaunch(new URL(view.newTabUrl).hash).path).toBe('/page?a=1')
  })

  it('reading the stored launch failing is reported, not thrown', async () => {
    const t = setup({ path: '/x' })
    t.env.loadLaunch = () => Promise.reject(new Error('idb'))
    await runBoot(t.env)
    expect(t.views).toEqual([{ kind: 'failed', newTabUrl: undefined }])
  })

  it('a refreshed /__anycon/enter (fragment already gone) goes to the stored path instead of reloading itself', async () => {
    const t = setup({ path: PREVIEW_ENTER_PATH, launch: stored({ path: '/dash?tab=2' }) })
    await runBoot(t.env)
    expect(t.calls).toEqual(['show starting', 'register', 'navigate /dash?tab=2'])
  })

  it('a bare # is not a launch', async () => {
    const t = setup({ hash: '#', launch: stored({ path: '/' }) })
    await runBoot(t.env)
    expect(t.calls).not.toContain('replaceState /__anycon/enter')
    expect(t.calls).toContain('navigate /')
  })
})

describe('safeLaunchPath', () => {
  it('returns the browser-normalised form of a same-origin path', () => {
    expect(safeLaunchPath('/a/b?c=d#e', ORIGIN)).toBe('/a/b?c=d#e')
    expect(safeLaunchPath('/a/../b', ORIGIN)).toBe('/b')
    expect(safeLaunchPath('/中文?q=é', ORIGIN)).toBe('/%E4%B8%AD%E6%96%87?q=%C3%A9')
  })

  it('falls back to / for anything that leaves the origin or lands on hub files', () => {
    for (const path of ['//evil.example', '/\\evil.example', '/\t/evil.example', '\\\\evil.example', 'https://evil.example/', 'javascript:alert(1)', '/__anycon/shim.js', '/__anycon/enter#x']) {
      expect(safeLaunchPath(path, ORIGIN), path).toBe('/')
    }
  })
})

describe('reload guard', () => {
  function memoryStorage() {
    const map = new Map<string, string>()
    return { getItem: (k: string) => map.get(k) ?? null, setItem: (k: string, v: string) => void map.set(k, v) }
  }

  it('allows three reloads in ten seconds, then stops', () => {
    const allow = createReloadGuard(memoryStorage())
    const results = [0, 1_000, 2_000, 3_000].map((t) => allow(NOW + t))
    expect(results).toEqual([true, true, true, false])
    expect(RELOAD_LIMIT).toBe(3)
  })

  it('forgets reloads older than ten seconds', () => {
    const allow = createReloadGuard(memoryStorage())
    for (const t of [0, 1_000, 2_000]) allow(NOW + t)
    expect(allow(NOW + 9_999)).toBe(false)
    expect(allow(NOW + 10_001)).toBe(true) // 最早的那次已经滑出 10 秒窗口
  })

  it('counts across page loads through the storage', () => {
    const storage = memoryStorage()
    createReloadGuard(storage)(NOW)
    createReloadGuard(storage)(NOW + 1)
    createReloadGuard(storage)(NOW + 2)
    expect(createReloadGuard(storage)(NOW + 3)).toBe(false)
  })

  it('falls back to memory when the storage is unusable', () => {
    const broken = {
      getItem: () => {
        throw new Error('blocked')
      },
      setItem: () => {
        throw new Error('blocked')
      },
    }
    const allow = createReloadGuard(broken)
    expect([0, 1, 2, 3].map((t) => allow(NOW + t))).toEqual([true, true, true, false])
    expect(createReloadGuard(undefined)(NOW)).toBe(true)
  })
})

describe('waitForActivation', () => {
  type State = 'installing' | 'installed' | 'activating' | 'activated' | 'redundant'
  function worker(state: State) {
    const listeners: Array<() => void> = []
    return {
      state,
      addEventListener: (_: string, l: () => void) => void listeners.push(l),
      set: (s: State) => {
        w.state = s
        for (const l of listeners) l()
      },
    }
  }
  let w = worker('installing')

  it('returns at once for an active worker', async () => {
    await expect(waitForActivation({ active: worker('activated') as never, waiting: null, installing: null })).resolves.toBeUndefined()
  })

  it('waits for the installing worker to activate', async () => {
    w = worker('installing')
    const done = waitForActivation({ active: null, waiting: null, installing: w as never })
    w.set('installed')
    w.set('activating')
    w.set('activated')
    await expect(done).resolves.toBeUndefined()
  })

  it('fails when the worker becomes redundant, has no worker, or never activates', async () => {
    w = worker('installing')
    const redundant = waitForActivation({ active: null, waiting: null, installing: w as never })
    w.set('redundant')
    await expect(redundant).rejects.toThrow('redundant')
    await expect(waitForActivation({ active: null, waiting: null, installing: null })).rejects.toThrow()
    const slow = waitForActivation({ active: null, waiting: null, installing: worker('installing') as never }, 10)
    await expect(slow).rejects.toThrow('did not activate')
  })
})
