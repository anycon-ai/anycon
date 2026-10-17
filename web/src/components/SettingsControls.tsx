import { useState, type ReactNode } from 'react'
import { Popover } from 'radix-ui'
import { Brain, ChevronDown, Cpu, ShieldAlert, ShieldCheck, ShieldHalf, SlidersHorizontal } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AgentInfo, ConfigOption, ModeInfo } from '@/protocol'
import { findModel, selectModel, thinkingChoices, type Selection } from '@/features/settings/resolve'
import { usePrefs } from '@/stores/prefs'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Input, Select } from '@/ui/Field'
import { Menu, MenuContent, MenuItem, MenuRadioGroup, MenuRadioItem, MenuSeparator, MenuTrigger } from '@/ui/Menu'
import { Chip, Switch } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

const NO_MODELS: readonly string[] = []

export interface SettingsControlsProps {
  agent: AgentInfo
  value: Selection
  onChange: (next: Selection, info?: { thinkingAdjusted?: boolean }) => void
  /** 整体禁用（离线 / 运行中锁定）。 */
  disabled?: boolean
  disabledReason?: string
  /** daemon 拒绝过危险模式：这些项禁用并说明。 */
  dangerousForbidden?: boolean
  /** 记「输入过的自定义模型」的 key：`${nodeId}:${agentId}`。 */
  customKey: string
  /** data-testid 前缀：composer 用 `option`，信息面板用 `info`。 */
  idPrefix: string
  layout?: 'inline' | 'stacked'
  /** 调用方自己的控件（新建会话的项目、工作位置）：内联时放在左组最前面，堆叠时放在最上面。 */
  left?: ReactNode
  /** 调用方自己的控件（新建会话的 agent）：内联时放在右组最前面（模型之前），堆叠时紧跟 `left`。 */
  right?: ReactNode
}

/**
 * 选项按钮的样式：内联（输入框下面那一行）是无边框的文字按钮，堆叠（底部弹层、信息面板）是撑满一行的带边框按钮。
 * 新建会话的项目 / 工作位置 / agent 按钮也用它，好和模型、思考、权限排成一样的一行。
 */
export function optionTrigger(stacked: boolean): string {
  return stacked
    ? 'inline-flex w-full max-w-full items-center justify-between gap-1.5 rounded-md border border-border-strong bg-bg px-2.5 py-1.5 text-[0.8125rem] font-medium hover:bg-bg-hover disabled:opacity-50'
    : 'inline-flex max-w-full items-center gap-1.5 rounded-md px-2 py-1 text-[0.8125rem] font-medium text-fg-muted hover:bg-bg-hover hover:text-fg disabled:opacity-50 data-[state=open]:bg-bg-hover data-[state=open]:text-fg'
}

/**
 * 模型 / 思考 / 权限模式 / 更多选项。全部由 AgentInfo 驱动，不内置 agent 知识（spec/web.md § 5）。
 * 内联布局：左组 `[left][权限][更多]`，右组 `[right][模型][思考]` 靠右；堆叠布局从上到下排。
 */
