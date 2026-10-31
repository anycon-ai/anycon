// 文件预览（docs/spec/web.md § 4.7）：文本（shiki 高亮）、Markdown（渲染 / 源码）、图片、二进制。
import { useEffect, useMemo, useState, type ReactNode } from 'react'
import { AlertTriangle, FileImage as FileImageIcon, FileText, ImageOff } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useInView } from '@/features/attachments/hooks'
import { Markdown, type LocalFiles } from '@/components/Markdown'
import { formatBytes } from '@/lib/format'
import { highlightCode } from '@/lib/highlight'
import { cn } from '@/ui/cn'
import { CopyButton, Segmented, Skeleton } from '@/ui/Misc'
import { IMAGE_MAX_BYTES, readImage, readText, TEXT_MAX_BYTES } from './api'
import { useAsync, useObjectUrl } from './hooks'
import { ErrorBlock, Loading } from './parts'
import { baseName, fileKind, parentOf, resolveLocalUrl } from './paths'

/** 超过这么多字符的文本不做语法高亮（shiki 在主线程里跑，1 MiB 的文件会卡住页面）。 */
export const HIGHLIGHT_MAX_CHARS = 200_000
/** 超过这么多字符的 Markdown 默认先看源码（渲染要多花时间）；可以手动切到渲染。 */
export const MARKDOWN_RENDER_MAX_CHARS = 300_000

export interface FilePreviewProps {
  nodeId: string
  /** 要预览的文件（`fs.read` 的 `path`）。 */
  path: string
  /** 树根（项目目录）：Markdown 里的 `/x` 相对它，`../` 不能超过它。 */
  root: string
  /** 目录列表里已知的大小；没有就等读出来。 */
  size?: number
  /** 加一就重新读（刷新按钮）。 */
  reloadKey: number
  /** Markdown 里指向节点上文件 / 目录的链接怎么渲染（应用内跳转）；要稳定的引用，Markdown 是 memo 的。 */
  linkTo: (path: string, children: ReactNode) => ReactNode
}

export function FilePreview({ nodeId, path, root, size, reloadKey, linkTo }: FilePreviewProps) {
  const kind = useMemo(() => fileKind(baseName(path)), [path])
  // 换文件、刷新都当作新组件：旧文件的状态（视图切换、高亮结果）不带到新文件上
  const key = `${nodeId}\0${path}\0${reloadKey}`
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="file-preview" data-kind={kind.type} data-path={path}>
      {kind.type === 'image' ? (
        <ImagePreview key={key} nodeId={nodeId} path={path} mime={kind.mime} size={size} />
      ) : (
        <TextPreview key={key} nodeId={nodeId} path={path} root={root} markdown={kind.type === 'markdown'} lang={kind.type === 'text' ? kind.lang : undefined} size={size} linkTo={linkTo} />
      )}
    </div>
  )
}

/** 预览区的框架：文件名、大小、复制路径，以及页面自己的工具（Markdown 的视图切换）和提示条。 */
function Frame({ path, size, icon, toolbar, notices, children }: { path: string; size?: number; icon: ReactNode; toolbar?: ReactNode; notices?: ReactNode; children: ReactNode }) {
  const { t } = useTranslation()
  return (
    <>
      <div className="flex shrink-0 items-center gap-2 border-b border-border px-3 py-2">
        <span className="text-fg-subtle" aria-hidden>
          {icon}
        </span>
        <span className="min-w-0 truncate text-sm font-medium" title={path} data-testid="file-name">
          {baseName(path)}
        </span>
        {size !== undefined ? (
          <span className="shrink-0 text-xs text-fg-subtle" data-testid="file-size">
            {formatBytes(size)}
          </span>
        ) : null}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {toolbar}
          <CopyButton text={path} iconOnly label={t('files.copyPath')} testId="file-copy-path" />
        </div>
      </div>
      {notices}
      {children}
    </>
  )
}

function Notice({ children, testId }: { children: ReactNode; testId: string }) {
  return (
    <p className="flex shrink-0 items-center gap-2 border-b border-border bg-warning-subtle px-3 py-1.5 text-xs text-warning" role="status" data-testid={testId}>
      <AlertTriangle className="size-3.5 shrink-0" aria-hidden />
      <span className="min-w-0">{children}</span>
    </p>
  )
}

