import { describe, expect, it } from 'vitest'
import { mockAgent } from '@/test/factories'
import {
  agentDefaultMode,
  defaultThinking,
  reconcileThinking,
  resolveDefaults,
  resolveDefaultsDetailed,
  selectModel,
  selectionFromSettings,
  applyUpdate,
  buildSettingsUpdate,
  diffSettings,
  mergeUpdates,
  thinkingChoices,
  toSessionSettings,
} from './resolve'

const agent = mockAgent()

describe('resolveDefaults', () => {
  it('falls back to the agent defaults when nothing is stored', () => {
    expect(resolveDefaults(agent, [])).toEqual({ model: 'mock-large', thinking: 'high', mode: 'ask', options: { verbose: false } })
  })

  it('prefers last-used over node defaults over agent defaults', () => {
    const sel = resolveDefaults(agent, [{ model: 'mock-large', thinking: 'low' }, { model: 'mock-small', mode: 'plan' }])
    expect(sel.model).toBe('mock-large')
    expect(sel.thinking).toBe('low')
    expect(sel.mode).toBe('plan') // 上次用的没记模式，落到节点默认
  })

  it('uses node defaults when last-used is absent', () => {
    expect(resolveDefaults(agent, [null, { model: 'mock-small', mode: 'plan' }])).toMatchObject({ model: 'mock-small', thinking: null, mode: 'plan' })
  })

  it('drops stored values that are no longer valid for the agent', () => {
    const sel = resolveDefaults(agent, [{ model: 'gone', thinking: 'xhigh', mode: 'nope' }])
    expect(sel).toMatchObject({ model: 'mock-large', thinking: 'high', mode: 'ask' })
  })

  it('ignores a stored thinking level that does not belong to the chosen model', () => {
    expect(resolveDefaults(agent, [{ model: 'mock-small', thinking: 'high' }]).thinking).toBeNull()
  })

  it('never auto-selects a dangerous mode from stored values', () => {
    expect(resolveDefaults(agent, [{ mode: 'yolo' }, { mode: 'yolo' }]).mode).toBe('ask')
  })

  describe('never auto-selects a dangerous mode, whatever the source', () => {
    const dangerousDefault = mockAgent({
      modes: [
        { id: 'ask', label: 'Ask', risk: 'normal' },
        { id: 'plan', label: 'Plan', risk: 'safe' },
        // daemon 把节点配置的 default_mode 标成 default: true，它可能就是危险模式
        { id: 'yolo', label: 'YOLO', default: true, risk: 'dangerous' },
      ],
    })

    it('ignores a dangerous mode flagged as the default (node default mirrored by the daemon) and falls back to the first non-dangerous mode', () => {
      expect(agentDefaultMode(dangerousDefault)?.id).toBe('ask')
      expect(resolveDefaults(dangerousDefault, []).mode).toBe('ask')
    })

    it('prefers a flagged non-dangerous default over the first one', () => {
      const a = mockAgent({ modes: [{ id: 'ask', label: 'Ask', risk: 'normal' }, { id: 'plan', label: 'Plan', default: true, risk: 'safe' }, { id: 'yolo', label: 'YOLO', risk: 'dangerous' }] })
      expect(resolveDefaults(a, []).mode).toBe('plan')
    })

    it('ignores dangerous last-used and node-default modes, and picks the next safe source', () => {
      expect(resolveDefaults(dangerousDefault, [{ mode: 'yolo' }, { mode: 'plan' }]).mode).toBe('plan')
      expect(resolveDefaults(dangerousDefault, [{ mode: 'yolo' }, { mode: 'yolo' }]).mode).toBe('ask')
    })

    it('selects no mode at all when the agent only has dangerous modes', () => {
      const a = mockAgent({ modes: [{ id: 'yolo', label: 'YOLO', default: true, risk: 'dangerous' }] })
      expect(resolveDefaults(a, [{ mode: 'yolo' }]).mode).toBeNull()
    })

    it('reports which dangerous mode was skipped so the UI can show a hint', () => {
      expect(resolveDefaultsDetailed(dangerousDefault, []).skippedDangerousMode?.id).toBe('yolo')
      expect(resolveDefaultsDetailed(dangerousDefault, [{ mode: 'yolo' }, { mode: 'plan' }])).toMatchObject({ selection: { mode: 'plan' }, skippedDangerousMode: { id: 'yolo' } })
      // 更高优先级的来源已经是安全模式：没有被跳过的东西
      expect(resolveDefaultsDetailed(dangerousDefault, [{ mode: 'plan' }, { mode: 'yolo' }]).skippedDangerousMode).toBeNull()
      expect(resolveDefaultsDetailed(mockAgent(), []).skippedDangerousMode).toBeNull()
    })

    it('a session that really runs in a dangerous mode still shows it (that is state, not a default)', () => {
      expect(selectionFromSettings(dangerousDefault, { model: 'mock-large', mode: 'yolo' }).mode).toBe('yolo')
      expect(selectionFromSettings(dangerousDefault, { model: 'mock-large' }).mode).toBe('ask')
    })
  })

  it('accepts a custom model id only when the agent allows it', () => {
    expect(resolveDefaults(agent, [{ model: 'my-model' }]).model).toBe('mock-large')
    expect(resolveDefaults(mockAgent({ allow_custom_model: true }), [{ model: 'my-model' }]).model).toBe('my-model')
  })

  it('ignores a ConfigOption.default (unknown on the wire) that does not fit the option kind', () => {
    const odd = mockAgent({
      options: [
        { id: 'verbose', kind: 'bool', label: 'Verbose', default: 'yes' },
        { id: 'level', kind: 'select', label: 'Level', choices: [{ id: 'lo', label: 'Lo' }, { id: 'hi', label: 'Hi' }], default: 'nope' },
        { id: 'note', kind: 'text', label: 'Note', default: { not: 'a string' } },
        { id: 'ok', kind: 'text', label: 'Ok', default: 'x' },
      ],
    })
    expect(resolveDefaults(odd, []).options).toEqual({ verbose: false, level: 'lo', note: '', ok: 'x' })
  })

  it('layers option values: agent default < node default < last used, validating types', () => {
    const sel = resolveDefaults(agent, [{ options: { verbose: 'yes' } }, { options: { verbose: true } }])
    expect(sel.options).toEqual({ verbose: true })
  })
})

