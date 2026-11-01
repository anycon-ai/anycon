import { useCallback, useEffect, useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { TFunction } from 'i18next'
import type { ConfigItem, NodeConfigResult } from '@/protocol'
import { describeError } from '@/lib/errors'
import { warnOnError } from '@/lib/log'
import { rpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Input, Select } from '@/ui/Field'
import { Card, SectionTitle, Skeleton, Switch } from '@/ui/Misc'
import { envNames, formatLimitDuration, initialDrafts, isChanged, localProblem, sectionOf, toWire, type Draft, type LimitProblem } from './limits'

type Load = { status: 'loading' } | { status: 'ready'; items: ConfigItem[] } | { status: 'error'; error: unknown }

/** 这几个原因表示「节点上的状态和这个页面不一致了」（被环境变量接管、开关已被别处放宽……）：重新读一遍。 */
const STALE_REASONS = new Set(['env_override', 'tighten_only', 'not_editable'])

/**
 * 节点设置 →「限制」（docs/spec/web.md § 4.8，ADR-43）：`node.get_config` 列出能远程看的配置项，按它的类型和约束渲染；
 * 保存时只把改过的项放进一次 `node.set_config`，节点整体校验、整体写入，所以出错时一项都没改，错误按项显示在对应的输入框下面。
 * 安全相关的两个开关（`tighten_only`）关着时禁用，只能在机器上用 `anycon config set` 打开；被环境变量覆盖的项只读并说明原因。
 */
export function LimitsTab({ nodeId }: { nodeId: string }) {
  const { t } = useTranslation()
  const entry = useNodes((s) => s.byId[nodeId])
  const offline = entry?.conn !== 'ready'
  const [load, setLoad] = useState<Load>({ status: 'loading' })
  const [drafts, setDrafts] = useState<Record<string, Draft>>({})
  const [errors, setErrors] = useState<Record<string, string>>({})
  const [saving, setSaving] = useState(false)

  const items = useMemo(() => (load.status === 'ready' ? load.items : []), [load])

  const fetchItems = useCallback(
    async (keepDrafts: boolean): Promise<void> => {
      const res = await rpc().request<NodeConfigResult>(nodeId, 'node.get_config', {})
      setLoad({ status: 'ready', items: res.items })
      setDrafts((cur) => (keepDrafts ? { ...initialDrafts(res.items), ...cur } : initialDrafts(res.items)))
    },
    [nodeId],
  )

  useEffect(() => {
    if (offline) return
    let alive = true
    fetchItems(false).catch((error: unknown) => {
      console.warn('anycon: could not read the node config', error)
      if (alive) setLoad({ status: 'error', error })
    })
    return () => {
      alive = false
    }
  }, [offline, fetchItems])

  const edit = (itemKey: string, value: Draft): void => {
    setDrafts((d) => ({ ...d, [itemKey]: value }))
    setErrors((e) => {
      if (!(itemKey in e)) return e
      const rest = { ...e }
      delete rest[itemKey]
      return rest
    })
  }

  // 只看改过的、能改的项：没碰过的项就算当前值不在约束之内（手写的 config.toml）也不拦着保存别的
  const dirty = items.filter((i) => i.editable !== 'no' && i.key in drafts && isChanged(i, drafts[i.key]!))
  const problems: Record<string, LimitProblem> = {}
  for (const i of dirty) {
    const p = localProblem(i, drafts[i.key]!)
    if (p) problems[i.key] = p
  }
  const canSave = !offline && !saving && dirty.length > 0 && Object.keys(problems).length === 0

  const reset = (): void => {
    setDrafts(initialDrafts(items))
    setErrors({})
  }

  const save = async (): Promise<void> => {
    const changes = dirty.map((i) => ({ key: i.key, value: toWire(i, drafts[i.key]!) }))
    setSaving(true)
    setErrors({})
    try {
      const res = await rpc().request<NodeConfigResult>(nodeId, 'node.set_config', { changes })
      setLoad({ status: 'ready', items: res.items })
      setDrafts(initialDrafts(res.items))
      toast.success(t('nodeSettings.limits.saved'))
    } catch (e) {
      const rpcError = e instanceof RpcCallError && e.kind === 'rpc' && (e.code === 'invalid' || e.code === 'forbidden') ? e : null
      const bad = rpcError ? items.find((i) => i.key === rpcError.field) : undefined
      if (rpcError && bad) {
        setErrors({ [bad.key]: serverMessage(bad, rpcError.code, rpcError.reason, t) })
        // 节点上的状态变了（环境变量接管了这个键、开关被改回……）：刷新项目列表，保留用户填的
        if (rpcError.reason && STALE_REASONS.has(rpcError.reason)) fetchItems(true).catch(warnOnError('refresh the node config'))
      } else {
        toast.error(describeError(e, t))
      }
    } finally {
      setSaving(false)
    }
  }

  const sections = useMemo(() => {
    const order: string[] = []
    const bySection: Record<string, ConfigItem[]> = {}
    for (const i of items) {
      const s = sectionOf(i.key)
      if (!bySection[s]) {
        bySection[s] = []
        order.push(s)
      }
      bySection[s]!.push(i)
    }
    return order.map((s) => ({ name: s, items: bySection[s]! }))
  }, [items])

  return (
    <div className="space-y-6" data-testid="limits-tab">
      <p className="text-sm text-fg-muted">{t('nodeSettings.limits.hint')}</p>
      {offline ? (
        <p className="text-sm text-fg-subtle" data-testid="limits-offline">
          {t('nodeSettings.limits.offline')}
        </p>
      ) : null}
      {load.status === 'loading' && !offline ? <Skeleton className="h-40 w-full" /> : null}
      {load.status === 'error' && !offline ? (
        <p className="text-sm text-danger" data-testid="limits-error">
          {describeError(load.error, t)}
        </p>
      ) : null}

      {sections.map((sec) => (
        <section key={sec.name} data-testid={`limits-section-${sec.name}`}>
          <SectionTitle>{t(`nodeSettings.limits.sections.${sec.name}`, { defaultValue: sec.name })}</SectionTitle>
          <Card className="divide-y divide-border">
            {sec.items.map((item) => (
              <LimitRow
                key={item.key}
                item={item}
                draft={drafts[item.key] ?? ''}
                disabled={offline || saving}
                problem={problems[item.key] ?? null}
                serverError={errors[item.key]}
                onChange={(v) => edit(item.key, v)}
              />
            ))}
          </Card>
        </section>
      ))}

      {sections.length > 0 ? (
        <div className="space-y-2">
          {Object.keys(errors).length > 0 ? (
            <p className="text-sm text-danger" role="alert" data-testid="limits-not-saved">
              {t('nodeSettings.limits.notSaved')}
            </p>
          ) : null}
          <div className="flex gap-2">
            <Button variant="primary" onClick={() => void save()} disabled={!canSave} loading={saving} data-testid="limits-save">
              {t('common.save')}
            </Button>
            <Button variant="outline" onClick={reset} disabled={saving || dirty.length === 0} data-testid="limits-reset">
              {t('nodeSettings.limits.discard')}
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  )
}

function LimitRow({ item, draft, disabled, problem, serverError, onChange }: { item: ConfigItem; draft: Draft; disabled: boolean; problem: LimitProblem | null; serverError: string | undefined; onChange: (v: Draft) => void }) {
  const { t } = useTranslation()
  const id = `limit-${item.key}`
  const label = t(`nodeSettings.limits.keys.${item.key}.label`, { defaultValue: item.key })
  const hint = t(`nodeSettings.limits.keys.${item.key}.hint`, { defaultValue: '' })
  const readOnly = item.editable === 'no'
  // 只能收紧的开关：节点上已经关着就禁用（想打开要在机器上运行 `anycon config set`）
  const tightenLocked = item.editable === 'tighten_only' && item.value === false
  const off = disabled || readOnly || tightenLocked
  const shown = serverError ?? (problem ? problemMessage(item, problem, t) : undefined)
  const range = rangeHint(item, t)

  let control: React.ReactNode
  switch (item.kind) {
    case 'bool':
      control = <Switch id={id} testId={id} label={label} checked={draft === true} disabled={off} onCheckedChange={onChange} />
      break
    case 'enum': {
      const options = item.options ?? []
      // 节点上的值不在选项里（比如手写的 `INFO`）：照样显示，免得下拉框悄悄显示成别的值
      const all = options.includes(String(draft)) ? options : [String(draft), ...options]
      control = (
        <Select id={id} data-testid={id} aria-label={label} className="w-44" value={String(draft)} disabled={off} onChange={(e) => onChange(e.target.value)}>
          {all.map((o) => (
            <option key={o} value={o}>
              {t(`nodeSettings.limits.options.${item.key}.${o}`, { defaultValue: o })}
            </option>
          ))}
        </Select>
      )
      break
    }
    case 'int':
      control = <Input id={id} data-testid={id} aria-label={label} type="number" inputMode="numeric" step={1} min={item.min ?? undefined} max={item.max ?? undefined} className="w-28" value={String(draft)} disabled={off} aria-invalid={shown ? true : undefined} onChange={(e) => onChange(e.target.value)} />
      break
    default:
      control = <Input id={id} data-testid={id} aria-label={label} className={item.kind === 'duration' ? 'w-28' : 'w-44'} value={String(draft)} disabled={off} aria-invalid={shown ? true : undefined} onChange={(e) => onChange(e.target.value)} />
  }

  return (
    <div className="space-y-1.5 px-4 py-3" data-testid="limit-row" data-key={item.key}>
      <div className="flex items-center justify-between gap-4">
        <label htmlFor={id} className="min-w-0 text-sm font-medium">
          {label}
        </label>
        <div className="shrink-0">{control}</div>
      </div>
      {hint ? <p className="text-xs text-fg-subtle">{hint}</p> : null}
      {range ? <p className="text-xs text-fg-subtle">{range}</p> : null}
      {item.env_override ? (
        <p className="text-xs text-warning" data-testid={`limit-env-${item.key}`}>
          {t('nodeSettings.limits.envOverride', { name: envNames(item.key) })}
        </p>
      ) : null}
      {item.editable === 'tighten_only' && !item.env_override ? (
        <p className="text-xs text-fg-subtle" data-testid={`limit-tighten-${item.key}`}>
          {t('nodeSettings.limits.tightenOnly', { command: `anycon config set ${item.key} true` })}
        </p>
      ) : null}
      {shown ? (
        <p className="text-xs text-danger" role="alert" data-testid={`limit-error-${item.key}`}>
          {shown}
        </p>
      ) : null}
    </div>
  )
}

function rangeHint(item: ConfigItem, t: TFunction): string {
  if (item.min == null || item.max == null) return ''
  if (item.kind === 'int') return t('nodeSettings.limits.rangeInt', { min: item.min, max: item.max })
  if (item.kind === 'duration') return t('nodeSettings.limits.rangeDuration', { min: formatLimitDuration(item.min), max: formatLimitDuration(item.max) })
  return ''
}

/** 本地校验没过时的说明。 */
function problemMessage(item: ConfigItem, problem: LimitProblem, t: TFunction): string {
  if (problem === 'format') return t(item.kind === 'int' ? 'nodeSettings.limits.errors.notInt' : 'nodeSettings.limits.errors.format')
  if (item.kind === 'int') return t('nodeSettings.limits.errors.rangeInt', { min: item.min ?? 0, max: item.max ?? 0 })
  if (item.kind === 'duration') return t('nodeSettings.limits.errors.rangeDuration', { min: formatLimitDuration(item.min ?? 0), max: formatLimitDuration(item.max ?? 0) })
  return t('nodeSettings.limits.errors.rangeText', { max: item.max ?? 0 })
}

/** `node.set_config` 回的 `invalid` / `forbidden` 里 `data.reason` 对应的说明。 */
function serverMessage(item: ConfigItem, code: string | undefined, reason: string | undefined, t: TFunction): string {
  switch (reason) {
    case 'env_override':
      return t('nodeSettings.limits.errors.envOverride', { name: envNames(item.key) })
    case 'tighten_only':
      return t('nodeSettings.limits.errors.tightenOnly', { command: `anycon config set ${item.key} true` })
    case 'not_editable':
      return t('nodeSettings.limits.errors.notEditable')
    case 'range':
      return problemMessage(item, 'range', t)
    case 'type':
    case 'format':
      return problemMessage(item, 'format', t)
    default:
      return code === 'forbidden' ? t('nodeSettings.limits.errors.notEditable') : t('nodeSettings.limits.errors.format')
  }
}