export function SettingsControls({ agent, value, onChange, disabled, disabledReason, dangerousForbidden, customKey, idPrefix, layout = 'inline', left, right }: SettingsControlsProps) {
  const { t } = useTranslation()
  const [pendingDanger, setPendingDanger] = useState<ModeInfo | null>(null)
  const [customOpen, setCustomOpen] = useState(false)
  const [custom, setCustom] = useState('')
  const customModels = usePrefs((s) => s.customModels[customKey]) ?? NO_MODELS
  const addCustom = usePrefs((s) => s.addCustomModel)

  const models = agent.models ?? []
  const modes = agent.modes ?? []
  const model = findModel(agent, value.model)
  const choices = thinkingChoices(agent, value.model)
  const thinking = choices.find((c) => c.id === value.thinking)
  const mode = modes.find((m) => m.id === value.mode)
  const extra = agent.options ?? []
  const stacked = layout === 'stacked'
  const knownIds = new Set(models.map((m) => m.id))
  const customs = [...new Set([...customModels, ...(value.model && !knownIds.has(value.model) ? [value.model] : [])])]

  const pickModel = (id: string): void => {
    const { selection, thinkingAdjusted } = selectModel(agent, value, id)
    onChange(selection, { thinkingAdjusted })
  }
  const pickMode = (id: string): void => {
    const m = modes.find((x) => x.id === id)
    if (!m || id === value.mode) return
    if (m.risk === 'dangerous') setPendingDanger(m)
    else onChange({ ...value, mode: id })
  }

  const trigger = optionTrigger(stacked)
  const title = disabled ? disabledReason : undefined
  // 右组的菜单从右边对齐（贴着输入框右缘），堆叠时从左边
  const rightAlign = stacked ? 'start' : 'end'

  // 模型：只有一个模型且不允许自定义时不显示
  const modelEl =
    models.length > 1 || agent.allow_custom_model ? (
      <div className={cn(stacked && 'space-y-1')}>
        {stacked ? <p className="text-xs font-medium text-fg-subtle">{t('options.model')}</p> : null}
        <Menu>
          <MenuTrigger asChild>
            <button type="button" disabled={disabled} title={title} className={trigger} data-testid={`${idPrefix}-model`} data-value={value.model} aria-label={t('options.model')}>
              {stacked ? <Cpu className="size-3.5 shrink-0 text-fg-subtle" aria-hidden /> : null}
              <span className="truncate">{model?.label ?? (value.model || t('options.modelAgentDefault'))}</span>
              {model?.tags?.map((tag) => (
                <Chip key={tag}>{tag}</Chip>
              ))}
              <ChevronDown className="size-3.5 shrink-0 opacity-60" aria-hidden />
            </button>
          </MenuTrigger>
          <MenuContent className="w-72" align={rightAlign}>
            <MenuRadioGroup value={value.model} onValueChange={pickModel}>
              {models.map((m) => (
                <MenuRadioItem key={m.id} value={m.id} testId={`${idPrefix}-model-${m.id}`}>
                  <span className="flex flex-wrap items-center gap-1.5">
                    <span className="font-medium">{m.label}</span>
                    {m.default ? <Chip tone="accent">{t('options.default')}</Chip> : null}
                    {m.tags?.map((tag) => (
                      <Chip key={tag}>{tag}</Chip>
                    ))}
                  </span>
                  {m.description ? <span className="block text-xs text-fg-subtle">{m.description}</span> : null}
                </MenuRadioItem>
              ))}
              {customs.map((id) => (
                <MenuRadioItem key={id} value={id} testId={`${idPrefix}-model-${id}`}>
                  <span className="font-mono text-[0.8125rem]">{id}</span>
                  <span className="block text-xs text-fg-subtle">{t('options.customModel')}</span>
                </MenuRadioItem>
              ))}
            </MenuRadioGroup>
            {agent.allow_custom_model ? (
              <>
                <MenuSeparator />
                <MenuItem onSelect={() => setCustomOpen(true)} testId={`${idPrefix}-model-custom`}>
                  {t('options.customEllipsis')}
                </MenuItem>
              </>
            ) : null}
          </MenuContent>
        </Menu>
      </div>
    ) : null

  // 思考等级：下拉，只列所选模型的等级；模型不支持时整体隐藏
  const thinkingEl =
    choices.length > 0 ? (
      <div className={cn(stacked && 'space-y-1')}>
        {stacked ? <p className="text-xs font-medium text-fg-subtle">{t('options.thinking')}</p> : null}
        <Menu>
          <MenuTrigger asChild>
            <button type="button" disabled={disabled} title={title ?? thinking?.description ?? undefined} className={trigger} data-testid={`${idPrefix}-thinking`} data-value={value.thinking ?? ''} aria-label={t('options.thinking')}>
              <Brain className="size-3.5 shrink-0 text-fg-subtle" aria-hidden />
              <span className="truncate">{thinking ? thinking.label || thinking.id : (value.thinking ?? t('options.thinking'))}</span>
              <ChevronDown className="size-3.5 shrink-0 opacity-60" aria-hidden />
            </button>
          </MenuTrigger>
          <MenuContent className="w-60" align={rightAlign}>
            <MenuRadioGroup value={value.thinking ?? ''} onValueChange={(v) => onChange({ ...value, thinking: v })}>
              {choices.map((c) => (
                <MenuRadioItem key={c.id} value={c.id} testId={`${idPrefix}-thinking-${c.id}`}>
                  <span className="font-medium">{c.label || c.id}</span>
                  {c.description ? <span className="block text-xs text-fg-subtle">{c.description}</span> : null}
                </MenuRadioItem>
              ))}
            </MenuRadioGroup>
          </MenuContent>
        </Menu>
      </div>
    ) : null

  // 权限模式
  const modeEl =
    modes.length > 0 ? (
      <div className={cn(stacked && 'space-y-1')}>
        {stacked ? <p className="text-xs font-medium text-fg-subtle">{t('options.mode')}</p> : null}
        <Menu>
          <MenuTrigger asChild>
            <button
              type="button"
              disabled={disabled}
              title={title}
              className={cn(
                trigger,
                mode?.risk === 'dangerous' && (stacked ? 'border-danger bg-danger-subtle text-danger hover:bg-danger-subtle' : 'bg-danger-subtle text-danger hover:bg-danger-subtle hover:text-danger'),
                mode?.risk === 'safe' && (stacked ? 'border-success/60 text-success' : 'text-success hover:text-success'),
              )}
              data-testid={`${idPrefix}-mode`}
              data-value={value.mode ?? ''}
              data-risk={mode?.risk}
              aria-label={t('options.mode')}
            >
              <RiskIcon risk={mode?.risk} />
              <span className="truncate">{mode?.label ?? value.mode ?? t('options.mode')}</span>
              <ChevronDown className="size-3.5 shrink-0 opacity-60" aria-hidden />
            </button>
          </MenuTrigger>
          <MenuContent className="w-72">
            <MenuRadioGroup value={value.mode ?? ''} onValueChange={pickMode}>
              {modes.map((m) => {
                const forbidden = !!dangerousForbidden && m.risk === 'dangerous'
                return (
                  <MenuRadioItem key={m.id} value={m.id} disabled={forbidden} testId={`${idPrefix}-mode-${m.id}`}>
                    <span className="flex items-center gap-1.5">
                      <RiskIcon risk={m.risk} />
                      <span className={cn('font-medium', m.risk === 'dangerous' && 'text-danger')}>{m.label}</span>
                      {m.default ? <Chip tone="accent">{t('options.default')}</Chip> : null}
                    </span>
                    {m.description ? <span className="block text-xs text-fg-subtle">{m.description}</span> : null}
                    {forbidden ? <span className="block text-xs text-warning">{t('options.dangerForbidden')}</span> : null}
                  </MenuRadioItem>
                )
              })}
            </MenuRadioGroup>
          </MenuContent>
        </Menu>
      </div>
    ) : null

  // 其余 agent 特定选项：折叠进「更多」
  const moreEl =
    extra.length > 0 ? (
      <Popover.Root>
        <Popover.Trigger asChild>
          <button type="button" disabled={disabled} title={title} className={trigger} data-testid={`${idPrefix}-more`} aria-label={t('options.more')}>
            <SlidersHorizontal className="size-3.5 text-fg-subtle" aria-hidden />
            {t('options.more')}
          </button>
        </Popover.Trigger>
        <Popover.Portal>
          <Popover.Content align="start" sideOffset={6} collisionPadding={8} className="z-50 w-72 space-y-3 rounded-lg border border-border bg-surface p-3 shadow-pop">
            {extra.map((o) => (
              <OptionControl key={o.id} option={o} value={value.options[o.id]} onChange={(v) => onChange({ ...value, options: { ...value.options, [o.id]: v } })} />
            ))}
          </Popover.Content>
        </Popover.Portal>
      </Popover.Root>
    ) : null

  return (
    <div className={cn('flex', stacked ? 'flex-col items-stretch gap-2' : 'flex-wrap items-center gap-x-1 gap-y-1')} data-testid={`${idPrefix}-controls`}>
      {stacked ? (
        <>
          {left}
          {right}
          {modelEl}
          {thinkingEl}
          {modeEl}
          {moreEl}
        </>
      ) : (
        <>
          <div className="flex min-w-0 flex-wrap items-center gap-1">
            {left}
            {modeEl}
            {moreEl}
          </div>
          <div className="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-1">
            {right}
            {modelEl}
            {thinkingEl}
          </div>
        </>
      )}

      <Dialog open={customOpen} onOpenChange={setCustomOpen} title={t('options.customModelTitle')} closeLabel={t('common.close')}>
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault()
            const id = custom.trim()
            if (!id) return
            addCustom(customKey, id)
            pickModel(id)
            setCustom('')
            setCustomOpen(false)
          }}
        >
          <Input value={custom} onChange={(e) => setCustom(e.target.value)} placeholder="model-id" autoFocus autoCapitalize="off" spellCheck={false} aria-label={t('options.customModelTitle')} data-testid={`${idPrefix}-model-custom-input`} />
          <p className="text-xs text-fg-subtle">{t('options.customModelHint')}</p>
          <div className="flex justify-end">
            <Button type="submit" variant="primary" disabled={!custom.trim()}>
              {t('options.use')}
            </Button>
          </div>
        </form>
      </Dialog>

      <Dialog open={!!pendingDanger} onOpenChange={(o) => !o && setPendingDanger(null)} title={t('options.dangerTitle', { mode: pendingDanger?.label ?? '' })} closeLabel={t('common.close')} testId="danger-confirm">
        <div className="space-y-4">
          <div className="flex items-start gap-3 rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm text-danger">
            <ShieldAlert className="mt-0.5 size-5 shrink-0" aria-hidden />
            <p>{pendingDanger?.description || t('options.dangerBody')}</p>
          </div>
          <p className="text-sm text-fg-muted">{t('options.dangerHint')}</p>
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={() => setPendingDanger(null)}>
              {t('common.cancel')}
            </Button>
            <Button
              variant="danger"
              data-testid="danger-confirm-submit"
              onClick={() => {
                if (pendingDanger) onChange({ ...value, mode: pendingDanger.id })
                setPendingDanger(null)
              }}
            >
              {t('options.dangerConfirm')}
            </Button>
          </div>
        </div>
      </Dialog>
    </div>
  )
}

