import { useCallback, useEffect, useMemo, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { FileDiff, Globe, MessageSquare, PanelRight, PanelRightClose, ShieldAlert } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { TFunction } from 'i18next'
import type { PermissionRespondParams } from '@/protocol'
import { SettingsControls } from '@/components/SettingsControls'
import { Composer } from '@/components/Composer'
import { PageHeader } from '@/components/PageHeader'
import { SessionStatusChip } from '@/components/SessionListItem'
import { attachmentMimes } from '@/features/attachments/support'
import { discardRefs, restoreAttachments } from '@/features/attachments/composerUploads'
import { describeAttachmentFailure, describeAttachNotice } from '@/features/attachments/errors'
import { archiveSession, cancelTurn, closeSession, deleteSession, loadOlder, needsReupload, openSession, renameSession, respondPermission, restartSession, resendUnsent, sendMessage, unarchiveSession, type SendOutcome } from '@/features/session/actions'
import { downloadText, exportMarkdown } from '@/features/session/export'
import { isRunning, useDeviceLabel, useSessionData, useSessionSettings } from '@/features/session/hooks'
import { latestSyncBlock, pendingPermissions } from '@/features/session/reducer'
import { backgroundCommands, isTurnActive, subagentRows } from '@/features/session/subagents'
import { backgroundTaskCount, runningTasks, tasksLive } from '@/features/session/tasks'
import { previewAvailable, usePreviewTemplate } from '@/features/preview/availability'
import { PreviewView } from '@/features/preview/PreviewView'
import { ApprovalBanner } from '@/components/ApprovalBanner'
import { ArchivedBar } from '@/features/session/ui/ArchivedBar'
import { InfoPanel } from '@/features/session/ui/InfoPanel'
import { BackgroundTasksChip } from '@/features/session/ui/Tasks'
import { BranchChip } from '@/features/worktree/BranchChip'
import { ChangesTab } from '@/features/worktree/ChangesTab'
import { describeArchiveError, describeGitError, describeUnarchiveError } from '@/features/worktree/errors'
import { refreshGitStatus, syncBase } from '@/features/worktree/git'
import { resolveConflictsMessage } from '@/features/worktree/messages'
import { sendQuickMessage } from '@/features/worktree/quick'
import { useGitRefresh } from '@/features/worktree/useGitRefresh'
import { MessageList } from '@/features/session/ui/MessageList'
import type { SettledReason } from '@/features/session/ui/PermissionCard'
import { describeError } from '@/lib/errors'
import { useCurrentNode, useNodeParam } from '@/lib/nav'
import { useMediaQuery } from '@/lib/useMediaQuery'
import { RpcCallError } from '@/transport/rpcClient'
import { pendingMessages, useDrafts } from '@/stores/drafts'
import { useGitEntry } from '@/stores/git'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { toast, useUi } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Input } from '@/ui/Field'
import { Chip, EmptyState, Segmented, Skeleton, Spinner, type SegmentedOption } from '@/ui/Misc'
import { Drawer, Dialog } from '@/ui/Overlay'

type MainView = 'chat' | 'changes' | 'preview'

/** 发送被 daemon 拒绝时 toast 里的那句话。 */
function failedText(reason: SendOutcome['reason'], t: TFunction): string {
  if (reason === 'archived') return t('session.archivedHint')
  return describeAttachmentFailure(reason, t) ?? t('message.failed')
}

