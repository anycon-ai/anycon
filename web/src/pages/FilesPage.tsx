// 只读文件浏览 `/n/:node/files/*`（docs/spec/web.md § 4.7）：左边目录树、右边预览；手机上树和预览是两屏。
// 路径就是 `fs.list` / `fs.read` 的 `path`（`<项目名>/<相对路径>`，scratch 会话是 `.scratch/<会话 id>`），选中的文件在 URL 里。
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { ChevronRight, Eye, EyeOff, Folder, RefreshCw, ServerOff } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { DirListing } from '@/features/files/DirListing'
import { isOfflineError } from '@/features/files/errors'
import { FilePreview } from '@/features/files/FilePreview'
import { FileTree, useExpanded } from '@/features/files/FileTree'
import { useDirCache, type DirCache } from '@/features/files/hooks'
import { ErrorBlock, Loading, PathLink } from '@/features/files/parts'
import { baseName, crumbsOf, parentOf, splitPath, treeRoot } from '@/features/files/paths'
import { useCurrentNode, useNodeParam } from '@/lib/nav'
import { useIsWide } from '@/lib/useMediaQuery'
import type { NodeEntry } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { Button } from '@/ui/Button'
import { cn } from '@/ui/cn'
import { Chip, EmptyState } from '@/ui/Misc'

export function FilesPage({ path }: { path: string }) {
  const { nodeId, entry } = useCurrentNode()
  if (!nodeId || !entry) return null
  // 换节点就是一个新的浏览器：目录缓存、展开状态都不带过去
  return <FilesBrowser key={nodeId} nodeId={nodeId} entry={entry} path={splitPath(path).join('/')} />
}

/** 选中的路径是什么：树根是目录；其它的看上一级的列表。 */
type Selection = { kind: 'home' } | { kind: 'dir' } | { kind: 'pending' } | { kind: 'error'; error: unknown } | { kind: 'file'; size?: number }

function selectionOf(path: string, root: string, cache: DirCache): Selection {
  if (!root) return { kind: 'home' }
  if (path === root) return { kind: 'dir' }
  const parent = cache.dirs[parentOf(path)]
  if (!parent || (parent.status === 'loading' && !parent.entries)) return { kind: 'pending' }
  if (parent.status === 'error') return { kind: 'error', error: parent.error }
  const hit = parent.entries?.find((e) => e.name === baseName(path))
  if (hit?.is_dir) return { kind: 'dir' }
  // 列表里没有（刚创建、列表是旧的）也当文件去读：读不到会显示 daemon 给的原因
  return { kind: 'file', size: hit?.bytes }
}

function FilesBrowser({ nodeId, entry, path }: { nodeId: string; entry: NodeEntry; path: string }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const nodeParam = useNodeParam(nodeId)
  const wide = useIsWide()
  const online = entry.conn === 'ready'
  const showHidden = usePrefs((s) => s.filesShowHidden)
  const root = treeRoot(path)
  const cache = useDirCache(nodeId, online)
  const { load } = cache
  const [expanded, setExpanded] = useExpanded(root)
  const [reloadKey, setReloadKey] = useState(0)
  // 目录和文件的区别要看上一级的列表；树根自己就是目录
  const sel = selectionOf(path, root, cache)
  const parent = parentOf(path)
  const needParent = root !== '' && path !== root
  useEffect(() => {
    if (needParent) load(parent)
  }, [needParent, parent, load])

  const scratchId = root.startsWith('.scratch/') ? root.slice('.scratch/'.length) : ''
  const scratchTitle = useSessions((s) => (scratchId ? s.list[scratchId]?.title : undefined))
  const rootLabel = scratchId ? scratchTitle || t('files.scratch') : root

  const openPath = useCallback((p: string): void => void navigate({ to: '/n/$node/files/$', params: { node: nodeParam, _splat: p } }), [navigate, nodeParam])
  const linkTo = useCallback(
    (p: string, children: ReactNode) => (
      <PathLink node={nodeParam} path={p}>
        {children}
      </PathLink>
    ),
    [nodeParam],
  )
  const refresh = (): void => {
    cache.reload()
    setReloadKey((k) => k + 1)
  }

  // 手机：URL 指向文件时只显示预览（带返回），否则只显示树
  const previewScreen = sel.kind !== 'home' && sel.kind !== 'dir'
  const rootState = root ? cache.dirs[root] : undefined
  const rootFailed = rootState?.status === 'error' ? rootState.error : undefined

  let body: ReactNode
  if (rootFailed !== undefined && isOfflineError(rootFailed)) {
    body = (
      <div className="flex flex-1 items-center justify-center" data-testid="files-offline">
        <EmptyState icon={<ServerOff className="size-10" />} title={t('files.offlineTitle')}>
          {t('files.offlineBody')}
        </EmptyState>
      </div>
    )
  } else if (rootFailed !== undefined) {
    body = <ErrorBlock error={rootFailed} onRetry={refresh} testId="files-root-error" />
  } else {
    const treePane = (
      <aside className={cn('flex min-h-0 shrink-0 flex-col', wide ? 'w-72 border-r border-border lg:w-80' : 'min-w-0 flex-1')} data-testid="files-tree-pane">
        <div className="scroll-thin min-h-0 flex-1 overflow-y-auto px-1">
          {root ? (
            <FileTree
              root={root}
              rootLabel={rootLabel}
              selected={path}
              selectedIsDir={sel.kind === 'dir' ? true : sel.kind === 'file' ? false : undefined}
              cache={cache}
              expanded={expanded}
              setExpanded={setExpanded}
              showHidden={showHidden}
              onOpenFile={openPath}
            />
          ) : (
            <ProjectList node={nodeParam} entry={entry} />
          )}
        </div>
      </aside>
    )
    let preview: ReactNode
    switch (sel.kind) {
      case 'home':
        preview = <EmptyState icon={<Folder className="size-10" />} title={t('files.chooseProject')} />
        break
      case 'dir':
        preview = <DirListing node={nodeParam} path={path} cache={cache} showHidden={showHidden} onRetry={refresh} />
        break
      case 'pending':
        preview = <Loading />
        break
      case 'error':
        preview = <ErrorBlock error={sel.error} onRetry={() => load(parent)} />
        break
      case 'file':
        preview = <FilePreview nodeId={nodeId} path={path} root={root} size={sel.size} reloadKey={reloadKey} linkTo={linkTo} />
        break
    }
    const previewPane = (
      <section className="flex min-h-0 min-w-0 flex-1 flex-col" data-testid="files-preview-pane">
        {preview}
      </section>
    )
    body = wide ? (
      <>
        {treePane}
        {previewPane}
      </>
    ) : previewScreen ? (
      previewPane
    ) : (
      treePane
    )
  }

  return (
    <>
      <PageHeader
        crumbs={[
          { label: entry.name, to: { to: '/n/$node', params: { node: nodeParam } } },
          path ? { label: t('files.title'), to: { to: '/n/$node/files/$', params: { node: nodeParam, _splat: '' } } } : { label: t('files.title') },
        ]}
        onBack={!wide && previewScreen ? () => openPath(parent) : undefined}
        actions={
          root ? (
            <>
              <Button
                variant="ghost"
                size="icon"
                aria-label={showHidden ? t('files.hiddenHide') : t('files.hiddenShow')}
                title={showHidden ? t('files.hiddenHide') : t('files.hiddenShow')}
                aria-pressed={showHidden}
                onClick={() => usePrefs.getState().set({ filesShowHidden: !showHidden })}
                data-testid="files-hidden-toggle"
              >
                {showHidden ? <Eye className="size-4" /> : <EyeOff className="size-4" />}
              </Button>
              <Button variant="ghost" size="icon" aria-label={t('files.refresh')} title={t('files.refresh')} onClick={refresh} disabled={!online} data-testid="files-refresh">
                <RefreshCw className="size-4" />
              </Button>
            </>
          ) : undefined
        }
      />
      {root ? <PathBar node={nodeParam} path={path} root={root} rootLabel={rootLabel} /> : null}
      <div className="flex min-h-0 flex-1" data-testid="files-page" data-screen={wide ? 'both' : previewScreen ? 'preview' : 'tree'}>
        {body}
      </div>
    </>
  )
}