function Centered({ children, testId, tone }: { children: ReactNode; testId: string; tone?: 'warning' }) {
  return (
    <div className={cn('flex flex-1 flex-col items-center justify-center gap-2 p-8 text-center text-sm', tone === 'warning' ? 'text-warning' : 'text-fg-muted')} data-testid={testId}>
      {children}
    </div>
  )
}

function TextPreview({ nodeId, path, root, markdown, lang, size, linkTo }: { nodeId: string; path: string; root: string; markdown: boolean; lang?: string; size?: number; linkTo: (path: string, children: ReactNode) => ReactNode }) {
  const { t, i18n } = useTranslation()
  const { state, retry } = useAsync(`${nodeId}\0${path}`, () => readText(nodeId, path))
  const text = state.status === 'ready' ? state.data.text : ''
  const [view, setView] = useState<'rendered' | 'source' | null>(null)
  const local = useMemo<LocalFiles>(
    () => ({
      resolve: (url) => resolveLocalUrl(url, parentOf(path), root),
      image: (p, alt) => <FileImage nodeId={nodeId} path={p} alt={alt} />,
      link: linkTo,
    }),
    [nodeId, path, root, linkTo],
  )
  const icon = <FileText className="size-4" />
  if (state.status === 'loading')
    return (
      <Frame path={path} size={size} icon={icon}>
        <Loading />
      </Frame>
    )
  if (state.status === 'error')
    return (
      <Frame path={path} size={size} icon={icon}>
        <ErrorBlock error={state.error} onRetry={retry} />
      </Frame>
    )
  const { binary, truncated, bytes } = state.data
  if (binary)
    return (
      <Frame path={path} size={bytes} icon={icon}>
        <Centered testId="files-binary">
          <FileImageIcon className="size-8 text-fg-subtle" aria-hidden />
          <p>{t('files.binary', { bytes: bytes.toLocaleString(i18n.language) })}</p>
        </Centered>
      </Frame>
    )
  const large = text.length > HIGHLIGHT_MAX_CHARS
  const shown = view ?? (markdown && text.length <= MARKDOWN_RENDER_MAX_CHARS ? 'rendered' : 'source')
  const notices = (
    <>
      {truncated ? <Notice testId="files-truncated">{t('files.truncated', { shown: formatBytes(TEXT_MAX_BYTES), total: formatBytes(bytes) })}</Notice> : null}
      {shown === 'source' && large && (lang || markdown) ? <Notice testId="files-no-highlight">{t('files.noHighlight')}</Notice> : null}
    </>
  )
  const toolbar = markdown ? (
    <Segmented
      size="sm"
      ariaLabel={t('files.viewLabel')}
      testId="files-md-view"
      value={shown}
      onChange={setView}
      options={[
        { value: 'rendered', label: t('files.viewRendered'), testId: 'files-md-rendered' },
        { value: 'source', label: t('files.viewSource'), testId: 'files-md-source' },
      ]}
    />
  ) : undefined
  return (
    <Frame path={path} size={bytes} icon={icon} toolbar={toolbar} notices={notices}>
      {text === '' && !truncated ? (
        <Centered testId="files-empty-file">{t('files.emptyFile')}</Centered>
      ) : markdown && shown === 'rendered' ? (
        <div className="scroll-thin min-h-0 flex-1 overflow-auto" data-testid="files-markdown">
          <div className="mx-auto max-w-3xl p-4 md:p-6">
            <Markdown text={text} local={local} />
          </div>
        </div>
      ) : (
        <CodeView text={text} lang={markdown ? 'markdown' : lang} highlight={!large} />
      )}
    </Frame>
  )
}

/** 带行号的代码。行号是单独一列（一个文本节点），不依赖每行一个元素，所以大文件不用渲染几万个 DOM 节点。 */
export function CodeView({ text, lang, highlight }: { text: string; lang?: string; highlight: boolean }) {
  // 末尾的换行不算一行
  const code = text.endsWith('\n') ? text.slice(0, -1) : text
  const [html, setHtml] = useState<string | null>(null)
  useEffect(() => {
    if (!highlight || !lang) return
    let cancelled = false
    void highlightCode(code, lang).then((h) => {
      if (!cancelled) setHtml(h)
    })
    return () => {
      cancelled = true
    }
  }, [code, lang, highlight])
  const gutter = useMemo(() => {
    const n = code.split('\n').length
    let s = '1'
    for (let i = 2; i <= n; i++) s += `\n${i}`
    return s
  }, [code])
  return (
    <div className="code-pre scroll-thin flex min-h-0 flex-1 overflow-auto font-mono" data-testid="file-code" data-highlighted={html !== null}>
      <pre aria-hidden className="sticky left-0 shrink-0 select-none border-r border-border bg-bg py-3 pr-3 pl-3 text-right text-fg-subtle" data-testid="file-gutter">
        {gutter}
      </pre>
      <div className="min-w-0 grow py-3 pr-3 pl-3">
        {html ? (
          // shiki 对代码内容做了转义，输出是受控的 HTML
          <div dangerouslySetInnerHTML={{ __html: html }} />
        ) : (
          <pre>
            <code>{code}</code>
          </pre>
        )}
      </div>
    </div>
  )
}

