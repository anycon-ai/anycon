import { describe, expect, it } from 'vitest'
import type { ConfigItem } from '@/protocol'
import { envNames, formatLimitDuration, initialDrafts, isChanged, localProblem, parseDurationMs, sectionOf, toWire } from './limits'

const int = (over: Partial<ConfigItem> = {}): ConfigItem => ({ key: 'sessions.max_concurrent', value: 3, kind: 'int', editable: 'any', min: 1, max: 32, ...over })
const dur = (over: Partial<ConfigItem> = {}): ConfigItem => ({ key: 'preview.ticket_ttl', value: '24h', kind: 'duration', editable: 'any', min: 300_000, max: 604_800_000, ...over })

describe('node settings limits: helpers', () => {
  it('groups by the first segment of the key', () => {
    expect(sectionOf('sessions.max_concurrent')).toBe('sessions')
    expect(sectionOf('log.level')).toBe('log')
  })

  it('names the environment variable that would override a key', () => {
    expect(envNames('sessions.max_concurrent')).toBe('ANYCON_SESSIONS__MAX_CONCURRENT')
    expect(envNames('worktree.auto_archive_on_pr_close')).toBe('ANYCON_WORKTREE__AUTO_ARCHIVE_ON_PR_CLOSE')
    // 日志过滤还能被 ANYCON_LOG / RUST_LOG 接管
    expect(envNames('log.level')).toBe('ANYCON_LOG__LEVEL, ANYCON_LOG, RUST_LOG')
  })

  it('parses durations the way the daemon does', () => {
    expect(parseDurationMs('30s')).toBe(30_000)
    expect(parseDurationMs('20m')).toBe(1_200_000)
    expect(parseDurationMs(' 2h ')).toBe(7_200_000)
    expect(parseDurationMs('7d')).toBe(604_800_000)
    expect(parseDurationMs('1w')).toBe(604_800_000)
    expect(parseDurationMs('500ms')).toBe(500)
    expect(parseDurationMs('1.5h')).toBe(5_400_000)
    expect(parseDurationMs('90')).toBe(90_000)
    expect(parseDurationMs('0')).toBe(0)
    expect(parseDurationMs('')).toBeNull()
    expect(parseDurationMs('soon')).toBeNull()
    expect(parseDurationMs('5 parsecs')).toBeNull()
    expect(parseDurationMs('-5m')).toBeNull()
  })

  it('writes a duration range with the shortest whole unit', () => {
    expect(formatLimitDuration(0)).toBe('0')
    expect(formatLimitDuration(300_000)).toBe('5m')
    expect(formatLimitDuration(604_800_000)).toBe('7d')
    expect(formatLimitDuration(5_400_000)).toBe('90m')
    expect(formatLimitDuration(1500)).toBe('1500ms')
  })

  it('turns node values into drafts and back into wire values', () => {
    const items: ConfigItem[] = [int(), dur(), { key: 'preview.enabled', value: true, kind: 'bool', editable: 'tighten_only' }, { key: 'log.level', value: 'info', kind: 'enum', editable: 'any', options: ['info', 'warn'] }]
    expect(initialDrafts(items)).toEqual({ 'sessions.max_concurrent': '3', 'preview.ticket_ttl': '24h', 'preview.enabled': true, 'log.level': 'info' })
    expect(toWire(int(), '8')).toBe(8)
    expect(toWire(int(), ' 8 ')).toBe(8)
    expect(toWire(int(), '8.5')).toBeUndefined()
    expect(toWire(int(), 'abc')).toBeUndefined()
    expect(toWire(int(), '')).toBeUndefined()
    expect(toWire(dur(), ' 45m ')).toBe('45m')
    expect(toWire(items[2]!, false)).toBe(false)
  })

  it('knows what is changed: only a different value counts, and a value that cannot be sent counts too', () => {
    expect(isChanged(int(), '3')).toBe(false)
    expect(isChanged(int(), '4')).toBe(true)
    expect(isChanged(int(), 'abc')).toBe(true)
    expect(isChanged(dur(), '24h')).toBe(false)
    expect(isChanged(dur(), ' 24h ')).toBe(false)
    expect(isChanged(dur(), '23h')).toBe(true)
    expect(isChanged({ key: 'preview.enabled', value: true, kind: 'bool', editable: 'any' }, false)).toBe(true)
  })

  it('finds the problems it can see without asking the node', () => {
    expect(localProblem(int(), '8')).toBeNull()
    expect(localProblem(int(), '0')).toBe('range')
    expect(localProblem(int(), '33')).toBe('range')
    expect(localProblem(int(), '2.5')).toBe('format')
    expect(localProblem(dur(), '90m')).toBeNull()
    expect(localProblem(dur(), '1m')).toBe('range')
    expect(localProblem(dur(), '8d')).toBe('range')
    expect(localProblem(dur(), 'soon')).toBe('format')
    // 0 在有「不限」含义的键上是合法的：下限由节点给
    expect(localProblem(dur({ min: 0 }), '0')).toBeNull()
    expect(localProblem({ key: 'worktree.branch_prefix', value: 'anycon', kind: 'text', editable: 'any', max: 40 }, 'a'.repeat(41))).toBe('range')
    expect(localProblem({ key: 'worktree.branch_prefix', value: 'anycon', kind: 'text', editable: 'any', max: 40 }, 'team')).toBeNull()
    const en: ConfigItem = { key: 'log.level', value: 'INFO', kind: 'enum', editable: 'any', options: ['info', 'warn'] }
    expect(localProblem(en, 'warn')).toBeNull()
    expect(localProblem(en, 'INFO')).toBeNull() // 节点上手写的值，没动就不算问题
    expect(localProblem(en, 'loud')).toBe('format')
  })
})
