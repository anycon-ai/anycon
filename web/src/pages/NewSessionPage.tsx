import { useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { MissingProjectNotice } from '@/components/MissingProjectNotice'
import { NewSessionForm } from '@/components/NewSessionForm'
import { PageHeader } from '@/components/PageHeader'
import { useCurrentNode, useNodeParam } from '@/lib/nav'

/** 新会话：一张空白页，中间是标题和输入框；项目在输入框下面的选项行里选。历史会话在左栏和项目页。 */
export function NewSessionPage({ project, agent }: { project?: string; agent?: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { nodeId, entry } = useCurrentNode()
  const nodeParam = useNodeParam(nodeId)
  if (!nodeId || !entry) return null
  // 关联的目录不在了：不能在里面新建会话
  const missingProject = project ? entry.projects.find((p) => p.name === project && p.missing) : undefined
  return (
    <>
      <PageHeader crumbs={[{ label: entry.name, to: { to: '/n/$node', params: { node: nodeParam } } }, { label: t('newSession.title') }]} />
      <div className="scroll-thin flex min-h-0 flex-1 flex-col overflow-y-auto">
        <div className="flex flex-1 flex-col justify-center py-6">
          <h1 className="mb-4 px-4 text-center text-2xl font-semibold" data-testid="new-session-heading">
            {project ? t('newSession.headingProject', { project }) : t('newSession.headingScratch')}
          </h1>
          {missingProject ? (
            <div className="mx-auto w-full max-w-3xl px-3 md:px-4">
              <MissingProjectNotice project={missingProject} />
            </div>
          ) : (
            <NewSessionForm
              key={`${nodeId}:${project ?? ''}`}
              nodeId={nodeId}
              project={project}
              agentId={agent}
              focus
              onProjectChange={(p) => void navigate({ to: '/n/$node/new', params: { node: nodeParam }, search: { project: p, agent }, replace: true })}
            />
          )}
        </div>
      </div>
    </>
  )
}
