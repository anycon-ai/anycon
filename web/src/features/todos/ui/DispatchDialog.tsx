import { useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { AlertTriangle } from 'lucide-react'
import type { AgentInfo, SessionWorkspace } from '@/protocol'
import { SettingsControls } from '@/components/SettingsControls'
import { resolveDefaultsDetailed, selectionKey, toSessionSettings, type DefaultsResolution, type Selection } from '@/features/settings/resolve'
import { cleanHintText } from '@/lib/hint'
import { shortNodeId } from '@/lib/ids'
import { navigateTo } from '@/lib/navigate'
import { readyAgents, useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Select } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { declaresNoPush, describeDispatchError, dispatchTodo, previewDispatch } from '../dispatch'
import { useTodoHistory, historyKey } from '../history'
import { RpcCallError } from '@/transport/rpcClient'
import { isHumanSource } from '../source'
import type { ViewTodo } from '../types'
import { cleanOneLine } from '../validate'

/** 缺了哪个条件就不能派发：给按钮旁边的说明用。 */
export type DispatchBlock = 'offline' | 'noAgent' | 'noProject' | 'gitPending' | 'noPushUnsupported'

/**
 * 「让 agent 处理」（spec/web.md § 4.12，docs/spec/daemon.md § 22.6）：agent / 模型 / 权限模式（和新建会话页同一套控件和缺省值）、
 * 项目（这一项没有时必选，派发成功后由 daemon 写回）、工作位置（能开 worktree 的项目缺省新 worktree；不能的写明「直接在项目目录里改，
 * 没有隔离」）、「不 push、不开 PR」（缺省勾上；所选 agent 做不到时说明，取消勾选才能派发）、能展开看 daemon 将要发出的完整第一条消息。
 * 派发要节点在线（不进待发队列）。带着同一个 `message_id` 重试不会开第二个会话。
 */
export function DispatchDialog({ item, open, onOpenChange }: { item: ViewTodo; open: boolean; onOpenChange: (o: boolean) => void }) {
  const { t } = useTranslation()
  const nodeId = item.node_id
  const entry = useNodes((s) => s.byId[nodeId])
  const lastUsed = usePrefs((s) => s.lastUsed)
  const setLastUsed = usePrefs((s) => s.setLastUsed)
  const agents = readyAgents(entry)
  const [agentId, setAgentId] = useState<string | undefined>(undefined)
  const agent: AgentInfo | undefined = agents.find((a) => a.id === agentId) ?? agents[0]
  const offline = entry?.conn !== 'ready'

  // 控件里不出现「不 push」这一格：它是下面单独的勾选框
  const controlAgent = useMemo<AgentInfo | undefined>(() => (agent ? { ...agent, options: (agent.options ?? []).filter((o) => o.id !== 'no_push') } : undefined), [agent])
  const resolved = useMemo<DefaultsResolution | null>(
    () => (agent ? resolveDefaultsDetailed(agent, [lastUsed[selectionKey(nodeId, agent.id)], entry?.agentDefaults[agent.id]]) : null),
    // 只在换 agent / 节点默认变化时重算：用户自己改的选择不被覆盖（和新建会话页一样）
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [agent?.id, entry?.agentDefaults, agent?.probed_at_ms],
  )
  const [selection, setSelection] = useState<Selection | null>(resolved?.selection ?? null)
  useEffect(() => setSelection(resolved?.selection ?? null), [resolved])

  const projects = (entry?.projects ?? []).filter((p) => p.missing !== true)
  const [project, setProject] = useState(item.project ?? '')
  useEffect(() => {
    if (open) setProject(item.project ?? '')
  }, [open, item.id, item.project])
  const projectMeta = entry?.projects.find((p) => p.name === project)
  const gitPending = project !== '' && projectMeta?.git_pending === true
  const worktreeReady = !!entry?.info?.capabilities.worktree && projectMeta?.git?.worktree_ready === true
  const [pickedMode, setPickedMode] = useState<'worktree' | 'local'>('worktree')
  const mode: 'worktree' | 'local' = worktreeReady ? pickedMode : 'local'

  const declared = declaresNoPush(agent)
  const [noPush, setNoPush] = useState(true)
  // 派发有没有带「不 push」：勾着但 agent 做不到时不能发（要先取消勾选，agent 就可以 push 和开 PR）
  const block: DispatchBlock | null = offline ? 'offline' : agents.length === 0 ? 'noAgent' : project === '' ? 'noProject' : gitPending ? 'gitPending' : noPush && !declared ? 'noPushUnsupported' : null

  // ---- 完整的第一条消息（`todo.dispatch_preview`）：展开时取，这一项变了 / 勾选变了 / 有新结果记进来就重新取 ----
  const [previewOpen, setPreviewOpen] = useState(false)
  const [preview, setPreview] = useState<{ text: string | null; error: string | null; loading: boolean }>({ text: null, error: null, loading: false })
  const historyLen = useTodoHistory((s) => s.byKey[historyKey(nodeId, item.id)]?.events.length ?? 0)
  const previewSeq = useRef(0)
  useEffect(() => {
    if (!open || !previewOpen || offline) return
    const seq = ++previewSeq.current
    setPreview((p) => ({ ...p, loading: true, error: null }))
    previewDispatch(nodeId, item.id, noPush).then(
      (text) => seq === previewSeq.current && setPreview({ text, error: null, loading: false }),
      (e) => seq === previewSeq.current && setPreview({ text: null, error: describeDispatchError(e, t, false), loading: false }),
    )
    // 这一项的内容（标题、正文、派发次数、activity）和已知的「经过」条数任何一个变了，第一条消息可能就变了
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, previewOpen, offline, nodeId, item.id, item.title, item.body, item.updated_ms, item.sessions?.length, item.activity, noPush, historyLen])

  // ---- 发送：`message_id` ----
  // 只有「回执丢了」（链路断了 / 超时，不知道这次派发有没有成功）之后的重试才沿用上一次的 id（同一个 id 不会开第二个会话）。
  // 节点回了错误、派发成功、或者这一项在这期间多了会话（说明上一次其实成功了），下一次发送都换新的：
  // 之后同样参数的一次有意的重新派发不能悄悄返回第一次的会话。
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const idRef = useRef<{ sig: string; id: string; sessions: number; attempt: { agentId: string; project: string; noPush: boolean; mode: 'worktree' | 'local' } } | null>(null)
  const [pendingNote, setPendingNote] = useState(false)
  const sessionCount = item.sessions?.length ?? 0
  const messageIdFor = (sig: string, attempt: { agentId: string; project: string; noPush: boolean; mode: 'worktree' | 'local' }): string => {
    const prev = idRef.current
    const reuse = prev && prev.sig === sig && prev.sessions === sessionCount
    const id = reuse ? prev.id : crypto.randomUUID()
    idRef.current = { sig, id, sessions: sessionCount, attempt }
    return id
  }

  // 每次打开都从头来：「不 push」重新勾上、旧的错误和预览清掉（上一次关掉时的状态不带进下一次派发）。
  // 唯一的例外：上一次的回执丢了、那次可能已经成功（还留着它的 id）——恢复那次的参数（agent、项目、「不 push」、工作位置）并写明，
  // 这样重试的是同一次尝试；要是这一项在这期间已经多了会话，那次就是成功了，不再沿用
  useEffect(() => {
    if (!open) return
    setError(null)
    setPreviewOpen(false)
    setPreview({ text: null, error: null, loading: false })
    const pending = idRef.current
    if (pending && pending.sessions === sessionCount) {
      setNoPush(pending.attempt.noPush)
      setAgentId(pending.attempt.agentId)
      setProject(pending.attempt.project)
      setPickedMode(pending.attempt.mode)
      setPendingNote(true)
    } else {
      idRef.current = null
      setNoPush(true)
      setPickedMode('worktree')
      setPendingNote(false)
    }
    // 只在打开的那一刻做；之后会话数怎么变都不重置用户正在改的参数
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const send = async (): Promise<void> => {
    if (!agent || !selection || block || busy) return
    setBusy(true)
    setError(null)
    const workspace: SessionWorkspace | undefined = worktreeReady ? (mode === 'worktree' ? { mode: 'worktree' } : { mode: 'local' }) : undefined
    // 「不 push」只由下面的勾选框决定（daemon 会覆盖同名选项）：不让它混进设置和「上次用的」里
    const options = Object.fromEntries(Object.entries(selection.options).filter(([k]) => k !== 'no_push'))
    const settings = toSessionSettings({ ...selection, options })
    try {
      const res = await dispatchTodo(nodeId, { id: item.id, agent: agent.id, project, ...(workspace ? { workspace } : {}), settings, noPush, messageId: messageIdFor(`${agent.id}\u0000${project}\u0000${noPush}\u0000${worktreeReady ? mode : 'default'}`, { agentId: agent.id, project, noPush, mode }) })
      idRef.current = null
      setPendingNote(false)
      setLastUsed(selectionKey(nodeId, agent.id), { model: selection.model, thinking: selection.thinking, mode: selection.mode, options })
      onOpenChange(false)
      const node = shortNodeId(nodeId, useNodes.getState().order)
      toast.info(t('dispatch.sent', { agent: cleanHintText(agent.label) || agent.id }), { label: t('dispatch.openSession'), run: () => navigateTo(`/n/${node}/s/${res.session.id}`) }, 10_000)
    } catch (e) {
      // 节点明确回了错误（没有开会话）：下一次发送换新的 id；链路断了（不知道成没成）：留着 id，重试安全
      if (e instanceof RpcCallError && e.kind === 'rpc') {
        idRef.current = null
        setPendingNote(false)
      }
      setError(describeDispatchError(e, t, worktreeReady && mode === 'worktree'))
    } finally {
      setBusy(false)
    }
  }

  const blockText = block === 'offline' ? t('dispatch.blocked.offline') : block === 'noAgent' ? t('dispatch.blocked.noAgent') : block === 'noProject' ? t('dispatch.blocked.noProject') : block === 'gitPending' ? t('dispatch.blocked.gitPending') : block === 'noPushUnsupported' ? t('dispatch.blocked.noPush', { agent: cleanHintText(agent?.label) || agent?.id || '' }) : null
  const unreviewed = item.unreviewed && !isHumanSource(item.source)

  return (
    <Dialog open={open} onOpenChange={(o) => !busy && onOpenChange(o)} title={t('dispatch.title')} description={cleanOneLine(item.title, 80)} closeLabel={t('common.close')} testId="dispatch-dialog" size="lg">
      <div className="space-y-3 text-sm">
        {unreviewed ? (
          <p className="rounded-md border border-border bg-bg-subtle p-2 text-fg-muted" data-testid="dispatch-accepts-note">
            {t('dispatch.acceptsNote')}
          </p>
        ) : null}

        {pendingNote ? (
          <p className="rounded-md border border-warning/40 bg-warning-subtle p-2" role="status" data-testid="dispatch-pending-note">
            {t('dispatch.pendingAttempt')}
          </p>
        ) : null}

        {agents.length > 1 ? (
          <label className="block space-y-1 font-medium">
            {t('options.agent')}
            <Select value={agent?.id ?? ''} onChange={(e) => setAgentId(e.target.value)} data-testid="dispatch-agent">
              {agents.map((a) => (
                <option key={a.id} value={a.id}>
                  {cleanHintText(a.label) || a.id}
                </option>
              ))}
            </Select>
          </label>
        ) : null}

        {controlAgent && selection ? (
          <div data-testid="dispatch-settings">
            <SettingsControls agent={controlAgent} value={selection} onChange={setSelection} dangerousForbidden={entry?.dangerousForbidden} customKey={`${nodeId}:${controlAgent.id}`} idPrefix="dispatch" layout="stacked" />
          </div>
        ) : null}

        <label className="block space-y-1 font-medium">
          {t('todos.project')}
          <Select value={project} onChange={(e) => setProject(e.target.value)} data-testid="dispatch-project">
            <option value="">{item.project ? '—' : t('dispatch.pickProject')}</option>
            {item.project && !projects.some((p) => p.name === item.project) ? <option value={item.project}>{cleanOneLine(item.project)}</option> : null}
            {projects.map((p) => (
              <option key={p.name} value={p.name}>
                {p.name}
              </option>
            ))}
          </Select>
        </label>
        {!item.project && project !== '' ? <p className="text-xs text-fg-muted">{t('dispatch.projectWrittenBack')}</p> : null}

        {project !== '' && !gitPending ? (
          <fieldset className="space-y-1" data-testid="dispatch-workspace">
            <legend className="font-medium">{t('dispatch.workspace')}</legend>
            {worktreeReady ? (
              <>
                <label className="flex items-center gap-2">
                  <input type="radio" name="dispatch-workspace" checked={mode === 'worktree'} onChange={() => setPickedMode('worktree')} className="accent-[var(--accent)]" data-testid="dispatch-workspace-worktree" />
                  {t('dispatch.workspaceWorktree')}
                </label>
                <label className="flex items-center gap-2">
                  <input type="radio" name="dispatch-workspace" checked={mode === 'local'} onChange={() => setPickedMode('local')} className="accent-[var(--accent)]" data-testid="dispatch-workspace-local" />
                  {t('dispatch.workspaceLocal')}
                </label>
              </>
            ) : null}
            {mode === 'local' ? (
              <p className="flex items-start gap-1.5 rounded-md border border-warning/40 bg-warning-subtle p-2 text-fg" role="note" data-testid="dispatch-no-isolation">
                <AlertTriangle className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
                {worktreeReady ? t('dispatch.noIsolation') : t('dispatch.noIsolationNoWorktree')}
              </p>
            ) : null}
          </fieldset>
        ) : null}

        <div className="space-y-1">
          <label className="flex items-start gap-2 font-medium">
            <input
              type="checkbox"
              checked={noPush}
              onChange={(e) => setNoPush(e.target.checked)}
              className="mt-0.5 accent-[var(--accent)]"
              data-testid="dispatch-no-push"
            />
            {t('dispatch.noPush')}
          </label>
          {noPush && agent && !declared ? (
            <p className="flex items-start gap-1.5 rounded-md border border-warning/40 bg-warning-subtle p-2" role="alert" data-testid="dispatch-no-push-warning">
              <AlertTriangle className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
              {t('dispatch.noPushUnsupported', { agent: cleanHintText(agent.label) || agent.id })}
            </p>
          ) : null}
          {!noPush ? (
            <p className="rounded-md border border-warning/40 bg-warning-subtle p-2" role="note" data-testid="dispatch-push-allowed">
              {t('dispatch.pushAllowed')}
            </p>
          ) : (
            <p className="text-xs text-fg-muted">{t('dispatch.noPushHint')}</p>
          )}
        </div>

        <div className="space-y-1">
          <button type="button" aria-expanded={previewOpen} onClick={() => setPreviewOpen((o) => !o)} className="text-sm font-medium text-accent-text hover:underline" data-testid="dispatch-preview-toggle">
            {previewOpen ? t('dispatch.hidePreview') : t('dispatch.showPreview')}
          </button>
          {previewOpen ? (
            <div className="space-y-1" data-testid="dispatch-preview-box">
              <p className="text-xs text-fg-muted">{t('dispatch.previewNote')}</p>
              {preview.loading && preview.text === null ? <p className="text-fg-muted" role="status">{t('dispatch.previewLoading')}</p> : null}
              {preview.error ? (
                <p className="text-danger" role="alert" data-testid="dispatch-preview-error">
                  {preview.error}
                </p>
              ) : null}
              {preview.text !== null ? (
                <pre className="scroll-thin max-h-64 overflow-auto rounded-md border border-border bg-bg-subtle p-2 text-xs whitespace-pre-wrap [overflow-wrap:anywhere]" data-testid="dispatch-preview" aria-busy={preview.loading}>
                  {preview.text}
                </pre>
              ) : null}
            </div>
          ) : null}
        </div>

        {blockText ? (
          <p className="text-fg-muted" role="status" data-testid="dispatch-blocked" data-reason={block}>
            {blockText}
          </p>
        ) : null}
        {error ? (
          <p className="text-danger" role="alert" data-testid="dispatch-error">
            {error}
          </p>
        ) : null}
        <div className="flex justify-end gap-2">
          <Button onClick={() => onOpenChange(false)} disabled={busy} data-testid="dispatch-cancel">
            {t('common.cancel')}
          </Button>
          <Button variant="primary" disabled={busy || block !== null || !selection} onClick={() => void send()} data-testid="dispatch-send">
            {busy ? t('dispatch.sending') : t('dispatch.send')}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
