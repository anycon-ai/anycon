import { useCallback, useEffect, useRef, useState } from 'react'
import { ArrowUp, Check, ChevronRight, Folder, FolderOpen, GitBranch } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { LinkBlock, WorkspaceBrowseResult } from '@/protocol'
import { browseFolders } from '@/features/workspace/actions'
import { baseName, crumbsFor } from '@/features/workspace/paths'
import { describeError, describeErrorIn } from '@/lib/errors'
import { RpcCallError } from '@/transport/rpcClient'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Input, Select } from '@/ui/Field'
import { Card, Chip, Spinner, Switch } from '@/ui/Misc'

export interface FolderPickerProps {
  nodeId: string
  /** 已选的目录（绝对路径）。 */
  selected: string | null
  onSelect: (path: string) => void
  /** 第一次 `workspace.browse` 就返回 `forbidden`（`link_roots` 为空）：整个功能在这个节点上不可用。 */
  onUnavailable?: () => void
}

/**
 * 「已有目录」选择器（ADR-33）：由 `workspace.browse` 驱动。
 * 面包屑 / 上一级 / root 切换 / 子目录列表 / 显示隐藏目录 / 粘贴路径再前往；
 * 点目录名进入，点「选择」选中（`linkable = false` 的不给选择按钮，但能进入）。
 * 注意：它常被放在 `<form>` 里，所以自己不用 `<form>`（嵌套是非法的），路径框的回车自己处理。
 */
