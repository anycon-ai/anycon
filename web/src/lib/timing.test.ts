import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mark, resetTiming, timed, timingEntries } from './timing'

beforeEach(() => resetTiming())
afterEach(() => {
  localStorage.removeItem('anycon.debug.timing')
  vi.useRealTimers()
  vi.restoreAllMocks()
})

describe('timing', () => {
  it('records marks and the duration of timed steps, even when they fail', async () => {
    mark('hub.connect')
    await timed('rpc node.info', async () => 1)
    await expect(timed('rpc session.list', () => Promise.reject(new Error('boom')))).rejects.toThrow('boom')
    const e = timingEntries()
    expect(e.map((x) => x.name)).toEqual(['hub.connect', 'rpc node.info', 'rpc session.list'])
    expect(e[0]!.durationMs).toBeUndefined()
    expect(typeof e[1]!.durationMs).toBe('number')
  })

  it('prints a table from the latest hub connect only when the debug flag is set', () => {
    vi.useFakeTimers()
    const table = vi.spyOn(console, 'table').mockImplementation(() => {})
    vi.spyOn(console, 'info').mockImplementation(() => {})
    mark('transport.start')
    mark('hub.connect')
    vi.advanceTimersByTime(5000)
    expect(table).not.toHaveBeenCalled()

    localStorage.setItem('anycon.debug.timing', '1')
    mark('hub.connect')
    mark('hub.auth_ok')
    vi.advanceTimersByTime(5000)
    expect(table).toHaveBeenCalledTimes(1)
    const rows = table.mock.calls[0]![0] as Array<{ step: string }>
    expect(rows.map((r) => r.step)).toEqual(['hub.connect', 'hub.auth_ok'])
  })

  it('keeps the browser performance timeline bounded on a long-lived page', async () => {
    performance.clearMarks()
    performance.clearMeasures()
    const ours = (): number =>
      [...performance.getEntriesByType('mark'), ...performance.getEntriesByType('measure')].filter((e) => e.name.startsWith('anycon:')).length
    for (let i = 0; i < 2000; i++) {
      mark('hub.connect')
      await timed('rpc node.info', async () => 1)
    }
    expect(ours()).toBeGreaterThan(0)
    expect(ours()).toBeLessThanOrEqual(300)
    expect(timingEntries()).toHaveLength(300)
  })
})
