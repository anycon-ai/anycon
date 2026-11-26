// 会话消息里的本地文件链接和图片（docs/spec/web.md § 4.6「本地文件链接」，ADR-62）。
//
// agent 写的 `[报告](/Users/…/a.html)` 指节点上的文件。原样当 `<a href>` 渲染时，浏览器把它当成 hub 上的地址：
// 打开的是 hub 的页面，路径还被发给了 hub。这里按节点上的项目目录解析它：
// - 项目里的 `.html` / `.htm`，节点支持目录预览时：点了在新标签页里经目录预览打开（`href` 仍是文件浏览的地址，中键 / 复制链接照常）；
// - 项目里的其他文件和目录：应用内链接，去文件浏览；
// - 项目外的路径（`~/Downloads/…`）：不跳转，点了复制路径，提示 anycon 打不开；
// - 本地图片经 `fs.read` 取（和文件浏览里 Markdown 的图片同一个组件），项目外的显示成说明文字。
import { useMemo, type MouseEvent, type ReactNode } from 'react'
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import type { LocalFiles } from '@/components/Markdown'
import { FileImage } from '@/features/files/FilePreview'
import { classifyLocalHref, isWebPage, type LocalPathContext } from '@/features/files/localPaths'
import { PathLink } from '@/features/files/parts'
import { describePreviewError } from '@/features/preview/errors'
import { openDirPreview } from '@/features/preview/openDir'
import { copyText } from '@/lib/clipboard'
import { toast } from '@/stores/ui'
import { SessionFilesContext } from './localFilesContext'

/** 会话页用它把 [`useSessionFiles`] 的结果交给消息（消息组件用 `useSessionLocalFiles` 读）。 */
export const SessionFilesProvider = SessionFilesContext.Provider

export interface SessionFilesOptions extends LocalPathContext {
  nodeId: string
  /** 路由里节点的写法（短 id）。 */
  nodeParam: string
  sessionId: string
  /** 目录预览能用时是 hub 的预览 origin 模板（`previewDirAvailable`），否则 null：HTML 也去文件浏览。 */
  dirTemplate: string | null
}

/** 会话页用：一个稳定的 [`LocalFiles`]（Markdown 是 memo 的，引用变了整条消息重新渲染）。 */
export function useSessionFiles({ nodeId, nodeParam, sessionId, dirTemplate, workDir, projects, primaryRoot }: SessionFilesOptions): LocalFiles {
  return useMemo<LocalFiles>(() => {
    const ctx: LocalPathContext = { workDir, projects, primaryRoot }
    return {
      absolutePaths: true,
      resolve: (url) => {
        const c = classifyLocalHref(url, ctx)
        return c.kind === 'resolved' ? c.path : null
      },
      image: (path, alt) => <FileImage nodeId={nodeId} path={path} alt={alt} />,
      link: (path, children) =>
        dirTemplate !== null && isWebPage(path) ? (
          <WebPageLink nodeId={nodeId} nodeParam={nodeParam} path={path} sessionId={sessionId} template={dirTemplate}>
            {children}
          </WebPageLink>
        ) : (
          <FileLink nodeParam={nodeParam} path={path}>
            {children}
          </FileLink>
        ),
      unresolved: (url, children) => {
        const c = classifyLocalHref(url, ctx)
        return c.kind === 'outside' ? <OutsideLink display={c.display}>{children}</OutsideLink> : <span>{children}</span>
      },
    }
  }, [nodeId, nodeParam, sessionId, dirTemplate, workDir, projects, primaryRoot])
}

/** 普通的左键点击（没有修饰键）：只有这种才拦下来自己打开预览，中键、Cmd / Ctrl 点击照浏览器的默认去文件浏览。 */
const plainClick = (e: MouseEvent): boolean => e.button === 0 && !e.metaKey && !e.ctrlKey && !e.shiftKey && !e.altKey

function WebPageLink({ nodeId, nodeParam, path, sessionId, template, children }: { nodeId: string; nodeParam: string; path: string; sessionId: string; template: string; children: ReactNode }) {
  const { t } = useTranslation()
  const onClick = (e: MouseEvent<HTMLAnchorElement>): void => {
    if (!plainClick(e)) return
    e.preventDefault()
    void openDirPreview({ nodeId, fsPath: path, template, sessionId, waitingText: t('markdown.opening'), onError: (err) => toast.error(describePreviewError(err, t)) })
  }
  return (
    <Link to="/n/$node/files/$" params={{ node: nodeParam, _splat: path }} onClick={onClick} title={t('markdown.localWebPage')} data-testid="md-local-web" data-path={path}>
      {children}
    </Link>
  )
}

function FileLink({ nodeParam, path, children }: { nodeParam: string; path: string; children: ReactNode }) {
  const { t } = useTranslation()
  return (
    <PathLink node={nodeParam} path={path} title={t('markdown.localFile')} testId="md-local-link">
      {children}
    </PathLink>
  )
}

function OutsideLink({ display, children }: { display: string; children: ReactNode }) {
  const { t } = useTranslation()
  const copy = (): void => {
    copyText(display).then(
      () => toast.success(t('markdown.pathCopied')),
      () => toast.error(t('common.copyFailed')),
    )
  }
  return (
    <button
      type="button"
      onClick={copy}
      title={t('markdown.localOutside', { path: display })}
      className="cursor-pointer text-left underline decoration-dotted underline-offset-2"
      data-testid="md-local-outside"
      data-path={display}
    >
      {children}
    </button>
  )
}