describe('thinking reconcile', () => {
  it('keeps the level if the new model still offers it', () => {
    expect(reconcileThinking(agent, 'mock-large', 'low')).toEqual({ thinking: 'low', adjusted: false })
  })
  it('falls back to the new model default when the level is not offered', () => {
    const a = mockAgent({
      models: [
        { id: 'a', label: 'A', default: true, thinking: { options: [{ id: 'low', label: 'low' }, { id: 'max', label: 'max' }], default: 'low' } },
        { id: 'b', label: 'B', thinking: { options: [{ id: 'medium', label: 'medium' }, { id: 'high', label: 'high' }], default: 'medium' } },
      ],
    })
    expect(reconcileThinking(a, 'b', 'max')).toEqual({ thinking: 'medium', adjusted: true })
  })
  it('clears thinking for a model without thinking', () => {
    expect(reconcileThinking(agent, 'mock-small', 'high')).toEqual({ thinking: null, adjusted: true })
    expect(thinkingChoices(agent, 'mock-small')).toEqual([])
  })
  it('selectModel returns the adjusted selection', () => {
    const sel = resolveDefaults(agent, [])
    const { selection, thinkingAdjusted } = selectModel(agent, sel, 'mock-small')
    expect(selection).toMatchObject({ model: 'mock-small', thinking: null })
    expect(thinkingAdjusted).toBe(false) // 清空不算「调整为某个值」
    const back = selectModel(agent, selection, 'mock-large')
    expect(back.selection.thinking).toBe('high')
  })
  it('uses the first option when the model default is missing', () => {
    const a = mockAgent({ models: [{ id: 'a', label: 'A', default: true, thinking: { options: [{ id: 'x', label: 'x' }] } }] })
    expect(defaultThinking(a, 'a')).toBe('x')
  })
})

