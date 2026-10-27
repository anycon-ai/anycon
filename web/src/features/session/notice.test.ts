import { beforeEach, describe, expect, it } from 'vitest'
import i18n from '@/i18n'
import type { NoticeBlock } from './reducer'
import { formatResetTime, isLocalizedNotice, noticeText, NOTICE_TEXT_KEYS } from './notice'

const block = (over: Partial<NoticeBlock>): NoticeBlock => ({ key: 'n', seq: 1, ts: 1, type: 'notice', code: 'agent_notice', level: 'info', message: '', ...over })
const text = (over: Partial<NoticeBlock>, label?: (id: string) => string) => noticeText(i18n.t, i18n.language, block(over), label, Date.UTC(2026, 9, 26, 12, 0, 0))

describe('noticeText', () => {
  beforeEach(async () => {
    await i18n.changeLanguage('en')
  })

  it('model_fallback names both models (through the agent’s model labels) and keeps the agent’s wording as the detail', () => {
    expect(text({ code: 'model_fallback', level: 'warning', message: 'Model A is overloaded', modelFrom: 'a', modelTo: 'b' }, (id) => `Model ${id.toUpperCase()}`)).toEqual({ summary: 'Model switched from Model A to Model B', detail: 'Model A is overloaded' })
    expect(text({ code: 'model_fallback', modelTo: 'b' }).summary).toBe('Model switched to b')
    expect(text({ code: 'model_fallback' }).summary).toBe('The model was switched')
  })

  it('rate_limit formats the reset time in the viewer’s locale; without one it just says the limit was reached', () => {
    const at = Date.UTC(2026, 9, 26, 12, 30, 0)
    expect(text({ code: 'rate_limit', resetsAtMs: at }).summary).toBe(`Usage limit reached — resets ${formatResetTime(at, 'en', Date.UTC(2026, 9, 26, 12, 0, 0))}`)
    expect(text({ code: 'rate_limit' }).summary).toBe('Usage limit reached')
  })

  it('has a localized line for every other known code and drops a detail that repeats it', () => {
    expect(text({ code: 'context_compacted', message: 'pre 120k → post 20k tokens' })).toEqual({ summary: 'Context was compacted', detail: 'pre 120k → post 20k tokens' })
    expect(text({ code: 'context_compacted', message: 'Context was compacted' })).toEqual({ summary: 'Context was compacted' })
    for (const code of Object.keys(NOTICE_TEXT_KEYS)) expect(text({ code, message: '' }).summary).not.toBe('')
    expect(text({ code: 'hook_failed', message: 'PreToolUse hook exited 2' })).toEqual({ summary: 'A hook failed or blocked the action', detail: 'PreToolUse hook exited 2' })
  })

  it('is written in the interface language', async () => {
    await i18n.changeLanguage('zh')
    expect(text({ code: 'model_fallback', modelFrom: 'a', modelTo: 'b' }).summary).toBe('模型已从 a 换成 b')
    expect(text({ code: 'context_compacted' }).summary).toBe('上下文已压缩')
  })

  it('shows the message of agent_notice and unknown codes as it is, folding long or multi-line ones', () => {
    expect(text({ code: 'agent_notice', message: ' Heads up ' })).toEqual({ summary: 'Heads up' })
    expect(text({ code: 'brand_new_code', level: 'warning', message: 'Something happened' })).toEqual({ summary: 'Something happened' })
    expect(text({ code: 'agent_notice', message: 'first\nsecond' })).toEqual({ summary: 'first', detail: 'first\nsecond' })
    const long = 'x'.repeat(300)
    const r = text({ code: 'agent_notice', message: long })
    expect(r.summary).toBe(`${'x'.repeat(160)}…`)
    expect(r.detail).toBe(long)
  })

  it('falls back to a generic line for an unknown code without a message', () => {
    expect(text({ code: 'brand_new_code', message: '  ' })).toEqual({ summary: 'Notice from the agent' })
  })

  it('does not treat inherited object keys as known codes', () => {
    for (const code of ['constructor', 'toString', '__proto__', 'hasOwnProperty']) expect(isLocalizedNotice(code)).toBe(false)
    expect(isLocalizedNotice('rate_limit')).toBe(true)
  })
})

describe('formatResetTime', () => {
  it('shows only the time for today and the date too for other days', () => {
    const now = new Date(2026, 9, 26, 12, 0, 0).getTime()
    const sameDay = new Date(2026, 9, 26, 15, 45, 0).getTime()
    const later = new Date(2026, 9, 28, 15, 45, 0).getTime()
    expect(formatResetTime(sameDay, 'en', now)).toBe(new Date(sameDay).toLocaleTimeString('en', { timeStyle: 'short' }))
    expect(formatResetTime(later, 'en', now)).toBe(new Date(later).toLocaleString('en', { dateStyle: 'medium', timeStyle: 'short' }))
    expect(formatResetTime(later, 'en', now)).not.toBe(formatResetTime(sameDay, 'en', now))
  })
})