export function SessionPage({ sessionId }: { sessionId: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { nodeId, entry } = useCurrentNode()
  const nodeParam = useNodeParam(nodeId)
  const { summary, open } = useSessionData(sessionId)
  const xl = useMediaQuery('(min-width: 1200px)', true)
  const panelOpen = usePrefs((s) => s.infoPanelOpen)
  const setPrefs = usePrefs((s) => s.set)
  const rightDrawer = useUi((s) => s.rightDrawerOpen)
  const setRightDrawer = useUi((s) => s.setRightDrawer)
  // 宽屏时右侧面板内联显示、抽屉被卸载；不清掉状态的话，窗口再拖窄抽屉会自己弹出来
  useEffect(() => {
    if (xl) setRightDrawer(false)
  }, [xl, setRightDrawer])
  const draft = useDrafts((s) => s.byKey[sessionId])
  const inflight = useDrafts((s) => s.inflight)
  const loadDraft = useDrafts((s) => s.load)
  const deviceLabel = useDeviceLabel(nodeId)
  const pendingReqs = useSessions((s) => s.pending)
  const [responding, setResponding] = useState<ReadonlySet<string>>(new Set())
  // 答复时 daemon 回了 not_found 的请求：卡片定格成说明、不再给按钮。原因是目录变了（`folder_changed`）时随后会来
  // permission_resolved（by_device: daemon），卡片换成「已被 anycon 自动拒绝」；别的 not_found（`gone`：请求已不存在）就停在说明上
  const [settled, setSettled] = useState<ReadonlyMap<string, SettledReason>>(new Map())
  const [loadingOlder, setLoadingOlder] = useState(false)
  const [renameOpen, setRenameOpen] = useState(false)
  const [focusToken, setFocusToken] = useState(0)
  const [scrollTo, setScrollTo] = useState<string | null>(null)
  const [announce, setAnnounce] = useState('')
  // 主区显示聊天、「改动」还是「预览」（只对这个会话；切到别的会话回到聊天）。「改动」「预览」都是整页大小的视图，不挤在右栏里
  const [mainChoice, setMainChoice] = useState<{ sessionId: string; view: MainView } | null>(null)
  const [mergeBusy, setMergeBusy] = useState(false)
  const [unarchiving, setUnarchiving] = useState(false)
  const gitEntry = useGitEntry(sessionId)
  const previewTemplate = usePreviewTemplate()

  useEffect(() => {
    if (!nodeId) return
    void loadDraft(sessionId)
    void openSession(nodeId, sessionId)
    return () => {
      void closeSession(nodeId, sessionId)
    }
  }, [nodeId, sessionId, loadDraft])

  const view = open?.view
  const status = (open?.watching ? view?.status ?? summary?.status : summary?.status ?? view?.status) ?? 'idle'
  const running = isRunning(status)
  // 主 turn 结束后 agent 进程里还在跑的后台任务（后台 subagent、后台命令）。旧 daemon 不发这个字段，按 0 处理
  const backgroundTasks = summary?.background_tasks ?? 0
  const offline = entry?.conn !== 'ready'
  // 已归档的会话不能 `send`（daemon 回 `invalid` / `archived`）：输入框换成「已归档 · 取消归档」，也不再给「重试这一轮」
  const archived = !!summary?.archived
  const agent = entry?.agents.find((a) => a.id === summary?.agent)

  // git（ADR-34）：节点支持，并且会话在 worktree 里，或者它的项目是 git 仓库
  const worktree = summary?.worktree ?? null
  const projectMeta = summary?.project ? entry?.projects.find((p) => p.name === summary.project) : undefined
  const gitAvailable = !!entry?.info?.capabilities.worktree && (!!worktree || !!projectMeta?.git)
  // worktree 还没建好 / 已经删了 / 目录不见了的时候 cwd 不可用，不去拉
  const wtUsable = !worktree || worktree.state === 'active' || worktree.state === 'kept'
  useGitRefresh({ nodeId, sessionId, enabled: gitAvailable && wtUsable && !offline })

  // turn 结束时给读屏播一句（token 级增量不播报）
  const [prevRunning, setPrevRunning] = useState(running)
  useEffect(() => {
    if (prevRunning && !running) setAnnounce(t('session.turnFinished'))
    setPrevRunning(running)
  }, [running, prevRunning, t])

  const baseSettings = view?.settings ?? summary?.settings
  const { selection, onChange, pending: pendingSettings } = useSessionSettings({ nodeId: nodeId ?? '', sessionId, agent, running, baseSettings, offline })

  const seen = useMemo(() => new Set(view?.blocks.filter((b) => b.type === 'user').map((b) => (b.type === 'user' ? b.messageId : '')) ?? []), [view?.blocks])
  const pending = useMemo(() => pendingMessages(draft, inflight, sessionId, seen), [draft, inflight, sessionId, seen])
  // `task` 事件的快照：会话没在跑、daemon 也没报后台任务时，历史里停在「运行中」的永远不会再变（显示成「已停止」，不计入后台任务）
  const tasksMayProgress = tasksLive(status, backgroundTasks)
  const subagents = useMemo(() => subagentRows(view?.blocks ?? [], status, tasksMayProgress), [view?.blocks, status, tasksMayProgress])
  const bgCommands = useMemo(() => backgroundCommands(view?.blocks ?? [], status, tasksMayProgress), [view?.blocks, status, tasksMayProgress])
  const liveTasks = useMemo(() => runningTasks(view?.blocks ?? [], tasksMayProgress), [view?.blocks, tasksMayProgress])
  const backgroundCount = backgroundTaskCount(liveTasks.length, backgroundTasks)

  const modelLabel = useCallback((id: string) => agent?.models?.find((m) => m.id === id)?.label ?? id, [agent])
  const modeLabel = useCallback((id: string) => agent?.modes?.find((m) => m.id === id)?.label ?? id, [agent])
  const isDangerousMode = useCallback((id: string) => agent?.modes?.find((m) => m.id === id)?.risk === 'dangerous', [agent])

  // recoverable 错误的「重试」：把这个会话最后一条用户消息再发一次（daemon 会 resume 并重新跑这一轮）
  const onRetryTurn = useCallback(() => {
    if (!nodeId) return
    const last = view?.blocks.findLast((b) => b.type === 'user')
    if (!last || last.type !== 'user') return
    void sendMessage(nodeId, sessionId, last.content, last.attachments).then((r) => {
      if (r.state === 'failed') toast.error(failedText(r.reason, t))
    })
  }, [nodeId, sessionId, view?.blocks, t])

  const onRespond = useCallback(
    (params: PermissionRespondParams) => {
      if (!nodeId) return
      setResponding((s) => new Set(s).add(params.request_id))
      respondPermission(nodeId, params)
        .then((outcome) => {
          if (outcome === 'answered') return
          const reason: SettledReason = outcome === 'denied_folder_changed' ? 'folder_changed' : 'gone'
          setSettled((s) => new Map(s).set(params.request_id, reason))
          toast.info(t(outcome === 'denied_folder_changed' ? 'permission.deniedFolderChanged' : 'permission.requestGone'))
        })
        .catch((e: unknown) => {
          if (e instanceof RpcCallError && e.kind === 'rpc' && e.code === 'already_resolved') toast.info(t('permission.alreadyResolved'))
          else toast.error(describeError(e, t))
        })
        .finally(() =>
          setResponding((s) => {
            const n = new Set(s)
            n.delete(params.request_id)
            return n
          }),
        )
    },
    [nodeId, t],
  )

  if (!nodeId || !entry) return null

  // 会话不存在（被别的设备删了 / 链接错）
  const gone = open?.phase === 'error' && open.error === 'not_found'
  if (gone || (!summary && entry.synced && open?.phase !== 'loading' && !open?.events.length))
    return (
      <>
        <PageHeader crumbs={[{ label: entry.name, to: { to: '/n/$node', params: { node: nodeParam } } }]} />
        <div className="flex flex-1 items-center justify-center" data-testid="session-gone">
          <EmptyState title={t('session.goneTitle')} action={<Button variant="primary" onClick={() => void navigate({ to: '/n/$node', params: { node: nodeParam } })}>{t('session.goneBack')}</Button>}>
            {t('session.goneBody')}
          </EmptyState>
        </div>
      </>
    )

  const title = view?.title ?? summary?.title ?? sessionId
  const goBack = (): void => {
    if (summary?.project) void navigate({ to: '/n/$node/p/$project', params: { node: nodeParam, project: summary.project } })
    else void navigate({ to: '/n/$node', params: { node: nodeParam } })
  }
  const dangerousMode = agent?.modes?.find((m) => m.id === selection?.mode)?.risk === 'dangerous' ? agent.modes?.find((m) => m.id === selection?.mode) : undefined
  const project = summary?.project
  const projectPath = project ? entry.projects.find((p) => p.name === project)?.path : undefined
  const workDir = projectPath && worktree ? `${projectPath.replace(/\/+$/, '')}/${worktree.path}` : projectPath
  // 「预览」（ADR-39）：hub 配了预览、节点也支持（web.md § 13.1）。和 git 无关，非 git 会话也有
  const previewTemplateOk = previewAvailable(previewTemplate, entry.info) ? previewTemplate : null
  const choice = mainChoice?.sessionId === sessionId ? mainChoice.view : 'chat'
  const mainView: MainView = (choice === 'changes' && gitAvailable) || (choice === 'preview' && previewTemplateOk !== null) ? choice : 'chat'
  const showChanges = mainView === 'changes'
  const showPreview = mainView === 'preview' && previewTemplateOk !== null
  const openChanges = (): void => setMainChoice({ sessionId, view: 'changes' })
  const syncBlock = view ? latestSyncBlock(view.blocks) : null
  const eventConflictFiles = syncBlock?.action === 'sync_conflicts' ? syncBlock.files : undefined
  const gitStatus = gitEntry.status
  const pendingList = (view ? pendingPermissions(view) : []).map((b) => ({ request_id: b.requestId, title: b.title }))
  const fromStore = Object.values(pendingReqs)
    .filter((p) => p.session_id === sessionId && !pendingList.some((x) => x.request_id === p.request_id))
    .map((p) => ({ request_id: p.request_id, title: p.title }))
  const allPending = [...pendingList, ...fromStore]
  const noAgent = !agent
  const blockedReason = noAgent ? t('session.agentMissing', { agent: summary?.agent ?? '' }) : undefined
  // 图片附件（ADR-35）：节点支持 blob、agent 收图片才显示回形针；节点不在线时禁用并说明
  const attachMimes = attachmentMimes(entry.info, agent)

  // 分支 chip 宽屏放在输入框上方靠右，窄屏和其他选项一起在「⚙」底部弹层里；点了切到「改动」
  const branchChip = (layout: 'inline' | 'stacked') => (gitAvailable ? <BranchChip className={layout === 'inline' ? 'min-h-0' : undefined} worktree={worktree} status={gitStatus} onOpen={openChanges} /> : null)
  const uncommitted = gitStatus?.uncommitted ?? 0

  const options = (layout: 'inline' | 'stacked') =>
    agent && selection ? (
      <div>
        <SettingsControls
          left={layout === 'stacked' ? branchChip(layout) : undefined}
          agent={agent}
          value={selection}
          onChange={onChange}
          disabled={offline}
          disabledReason={t('options.offlineLocked')}
          pending={pendingSettings}
          dangerousForbidden={entry.dangerousForbidden}
          customKey={`${nodeId}:${agent.id}`}
          idPrefix="option"
          layout={layout}
        />
      </div>
    ) : null

  const panel = (
    <InfoPanel
      agentLabel={agent?.label ?? summary?.agent ?? ''}
      status={status}
      subagents={subagents}
      backgroundCommands={bgCommands}
      backgroundTasks={backgroundTasks}
      workDir={workDir}
      project={project}
      createdAt={summary?.created_at_ms ?? 0}
      usage={view?.usage ?? { turns: 0, inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, costUsd: null }}
      context={view?.context}
      usageTotals={summary?.usage}
      pending={allPending}
      onFocusPending={(id) => {
        setScrollTo(null)
        setTimeout(() => setScrollTo(id), 0)
        setRightDrawer(false)
      }}
      archived={archived}
      archiveBlocked={running}
      disabled={offline}
      worktree={worktree}
      gitStatus={gitStatus}
      onDisposeOpen={() => {
        if (gitAvailable && wtUsable && !offline) void refreshGitStatus(nodeId, sessionId)
      }}
      onRename={() => setRenameOpen(true)}
      onArchive={(opts) => void archiveSession(nodeId, sessionId, !archived, opts).catch((e: unknown) => toast.error(archived ? describeUnarchiveError(e, t) : describeArchiveError(e, t)))}
      onDelete={(opts) => {
        deleteSession(nodeId, sessionId, opts).then(goBack).catch((e: unknown) => toast.error(describeGitError(e, t)))
      }}
      onExport={() => downloadText(`${title.replace(/[^\w.-]+/g, '_').slice(0, 60) || 'session'}.md`, exportMarkdown(title, view?.blocks ?? [], (name) => t('export.image', { name })))}
      onRestart={() => void restartSession(nodeId, sessionId).catch((e: unknown) => toast.error(describeError(e, t)))}
    />
  )

  const loading = !open || (open.phase === 'loading' && (view?.blocks.length ?? 0) === 0)

  // 顶栏的视图开关：「聊天」一直有；「改动」要 git；「预览」要 hub 和节点都支持（只剩「聊天」时不显示开关）
  const viewOptions: SegmentedOption<MainView>[] = [
    {
      value: 'chat',
      title: t('session.view.chat'),
      testId: 'main-view-chat',
      label: (
        <span className="inline-flex items-center gap-1.5">
          <MessageSquare className="size-3.5" aria-hidden />
          <span className="sr-only sm:not-sr-only">{t('session.view.chat')}</span>
        </span>
      ),
    },
    ...(gitAvailable
      ? [
          {
            value: 'changes' as const,
            title: t('session.view.changes'),
            testId: 'main-view-changes',
            label: (
              <span className="inline-flex items-center gap-1.5">
                <FileDiff className="size-3.5" aria-hidden />
                <span className="sr-only sm:not-sr-only">{t('session.view.changes')}</span>
                {uncommitted > 0 ? (
                  <span className="rounded-full bg-bg-muted px-1.5 text-[0.6875rem] leading-4 tabular-nums" title={t('git.chipUncommitted', { count: uncommitted })} data-testid="main-view-changes-count">
                    {uncommitted}
                  </span>
                ) : null}
              </span>
            ),
          },
        ]
      : []),
    ...(previewTemplateOk !== null
      ? [
          {
            value: 'preview' as const,
            title: t('session.view.preview'),
            testId: 'main-view-preview',
            label: (
              <span className="inline-flex items-center gap-1.5">
                <Globe className="size-3.5" aria-hidden />
                <span className="sr-only sm:not-sr-only">{t('session.view.preview')}</span>
              </span>
            ),
          },
        ]
      : []),
  ]

  return (
    <>
      <PageHeader
        crumbs={[
          { label: entry.name, to: { to: '/n/$node', params: { node: nodeParam } } },
          ...(project ? [{ label: project, to: { to: '/n/$node/p/$project', params: { node: nodeParam, project } } }] : []),
          { label: title, onClick: () => setRenameOpen(true), actionLabel: t('info.rename'), testId: 'session-title' },
        ]}
        onBack={goBack}
        meta={
          <>
            {viewOptions.length > 1 ? (
              <Segmented size="sm" ariaLabel={t('session.view.label')} value={mainView} onChange={(v) => setMainChoice({ sessionId, view: v })} testId="main-view" options={viewOptions} />
            ) : null}
            {dangerousMode ? (
              <Chip tone="danger" testId="mode-chip-dangerous" icon={<ShieldAlert className="size-3" aria-hidden />} className="shrink-0">
                {dangerousMode.label}
              </Chip>
            ) : null}
            {offline ? (
              <Chip tone="neutral" testId="session-status" dotOnMobile title={t('status.session.offline')} className="shrink-0">
                <span data-status="offline">{t('status.session.offline')}</span>
              </Chip>
            ) : (
              <span className="shrink-0" data-testid="session-status" data-status={status}>
                <SessionStatusChip status={status} dotOnMobile />
              </span>
            )}
          </>
        }
        actions={
          <Button variant="ghost" size="icon" aria-label={t('info.toggle')} onClick={() => (xl ? setPrefs({ infoPanelOpen: !panelOpen }) : setRightDrawer(true))} data-testid="info-toggle">
            {xl && panelOpen ? <PanelRightClose className="size-4" /> : <PanelRight className="size-4" />}
          </Button>
        }
      />
      <div className="flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          {showChanges ? (
            <div className="scroll-thin min-h-0 flex-1 overflow-y-auto p-3 md:p-4" data-testid="changes-view">
              <ChangesTab key={sessionId} nodeId={nodeId} sessionId={sessionId} worktree={worktree} running={running} offline={offline} archived={archived} eventConflictFiles={eventConflictFiles} />
            </div>
          ) : showPreview ? (
            <PreviewView key={sessionId} nodeId={nodeId} sessionId={sessionId} template={previewTemplateOk} offline={offline} />
          ) : loading ? (
            <div className="flex-1 space-y-4 p-6" aria-busy="true">
              <Skeleton className="ml-auto h-10 w-2/5" />
              <Skeleton className="h-24 w-4/5" />
              <Spinner className="mx-auto" />
            </div>
          ) : (
            <MessageList
              nodeId={nodeId}
              blocks={view?.blocks ?? []}
              pending={pending}
              hasMoreOlder={open?.hasMoreOlder ?? false}
              loadingOlder={loadingOlder}
              onLoadOlder={() => {
                setLoadingOlder(true)
                // 翻页失败（节点离线 / 超时）要让用户看到，而不是留下一个未处理的 rejection
                loadOlder(sessionId)
                  .catch((e: unknown) => toast.error(describeError(e, t)))
                  .finally(() => setLoadingOlder(false))
              }}
              deviceLabel={deviceLabel}
              respondingIds={responding}
              settledIds={settled}
              onRespond={onRespond}
              onRetryPending={(id) => void resendUnsent(nodeId, sessionId, id)}
              onEditPending={(id) => {
                const m = draft?.unsent.find((u) => u.message_id === id)
                if (!m) return
                const d = useDrafts.getState()
                d.removeUnsent(sessionId, id)
                d.setText(sessionId, (d.byKey[sessionId]?.text ? `${d.byKey[sessionId]!.text}\n` : '') + m.content)
                // 附件放回输入区；已过期的（daemon 回收了）从本地缓存取回字节重新上传
                if (m.attachments?.length) {
                  void restoreAttachments({ draftKey: sessionId, nodeId, accepted: attachMimes ?? [], notify: (n) => toast.info(describeAttachNotice(n, t)) }, m.attachments, { expired: needsReupload(m.error) }).then(({ lost }) => {
                    if (lost > 0) toast.info(t('message.attachmentLost', { count: lost }))
                  })
                }
                setFocusToken((n) => n + 1)
              }}
              onDeletePending={(id) => {
                const m = draft?.unsent.find((u) => u.message_id === id)
                // 没发出去的消息删了，它的图也没人要了
                if (m?.attachments?.length) discardRefs(nodeId, m.attachments)
                useDrafts.getState().removeUnsent(sessionId, id)
              }}
              modelLabel={modelLabel}
              modeLabel={modeLabel}
              isDangerousMode={isDangerousMode}
              onRetryTurn={running || offline || archived ? undefined : onRetryTurn}
              activity={isTurnActive(status) ? view?.activity : null}
              tasksLive={tasksMayProgress}
              scrollToRequest={scrollTo}
              git={
                worktree
                  ? {
                      // 状态读不到（worktree 不可用）时当作没有合并在进行；还没读到（离线 / 刚进入）时不知道，先显示
                      merging: gitStatus ? gitStatus.merging || gitStatus.conflicts > 0 : wtUsable ? undefined : false,
                      busy: offline || mergeBusy || archived,
                      onResolve: () => void sendQuickMessage(nodeId, sessionId, resolveConflictsMessage(t, worktree.branch, worktree.base), t),
                      onAbort: () => {
                        setMergeBusy(true)
                        syncBase(nodeId, sessionId, { abort: true })
                          .catch((e: unknown) => toast.error(describeGitError(e, t)))
                          .finally(() => setMergeBusy(false))
                      },
                    }
                  : undefined
              }
            />
          )}

          {allPending.length > 0 ? (
            <ApprovalBanner
              count={allPending.length}
              title={allPending[0]!.title}
              onReview={() => {
                setMainChoice({ sessionId, view: 'chat' })
                setScrollTo(null)
                setTimeout(() => setScrollTo(allPending[0]!.request_id), 0)
              }}
            />
          ) : null}

          {backgroundCount > 0 ? (
            // 低调的一个 chip：有后台任务（workflow、后台子 agent / 命令）在干活，点开列出。会话已经 idle 时，结束后 agent 会自己再跑一轮
            <div className="chat-column flex shrink-0 items-center pt-2" data-testid="background-tasks" data-count={backgroundCount}>
              <BackgroundTasksChip tasks={liveTasks} count={backgroundCount} idle={!running} />
            </div>
          ) : null}

          <div className="sr-only" aria-live="polite" role="status">
            {announce}
          </div>

          {showChanges ? (
            // 「改动」视图没有输入框：分支 chip 照样留在底部
            gitAvailable ? (
              <div className="shrink-0 border-t border-border px-3 pb-[calc(0.5rem+env(safe-area-inset-bottom))] pt-1.5 md:px-4" data-testid="changes-footer">
                {branchChip('inline')}
              </div>
            ) : null
          ) : showPreview ? null : archived ? (
            <ArchivedBar
              leading={branchChip('inline')}
              busy={unarchiving}
              disabled={offline}
              running={running}
              onStop={() => void cancelTurn(nodeId, sessionId).catch((e: unknown) => toast.error(describeError(e, t)))}
              onUnarchive={() => {
                setUnarchiving(true)
                unarchiveSession(nodeId, sessionId)
                  .catch((e: unknown) => toast.error(describeUnarchiveError(e, t)))
                  .finally(() => setUnarchiving(false))
              }}
            />
          ) : (
            <Composer
              draftKey={sessionId}
              running={running}
              queueNotice={!agent?.capabilities?.interject}
              blockedReason={blockedReason}
              aside={branchChip('inline')}
              options={agent ? options : undefined}
              attachments={attachMimes ? { nodeId, accept: attachMimes, disabledReason: offline ? t('composer.attachOffline') : undefined } : undefined}
              focusToken={focusToken}
              autoFocus
              onStop={() => void cancelTurn(nodeId, sessionId).catch((e: unknown) => toast.error(describeError(e, t)))}
              onSend={async (text, attachments) => {
                const r = await sendMessage(nodeId, sessionId, text, attachments)
                if (r.state === 'failed') toast.error(failedText(r.reason, t))
              }}
            />
          )}
        </div>

        {xl && panelOpen ? (
          <aside className="hidden w-80 shrink-0 xl:block" aria-label={t('info.title')}>
            {panel}
          </aside>
        ) : null}
      </div>

      {!xl ? (
        <Drawer side="right" open={rightDrawer} onOpenChange={setRightDrawer} title={t('info.title')} closeLabel={t('common.close')}>
          {panel}
        </Drawer>
      ) : null}

      <RenameDialog
        open={renameOpen}
        onOpenChange={setRenameOpen}
        initial={title}
        onSubmit={async (name) => {
          await renameSession(nodeId, sessionId, name)
        }}
      />
    </>
  )
}

function RenameDialog({ open, onOpenChange, initial, onSubmit }: { open: boolean; onOpenChange: (v: boolean) => void; initial: string; onSubmit: (name: string) => Promise<void> }) {
  const { t } = useTranslation()
  const [name, setName] = useState(initial)
  useEffect(() => {
    if (open) setName(initial)
  }, [open, initial])
  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('info.renameTitle')} closeLabel={t('common.close')}>
      <form
        className="space-y-4"
        onSubmit={(e) => {
          e.preventDefault()
          const n = name.trim()
          if (!n) return
          onSubmit(n)
            .then(() => onOpenChange(false))
            .catch((err: unknown) => toast.error(describeError(err, t)))
        }}
      >
        <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus aria-label={t('info.renameTitle')} maxLength={200} data-testid="session-rename-input" />
        <div className="flex justify-end">
          <Button type="submit" variant="primary" disabled={!name.trim()}>
            {t('common.save')}
          </Button>
        </div>
      </form>
    </Dialog>
  )
}