function RiskIcon({ risk }: { risk: ModeInfo['risk'] | undefined }) {
  const { t } = useTranslation()
  if (risk === 'dangerous') return <ShieldAlert className="size-3.5 shrink-0 text-danger" aria-label={t('options.risk.dangerous')} />
  if (risk === 'safe') return <ShieldCheck className="size-3.5 shrink-0 text-success" aria-label={t('options.risk.safe')} />
  return <ShieldHalf className="size-3.5 shrink-0 text-fg-subtle" aria-hidden />
}

function OptionControl({ option, value, onChange }: { option: ConfigOption; value: unknown; onChange: (v: unknown) => void }) {
  const id = `opt-${option.id}`
  const label = option.label
  if (option.kind === 'bool') {
    return (
      <div className="flex items-start justify-between gap-3" data-testid={`option-extra-${option.id}`}>
        <div className="min-w-0">
          <label htmlFor={id} className="text-sm font-medium">
            {label}
          </label>
          {option.description ? <p className="text-xs text-fg-subtle">{option.description}</p> : null}
        </div>
        <Switch id={id} label={label} checked={value === true} onCheckedChange={onChange} />
      </div>
    )
  }
  if (option.kind === 'select') {
    return (
      <div className="space-y-1" data-testid={`option-extra-${option.id}`}>
        <label htmlFor={id} className="text-sm font-medium">
          {label}
        </label>
        <Select id={id} value={typeof value === 'string' ? value : ''} onChange={(e) => onChange(e.target.value)}>
          {(option.choices ?? []).map((c) => (
            <option key={c.id} value={c.id}>
              {c.label}
            </option>
          ))}
        </Select>
        {option.description ? <p className="text-xs text-fg-subtle">{option.description}</p> : null}
      </div>
    )
  }
  return (
    <div className="space-y-1" data-testid={`option-extra-${option.id}`}>
      <label htmlFor={id} className="text-sm font-medium">
        {label}
      </label>
      <Input id={id} value={typeof value === 'string' ? value : ''} onChange={(e) => onChange(e.target.value)} />
      {option.description ? <p className="text-xs text-fg-subtle">{option.description}</p> : null}
    </div>
  )
}