describe('settings conversion', () => {
  it('omits empty fields in session settings', () => {
    expect(toSessionSettings({ model: 'mock-small', thinking: null, mode: null, options: {} })).toEqual({ model: 'mock-small' })
  })
  it('does not send a model when none is selected (an agent whose model list could not be fetched)', () => {
    expect(toSessionSettings({ model: '', thinking: null, mode: 'workspace', options: {} })).toEqual({ mode: 'workspace' })
  })
  it('builds a patch with only changed fields', () => {
    const a = resolveDefaults(agent, [])
    const b = { ...a, mode: 'plan', options: { verbose: true } }
    expect(buildSettingsUpdate(a, b)).toEqual({ settings: { mode: 'plan', options: { verbose: true } }, clear: [] })
  })
  it('restores a selection from session settings', () => {
    expect(selectionFromSettings(agent, { model: 'mock-small', mode: 'plan' })).toMatchObject({ model: 'mock-small', thinking: null, mode: 'plan' })
  })
})

describe('clearing settings (session.set_settings.clear)', () => {
  it('names a field in `clear` — and drops it from `settings` — when it goes from a value to empty', () => {
    expect(diffSettings({ system_prompt_append: 'be terse', max_turns: 5, max_budget_usd: 2 }, {})).toEqual({
      settings: {},
      clear: ['system_prompt_append', 'max_turns', 'max_budget_usd'],
    })
    // 空串和 null 都算清空
    expect(diffSettings({ system_prompt_append: 'x', max_turns: 3 }, { system_prompt_append: '', max_turns: null })).toEqual({ settings: {}, clear: ['system_prompt_append', 'max_turns'] })
  })

  it('sends changed values in `settings` and leaves unchanged / still-empty fields alone', () => {
    expect(diffSettings({ max_turns: 3, system_prompt_append: 'a' }, { max_turns: 4, system_prompt_append: 'a' })).toEqual({ settings: { max_turns: 4 }, clear: [] })
    expect(diffSettings({}, { max_budget_usd: 1.5 })).toEqual({ settings: { max_budget_usd: 1.5 }, clear: [] })
    expect(diffSettings({}, {})).toEqual({ settings: {}, clear: [] })
  })

  it('clears env when every variable is removed, and sends the new map otherwise', () => {
    expect(diffSettings({ env: { A: '1' } }, { env: {} })).toEqual({ settings: {}, clear: ['env'] })
    expect(diffSettings({ env: { A: '1' } }, {})).toEqual({ settings: {}, clear: ['env'] })
    expect(diffSettings({ env: { A: '1' } }, { env: { A: '1', B: '2' } })).toEqual({ settings: { env: { A: '1', B: '2' } }, clear: [] })
  })

  it('deletes an emptied option with null (the protocol’s delete-key form) instead of sending an empty string', () => {
    expect(diffSettings({ options: { note: 'x', verbose: true } }, { options: { verbose: true } })).toEqual({ settings: { options: { note: null } }, clear: [] })
    expect(diffSettings({ options: { note: 'x' } }, { options: { note: '' } })).toEqual({ settings: { options: { note: null } }, clear: [] })
    // false 是有效值，不是清空
    expect(diffSettings({ options: { verbose: true } }, { options: { verbose: false } })).toEqual({ settings: { options: { verbose: false } }, clear: [] })
  })

  it('selecting a model without thinking levels clears the old thinking level', () => {
    const a = resolveDefaults(agent, [])
    expect(a.thinking).toBe('high')
    expect(buildSettingsUpdate(a, selectModel(agent, a, 'mock-small').selection)).toEqual({ settings: { model: 'mock-small' }, clear: ['thinking'] })
  })

  it('queued updates merge: a later value cancels an earlier clear and a later clear cancels an earlier value', () => {
    const cleared = { settings: {}, clear: ['max_turns' as const] }
    expect(mergeUpdates(cleared, { settings: { max_turns: 9 }, clear: [] })).toEqual({ settings: { max_turns: 9 }, clear: [] })
    expect(mergeUpdates({ settings: { max_turns: 9, mode: 'plan' }, clear: [] }, cleared)).toEqual({ settings: { mode: 'plan' }, clear: ['max_turns'] })
    expect(mergeUpdates(null, cleared)).toBe(cleared)
    expect(mergeUpdates({ settings: { options: { a: 1 } }, clear: [] }, { settings: { options: { b: 2 } }, clear: [] }).settings.options).toEqual({ a: 1, b: 2 })
  })

  it('applyUpdate reflects queued changes in the controls', () => {
    const a = resolveDefaults(agent, [])
    expect(applyUpdate(a, { settings: { mode: 'plan', options: { verbose: true } }, clear: ['thinking'] })).toMatchObject({ mode: 'plan', thinking: null, options: { verbose: true } })
  })
})