/** 路径面包屑：每一级都能点，回到那一级目录。树根显示项目名（scratch 会话显示它的标题）。 */
function PathBar({ node, path, root, rootLabel }: { node: string; path: string; root: string; rootLabel: string }) {
  const { t } = useTranslation()
  const crumbs = crumbsOf(path)
    .slice(splitPath(root).length - 1)
    .map((c, i) => (i === 0 ? { ...c, label: rootLabel } : c))
  return (
    <nav aria-label={t('files.path')} className="scroll-thin flex shrink-0 items-center gap-1 overflow-x-auto border-b border-border px-3 py-1.5 text-sm" data-testid="files-path">
      {crumbs.map((c, i) => {
        const last = i === crumbs.length - 1
        return (
          <span key={c.path} className="flex shrink-0 items-center gap-1">
            {i > 0 ? <ChevronRight className="size-3.5 text-fg-subtle" aria-hidden /> : null}
            {last ? (
              <span className="font-medium" aria-current="page" data-testid="files-crumb-current">
                {c.label}
              </span>
            ) : (
              <PathLink node={node} path={c.path} className="text-fg-muted hover:text-fg hover:underline" testId="files-crumb">
                {c.label}
              </PathLink>
            )}
          </span>
        )
      })}
    </nav>
  )
}

/** 没有选中项目时：节点上的项目列表，点一个进它的文件。 */
function ProjectList({ node, entry }: { node: string; entry: NodeEntry }) {
  const { t } = useTranslation()
  if (entry.projects.length === 0) return <EmptyState icon={<Folder className="size-10" />} title={t('files.noProjects')} />
  const sorted = [...entry.projects].sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' }))
  return (
    <nav aria-label={t('files.projects')} className="py-1" data-testid="files-projects">
      <ul>
        {sorted.map((p) => (
          <li key={p.name}>
            {p.missing ? (
              <span className="flex min-h-9 items-center gap-2 rounded-md px-2 py-1.5 text-sm text-fg-subtle" data-testid="files-project-missing">
                <Folder className="size-4 shrink-0" aria-hidden />
                <span className="min-w-0 truncate">{p.name}</span>
                <Chip tone="warning">{t('files.missingProject')}</Chip>
              </span>
            ) : (
              <PathLink node={node} path={p.name} className="flex min-h-9 items-center gap-2 rounded-md px-2 py-1.5 text-sm hover:bg-bg-hover" testId="files-project">
                <Folder className="size-4 shrink-0 text-fg-subtle" aria-hidden />
                <span className="min-w-0 truncate">{p.name}</span>
              </PathLink>
            )}
          </li>
        ))}
      </ul>
    </nav>
  )
}
