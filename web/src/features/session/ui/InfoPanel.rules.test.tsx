// 信息面板里的「交给 agent 的规则」（docs/spec/daemon.md § 23.5）：会话创建时写进系统提示的规则，只读、纯文字；没有注入就没有这一项。
import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { injectedRulesOf, injectedRuleCount, RULES_HEADER } from '@/features/rules/injected'
import { InfoPanel, type InfoPanelProps } from './InfoPanel'

const props = (over: Partial<InfoPanelProps> = {}): InfoPanelProps => ({
  agentLabel: 'Claude Code',
  status: 'idle',
  subagents: [],
  createdAt: 1,
  usage: { turns: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: null },
  pending: [],
  onFocusPending: () => {},
  archived: false,
  onRename: () => {},
  onArchive: () => {},
  onDelete: () => {},
  onExport: () => {},
  onRestart: () => {},
  ...over,
})

const RULES = `${RULES_HEADER}\n- Write the failing test first.\n- Ask first.\n  <b>Explain why</b>.`

describe('the rules a session was given', () => {
  it('shows them as plain text with their count, under their own heading', () => {
    render(<InfoPanel {...props({ injectedRules: { text: RULES, verdict: 'ok' } })} />)
    const field = screen.getByTestId('info-rules')
    expect(field).toHaveTextContent('Rules given to the agent')
    expect(field).toHaveTextContent('(2)')
    const text = screen.getByTestId('info-rules-text')
    expect(text.textContent).toBe(RULES)
    expect(text.querySelector('b')).toBeNull()
  })

  it('says it could not confirm the rules when they do not match what the node recorded, and does not call them the created-time rules', () => {
    render(<InfoPanel {...props({ injectedRules: { text: RULES, verdict: 'mismatch' } })} />)
    expect(screen.getByTestId('info-rules-unconfirmed')).toHaveTextContent('Could not confirm')
    expect(screen.getByTestId('info-rules-text').textContent).toBe(RULES)
  })

  it('while the check is still running it only says where the text was found', () => {
    render(<InfoPanel {...props({ injectedRules: { text: RULES, verdict: 'checking' } })} />)
    expect(screen.queryByTestId('info-rules-unconfirmed')).toBeNull()
    expect(screen.getByTestId('info-rules')).toHaveTextContent('Found at the start of this session’s system prompt')
    expect(screen.getByTestId('info-rules')).not.toHaveTextContent('when the session was created')
  })

  it('has no such item when none were injected', () => {
    render(<InfoPanel {...props({ injectedRules: null })} />)
    expect(screen.queryByTestId('info-rules')).toBeNull()
  })
})

describe('finding the injected rules in system_prompt_append', () => {
  it('takes the rules at the start; the caller’s own text after the blank line is not part of them', () => {
    expect(injectedRulesOf(`${RULES}\n\nBe terse.`)).toBe(RULES)
    expect(injectedRulesOf(RULES)).toBe(RULES)
  })

  it('a blank line inside a rule is indented, so it does not end the rules early', () => {
    const multi = `${RULES_HEADER}\n- first paragraph\n  \n  second paragraph\n- next`
    expect(injectedRulesOf(`${multi}\n\ntail`)).toBe(multi)
  })

  it('is null for a prompt that does not start with the rules header (the caller’s own text, empty, missing)', () => {
    expect(injectedRulesOf('Be terse.')).toBeNull()
    expect(injectedRulesOf(`Be terse.\n\n${RULES}`)).toBeNull()
    expect(injectedRulesOf('')).toBeNull()
    expect(injectedRulesOf(undefined)).toBeNull()
    expect(injectedRulesOf(null)).toBeNull()
  })

  it('counts the rules by their list markers', () => {
    expect(injectedRuleCount(RULES)).toBe(2)
    expect(injectedRuleCount(`${RULES_HEADER}\n- a\n  - not a rule, indented`)).toBe(1)
  })
})
