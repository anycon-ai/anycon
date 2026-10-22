// 模型 / 思考 / 权限模式的取值逻辑（spec/web.md § 5）。全部由 AgentInfo 驱动，不内置任何 agent 知识。
//
// 默认值优先级（§ 5.4）：本浏览器对 (节点, agent) 上次用的值 → 节点上该 agent 的默认选项 → agent 自己的默认。
import type { AgentInfo, Choice, ClearableSetting, ConfigOption, ModeInfo, ModelInfo, SessionSettings } from '@/protocol'

/** UI 里正在编辑的一组选项。thinking = null 表示所选模型不支持思考等级。 */
export interface Selection {
  model: string
  thinking: string | null
  mode: string | null
  options: Record<string, unknown>
}

/** 一份可能不完整 / 不再合法的候选值（来自 localStorage 或节点默认）。 */
export interface Candidate {
  model?: string | null
  thinking?: string | null
  mode?: string | null
  options?: Record<string, unknown> | null
}

export function findModel(agent: AgentInfo, id: string | null | undefined): ModelInfo | undefined {
  return id ? (agent.models ?? []).find((m) => m.id === id) : undefined
}

export function agentDefaultModel(agent: AgentInfo): ModelInfo | undefined {
  const models = agent.models ?? []
  return models.find((m) => m.default) ?? models[0]
}

/** agent 自己（或节点配置）标成默认的模式。可能是危险模式——自动选择时不能用它，见 `agentDefaultMode`。 */
export function flaggedDefaultMode(agent: AgentInfo): ModeInfo | undefined {
  return (agent.modes ?? []).find((m) => m.default)
}

/**
 * 自动选择用的默认模式：**永远不是危险模式**。
 * 带 default 标记的非危险模式优先，否则第一个非危险模式；全是危险模式就不选（让用户显式选并确认）。
 * 注意 daemon 会把节点配置的 `default_mode` 标成 `default: true`，那可能是危险模式。
 */
export function agentDefaultMode(agent: AgentInfo): ModeInfo | undefined {
  const safe = (agent.modes ?? []).filter((m) => m.risk !== 'dangerous')
  return safe.find((m) => m.default) ?? safe[0]
}

export function thinkingChoices(agent: AgentInfo, modelId: string | null | undefined): Choice[] {
  return findModel(agent, modelId)?.thinking?.options ?? []
}

export function isDangerousMode(agent: AgentInfo, modeId: string | null | undefined): boolean {
  return (agent.modes ?? []).find((m) => m.id === modeId)?.risk === 'dangerous'
}

export function isValidModel(agent: AgentInfo, id: string | null | undefined): id is string {
  if (!id) return false
  return findModel(agent, id) !== undefined || agent.allow_custom_model === true
}

/** 所选模型的默认思考等级；模型不支持思考时 null。 */
export function defaultThinking(agent: AgentInfo, modelId: string | null | undefined): string | null {
  const t = findModel(agent, modelId)?.thinking
  if (!t || t.options.length === 0) return null
  if (t.default && t.options.some((o) => o.id === t.default)) return t.default
  return t.options[0]!.id
}

/**
 * 切换模型后让思考等级落在新模型的列表里。
 * 原等级仍然合法就保留；否则回落到新模型默认值（`adjusted = true`，UI 提示「思考等级已调整」）；新模型不支持思考则清空。
 */
export function reconcileThinking(
  agent: AgentInfo,
  modelId: string,
  current: string | null | undefined,
): { thinking: string | null; adjusted: boolean } {
  const choices = thinkingChoices(agent, modelId)
  if (choices.length === 0) return { thinking: null, adjusted: current != null }
  if (current && choices.some((c) => c.id === current)) return { thinking: current, adjusted: false }
  return { thinking: defaultThinking(agent, modelId), adjusted: current != null }
}

/** `ConfigOption.default` 是 `unknown`：必须和选项的 kind 对得上才采用，对不上（daemon / agent 给了奇怪的值）就用该 kind 的兜底值。 */
function optionDefault(o: ConfigOption): unknown {
  if (o.default !== undefined && validOption(o, o.default)) return o.default
  if (o.kind === 'bool') return false
  if (o.kind === 'select') return o.choices?.[0]?.id ?? ''
  return ''
}

function validOption(o: ConfigOption, v: unknown): boolean {
  switch (o.kind) {
    case 'bool':
      return typeof v === 'boolean'
    case 'text':
      return typeof v === 'string'
    case 'select':
      return typeof v === 'string' && (o.choices ?? []).some((c) => c.id === v)
  }
}

/** 每个选项的 agent 默认值。 */
export function agentDefaultOptions(agent: AgentInfo): Record<string, unknown> {
  const out: Record<string, unknown> = {}
  for (const o of agent.options ?? []) out[o.id] = optionDefault(o)
  return out
}

