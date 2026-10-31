// 文件浏览里几处共用的小部件：加载中、读失败（带重试）、指向节点上某个路径的应用内链接。
import type { ReactNode } from 'react'
import { Link } from '@tanstack/react-router'
import { AlertTriangle, RefreshCw } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'
import { Spinner } from '@/ui/Misc'
import { describeFsError } from './errors'

export function Loading() {
  const { t } = useTranslation()
  return (
    <div className="flex flex-1 items-center justify-center gap-2 p-8 text-sm text-fg-muted" role="status" aria-busy="true" data-testid="files-loading">
      <Spinner />
      {t('files.loading')}
    </div>
  )
}

/** 读失败：原因 + 重试。 */
export function ErrorBlock({ error, onRetry, testId = 'files-error' }: { error: unknown; onRetry: () => void; testId?: string }) {
  const { t } = useTranslation()
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-3 p-8 text-center" role="alert" data-testid={testId}>
      <AlertTriangle className="size-8 text-warning" aria-hidden />
      <div className="space-y-1">
        <p className="text-sm font-medium">{t('files.errorTitle')}</p>
        <p className="max-w-sm text-sm text-fg-muted">{describeFsError(error, t)}</p>
      </div>
      <Button variant="outline" size="sm" icon={<RefreshCw className="size-4" />} onClick={onRetry} data-testid="files-retry">
        {t('files.retry')}
      </Button>
    </div>
  )
}

/** 目录视图和 Markdown 里的链接共用：一个指向节点上某个路径的应用内链接。 */
export function PathLink({ node, path, children, className, testId }: { node: string; path: string; children: ReactNode; className?: string; testId?: string }) {
  return (
    <Link to="/n/$node/files/$" params={{ node, _splat: path }} className={className} data-testid={testId} data-path={path}>
      {children}
    </Link>
  )
}
