import { useMemo, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { FolderOpen, FolderPlus, GitFork, LayoutGrid, List, Plus, Settings, Sparkles } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { NewProjectDialog, type NewProjectMode } from '@/components/NewProjectDialog'
import { NoAgentsNotice } from '@/components/AgentIssueCards'
import { PageHeader } from '@/components/PageHeader'
import { ProjectCard } from '@/components/ProjectCard'
import { PromoteDialog } from '@/components/PromoteDialog'
import { SessionListItem } from '@/components/SessionListItem'
import { readyAgents } from '@/stores/nodes'
import { useCurrentNode, useNodeParam } from '@/lib/nav'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Card, Chip, EmptyState, SectionTitle, Skeleton } from '@/ui/Misc'

export function NodeHomePage() {
  const { t } = useTranslation()
  const { nodeId, entry } = useCurrentNode()
  const nodeParam = useNodeParam(nodeId)
  const view = usePrefs((s) => s.projectView)
  const setPrefs = usePrefs((s) => s.set)
  const list = useSessions((s) => s.list)
  const [newOpen, setNewOpen] = useState(false)
  const [newMode, setNewMode] = useState<NewProjectMode>('empty')
  const [promote, setPromote] = useState<string | null>(null)

  const sessions = useMemo(
    () =>
      Object.values(list)
        .filter((s) => s.node_id === nodeId && !s.archived)
        .sort((a, b) => b.updated_at_ms - a.updated_at_ms),
    [list, nodeId],
  )
  if (!nodeId || !entry) return null
  const offline = entry.conn !== 'ready'
  const projects = [...entry.projects].sort((a, b) => b.modified_at_ms - a.modified_at_ms)
  const scratch = sessions.filter((s) => !s.project)
  const recent = sessions.slice(0, 8)
  const hasAgents = readyAgents(entry).length > 0
  const workspace = entry.info?.capabilities.workspace !== false
  const openNew = (mode: NewProjectMode): void => {
    setNewMode(mode)
    setNewOpen(true)
  }

  return (
    <>
      <PageHeader
        crumbs={[{ label: entry.name }]}
        actions={
          <Link to="/n/$node/settings" params={{ node: nodeParam }} aria-label={t('node.settings')}>
            <Button variant="ghost" size="icon" tabIndex={-1}>
              <Settings className="size-4" />
            </Button>
          </Link>
        }
      />
      <div className="scroll-thin min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-4xl space-y-8 p-4 md:p-6">
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
            <h1 className="text-xl font-semibold">{entry.name}</h1>
            {entry.info ? (
              <span className="text-sm text-fg-subtle">
                {entry.info.platform} · v{entry.info.version}
              </span>
            ) : null}
            {entry.info?.workspace_root ? <span className="truncate font-mono text-xs text-fg-subtle">{entry.info.workspace_root}</span> : null}
          </div>

          {entry.agentsLoaded && !hasAgents ? <NoAgentsNotice agents={entry.agents} /> : null}

          {workspace ? (
            <section aria-labelledby="projects-title">
              <SectionTitle
                action={
                  <div className="flex items-center gap-1">
                    <div className="hidden rounded-md border border-border sm:flex">
                      <button type="button" aria-label={t('node.viewCards')} aria-pressed={view === 'cards'} onClick={() => setPrefs({ projectView: 'cards' })} className={cn('rounded-l-md p-1.5', view === 'cards' ? 'bg-bg-muted' : 'text-fg-subtle')}>
                        <LayoutGrid className="size-4" />
                      </button>
                      <button type="button" aria-label={t('node.viewList')} aria-pressed={view === 'list'} onClick={() => setPrefs({ projectView: 'list' })} className={cn('rounded-r-md p-1.5', view === 'list' ? 'bg-bg-muted' : 'text-fg-subtle')}>
                        <List className="size-4" />
                      </button>
                    </div>
                    <Button size="sm" variant="primary" icon={<Plus className="size-4" />} disabled={offline} onClick={() => openNew('empty')} data-testid="project-new">
                      {t('project.new')}
                    </Button>
                  </div>
                }
              >
                <span id="projects-title">{t('node.projects')}</span>
              </SectionTitle>

              {!entry.projectsLoaded && entry.conn === 'ready' ? (
                <div className="grid gap-3 sm:grid-cols-2">
                  <Skeleton className="h-20" />
                  <Skeleton className="h-20" />
                </div>
              ) : !entry.projectsLoaded ? (
                // 离线且没有缓存：不能说「没有项目」，只是现在看不到
                <p className="text-sm text-fg-subtle" data-testid="projects-unavailable">
                  {t('node.projectsOffline')}
                </p>
              ) : projects.length === 0 ? (
                <Card className="p-2">
                  <EmptyState
                    icon={<FolderPlus className="size-10" />}
                    title={t('node.noProjects')}
                    action={
                      <div className="flex flex-wrap justify-center gap-2">
                        <Button variant="primary" icon={<Plus className="size-4" />} disabled={offline} onClick={() => openNew('empty')} data-testid="project-new-empty">
                          {t('project.new')}
                        </Button>
                        <Button variant="outline" icon={<GitFork className="size-4" />} disabled={offline} onClick={() => openNew('clone')} data-testid="project-clone-empty">
                          {t('project.newClone')}
                        </Button>
                        <Button variant="outline" icon={<FolderOpen className="size-4" />} disabled={offline} onClick={() => openNew('existing')} data-testid="project-open-empty">
                          {t('project.openExisting')}
                        </Button>
                      </div>
                    }
                  >
                    {t('node.noProjectsHint')}
                  </EmptyState>
                </Card>
              ) : (
                <div className={cn('grid gap-3', view === 'cards' ? 'sm:grid-cols-2 lg:grid-cols-3' : 'grid-cols-1')}>
                  {projects.map((p) => (
                    <ProjectCard key={p.name} nodeParam={nodeParam} project={p} view={view} disabled={offline} />
                  ))}
                </div>
              )}
            </section>
          ) : null}

          <section aria-labelledby="scratch-title">
            <SectionTitle
              action={
                <Link to="/n/$node/new" params={{ node: nodeParam }}>
                  <Button size="sm" variant="outline" icon={<Sparkles className="size-4" />} tabIndex={-1} disabled={offline || !hasAgents}>
                    {t('node.newScratch')}
                  </Button>
                </Link>
              }
            >
              <span id="scratch-title">{t('node.scratch')}</span>
            </SectionTitle>
            {scratch.length === 0 ? (
              <p className="text-sm text-fg-subtle">{t('node.scratchEmpty')}</p>
            ) : (
              <ul className="space-y-2">
                {scratch.map((s) => (
                  <li key={s.id} className="flex items-stretch gap-2">
                    <div className="min-w-0 flex-1">
                      <SessionListItem session={s} />
                    </div>
                    <Button variant="outline" size="sm" className="self-center" disabled={offline} onClick={() => setPromote(s.id)}>
                      {t('session.promote')}
                    </Button>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section aria-labelledby="recent-title">
            <SectionTitle>
              <span id="recent-title">{t('node.recent')}</span>
            </SectionTitle>
            {recent.length === 0 ? (
              <p className="text-sm text-fg-subtle">{t('node.recentEmpty')}</p>
            ) : (
              <ul className="space-y-2">
                {recent.map((s) => (
                  <li key={s.id}>
                    <SessionListItem session={s} showProject />
                  </li>
                ))}
              </ul>
            )}
          </section>

          {offline && sessions.length > 0 ? <Chip tone="warning">{t('node.cachedHint')}</Chip> : null}
        </div>
      </div>
      <NewProjectDialog nodeId={nodeId} open={newOpen} onOpenChange={setNewOpen} initialMode={newMode} />
      {promote ? <PromoteDialog nodeId={nodeId} sessionId={promote} open onOpenChange={(v) => !v && setPromote(null)} /> : null}
    </>
  )
}
