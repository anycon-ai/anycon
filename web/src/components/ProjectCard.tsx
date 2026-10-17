import { Link } from '@tanstack/react-router'
import { FolderGit2, FolderSymlink, FolderX, GitBranch } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { Project } from '@/protocol'
import { formatRelative } from '@/lib/format'
import { cn } from '@/ui/cn'
import { Chip } from '@/ui/Misc'

/** 项目徽标：关联的目录（kind = linked）标「已关联」，目录不在了（missing）标「目录不存在」。 */
export function ProjectBadges({ project }: { project: Pick<Project, 'kind' | 'missing'> }) {
  const { t } = useTranslation()
  return (
    <>
      {project.kind === 'linked' ? (
        <Chip tone="info" title={t('project.linkedHint')} testId="project-linked">
          {t('project.linked')}
        </Chip>
      ) : null}
      {project.missing ? (
        <Chip tone="danger" testId="project-missing">
          {t('project.missing')}
        </Chip>
      ) : null}
    </>
  )
}

export function ProjectCard({ nodeParam, project, view, disabled }: { nodeParam: string; project: Project; view: 'cards' | 'list'; disabled?: boolean }) {
  const { t, i18n } = useTranslation()
  const linked = project.kind === 'linked'
  const missing = project.missing === true
  const Icon = missing ? FolderX : linked ? FolderSymlink : FolderGit2
  return (
    <Link
      to="/n/$node/p/$project"
      params={{ node: nodeParam, project: project.name }}
      data-testid="project-card"
      data-project={project.name}
      data-kind={project.kind ?? 'folder'}
      data-missing={missing}
      className={cn(
        'rounded-lg border border-border bg-surface p-3 transition-colors hover:bg-bg-hover',
        view === 'cards' ? 'flex flex-col gap-2' : 'flex items-center gap-3',
        (disabled || missing) && 'opacity-60',
      )}
    >
      <span className="flex min-w-0 flex-col gap-0.5">
        <span className="flex min-w-0 items-center gap-2">
          <Icon className={cn('size-4 shrink-0', missing ? 'text-danger' : 'text-accent-text')} aria-hidden />
          <span className="truncate font-medium">{project.name}</span>
          <ProjectBadges project={project} />
        </span>
        {linked ? (
          <span className="truncate font-mono text-xs text-fg-subtle" title={project.path} data-testid="project-path">
            {project.path}
          </span>
        ) : null}
      </span>
      <span className={cn('flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-fg-subtle', view === 'list' && 'ml-auto')}>
        {project.git?.branch ? (
          <span className="inline-flex items-center gap-1">
            <GitBranch className="size-3" aria-hidden />
            {project.git.branch}
          </span>
        ) : null}
        {project.git?.dirty ? <Chip tone="warning">{t('project.dirty')}</Chip> : null}
        {project.active_sessions > 0 ? <Chip tone="accent">{t('project.activeSessions', { count: project.active_sessions })}</Chip> : null}
        {missing ? null : <span>{formatRelative(project.modified_at_ms, i18n.language)}</span>}
      </span>
    </Link>
  )
}