export interface DefaultsResolution {
  selection: Selection
  /** 某个来源（上次用的 / 节点默认 / agent 默认标记）指向了危险模式，被跳过了：UI 据此给个提示。 */
  skippedDangerousMode: ModeInfo | null
}

/** 同 `resolveDefaults`，另外报告有没有因为危险模式而回落。 */
export function resolveDefaultsDetailed(agent: AgentInfo, candidates: Array<Candidate | null | undefined>): DefaultsResolution {
  const cs = candidates.filter((c): c is Candidate => !!c)
  const modes = agent.modes ?? []
  // 按优先级找第一个指向「存在的模式」的来源：它是危险模式就说明被跳过了（安全模式则它自己胜出）
  let skipped: ModeInfo | null = null
  for (const id of [...cs.map((c) => c.mode), flaggedDefaultMode(agent)?.id]) {
    const m = modes.find((x) => x.id === id)
    if (!m) continue
    if (m.risk === 'dangerous') skipped = m
    break
  }
  return { selection: resolveSelection(agent, cs), skippedDangerousMode: skipped }
}

/**
 * 解析新建会话的初始选项。`candidates` 从高到低优先（通常是 [lastUsed, nodeDefaults]），
 * 每一项都会按 AgentInfo 校验，不合法的字段跳到下一个来源，最后落到 agent 自己的默认。
 * **任何来源的危险模式都不会被自动选中**（上次用的、节点默认、agent 默认标记都一样）——
 * 危险模式必须每次显式选择并确认；回落到第一个非危险的默认模式。
 */
export function resolveDefaults(agent: AgentInfo, candidates: Array<Candidate | null | undefined>): Selection {
  return resolveSelection(agent, candidates.filter((c): c is Candidate => !!c))
}

function resolveSelection(agent: AgentInfo, cs: Candidate[]): Selection {
  const model = cs.map((c) => c.model).find((m) => isValidModel(agent, m)) ?? agentDefaultModel(agent)?.id ?? ''

  // 思考等级跟着最终选中的模型走：来自哪个来源都可以，只要对该模型合法。
  const choices = thinkingChoices(agent, model)
  let thinking: string | null = null
  if (choices.length > 0) {
    thinking = cs.map((c) => c.thinking).find((t) => t != null && choices.some((o) => o.id === t)) ?? defaultThinking(agent, model)
  }

  const modes = agent.modes ?? []
  const mode =
    cs
      .map((c) => c.mode)
      .find((m) => m != null && modes.some((x) => x.id === m && x.risk !== 'dangerous')) ??
    agentDefaultMode(agent)?.id ??
    null

  const options = agentDefaultOptions(agent)
  for (const c of [...cs].reverse()) {
    for (const o of agent.options ?? []) {
      const v = c.options?.[o.id]
      if (v !== undefined && validOption(o, v)) options[o.id] = v
    }
  }

  return { model, thinking, mode, options }
}

/** 把选择变成 `session.create` / `session.set_settings` 的 settings。 */
export function toSessionSettings(sel: Selection): SessionSettings {
  const s: SessionSettings = {}
  // 模型列表为空（比如 Codex 还没登录、拿不到 model/list）时没有可选的模型：不发 model，让 agent 用自己的默认
  if (sel.model) s.model = sel.model
  if (sel.thinking) s.thinking = sel.thinking
  if (sel.mode) s.mode = sel.mode
  if (Object.keys(sel.options).length > 0) s.options = sel.options
  return s
}

/** 从会话当前 settings 还原成 Selection（用于已有会话的控件）。缺项用 agent 默认补。 */
export function selectionFromSettings(agent: AgentInfo, s: SessionSettings | null | undefined): Selection {
  const base = resolveDefaults(agent, [])
  const model = s?.model ?? base.model
  const rec = reconcileThinking(agent, model, s?.thinking ?? null)
  return {
    model,
    thinking: s?.thinking && thinkingChoices(agent, model).some((c) => c.id === s.thinking) ? s.thinking : rec.thinking,
    mode: s?.mode ?? base.mode,
    options: { ...base.options, ...(s?.options ?? {}) },
  }
}

/** 选模型：返回新 Selection 及思考等级是否被调整。 */
export function selectModel(agent: AgentInfo, sel: Selection, modelId: string): { selection: Selection; thinkingAdjusted: boolean } {
  const { thinking, adjusted } = reconcileThinking(agent, modelId, sel.thinking)
  return { selection: { ...sel, model: modelId, thinking }, thinkingAdjusted: adjusted && thinking !== null }
}

/**
 * `session.set_settings` 的载荷：`settings` 是增量（缺省 = 保持），要把一个字段清回「未设置」必须在 `clear` 里点名——
 * 光把它从 `settings` 里省掉不会清掉旧值。
 */
