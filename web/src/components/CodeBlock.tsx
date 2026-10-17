import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { highlightCode } from '@/lib/highlight'
import { CopyButton } from '@/ui/Misc'

/** 代码块：语言标签 + 复制按钮；高亮异步加载，流式输出期间保持纯文本避免每个 token 重新高亮。 */
export function CodeBlock({ code, lang, streaming }: { code: string; lang?: string; streaming?: boolean }) {
  const { t } = useTranslation()
  const [html, setHtml] = useState<string | null>(null)

  useEffect(() => {
    if (streaming) return
    let cancelled = false
    void highlightCode(code, lang).then((h) => {
      if (!cancelled) setHtml(h)
    })
    return () => {
      cancelled = true
    }
  }, [code, lang, streaming])

  return (
    <div className="my-3 overflow-hidden rounded-lg border border-border bg-bg-subtle" data-testid="code-block">
      <div className="flex items-center justify-between border-b border-border px-3 py-1 text-xs text-fg-subtle">
        <span className="font-mono">{lang || t('code.text')}</span>
        <CopyButton text={code} />
      </div>
      <div className="code-pre scroll-thin overflow-x-auto p-3">
        {html && !streaming ? (
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
