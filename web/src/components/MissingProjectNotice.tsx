import type { ReactNode } from 'react'
import { FolderX } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { Project } from '@/protocol'
import { Card } from '@/ui/Misc'

/** 关联的目录不在了（`missing`）：说明原因，不能新建会话；`action` 通常是「从列表移除」。 */
export function MissingProjectNotice({ project, action }: { project: Pick<Project, 'name' | 'path'>; action?: ReactNode }) {
  const { t } = useTranslation()
  return (
    <Card tone="danger" className="p-3" testId="project-missing-notice">
      <div className="flex items-start gap-2">
        <FolderX className="mt-0.5 size-4 shrink-0 text-danger" aria-hidden />
        <div className="min-w-0 flex-1 text-sm">
          <p className="font-medium">{t('project.missingTitle')}</p>
          <p className="mt-0.5 break-words text-fg-muted">{t('project.missingBody', { path: project.path })}</p>
          {action ? <div className="mt-2">{action}</div> : null}
        </div>
      </div>
    </Card>
  )
}
