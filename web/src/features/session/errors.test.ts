import { describe, expect, it } from 'vitest'
import i18n from '@/i18n'
import { errorExtras, errorSeverity, isExplainedError } from './errors'
import { formatResetTime } from './notice'
import type { ErrorBlock } from './reducer'

describe('errorSeverity', () => {
  it('grades the known notice / warning codes and treats everything else as an error', () => {
    expect(errorSeverity('agent_reconnecting')).toBe('notice')
    expect(errorSeverity('agent_context_reset')).toBe('notice')
    expect(errorSeverity('unsupported_request')).toBe('warning')
    for (const code of ['context_window_exceeded', 'sandbox_error', 'policy_violation', 'usage_limit', 'rate_limited', 'overloaded', 'budget_exceeded', 'auth_failed', 'attachment_failed', 'agent_error', 'agent_exited', '']) {
      expect(errorSeverity(code)).toBe('error')
    }
  })

  it('does not look codes up on the object prototype (a code comes from the adapter)', () => {
    for (const code of ['constructor', '__proto__', 'toString', 'hasOwnProperty', 'valueOf']) {
      expect(errorSeverity(code)).toBe('error')
      expect(isExplainedError(code)).toBe(false)
    }
  })

  it('knows which codes have localized copy', () => {
    for (const code of ['agent_reconnecting', 'agent_context_reset', 'unsupported_request', 'context_window_exceeded', 'sandbox_error', 'policy_violation', 'usage_limit', 'rate_limited', 'overloaded', 'budget_exceeded', 'auth_failed', 'attachment_failed', 'agent_config', 'model_not_found']) {
      expect(isExplainedError(code)).toBe(true)
    }
    expect(isExplainedError('agent_error')).toBe(false)
  })
})

describe('errorExtras', () => {
  const block = (extra: Partial<ErrorBlock>): ErrorBlock => ({ key: 'e1', seq: 1, ts: 0, type: 'error', code: 'usage_limit', message: 'm', recoverable: true, ...extra }) as ErrorBlock
  const t = i18n.getFixedT('en')

  it('says nothing extra when the agent gave neither a reset time nor a scope', () => {
    expect(errorExtras(t, 'en', block({}))).toEqual({})
  })

  it('writes the reset time for any code, and the model hint only for a per-model usage_limit', () => {
    const at = Date.UTC(2100, 0, 5, 12, 0, 0)
    const now = Date.UTC(2100, 0, 5, 1, 0, 0)
    expect(errorExtras(t, 'en', block({ resetsAtMs: at }), now).resets).toBe(`Resets ${formatResetTime(at, 'en', now)}.`)
    expect(errorExtras(t, 'en', block({ resetsAtMs: at, limitScope: 'account' }), now).modelHint).toBeUndefined()
    expect(errorExtras(t, 'en', block({ limitScope: 'model' })).modelHint).toMatch(/another model/)
    // 范围只对 usage_limit 有意义
    expect(errorExtras(t, 'en', block({ code: 'rate_limited', limitScope: 'model' })).modelHint).toBeUndefined()
    expect(errorExtras(t, 'en', block({ code: 'credits_depleted', resetsAtMs: at }), now).resets).toContain('Resets')
  })
})