export function FolderPicker({ nodeId, selected, onSelect, onUnavailable }: FolderPickerProps) {
  const { t } = useTranslation()
  const [result, setResult] = useState<WorkspaceBrowseResult | null>(null)
  const [loading, setLoading] = useState(true)
  const [showHidden, setShowHidden] = useState(false)
  const [pathInput, setPathInput] = useState('')
  // 浏览中的失败（越界、不存在…）：留在当前目录，只显示一行错误
  const [error, setError] = useState<string | null>(null)
  // 受保护目录（拒绝表 / anycon 数据目录）被 daemon 拒绝进入：这是预期内的，不是故障，样式不一样
  const [errorKind, setErrorKind] = useState<'error' | 'protected'>('error')
  // 点进去被拒绝过的目录：协议里没有「能不能进入」的标记（拒绝表目录本身进不去，只是包含它们的上级可以进），
  // 所以先让用户点一次，被拒之后这一行就不再提供「打开」
  const [refused, setRefused] = useState<ReadonlySet<string>>(() => new Set())
  const resultRef = useRef<WorkspaceBrowseResult | null>(null)
  // 第一次就失败：没有可显示的目录
  const [unavailable, setUnavailable] = useState(false)
  const [loadError, setLoadError] = useState<string | null>(null)
  const seq = useRef(0)
  const crumbsRef = useRef<HTMLElement>(null)
  const unavailableCb = useRef(onUnavailable)
  useEffect(() => {
    unavailableCb.current = onUnavailable
  })

  const load = useCallback(
    async (path: string | undefined, hidden: boolean): Promise<void> => {
      const n = ++seq.current
      setLoading(true)
      try {
        const res = await browseFolders(nodeId, path, hidden)
        if (n !== seq.current) return
        resultRef.current = res
        setResult(res)
        setPathInput(res.path)
        setError(null)
        setLoadError(null)
        setUnavailable(false)
      } catch (e) {
        if (n !== seq.current) return
        const forbidden = e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'forbidden'
        if (path === undefined && forbidden) {
          // 不带路径的 browse 被拒：link_roots 是空的，这个功能整体不可用
          setUnavailable(true)
          resultRef.current = null
          setResult(null)
          unavailableCb.current?.()
        } else {
          // 点的是列表里一个受保护的目录（系统 / 凭证 / anycon 数据目录）被拒：直接说明它是受保护的目录
          const hit = forbidden && path !== undefined ? resultRef.current?.entries.find((x) => x.path === path) : undefined
          if (hit && (hit.blocked === 'sensitive' || hit.blocked === 'data_dir')) {
            setRefused((prev) => new Set(prev).add(hit.path))
            setError(t(`folderPicker.errors.protected_${hit.blocked}`, { name: hit.name }))
            setErrorKind('protected')
          } else {
            setError(describeErrorIn(e, t, 'folderPicker.errors'))
            setErrorKind('error')
          }
          setLoadError(describeError(e, t))
        }
      } finally {
        if (n === seq.current) setLoading(false)
      }
    },
    [nodeId, t],
  )

  useEffect(() => {
    void load(undefined, false)
    return () => {
      seq.current += 1
    }
  }, [load])

  // 面包屑很长时（手机）保持最后一段可见
  useEffect(() => {
    const el = crumbsRef.current
    if (el) el.scrollLeft = el.scrollWidth
  }, [result?.path])

  if (unavailable) {
    return (
      <Card tone="warning" className="p-3" testId="folder-picker-unavailable">
        <div role="status" className="space-y-1 text-sm">
          <p className="font-medium">{t('folderPicker.unavailableTitle')}</p>
          <p className="text-fg-muted">{t('folderPicker.unavailableBody')}</p>
        </div>
      </Card>
    )
  }
  if (!result) {
    return loadError ? (
      <div className="space-y-2" data-testid="folder-picker-error">
        <p role="alert" className="text-sm text-danger">
          {loadError}
        </p>
        <Button variant="outline" size="sm" onClick={() => void load(undefined, showHidden)}>
          {t('folderPicker.retry')}
        </Button>
      </div>
    ) : (
      <div className="flex items-center gap-2 py-6 text-sm text-fg-subtle" data-testid="folder-picker-loading">
        <Spinner /> {t('folderPicker.loading')}
      </div>
    )
  }

  const go = (): void => {
    const p = pathInput.trim()
    if (p && !loading) void load(p, showHidden)
  }
  // 不能关联的原因（`blocked`）；daemon 以后加了新的原因也有兜底文案
  const blockedText = (b: LinkBlock): string => t(`folderPicker.blocked.${b}`, { defaultValue: t('folderPicker.cannotLinkHint') })
  const crumbs = crumbsFor(result.path, result.roots)
  const currentRoot = crumbs[0]?.path ?? result.path
  const currentSelected = selected !== null && selected === result.path

  return (
    <div className="space-y-3" role="group" aria-label={t('folderPicker.label')} data-testid="folder-picker">
      <div className="flex gap-2">
        <Input
          value={pathInput}
          onChange={(e) => setPathInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault()
              go()
            }
          }}
          aria-label={t('folderPicker.path')}
          placeholder={t('folderPicker.pathPlaceholder')}
          data-testid="folder-path"
          className="font-mono text-xs"
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
        />
        <Button variant="outline" onClick={go} disabled={!pathInput.trim() || loading} data-testid="folder-go">
          {t('folderPicker.go')}
        </Button>
      </div>

      <div className="flex items-center gap-1.5">
        <Button variant="outline" size="icon" aria-label={t('folderPicker.up')} title={t('folderPicker.up')} disabled={!result.parent || loading} onClick={() => result.parent && void load(result.parent, showHidden)} data-testid="folder-up">
          <ArrowUp className="size-4" />
        </Button>
        {result.roots.length > 1 ? (
          <Select
            aria-label={t('folderPicker.roots')}
            value={currentRoot}
            onChange={(e) => void load(e.target.value, showHidden)}
            disabled={loading}
            className="w-auto max-w-[45%] shrink-0 font-mono text-xs"
            data-testid="folder-roots"
          >
            {result.roots.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
            {result.roots.includes(currentRoot) ? null : <option value={currentRoot}>{currentRoot}</option>}
          </Select>
        ) : null}
        <nav ref={crumbsRef} aria-label={t('common.breadcrumb')} className="scroll-thin min-w-0 flex-1 overflow-x-auto" data-testid="folder-crumbs">
          <ol className="flex w-max items-center text-sm">
            {crumbs.map((c, i) => {
              const last = i === crumbs.length - 1
              return (
                <li key={c.path} className="flex items-center">
                  {i > 0 ? <ChevronRight className="size-3.5 shrink-0 text-fg-subtle" aria-hidden /> : null}
                  {last ? (
                    <span aria-current="page" className="px-1.5 py-1 font-medium" title={c.path}>
                      {c.label}
                    </span>
                  ) : (
                    <button type="button" className="rounded-md px-1.5 py-1 text-fg-muted hover:bg-bg-hover hover:text-fg disabled:opacity-50" onClick={() => void load(c.path, showHidden)} disabled={loading} title={c.path} data-testid="folder-crumb">
                      {c.label}
                    </button>
                  )}
                </li>
              )
            })}
          </ol>
        </nav>
      </div>

      <div className="flex items-center gap-2 rounded-md border border-border bg-bg-subtle px-3 py-2" data-testid="folder-current" data-path={result.path} data-linkable={result.linkable} data-blocked={result.blocked ?? ''} data-selected={currentSelected}>
        <FolderOpen className="size-4 shrink-0 text-accent-text" aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="text-xs text-fg-subtle">{t('folderPicker.current')}</p>
          <p className="truncate text-sm font-medium" title={result.path}>
            {baseName(result.path) || result.path}
          </p>
          {result.project ? <Chip tone="accent">{t('folderPicker.inProject', { name: result.project })}</Chip> : null}
          {result.blocked && !result.project ? (
            <p className="text-xs text-fg-subtle" data-testid="folder-current-blocked">
              {blockedText(result.blocked)}
            </p>
          ) : null}
        </div>
        <Button
          size="sm"
          variant={currentSelected ? 'primary' : 'outline'}
          disabled={!result.linkable}
          title={result.linkable ? undefined : result.blocked ? blockedText(result.blocked) : t('folderPicker.cannotLinkHint')}
          onClick={() => onSelect(result.path)}
          icon={currentSelected ? <Check className="size-4" /> : undefined}
          data-testid="folder-select-current"
        >
          {currentSelected ? t('folderPicker.selected') : t('folderPicker.selectCurrent')}
        </Button>
      </div>

      <div className="flex items-center justify-between gap-2">
        <label htmlFor="folder-hidden" className="text-sm">
          {t('folderPicker.showHidden')}
        </label>
        <Switch
          id="folder-hidden"
          label={t('folderPicker.showHidden')}
          checked={showHidden}
          disabled={loading}
          onCheckedChange={(v) => {
            setShowHidden(v)
            void load(result.path, v)
          }}
        />
      </div>

      <ul className={cn('scroll-thin max-h-64 divide-y divide-border overflow-y-auto rounded-md border border-border', loading && 'opacity-60')} aria-busy={loading} data-testid="folder-list">
        {result.entries.length === 0 ? (
          <li className="px-3 py-4 text-center text-sm text-fg-subtle" data-testid="folder-empty">
            {t('folderPicker.empty')}
          </li>
        ) : null}
        {result.entries.map((e) => {
          const isSelected = selected === e.path
          const noEnter = refused.has(e.path)
          return (
            <li key={e.path} className={cn('flex items-stretch', isSelected && 'bg-accent-subtle')} data-testid="folder-entry" data-name={e.name} data-path={e.path} data-linkable={e.linkable} data-blocked={e.blocked ?? ''} data-refused={noEnter} data-project={e.project ?? ''} data-selected={isSelected}>
              <button
                type="button"
                onClick={() => void load(e.path, showHidden)}
                disabled={loading || noEnter}
                className={cn('flex min-h-11 min-w-0 flex-1 items-center gap-2 px-3 py-2 text-left text-sm hover:bg-bg-hover disabled:hover:bg-transparent', !e.linkable && 'text-fg-muted', (e.project || noEnter) && 'opacity-60')}
                data-testid="folder-enter"
              >
                <Folder className="size-4 shrink-0 text-fg-subtle" aria-hidden />
                <span className="min-w-0 flex-1">
                  <span className="block truncate">{e.name}</span>
                  {e.blocked && !e.project ? (
                    <span className="block text-xs text-fg-subtle" data-testid="folder-blocked-reason">
                      {blockedText(e.blocked)}
                    </span>
                  ) : null}
                </span>
                {e.git ? (
                  <Chip icon={<GitBranch className="size-3" aria-hidden />} testId="folder-git">
                    {t('folderPicker.git')}
                  </Chip>
                ) : null}
                {e.project ? (
                  <Chip tone="accent" testId="folder-project">
                    {t('folderPicker.inProject', { name: e.project })}
                  </Chip>
                ) : !e.linkable && !e.blocked ? (
                  <Chip title={t('folderPicker.cannotLinkHint')} testId="folder-unlinkable">
                    {t('folderPicker.cannotLink')}
                  </Chip>
                ) : null}
                {noEnter ? null : <ChevronRight className="size-4 shrink-0 text-fg-subtle" aria-hidden />}
              </button>
              {e.linkable ? (
                <Button size="sm" variant={isSelected ? 'primary' : 'outline'} className="mx-1.5 min-h-9 self-center" aria-pressed={isSelected} aria-label={t('folderPicker.selectNamed', { name: e.name })} onClick={() => onSelect(e.path)} disabled={loading} icon={isSelected ? <Check className="size-4" /> : undefined} data-testid="folder-select">
                  {isSelected ? t('folderPicker.selected') : t('folderPicker.select')}
                </Button>
              ) : null}
            </li>
          )
        })}
      </ul>
      {result.truncated ? (
        <p className="text-xs text-fg-subtle" data-testid="folder-truncated">
          {t('folderPicker.truncated')}
        </p>
      ) : null}
      {error ? (
        errorKind === 'protected' ? (
          // 预期内的拒绝（受保护的目录）：说明原因，不用红色的错误样式
          <p role="status" className="text-sm text-fg-muted" data-testid="folder-error" data-kind="protected">
            {error}
          </p>
        ) : (
          <p role="alert" className="text-sm text-danger" data-testid="folder-error" data-kind="error">
            {error}
          </p>
        )
      ) : null}
    </div>
  )
}
