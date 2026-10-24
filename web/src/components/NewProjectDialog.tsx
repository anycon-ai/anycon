import { useEffect, useState, type FormEvent } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { Lock } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { cancelJob, cloneProject, createProject, linkProject } from '@/features/workspace/actions'
import { caseInsensitiveHost, defaultProjectName, nameClash, projectNameProblem } from '@/features/workspace/paths'
import { describeError, describeErrorIn } from '@/lib/errors'
import { useNodeParam } from '@/lib/nav'
import type { NewProjectMode } from '@/stores/newProject'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input, Select } from '@/ui/Field'
import { Dialog } from '@/ui/Overlay'
import { Segmented } from '@/ui/Misc'
import { FolderPicker } from './FolderPicker'

export type { NewProjectMode }

/** 从仓库 URL 推导目录名（daemon 未给名字时的默认行为）。 */
export function repoName(url: string): string {
  const last = url.replace(/\/+$/, '').split(/[/:]/).pop() ?? ''
  return last.replace(/\.git$/, '')
}

/**
 * 新建项目：空项目（名称、git init）/ 从 Git 克隆（URL、名称可选，进度条走 job.progress）/
 * 已有目录（目录选择器 + 名称，提交 `workspace.link`，ADR-33）。
 * `initialMode` 决定每次打开时停在哪一项（空状态的三个按钮各开一项）。
 * `then`：建好之后去项目页（默认），还是去新建会话页并选中这个项目。
 * `onNodeChange` 有值且这个浏览器配对了不止一台节点时，顶上多一个节点下拉（从左栏、命令面板打开时）。
 */
