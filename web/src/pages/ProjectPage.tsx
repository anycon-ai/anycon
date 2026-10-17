import { useMemo, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { FolderPen, GitBranch, MoreHorizontal, Plus, Trash2, Unlink } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { MissingProjectNotice } from '@/components/MissingProjectNotice'
import { PageHeader } from '@/components/PageHeader'
import { ProjectBadges } from '@/components/ProjectCard'
import { SessionListItem } from '@/components/SessionListItem'
import { isActiveStatus } from '@/features/session/group'
import { renameProject, trashProject, unlinkProject } from '@/features/workspace/actions'
import { describeError, describeErrorIn } from '@/lib/errors'
import { useCurrentNode, useNodeParam } from '@/lib/nav'
import { useSessions } from '@/stores/sessions'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Field, Input } from '@/ui/Field'
import { Menu, MenuContent, MenuItem, MenuTrigger } from '@/ui/Menu'
import { Chip, EmptyState, Segmented } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

type Filter = 'all' | 'active' | 'waiting' | 'archived'

export function ProjectPage({ project }: { project: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { nodeId, entry } = useCurrentNode()
  const nodeParam = useNodeParam(nodeId)
  const list = useSessions((s) => s.list)
  const [filter, setFilter] = useState<Filter>('all')
  const [renameOpen, setRenameOpen] = useState(false)
  const [newName, setNewName] = useState(project)
  const [trashOpen, setTrashOpen] = useState(false)
  const [removeOpen, setRemoveOpen] = useState(false)
  const [removeError, setRemoveError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const sessions = useMemo(
    () =>
      Object.values(list)
        .filter((s) => s.node_id === nodeId && s.project === project)
        .filter((s) => (filter === 'archived' ? s.archived : !s.archived))
        .filter((s) => (filter === 'active' ? isActiveStatus(s.status) : filter === 'waiting' ? s.status === 'waiting_permission' || s.pending_permissions > 0 : true))
        .sort((a, b) => b.updated_at_ms - a.updated_at_ms),
    [list, nodeId, project, filter],
  )
  if (!nodeId || !entry) return null
  const meta = entry.projects.find((p) => p.name === project)
  const offline = entry.conn !== 'ready'
  const linked = meta?.kind === 'linked'
  const missing = meta?.missing === true

  const onRename = async (): Promise<void> => {
    setBusy(true)
    try {
      await renameProject(nodeId, project, newName.trim())
      setRenameOpen(false)
      await navigate({ to: '/n/$node/p/$project', params: { node: nodeParam, project: newName.trim() }, replace: true })
    } catch (e) {
      toast.error(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }
  const onTrash = async (): Promise<void> => {
    setBusy(true)
    try {
      await trashProject(nodeId, project)
      setTrashOpen(false)
      await navigate({ to: '/n/$node', params: { node: nodeParam } })
    } catch (e) {
      toast.error(describeError(e, t))
    } finally {
      setBusy(false)
    }
  }

  const askRemove = (): void => {
    setRemoveError(null)
    setRemoveOpen(true)
  }
  const onRemove = async (): Promise<void> => {
    setBusy(true)
    setRemoveError(null)
    try {
      await unlinkProject(nodeId, project)
      setRemoveOpen(false)
      toast.success(t('project.removed', { name: project }))
      await navigate({ to: '/n/$node', params: { node: nodeParam } })
    } catch (e) {
      // 留在对话框里说明原因（比如还有运行中的会话），用户处理完可以直接重试
      setRemoveError(describeErrorIn(e, t, 'project.removeErrors'))
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <PageHeader
        crumbs={[{ label: entry.name, to: { to: '/n/$node', params: { node: nodeParam } } }, { label: project }]}
        actions={
          <>
            {missing ? null : (
              <Button variant="primary" size="sm" icon={<Plus className="size-4" />} onClick={() => void navigate({ to: '/n/$node/new', params: { node: nodeParam }, search: { project } })} data-testid="project-new-session">
                {t('sidebar.newSession')}
              </Button>
            )}
            <Menu>
              <MenuTrigger asChild>
                <Button variant="ghost" size="icon" aria-label={t('project.menu')} disabled={offline} data-testid="project-menu">
                  <MoreHorizontal className="size-4" />
                </Button>
              </MenuTrigger>
              <MenuContent align="end">
                {missing ? null : (
                  <MenuItem icon={<FolderPen className="size-4" />} onSelect={() => { setNewName(project); setRenameOpen(true) }}>
                    {t('project.rename')}
                  </MenuItem>
                )}
                {linked ? (
                  // 关联的项目只能从列表移除（不动目录里的文件）；根目录下的项目才能进回收站
                  <MenuItem icon={<Unlink className="size-4" />} danger onSelect={askRemove} testId="project-remove">
                    {t('project.remove')}
                  </MenuItem>
                ) : (
                  <MenuItem icon={<Trash2 className="size-4" />} danger onSelect={() => setTrashOpen(true)}>
                    {t('project.trash')}
                  </MenuItem>
                )}
              </MenuContent>
            </Menu>
          </>
        }
      />
      <div className="flex min-h-0 flex-1 flex-col">
        {/* 新建会话在单独的空白页（头部「新会话」、左栏项目旁的「＋」），这里只是项目的会话历史和管理 */}
        <div className="scroll-thin min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto w-full max-w-3xl space-y-5 p-4 md:p-6">
            <div className="space-y-1">
              <div className="flex flex-wrap items-center gap-2">
                <h1 className="text-xl font-semibold">{project}</h1>
                {meta ? <ProjectBadges project={meta} /> : null}
              </div>
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-fg-subtle">
                {meta?.git?.branch ? (
                  <span className="inline-flex items-center gap-1">
                    <GitBranch className="size-3" aria-hidden />
                    {meta.git.branch}
                  </span>
                ) : null}
                {meta?.git?.dirty ? <Chip tone="warning">{t('project.dirty')}</Chip> : null}
                {meta?.path ? (
                  <span className="break-all font-mono" data-testid="project-path">
                    {meta.path}
                  </span>
                ) : null}
              </div>
            </div>

            {meta && missing ? (
              <MissingProjectNotice
                project={meta}
                action={
                  <Button variant="outline" size="sm" icon={<Unlink className="size-4" />} disabled={offline} onClick={askRemove} data-testid="project-missing-remove">
                    {t('project.remove')}
                  </Button>
                }
              />
            ) : null}

            <Segmented
              ariaLabel={t('project.filter')}
              value={filter}
              onChange={setFilter}
              options={[
                { value: 'all', label: t('project.filterAll') },
                { value: 'active', label: t('project.filterActive') },
                { value: 'waiting', label: t('project.filterWaiting') },
                { value: 'archived', label: t('project.filterArchived') },
              ]}
            />

            {sessions.length === 0 ? (
              <EmptyState title={filter === 'all' ? t('project.noSessions') : t('project.noMatch')}>{filter === 'all' ? t('project.noSessionsHint') : undefined}</EmptyState>
            ) : (
              <ul className="space-y-2">
                {sessions.map((s) => (
                  <li key={s.id}>
                    <SessionListItem session={s} />
                  </li>
                ))}
              </ul>
            )}
          </div>
        </div>
      </div>

      <Dialog open={renameOpen} onOpenChange={setRenameOpen} title={t('project.rename')} closeLabel={t('common.close')}>
        <form className="space-y-4" onSubmit={(e) => { e.preventDefault(); void onRename() }}>
          <Field label={t('project.name')} htmlFor="rename-project">
            <Input id="rename-project" value={newName} onChange={(e) => setNewName(e.target.value)} autoFocus autoCapitalize="off" spellCheck={false} />
          </Field>
          <div className="flex justify-end">
            <Button type="submit" variant="primary" loading={busy} disabled={!newName.trim() || newName.trim() === project}>
              {t('common.save')}
            </Button>
          </div>
        </form>
      </Dialog>
      <Dialog open={removeOpen} onOpenChange={setRemoveOpen} title={t('project.removeTitle', { name: project })} closeLabel={t('common.close')} testId="project-remove-dialog">
        <div className="space-y-4">
          <p className="text-sm text-fg-muted">{t('project.removeBody')}</p>
          {meta?.path ? <p className="break-all rounded-md bg-bg-muted px-2 py-1.5 font-mono text-xs">{meta.path}</p> : null}
          <p className="text-sm text-fg-muted">{t('project.removeSessions')}</p>
          {removeError ? (
            <p role="alert" className="text-sm text-danger" data-testid="project-remove-error">
              {removeError}
            </p>
          ) : null}
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={() => setRemoveOpen(false)}>
              {t('common.cancel')}
            </Button>
            <Button variant="danger" loading={busy} onClick={() => void onRemove()} data-testid="project-remove-confirm">
              {t('project.removeConfirm')}
            </Button>
          </div>
        </div>
      </Dialog>
      <Dialog open={trashOpen} onOpenChange={setTrashOpen} title={t('project.trashTitle', { name: project })} description={t('project.trashBody')} closeLabel={t('common.close')}>
        <div className="flex justify-end gap-2">
          <Button variant="outline" onClick={() => setTrashOpen(false)}>
            {t('common.cancel')}
          </Button>
          <Button variant="danger" loading={busy} onClick={() => void onTrash()} data-testid="project-trash-confirm">
            {t('project.trash')}
          </Button>
        </div>
      </Dialog>
    </>
  )
}