function ImagePreview({ nodeId, path, mime, size }: { nodeId: string; path: string; mime: string; size?: number }) {
  const { t } = useTranslation()
  // 目录列表已经说明它超过上限就不用取了（取回来也是被截断的）
  const knownTooLarge = size !== undefined && size > IMAGE_MAX_BYTES
  const { state, retry } = useAsync(`${nodeId}\0${path}`, () => readImage(nodeId, path), !knownTooLarge)
  const ready = state.status === 'ready' ? state.data : null
  const bytes = ready && !ready.truncated ? ready.bytes : null
  const url = useObjectUrl(bytes, mime)
  const [failed, setFailed] = useState(false)
  const tooLarge = knownTooLarge || ready?.truncated === true
  // 读回来的总长是最新的；列表里的可能已经过时
  const total = ready?.total ?? size
  const icon = <FileImageIcon className="size-4" />
  let body: ReactNode
  if (tooLarge)
    body = (
      <Centered testId="files-image-too-large">
        <ImageOff className="size-8 text-fg-subtle" aria-hidden />
        <p>{t('files.imageTooLarge', { size: formatBytes(total ?? IMAGE_MAX_BYTES), limit: formatBytes(IMAGE_MAX_BYTES) })}</p>
      </Centered>
    )
  else if (state.status === 'error') body = <ErrorBlock error={state.error} onRetry={retry} />
  else if (failed)
    body = (
      <Centered testId="files-image-failed">
        <ImageOff className="size-8 text-fg-subtle" aria-hidden />
        <p>{t('files.imageFailed')}</p>
      </Centered>
    )
  else if (url)
    body = (
      <div className="scroll-thin flex min-h-0 flex-1 items-center justify-center overflow-auto bg-bg-muted p-4">
        {/* blob: URL 放进 <img>：SVG 里的脚本不会执行，也不会进 DOM */}
        <img src={url} alt={baseName(path)} decoding="async" referrerPolicy="no-referrer" className="max-h-full max-w-full object-contain" data-testid="file-image" onError={() => setFailed(true)} />
      </div>
    )
  else body = <Loading />
  return (
    <Frame path={path} size={total} icon={icon}>
      {body}
    </Frame>
  )
}

/** Markdown 里的相对图片：进入视口才经 `fs.read` 取，显示成 `blob:` 图片；不是图片、取不到、太大都退回成说明文字。 */
export function FileImage({ nodeId, path, alt }: { nodeId: string; path: string; alt: string | undefined }) {
  const { t } = useTranslation()
  const kind = fileKind(baseName(path))
  const [ref, seen] = useInView<HTMLSpanElement>()
  const isImage = kind.type === 'image'
  const { state } = useAsync(`${nodeId}\0${path}`, () => readImage(nodeId, path), seen && isImage)
  const ready = state.status === 'ready' && !state.data.truncated ? state.data.bytes : null
  const url = useObjectUrl(ready, kind.type === 'image' ? kind.mime : '')
  const [failed, setFailed] = useState(false)
  const label = alt || baseName(path)
  if (!isImage || failed || state.status === 'error' || (state.status === 'ready' && state.data.truncated))
    return (
      <span className="my-2 inline-flex max-w-full items-center gap-2 text-sm text-fg-subtle" title={path} data-testid="md-local-image-missing">
        <ImageOff className="size-4 shrink-0" aria-hidden />
        <span className="min-w-0 truncate">{t('files.markdownImage', { alt: label })}</span>
      </span>
    )
  return (
    <span ref={ref} className="my-2 inline-block max-w-full align-top" data-testid="md-local-image">
      {url ? <img src={url} alt={alt ?? ''} decoding="async" referrerPolicy="no-referrer" className="max-w-full rounded-md" onError={() => setFailed(true)} /> : <Skeleton className="h-24 w-48 max-w-full" />}
    </span>
  )
}