export function NewProjectDialog({
  nodeId,
  open,
  onOpenChange,
  initialMode = 'empty',
  then = 'project',
  onNodeChange,
}: {
  nodeId: string
  open: boolean
  onOpenChange: (v: boolean) => void
  initialMode?: NewProjectMode
  then?: 'project' | 'newSession'
  onNodeChange?: (nodeId: string) => void
}) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const nodeParam = useNodeParam(nodeId)
  const order = useNodes((s) => s.order)
  const byId = useNodes((s) => s.byId)
  const [mode, setMode] = useState<NewProjectMode>(initialMode)
  const [name, setName] = useState('')
  const [gitInit, setGitInit] = useState(true)
  const [url, setUrl] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [jobId, setJobId] = useState<string | null>(null)
  const [cloned, setCloned] = useState('')
  // 已有目录
  const [picked, setPicked] = useState<string | null>(null)
  const [linkName, setLinkName] = useState('')
  const [linkNameTouched, setLinkNameTouched] = useState(false)
  const [linkUnavailable, setLinkUnavailable] = useState(false)
  const job = useNodes((s) => (jobId ? s.byId[nodeId]?.jobs[jobId] : undefined))
  const taken = useNodes((s) => s.byId[nodeId]?.projects)
  const platform = useNodes((s) => s.byId[nodeId]?.info?.platform)
  // 节点离线、或者它不管理 workspace（`capabilities.workspace = false`）：这里建不了项目
  const usable = (id: string): boolean => byId[id]?.conn === 'ready' && byId[id]?.info?.capabilities.workspace !== false
  const unusableReason = !byId[nodeId] || byId[nodeId]?.conn !== 'ready' ? t('project.nodeOffline') : !usable(nodeId) ? t('project.nodeNoWorkspace') : null

  useEffect(() => {
    if (open) setMode(initialMode)
  }, [open, initialMode])

  // 换了节点：上一台节点上选的目录、报错都不算数了
  useEffect(() => {
    setPicked(null)
    setLinkName('')
    setLinkNameTouched(false)
    setLinkUnavailable(false)
    setError(null)
  }, [nodeId])

  useEffect(() => {
    if (!open) {
      setName('')
      setUrl('')
      setError(null)
      setJobId(null)
      setBusy(false)
      setPicked(null)
      setLinkName('')
      setLinkNameTouched(false)
      setLinkUnavailable(false)
    }
  }, [open])

  const goTo = (project: string): Promise<void> =>
    then === 'newSession' ? navigate({ to: '/n/$node/new', params: { node: nodeParam }, search: { project } }) : navigate({ to: '/n/$node/p/$project', params: { node: nodeParam, project } })

  // 克隆完成
  useEffect(() => {
    if (!job?.done) return
    setBusy(false)
    if (job.ok) {
      toast.success(t('project.cloned'))
      onOpenChange(false)
      if (cloned) void goTo(cloned)
    } else {
      setError(job.error || t('project.cloneFailed'))
      setJobId(null)
    }
  }, [job?.done, job?.ok, job?.error, onOpenChange, t]) // eslint-disable-line react-hooks/exhaustive-deps

  const pick = (path: string): void => {
    setPicked(path)
    setError(null)
    // 名称默认取目录名；用户改过之后换目录不再覆盖
    if (!linkNameTouched) setLinkName(defaultProjectName(path))
  }

  const linkProblem = projectNameProblem(linkName)
  // 本地重名检查（和 daemon 一致：NFC 之后比较；不区分大小写的文件系统上再忽略大小写）。
  // 只有大小写不同时：macOS / Windows 上一定被拒绝，别处（Linux）不一定，所以那里只提示、不拦。
  const clash = linkProblem === null ? nameClash((taken ?? []).map((p) => p.name), linkName) : null
  const linkNameExists = clash !== null && (clash.kind === 'exact' || caseInsensitiveHost(platform))
  const linkNameError =
    linkProblem === 'invalid'
      ? t('project.nameInvalid')
      : linkProblem === 'too_long'
        ? t('project.nameTooLong')
        : clash?.kind === 'exact'
          ? t('project.nameExists')
          : linkNameExists
            ? t('project.nameCaseClash', { name: clash?.with })
            : // 默认名不合法（比如根目录）时提醒要自己起个名，但还没输入过就不报红
              linkProblem === 'empty' && linkNameTouched
              ? t('project.nameInvalid')
              : undefined
  const linkNameHint = clash?.kind === 'case' && !linkNameExists ? t('project.nameCase', { name: clash.with }) : undefined

  const submit = async (e: FormEvent): Promise<void> => {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      if (mode === 'empty') {
        const created = name.trim()
        await createProject(nodeId, created, gitInit)
        onOpenChange(false)
        setBusy(false)
        await goTo(created)
      } else if (mode === 'existing') {
        if (!picked) return
        const linked = await linkProject(nodeId, picked, linkName.trim())
        onOpenChange(false)
        setBusy(false)
        await goTo(linked.name)
      } else {
        setCloned(name.trim() || repoName(url.trim()))
        setJobId(await cloneProject(nodeId, url.trim(), name.trim() || undefined))
      }
    } catch (err) {
      setError(mode === 'existing' ? describeErrorIn(err, t, 'project.linkErrors') : describeError(err, t))
      setBusy(false)
    }
  }

  const valid =
    unusableReason === null &&
    (mode === 'empty' ? name.trim().length > 0 : mode === 'clone' ? url.trim().length > 0 : picked !== null && !linkUnavailable && linkProblem === null && !linkNameExists)

  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('project.newTitle')} closeLabel={t('common.close')} testId="project-new-dialog" size={mode === 'existing' ? 'lg' : 'md'}>
      <form onSubmit={(e) => void submit(e)} className="space-y-4">
        {onNodeChange && order.length > 1 ? (
          <Field label={t('project.node')} htmlFor="project-new-node">
            <Select id="project-new-node" data-testid="project-new-node" value={nodeId} onChange={(e) => onNodeChange(e.target.value)} disabled={busy}>
              {order.map((id) => (
                <option key={id} value={id}>
                  {usable(id) ? byId[id]?.name : t('project.nodeOptionUnavailable', { name: byId[id]?.name ?? id.slice(0, 8) })}
                </option>
              ))}
            </Select>
          </Field>
        ) : null}
        {unusableReason ? (
          <p role="status" className="text-sm text-warning" data-testid="project-new-unavailable">
            {unusableReason}
          </p>
        ) : null}
        <Segmented
          ariaLabel={t('project.newTitle')}
          fill
          value={mode}
          onChange={(m) => {
            setMode(m)
            setError(null)
          }}
          disabled={busy}
          options={[
            { value: 'empty', label: t('project.modeEmpty'), testId: 'project-new-mode-empty' },
            { value: 'clone', label: t('project.modeClone'), testId: 'project-new-mode-clone' },
            {
              value: 'existing',
              label: linkUnavailable ? (
                <span className="inline-flex items-center gap-1">
                  <Lock className="size-3" aria-hidden />
                  {t('project.modeExisting')}
                </span>
              ) : (
                t('project.modeExisting')
              ),
              testId: 'project-new-mode-existing',
              title: linkUnavailable ? t('folderPicker.unavailableShort') : undefined,
            },
          ]}
        />
        {mode === 'existing' ? (
          <>
            {unusableReason ? null : <FolderPicker key={nodeId} nodeId={nodeId} selected={picked} onSelect={pick} onUnavailable={() => setLinkUnavailable(true)} />}
            {!linkUnavailable && !unusableReason ? (
              <>
                <p className="text-sm" data-testid="folder-selected">
                  <span className="text-fg-subtle">{t('folderPicker.selectedFolder')}: </span>
                  {picked ? <span className="break-all font-mono text-xs">{picked}</span> : <span className="text-fg-subtle">{t('folderPicker.nothingSelected')}</span>}
                </p>
                <Field label={t('project.name')} htmlFor="project-link-name" error={linkNameError} hint={linkNameHint}>
                  <Input
                    id="project-link-name"
                    data-testid="project-link-name"
                    value={linkName}
                    onChange={(e) => {
                      setLinkName(e.target.value)
                      setLinkNameTouched(true)
                    }}
                    disabled={busy || picked === null}
                    autoCapitalize="off"
                    spellCheck={false}
                    placeholder="my-app"
                  />
                </Field>
              </>
            ) : null}
          </>
        ) : null}
        {mode === 'clone' ? (
          <Field label={t('project.cloneUrl')} htmlFor="project-clone-url" hint={t('project.cloneHint')}>
            <Input id="project-clone-url" data-testid="project-clone-url" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://github.com/org/repo.git" disabled={busy} autoFocus autoCapitalize="off" spellCheck={false} />
          </Field>
        ) : null}
        {mode !== 'existing' ? (
          <Field label={mode === 'clone' ? t('project.nameOptional') : t('project.name')} htmlFor="project-name">
            <Input id="project-name" data-testid="project-new-name" value={name} onChange={(e) => setName(e.target.value)} disabled={busy} autoFocus={mode === 'empty'} autoCapitalize="off" spellCheck={false} placeholder="my-app" />
          </Field>
        ) : null}
        {mode === 'empty' ? (
          <label className="flex items-center gap-2 text-sm">
            <input type="checkbox" checked={gitInit} onChange={(e) => setGitInit(e.target.checked)} disabled={busy} className="size-4 accent-[var(--accent)]" />
            {t('project.gitInit')}
          </label>
        ) : null}
        {job && !job.done ? (
          <div role="progressbar" aria-valuenow={job.percent ?? undefined} aria-valuemin={0} aria-valuemax={100} aria-label={t('project.cloning')} className="space-y-1" data-testid="clone-progress">
            <div className="h-1.5 overflow-hidden rounded-full bg-bg-muted">
              <div className="h-full bg-accent transition-all" style={{ width: `${Math.max(5, job.percent ?? 5)}%` }} />
            </div>
            <p className="truncate text-xs text-fg-subtle">{job.log || job.stage}</p>
          </div>
        ) : null}
        {error ? (
          <p role="alert" className="text-sm text-danger" data-testid="project-new-error">
            {error}
          </p>
        ) : null}
        <div className="flex justify-end gap-2">
          {jobId && job && !job.done ? (
            <Button variant="outline" onClick={() => void cancelJob(nodeId, jobId).catch((e: unknown) => toast.error(describeError(e, t)))}>
              {t('common.cancel')}
            </Button>
          ) : null}
          <Button type="submit" variant="primary" loading={busy} disabled={!valid} data-testid="project-new-submit">
            {mode === 'clone' ? t('project.clone') : mode === 'existing' ? t('project.link') : t('project.create')}
          </Button>
        </div>
      </form>
    </Dialog>
  )
}