export interface SettingsUpdate {
  settings: SessionSettings
  clear: ClearableSetting[]
}

export const NO_UPDATE: SettingsUpdate = { settings: {}, clear: [] }

export function isEmptyUpdate(u: SettingsUpdate | null | undefined): boolean {
  return !u || (u.clear.length === 0 && Object.keys(u.settings).length === 0)
}

const isBlank = (v: unknown): boolean => v === null || v === undefined || v === ''
const SCALARS = ['model', 'thinking', 'mode', 'system_prompt_append', 'max_turns', 'max_budget_usd'] as const satisfies readonly ClearableSetting[]

/**
 * 两份 settings 之间的差量。
 * - 新值非空且不同 → 放进 `settings`；
 * - 旧值有、新值空（null / 缺省 / 空串）→ 放进 `clear`，`settings` 里不再带它；
 * - `env` 全部删光 → `clear: ['env']`；`options` 里被清空的键发 `null`（协议规定的删键写法）。
 */
export function diffSettings(prev: SessionSettings, next: SessionSettings): SettingsUpdate {
  const settings: SessionSettings = {}
  const clear: ClearableSetting[] = []
  for (const k of SCALARS) {
    const a = prev[k]
    const b = next[k]
    if (isBlank(b)) {
      if (!isBlank(a)) clear.push(k)
    } else if (a !== b) {
      ;(settings as Record<string, unknown>)[k] = b
    }
  }
  const prevEnv = prev.env ?? {}
  const nextEnv = next.env ?? {}
  if (Object.keys(nextEnv).length === 0) {
    if (Object.keys(prevEnv).length > 0) clear.push('env')
  } else if (JSON.stringify(prevEnv) !== JSON.stringify(nextEnv)) {
    settings.env = nextEnv
  }
  const opts: Record<string, unknown> = {}
  for (const id of new Set([...Object.keys(prev.options ?? {}), ...Object.keys(next.options ?? {})])) {
    const a = prev.options?.[id]
    const b = next.options?.[id]
    if (isBlank(b)) {
      if (!isBlank(a)) opts[id] = null
    } else if (JSON.stringify(a) !== JSON.stringify(b)) {
      opts[id] = b
    }
  }
  if (Object.keys(opts).length > 0) settings.options = opts
  return { settings, clear }
}

/** 两份选择之间要发给 daemon 的更新（用户把某项改回「无」时带上 `clear`）。 */
export function buildSettingsUpdate(prev: Selection, next: Selection): SettingsUpdate {
  return diffSettings(toSessionSettings(prev), toSessionSettings(next))
}

/** 把后一个更新叠到前一个上（运行中排队的改动）：后来的设置覆盖先前的清除，反之亦然。 */
export function mergeUpdates(a: SettingsUpdate | null, b: SettingsUpdate): SettingsUpdate {
  if (!a) return b
  const settings: SessionSettings = { ...a.settings, ...b.settings }
  const options = { ...(a.settings.options ?? {}), ...(b.settings.options ?? {}) }
  if (Object.keys(options).length > 0) settings.options = options
  const clear = new Set<ClearableSetting>([...a.clear, ...b.clear])
  for (const k of clear) if (k in b.settings && !b.clear.includes(k)) clear.delete(k) // 后来又设了值
  for (const k of b.clear) delete (settings as Record<string, unknown>)[k] // 后来又清掉了
  return { settings, clear: [...clear] }
}

/** 把一个更新应用到界面上显示的选择（排队中的改动要立刻反映在控件上）。 */
export function applyUpdate(sel: Selection, u: SettingsUpdate): Selection {
  const out: Selection = { ...sel, options: { ...sel.options } }
  if (u.settings.model) out.model = u.settings.model
  if (u.settings.thinking) out.thinking = u.settings.thinking
  if (u.settings.mode) out.mode = u.settings.mode
  for (const [k, v] of Object.entries(u.settings.options ?? {})) {
    if (v === null) delete out.options[k]
    else out.options[k] = v
  }
  if (u.clear.includes('thinking')) out.thinking = null
  if (u.clear.includes('mode')) out.mode = null
  return out
}

export function selectionKey(nodeId: string, agentId: string): string {
  return `${nodeId}:${agentId}`
}

/** 选项控件上的一格：模型 / 思考 / 权限模式，其余设置都算在「更多」里。 */
export type SettingsField = 'model' | 'thinking' | 'mode' | 'more'

/** 界面上显示的选择里，还没生效（和会话实际设置不同）的那几格。 */
export function pendingFields(current: Selection, shown: Selection): ReadonlySet<SettingsField> {
  const d = buildSettingsUpdate(current, shown)
  const out = new Set<SettingsField>()
  for (const k of [...Object.keys(d.settings), ...d.clear]) out.add(k === 'model' || k === 'thinking' || k === 'mode' ? k : 'more')
  return out
}
