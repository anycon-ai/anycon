import { useEffect, useMemo, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { Bot, ChevronDown, FolderGit2, FolderPlus, MessageSquare } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { AgentInfo, Project, SessionWorkspace, SessionWorkspaceMode } from '@/protocol'
import { attachmentMimes } from '@/features/attachments/support'
import { restoreAttachments } from '@/features/attachments/composerUploads'
import { describeAttachmentFailure, describeAttachNotice } from '@/features/attachments/errors'
import { attachmentFailure, createSession, needsReupload } from '@/features/session/actions'
import { useGitPending } from '@/features/workspace/gitProbe'
import { branchNameProblem } from '@/features/worktree/branch'
import { describeCreateError } from '@/features/worktree/errors'
import { useBranches } from '@/features/worktree/useBranches'
import { resolveDefaultsDetailed, selectionKey, toSessionSettings, type DefaultsResolution, type Selection } from '@/features/settings/resolve'
import { isDangerousModeRejection } from '@/lib/errors'
import { useNodeParam } from '@/lib/nav'
import { useDrafts } from '@/stores/drafts'
import { useNewProject } from '@/stores/newProject'
import { readyAgents, useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { toast } from '@/stores/ui'
import { cn } from '@/ui/cn'
import { Menu, MenuContent, MenuItem, MenuRadioGroup, MenuRadioItem, MenuSeparator, MenuTrigger } from '@/ui/Menu'
import { Composer } from './Composer'
import { NoAgentsNotice } from './AgentIssueCards'
import { optionTrigger, SettingsControls } from './SettingsControls'
import { effectiveBase, WorkspaceButton, WorkspacePending, WorkspacePicker, type WorkspacePickerProps } from './WorkspacePicker'

/**
 * 新建会话（`/n/:node/new`）：输入框顶上一条上下文栏（项目、工作位置），下面一行选项——左边权限，右边 agent、模型、思考。
 * 窄屏这些都收进「⚙」底部弹层。发送后创建会话并跳转到会话页。
 * `onProjectChange` 有值时显示项目下拉（选「临时会话」传 undefined）。
 */
export function NewSessionForm({ nodeId, project, agentId, focus, onProjectChange }: { nodeId: string; project?: string | null; agentId?: string; focus?: boolean; onProjectChange?: (project: string | undefined) => void }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const nodeParam = useNodeParam(nodeId)
  const entry = useNodes((s) => s.byId[nodeId])
  const lastUsed = usePrefs((s) => s.lastUsed)
  const setLastUsed = usePrefs((s) => s.setLastUsed)
  const agents = readyAgents(entry)
  const [chosen, setChosen] = useState<string | undefined>(agentId)
  const agent = agents.find((a) => a.id === chosen) ?? agents.find((a) => a.id === agentId) ?? agents[0]
  const offline = entry?.conn !== 'ready'

  // 优先级（spec § 5.4）：本浏览器上次用的 → 节点默认 → agent 自己的默认
  // 危险模式不会被自动选中（任何来源）；`skippedDangerousMode` 用来提示用户为什么没选
  const resolved = useMemo<DefaultsResolution | null>(
    () => (agent ? resolveDefaultsDetailed(agent, [lastUsed[selectionKey(nodeId, agent.id)], entry?.agentDefaults[agent.id]]) : null),
    // 只在换 agent / 节点默认变化时重算；lastUsed 变化（来自用户自己的选择）不应覆盖正在编辑的值
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [agent?.id, entry?.agentDefaults, agent?.probed_at_ms],
  )
  const initial: Selection | null = resolved?.selection ?? null
  const [selection, setSelection] = useState<Selection | null>(initial)
  const [busy, setBusy] = useState(false)
  useEffect(() => setSelection(initial), [initial])

  // 工作位置（ADR-34）：节点支持 worktree 且项目是 git 仓库时多一行。仓库根目录 + 至少一个提交（`worktree_ready`）才能选「新 worktree」
  const lastWorkspace = usePrefs((s) => s.lastWorkspace)
  const setLastWorkspace = usePrefs((s) => s.setLastWorkspace)
  const projectMeta = project ? entry?.projects.find((p) => p.name === project) : undefined
  const supportsWorktree = !!entry?.info?.capabilities.worktree
  const showWorkspace = !!project && supportsWorktree && !!projectMeta?.git
  // 项目没有 `git`：可能是非 git 项目，也可能是 daemon 还没算完（冷启动）。读完之前占位并挡住发送（不能悄悄按「当前目录」建会话）：
  //  - daemon 说 `git_pending`：一直占到 `workspace.changed { updated }` 之后重新拉到的项目不再 `git_pending`，补拉不会让占位消失；
  //  - 旧 daemon（既没有 `git` 也没有 `git_pending`）：补拉一次 `workspace.list`，补拉回来仍没有 `git` 就按非 git 项目处理
  const gitUnknown = !!project && supportsWorktree && !!projectMeta && !projectMeta.git && !offline
  const probing = useGitPending(nodeId, project, gitUnknown)
  const gitPending = gitUnknown && (projectMeta?.git_pending === true || probing)
  const worktreeReady = showWorkspace && projectMeta?.git?.worktree_ready === true
  const workspaceKey = `${nodeId}:${project ?? ''}`
  const [pickedMode, setPickedMode] = useState<SessionWorkspaceMode | null>(null)
  // 初始选择：这个浏览器在这个 (节点, 项目) 上次的选择，没有就用节点配置的 `worktree.default`（默认 worktree）
  const initialMode: SessionWorkspaceMode = lastWorkspace[workspaceKey] ?? entry?.info?.worktree?.default_mode ?? 'worktree'
  const workspaceMode: SessionWorkspaceMode = worktreeReady ? (pickedMode ?? initialMode) : 'local'
  const [pickedBase, setPickedBase] = useState<string | null>(null)
  const [branch, setBranch] = useState('')
  const branches = useBranches(nodeId, project, worktreeReady && workspaceMode === 'worktree' && !offline)
  const branchProblem = worktreeReady && workspaceMode === 'worktree' ? branchNameProblem(branch) : null

  // 图片附件（ADR-35）：节点支持 blob、选中的 agent 收图片才显示回形针
  const attachMimes = attachmentMimes(entry?.info, agent)
  // 换到不收图片的 agent 时，输入区里已经添加的图（内存里的，或刷新前存在草稿里的）发送时不会带上：说一声
  const draftKey = `new:${nodeId}:${project ?? ''}`
  const imageCount = useDrafts((s) => (s.items[draftKey] ?? s.byKey[draftKey]?.attachments ?? []).length)

  if (entry && entry.agentsLoaded && agents.length === 0) return <NoAgentsNotice agents={entry.agents} />
  if (!agent || !selection) return null

  const blockedReason = offline
    ? t('composer.offlineNew')
    : gitPending
      ? t('workspace.readingBlocked')
      : branchProblem
        ? t(branchProblem === 'reserved' ? 'workspace.branchReserved' : 'workspace.branchInvalid')
        : undefined

  const workspaceProps: WorkspacePickerProps = {
    mode: workspaceMode,
    onModeChange: setPickedMode,
    disabled: !worktreeReady,
    branches,
    base: pickedBase,
    onBaseChange: setPickedBase,
    branch,
    onBranchChange: setBranch,
    branchPrefix: entry?.info?.worktree?.branch_prefix,
  }
  // 项目、工作位置：宽屏在输入框顶上的上下文栏里（像桌面端的 agent 应用那样），窄屏和其他选项一起进「⚙」弹层
  const contextControls = (stacked: boolean) =>
    (onProjectChange && entry) || gitPending || showWorkspace ? (
      <div className={cn('flex', stacked ? 'flex-col items-stretch gap-2' : 'min-w-0 flex-wrap items-center gap-1')} data-testid={stacked ? undefined : 'new-session-context'}>
        {onProjectChange && entry ? (
          <ProjectPicker
            projects={entry.projects}
            value={project ?? undefined}
            onChange={onProjectChange}
            stacked={stacked}
            // 建好之后回到这一页并选中它（spec/web.md § 4.5）
            onNewProject={entry.info?.capabilities.workspace === false ? undefined : () => useNewProject.getState().open({ nodeId, mode: 'empty', then: 'newSession', pickNode: false })}
            newProjectDisabled={offline}
          />
        ) : null}
        {gitPending ? <WorkspacePending stacked={stacked} /> : showWorkspace ? stacked ? <WorkspacePicker {...workspaceProps} /> : <WorkspaceButton {...workspaceProps} /> : null}
      </div>
    ) : undefined
  const notice = (
    <>
      {resolved?.skippedDangerousMode && !(agent.modes ?? []).some((m) => m.id === selection.mode && m.risk === 'dangerous') ? (
        <p className="mb-1 px-1 text-xs text-warning" data-testid="dangerous-default-hint">
          {t('options.dangerousDefaultSkipped', { mode: resolved.skippedDangerousMode.label })}
        </p>
      ) : null}
      {!attachMimes && imageCount > 0 ? (
        <p className="mb-1 px-1 text-xs text-warning" role="status" data-testid="composer-attachments-unsupported">
          {t('composer.attachmentsUnsupported')}
        </p>
      ) : null}
    </>
  )

  return (
    <div data-testid="new-session-form">
      <Composer
        draftKey={draftKey}
        autoFocus={focus}
        sending={busy}
        blockedReason={blockedReason}
        notice={notice}
        context={contextControls(false)}
        placeholder={project ? t('composer.placeholderProject', { project }) : t('composer.placeholderScratch')}
        attachments={attachMimes ? { nodeId, accept: attachMimes, disabledReason: offline ? t('composer.attachOffline') : undefined } : undefined}
        options={(layout) => {
          const stacked = layout === 'stacked'
          return (
            <SettingsControls
              agent={agent}
              value={selection}
              onChange={(next, info) => {
                setSelection(next)
                if (info?.thinkingAdjusted && next.thinking) toast.info(t('options.thinkingAdjusted', { value: next.thinking }))
              }}
              dangerousForbidden={entry?.dangerousForbidden}
              customKey={`${nodeId}:${agent.id}`}
              idPrefix="option"
              layout={layout}
              left={stacked ? contextControls(true) : undefined}
              right={agents.length > 1 ? <AgentPicker agents={agents} value={agent.id} onChange={setChosen} stacked={stacked} /> : null}
            />
          )
        }}
        onSend={async (prompt, attachments) => {
          setBusy(true)
          try {
            // 线路上不猜默认值：这一行出现并可用时，把用户看到的选择明确发出去；不可用 / 不显示时不带 `workspace`
            const base = effectiveBase(branches, pickedBase)
            const workspace: SessionWorkspace | undefined = !worktreeReady
              ? undefined
              : workspaceMode === 'worktree'
                ? { mode: 'worktree', ...(base ? { base } : {}), ...(branch.trim() ? { branch: branch.trim() } : {}) }
                : { mode: 'local' }
            const id = await createSession(nodeId, { project: project ?? null, agent: agent.id, prompt, attachments, settings: toSessionSettings(selection), ...(workspace ? { workspace } : {}) })
            setLastUsed(selectionKey(nodeId, agent.id), { model: selection.model, thinking: selection.thinking, mode: selection.mode, options: selection.options })
            if (workspace) setLastWorkspace(workspaceKey, workspace.mode)
            await navigate({ to: '/n/$node/s/$session', params: { node: nodeParam, session: id } })
          } catch (e) {
            if (isDangerousModeRejection(e, (agent.modes ?? []).some((m) => m.id === selection.mode && m.risk === 'dangerous'))) useNodes.getState().patch(nodeId, { dangerousForbidden: true })
            // 附件的问题（过期 / 已被别的会话占了 / 合计太大）有专门的说明；前两种从本地缓存重传一份就能再发
            const failure = attachmentFailure(e)
            const expired = needsReupload(failure)
            toast.error(describeAttachmentFailure(failure, t) ?? describeCreateError(e, t, worktreeReady && workspaceMode === 'worktree'))
            // 发送失败把文字和图片放回输入框，别丢
            useDrafts.getState().setText(draftKey, prompt)
            if (attachments.length > 0) void restoreAttachments({ draftKey, nodeId, accepted: attachMimes ?? [], notify: (n) => toast.info(describeAttachNotice(n, t)) }, attachments, { expired })
          } finally {
            setBusy(false)
          }
        }}
      />
    </div>
  )
}

function PickerLabel({ children }: { children: string }) {
  return <p className="text-xs font-medium text-fg-subtle">{children}</p>
}

/** 新会话的项目：临时会话 + 节点上的项目（目录不在了的禁用），末尾「新建项目…」。 */
function ProjectPicker({
  projects,
  value,
  onChange,
  stacked,
  onNewProject,
  newProjectDisabled,
}: {
  projects: readonly Project[]
  value: string | undefined
  onChange: (project: string | undefined) => void
  stacked: boolean
  onNewProject?: () => void
  newProjectDisabled?: boolean
}) {
  const { t } = useTranslation()
  return (
    <div className={cn(stacked && 'space-y-1')}>
      {stacked ? <PickerLabel>{t('newSession.project')}</PickerLabel> : null}
      <Menu>
        <MenuTrigger asChild>
          <button type="button" className={optionTrigger(stacked)} aria-label={t('newSession.project')} data-testid="new-session-project" data-value={value ?? ''}>
            {value ? <FolderGit2 className="size-3.5 shrink-0 text-fg-subtle" aria-hidden /> : <MessageSquare className="size-3.5 shrink-0 text-fg-subtle" aria-hidden />}
            <span className="max-w-48 truncate">{value ?? t('newSession.scratch')}</span>
            <ChevronDown className="size-3.5 shrink-0 opacity-60" aria-hidden />
          </button>
        </MenuTrigger>
        <MenuContent className="w-64">
          <MenuRadioGroup value={value ?? ''} onValueChange={(v) => onChange(v || undefined)}>
            <MenuRadioItem value="" testId="new-session-project-scratch">
              {t('newSession.scratch')}
            </MenuRadioItem>
            {projects.map((p) => (
              <MenuRadioItem key={p.name} value={p.name} disabled={p.missing === true} testId={`new-session-project-${p.name}`}>
                {p.missing ? t('project.missingOption', { name: p.name }) : p.name}
              </MenuRadioItem>
            ))}
          </MenuRadioGroup>
          {onNewProject ? (
            <>
              <MenuSeparator />
              <MenuItem icon={<FolderPlus className="size-4" />} onSelect={onNewProject} disabled={newProjectDisabled} testId="new-session-project-new">
                {t('newSession.newProject')}
              </MenuItem>
            </>
          ) : null}
        </MenuContent>
      </Menu>
    </div>
  )
}

/** 节点上可用的 agent（只有一个时不显示）。 */
function AgentPicker({ agents, value, onChange, stacked }: { agents: readonly AgentInfo[]; value: string; onChange: (id: string) => void; stacked: boolean }) {
  const { t } = useTranslation()
  const current = agents.find((a) => a.id === value)
  return (
    <div className={cn(stacked && 'space-y-1')}>
      {stacked ? <PickerLabel>{t('options.agent')}</PickerLabel> : null}
      <Menu>
        <MenuTrigger asChild>
          <button type="button" className={optionTrigger(stacked)} aria-label={t('options.agent')} data-testid="new-session-agent" data-value={value}>
            <Bot className="size-3.5 shrink-0 text-fg-subtle" aria-hidden />
            <span className="truncate">{current?.label ?? value}</span>
            <ChevronDown className="size-3.5 shrink-0 opacity-60" aria-hidden />
          </button>
        </MenuTrigger>
        <MenuContent className="w-56" align={stacked ? 'start' : 'end'}>
          <MenuRadioGroup value={value} onValueChange={onChange}>
            {agents.map((a) => (
              <MenuRadioItem key={a.id} value={a.id} testId={`new-session-agent-${a.id}`}>
                {a.label}
              </MenuRadioItem>
            ))}
          </MenuRadioGroup>
        </MenuContent>
      </Menu>
    </div>
  )
}
