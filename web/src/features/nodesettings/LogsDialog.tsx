import { useEffect, useRef, useState } from 'react'
import { RefreshCw } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { NodeLogLevel, NodeLogsResult } from '@/protocol'
import { describeError } from '@/lib/errors'
import { rpc } from '@/transport/instance'
import { Button } from '@/ui/Button'
import { Select } from '@/ui/Field'
import { CopyButton, Spinner } from '@/ui/Misc'
import { Dialog } from '@/ui/Overlay'

const LEVELS: NodeLogLevel[] = ['trace', 'debug', 'info', 'warn', 'error']
const LINE_CHOICES = [200, 500, 1000] as const

type Load = { status: 'loading' } | { status: 'ready'; logs: NodeLogsResult } | { status: 'error'; error: unknown }

/**
 * 节点设置 →「关于」→「查看日志」（docs/spec/web.md § 4.8）：`node.logs` 返回的 daemon 日志尾部。
 * 可以按最低级别过滤、选行数、刷新、复制。节点上已经把可能带会话内容的行去掉了（docs/spec/daemon.md § 17），这里原样显示。
 */
export function LogsDialog({ nodeId, open, onOpenChange }: { nodeId: string; open: boolean; onOpenChange: (open: boolean) => void }) {
  const { t } = useTranslation()
  const [level, setLevel] = useState<NodeLogLevel>('info')
  const [lines, setLines] = useState<number>(500)
  const [load, setLoad] = useState<Load>({ status: 'loading' })
  const [reload, setReload] = useState(0)
  const box = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    let alive = true
    setLoad({ status: 'loading' })
    rpc()
      .request<NodeLogsResult>(nodeId, 'node.logs', { lines, level })
      .then((logs) => alive && setLoad({ status: 'ready', logs }))
      .catch((error: unknown) => {
        console.warn('anycon: could not read the node logs', error)
        if (alive) setLoad({ status: 'error', error })
      })
    return () => {
      alive = false
    }
  }, [nodeId, open, level, lines, reload])

  // 新的在最下面：读完滚到底
  useEffect(() => {
    if (load.status === 'ready' && box.current) box.current.scrollTop = box.current.scrollHeight
  }, [load])

  const text = load.status === 'ready' ? load.logs.lines.join('\n') : ''

  return (
    <Dialog open={open} onOpenChange={onOpenChange} title={t('nodeSettings.logs.title')} description={t('nodeSettings.logs.hint')} closeLabel={t('common.close')} size="lg" testId="logs-dialog">
      <div className="space-y-3">
        <div className="flex flex-wrap items-center gap-2">
          <label className="flex items-center gap-2 text-sm text-fg-muted">
            {t('nodeSettings.logs.level')}
            <Select className="w-32" value={level} onChange={(e) => setLevel(e.target.value as NodeLogLevel)} data-testid="logs-level">
              {LEVELS.map((l) => (
                <option key={l} value={l}>
                  {t(`nodeSettings.logs.levels.${l}`)}
                </option>
              ))}
            </Select>
          </label>
          <label className="flex items-center gap-2 text-sm text-fg-muted">
            {t('nodeSettings.logs.lines')}
            <Select className="w-24" value={String(lines)} onChange={(e) => setLines(Number(e.target.value))} data-testid="logs-lines">
              {LINE_CHOICES.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </Select>
          </label>
          <div className="ml-auto flex items-center gap-1">
            <Button size="sm" variant="outline" icon={<RefreshCw className="size-3.5" />} onClick={() => setReload((n) => n + 1)} disabled={load.status === 'loading'} data-testid="logs-refresh">
              {t('nodeSettings.logs.refresh')}
            </Button>
            {text ? <CopyButton text={text} testId="logs-copy" /> : null}
          </div>
        </div>

        {load.status === 'loading' ? (
          <div className="flex items-center gap-2 text-sm text-fg-subtle" data-testid="logs-loading">
            <Spinner /> {t('nodeSettings.logs.loading')}
          </div>
        ) : null}
        {load.status === 'error' ? (
          <p className="text-sm text-danger" data-testid="logs-error">
            {describeError(load.error, t)}
          </p>
        ) : null}
        {load.status === 'ready' && load.logs.lines.length === 0 ? (
          <p className="text-sm text-fg-subtle" data-testid="logs-empty">
            {t('nodeSettings.logs.empty')}
          </p>
        ) : null}
        {load.status === 'ready' && load.logs.lines.length > 0 ? (
          <>
            <div ref={box} className="scroll-thin max-h-[50dvh] overflow-auto rounded-md border border-border bg-bg-muted p-2" tabIndex={0} aria-label={t('nodeSettings.logs.title')}>
              <pre className="font-mono text-xs leading-relaxed break-all whitespace-pre-wrap" data-testid="logs-output">
                {text}
              </pre>
            </div>
            <p className="text-xs text-fg-subtle" data-testid="logs-count">
              {t('nodeSettings.logs.count', { count: load.logs.lines.length })}
              {load.logs.truncated ? ` ${t('nodeSettings.logs.truncated')}` : ''}
            </p>
          </>
        ) : null}
      </div>
    </Dialog>
  )
}
