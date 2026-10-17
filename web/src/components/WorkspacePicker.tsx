import { Popover } from 'radix-ui'
import { ChevronDown, Folder, GitBranch } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { SessionWorkspaceMode, WorkspaceBranchesResult } from '@/protocol'
import { branchNameProblem, branchPlaceholder } from '@/features/worktree/branch'
import type { BranchesState } from '@/features/worktree/useBranches'
import { cn } from '@/ui/cn'
import { Input, Select } from '@/ui/Field'
import { Segmented, Spinner } from '@/ui/Misc'
import { optionTrigger } from './SettingsControls'

export interface WorkspacePickerProps {
  mode: SessionWorkspaceMode
  onModeChange: (m: SessionWorkspaceMode) => void
  /** 项目目录不是仓库根目录，或者还没有提交：分段控件禁用并说明。 */
  disabled: boolean
  branches: BranchesState
  /** 用户选的 base；null = 用 `default_base`。 */
  base: string | null
  onBaseChange: (b: string) => void
  branch: string
  onBranchChange: (b: string) => void
  /** 节点配置 `worktree.branch_prefix`，用来拼分支名输入框的占位符。 */
  branchPrefix?: string
}

/** 当前下拉里显示的 base：用户选的，没选就是 `default_base`；分支列表没取到时为空（由 daemon 决定）。 */
export function effectiveBase(branches: BranchesState, base: string | null): string | undefined {
  return branches.state === 'ready' ? (base ?? branches.branches.default_base) : undefined
}

/**
 * 项目的 git 元数据还没读到（daemon 冷启动时可能先不带）：占住「工作位置」的位置，说明在读什么。
 * 这时不能当作非 git 项目——发送被挡着，不会悄悄按「当前目录」建会话。内联时是选项行里的一个小标签，堆叠时是一行。
 */
export function WorkspacePending({ stacked = false }: { stacked?: boolean }) {
  const { t } = useTranslation()
  if (!stacked) {
    return (
      <span className="inline-flex items-center gap-1.5 px-2 py-1 text-[0.8125rem] text-fg-subtle" data-testid="new-session-workspace-pending" role="status">
        <Spinner className="size-3.5" />
        {t('workspace.reading')}
      </span>
    )
  }
  return (
    <div data-testid="new-session-workspace-pending" role="status">
      <div className="flex min-h-9 items-center gap-3">
        <span className="shrink-0 text-[0.8125rem] font-medium text-fg-muted">{t('workspace.label')}</span>
        <span className="inline-flex items-center gap-1.5 text-xs text-fg-subtle">
          <Spinner className="size-3.5" />
          {t('workspace.reading')}
        </span>
      </div>
    </div>
  )
}

/**
 * 选项行里的「工作位置」按钮：`⑂ 新 worktree · main ▾` / `📁 当前目录 ▾`，点开是竖排的 WorkspacePicker。
 * 分支名不合法时按钮标红（发送也被挡着，原因显示在输入框上方）。
 */
export function WorkspaceButton(props: WorkspacePickerProps) {
  const { t } = useTranslation()
  const { mode, disabled, branches, base, branch } = props
  const worktree = mode === 'worktree' && !disabled
  const value = effectiveBase(branches, base)
  const problem = worktree ? branchNameProblem(branch) : null
  const label = worktree ? [t('workspace.worktree'), branch.trim() || value].filter(Boolean).join(' · ') : t('workspace.local')
  return (
    <Popover.Root>
      <Popover.Trigger asChild>
        <button type="button" className={cn(optionTrigger(false), problem && 'text-danger hover:text-danger')} aria-label={`${t('workspace.label')}: ${label}`} title={t('workspace.label')} data-testid="new-session-workspace-button" data-mode={mode}>
          {worktree ? <GitBranch className="size-3.5 shrink-0 text-fg-subtle" aria-hidden /> : <Folder className="size-3.5 shrink-0 text-fg-subtle" aria-hidden />}
          <span className="max-w-56 truncate">{label}</span>
          <ChevronDown className="size-3.5 shrink-0 opacity-60" aria-hidden />
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content align="start" sideOffset={6} collisionPadding={8} className="z-50 w-80 rounded-lg border border-border bg-surface p-3 shadow-pop">
          <WorkspacePicker {...props} />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  )
}

