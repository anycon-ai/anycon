// 分身范围表单：本机分身和一条分身连接（B 上给那个节点的分身开放的范围）共用，缺省什么都不开放。
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { NodeConfigResult, TwinGetResult, TwinSetParams } from '@/protocol'
import type { NodeEntry } from '@/stores/nodes'
import { rpc } from '@/transport/instance'
import { Button } from '@/ui/Button'
import { Input, Select } from '@/ui/Field'

export function ScopeForm({ node, value, busy, onSave, formId = `scope-${node.id}`, hint }: { node: NodeEntry; value: TwinGetResult; busy: boolean; onSave: (params: TwinSetParams) => Promise<void>; formId?: string; hint?: string }) {
  const { t } = useTranslation()
  const [projects, setProjects] = useState(value.scope.projects)
  const [agent, setAgent] = useState(value.scope.agent ?? '')
  const [model, setModel] = useState(value.scope.model ?? '')
  const [mode, setMode] = useState(value.scope.mode ?? '')
  const [max, setMax] = useState(value.scope.max_sessions)
  const [daily, setDaily] = useState(value.scope.daily_dispatches)
  const [cap, setCap] = useState<number | null>(null)
  const [capFailed, setCapFailed] = useState(false)
  const [retry, setRetry] = useState(0)
  const ready = node.conn === 'ready'
  useEffect(() => {
    if (!ready) return
    let live = true
    void rpc()
      .request<NodeConfigResult>(node.id, 'node.get_config', {})
      .then(
        (r) => {
          if (!live) return
          const n = r.items.find((x) => x.key === 'sessions.max_concurrent')?.value
          if (typeof n === 'number') {
            setCap(Math.max(0, n - 1))
            setCapFailed(false)
          } else setCapFailed(true)
        },
        () => {
          if (live) setCapFailed(true)
        },
      )
    return () => {
      live = false
    }
  }, [node.id, ready, retry])
  const selected = node.agents.find((a) => a.id === agent)
  const modes = (selected?.modes ?? []).filter((m) => m.risk !== 'dangerous')
  const unattended = modes.find((m) => m.unattended)
  const valid = Number.isInteger(max) && max >= 1 && cap !== null && max <= cap && Number.isInteger(daily) && daily >= 0 && (!mode || modes.some((m) => m.id === mode))
  return (
    <form
      id={formId}
      className="space-y-3"
      onSubmit={(e) => {
        e.preventDefault()
        if (valid)
          void onSave({
            projects,
            ...(agent ? { agent } : {}),
            ...(model ? { model } : {}),
            ...(mode ? { mode } : {}),
            clear: [...(!agent ? ['agent' as const] : []), ...(!model ? ['model' as const] : []), ...(!mode ? ['mode' as const] : [])],
            max_sessions: max,
            daily_dispatches: daily,
          })
      }}
    >
      <h3 className="font-semibold">{t('twin.scope')}</h3>
      {hint ? <p className="text-sm text-fg-subtle">{hint}</p> : null}
      <fieldset disabled={!ready || busy} className="space-y-3">
        <legend className="text-sm">{t('twin.projects')}</legend>
        {[...new Set([...node.projects.map((p) => p.name), ...projects])].map((p) => (
          <label key={p} className="mr-4 inline-flex min-h-11 items-center gap-2">
            <input type="checkbox" checked={projects.includes(p)} onChange={(e) => setProjects((cur) => (e.target.checked ? [...cur, p] : cur.filter((x) => x !== p)))} />
            {p}
          </label>
        ))}
        <label className="block">
          {t('twin.agent')}
          <Select
            value={agent}
            onChange={(e) => {
              setAgent(e.target.value)
              setModel('')
              setMode('')
            }}
          >
            <option value="">{t('twin.default')}</option>
            {node.agents
              .filter((a) => a.enabled)
              .map((a) => (
                <option key={a.id} value={a.id}>
                  {a.label}
                </option>
              ))}
          </Select>
        </label>
        {!selected ? <p className="text-xs text-fg-subtle">{t('twin.chooseAgent')}</p> : null}
        <label className="block">
          {t('twin.model')}
          <Input disabled={!selected} list={`models-${formId}`} value={model} onChange={(e) => setModel(e.target.value)} placeholder={t('twin.default')} />
          <datalist id={`models-${formId}`}>
            {selected?.models?.map((m) => (
              <option key={m.id} value={m.id}>
                {m.label}
              </option>
            ))}
          </datalist>
        </label>
        <label className="block">
          {t('twin.mode')}
          <Select disabled={!selected} value={mode} onChange={(e) => setMode(e.target.value)}>
            <option value="">
              {t('twin.default')}
              {unattended ? ` · ${unattended.label} (${t('twin.unattended')})` : ''}
            </option>
            {modes.map((m) => (
              <option key={m.id} value={m.id}>
                {m.label}
                {m.unattended ? ` · ${t('twin.unattended')}` : ''}
              </option>
            ))}
          </Select>
        </label>
        <label className="block">
          {t('twin.maxSessions')}
          <Input type="number" min={1} max={cap ?? undefined} value={max} onChange={(e) => setMax(Number(e.target.value))} />
        </label>
        <p className="text-xs text-fg-subtle">{cap === null ? t('twin.capacityLoading') : cap < 1 ? t('twin.capacityZero') : t('twin.capacity', { max: cap })}</p>
        {capFailed ? (
          <p role="alert">
            {t('twin.capacityFailed')} <Button onClick={() => setRetry((n) => n + 1)}>{t('todos.retry')}</Button>
          </p>
        ) : null}
        <label className="block">
          {t('twin.dailyDispatches')}
          <Input type="number" min={0} value={daily} onChange={(e) => setDaily(Number(e.target.value))} />
        </label>
        <Button type="submit" disabled={!valid}>
          {t('twin.save')}
        </Button>
      </fieldset>
    </form>
  )
}
