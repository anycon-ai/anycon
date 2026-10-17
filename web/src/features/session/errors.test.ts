import { describe, expect, it } from 'vitest'
import { errorSeverity, isExplainedError } from './errors'

describe('errorSeverity', () => {
  it('grades the known notice / warning codes and treats everything else as an error', () => {
    expect(errorSeverity('agent_reconnecting')).toBe('notice')
    expect(errorSeverity('agent_context_reset')).toBe('notice')
    expect(errorSeverity('unsupported_request')).toBe('warning')
    for (const code of ['context_window_exceeded', 'sandbox_error', 'policy_violation', 'rate_limited', 'auth_failed', 'agent_error', 'agent_exited', '']) {
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
    for (const code of ['agent_reconnecting', 'agent_context_reset', 'unsupported_request', 'context_window_exceeded', 'sandbox_error', 'policy_violation', 'rate_limited', 'auth_failed']) {
      expect(isExplainedError(code)).toBe(true)
    }
    expect(isExplainedError('agent_error')).toBe(false)
  })
})