/**
 * 新建会话的「工作位置」（web.md § 4.5，ADR-34）：
 * `工作位置 [ 新 worktree ][ 当前目录 ]   基于 [main ▾]   分支 [anycon/自动]`。每项一行（放在「工作位置」按钮的浮层和手机的底部弹层里）。
 */
export function WorkspacePicker({ mode, onModeChange, disabled, branches, base, onBaseChange, branch, onBranchChange, branchPrefix }: WorkspacePickerProps) {
  const { t } = useTranslation()
  const editing = mode === 'worktree' && !disabled
  const problem = editing ? branchNameProblem(branch) : null
  const value = effectiveBase(branches, base)
  return (
    <div data-testid="new-session-workspace-row">
      <div className="flex flex-col items-stretch gap-2">
        <div className="flex w-full items-center gap-2">
          <span className="w-16 shrink-0 text-xs text-fg-muted">{t('workspace.label')}</span>
          <div className="min-w-0 flex-1">
            <Segmented<SessionWorkspaceMode>
              fill
              ariaLabel={t('workspace.label')}
              testId="new-session-workspace"
              value={mode}
              disabled={disabled}
              onChange={onModeChange}
              options={[
                { value: 'worktree', label: t('workspace.worktree'), testId: 'new-session-workspace-worktree' },
                { value: 'local', label: t('workspace.local'), testId: 'new-session-workspace-local' },
              ]}
            />
          </div>
        </div>
        {editing ? (
          <>
            <label className="flex w-full items-center gap-2 text-xs text-fg-muted">
              <span className="w-16 shrink-0">{t('workspace.basedOn')}</span>
              <Select
                aria-label={t('workspace.basedOn')}
                data-testid="new-session-base"
                className="min-w-0 flex-1"
                value={value ?? ''}
                disabled={branches.state !== 'ready'}
                onChange={(e) => onBaseChange(e.target.value)}
              >
                <BaseOptions branches={branches} />
              </Select>
            </label>
            <label className="flex w-full items-center gap-2 text-xs text-fg-muted">
              <span className="w-16 shrink-0">{t('workspace.branch')}</span>
              <Input
                aria-label={t('workspace.branch')}
                aria-invalid={problem !== null}
                data-testid="new-session-branch"
                className="min-w-0 flex-1 font-mono text-xs"
                placeholder={branchPlaceholder(branchPrefix, t('workspace.branchAuto'))}
                value={branch}
                maxLength={255}
                autoCapitalize="off"
                autoCorrect="off"
                spellCheck={false}
                onChange={(e) => onBranchChange(e.target.value)}
              />
            </label>
          </>
        ) : null}
      </div>
      {disabled ? (
        <p className="mt-1 text-xs text-fg-subtle" data-testid="new-session-workspace-disabled">
          {t('workspace.notReady')}
        </p>
      ) : null}
      {problem ? (
        <p className="mt-1 text-xs text-danger" role="alert" data-testid="new-session-branch-error">
          {problem === 'reserved' ? t('workspace.branchReserved') : t('workspace.branchInvalid')}
        </p>
      ) : null}
    </div>
  )
}

function BaseOptions({ branches }: { branches: BranchesState }) {
  const { t } = useTranslation()
  if (branches.state !== 'ready') return <option value="">{branches.state === 'loading' ? t('workspace.baseLoading') : t('workspace.baseDefault')}</option>
  const b: WorkspaceBranchesResult = branches.branches
  const listed = new Set([...b.local, ...b.remote])
  return (
    <>
      {listed.has(b.default_base) ? null : <option value={b.default_base}>{b.default_base}</option>}
      {b.local.length > 0 ? (
        <optgroup label={t('workspace.localBranches')}>
          {b.local.map((name) => (
            <option key={`l:${name}`} value={name}>
              {name}
            </option>
          ))}
        </optgroup>
      ) : null}
      {b.remote.length > 0 ? (
        <optgroup label={t('workspace.remoteBranches')}>
          {b.remote.map((name) => (
            <option key={`r:${name}`} value={name}>
              {name}
            </option>
          ))}
        </optgroup>
      ) : null}
    </>
  )
}
