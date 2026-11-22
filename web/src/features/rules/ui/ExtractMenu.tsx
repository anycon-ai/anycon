// 会话顶栏的提炼入口与当前调用的结果提示；离开页面时解除订阅，不留下等待提示。
import { useEffect, useRef, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { Ellipsis } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Menu, MenuContent, MenuItem, MenuTrigger } from '@/ui/Menu'
import { Spinner } from '@/ui/Misc'
import { extractRejectedText, extractResultText, startExtract, type ExtractState } from '../extract'

export function ExtractMenu({ nodeId, sessionId }: { nodeId: string; sessionId: string }) {
  const { t } = useTranslation()
  const node = useNodes((s) => s.byId[nodeId])
  const [state, setState] = useState<ExtractState | null>(null)
  const stop = useRef<(() => void) | null>(null)
  useEffect(() => () => { stop.current?.() }, [nodeId, sessionId])
  if (!node?.info?.capabilities.rule_extract) return null
  const configured = !!node.info.judge?.extract
  const offline = node.conn !== 'ready'
  const running = state?.kind === 'running'
  const hint = !configured ? t('rules.extract.not_configured') : offline ? t('rules.extract.offline') : undefined
  const message = state?.kind === 'done' ? extractResultText(state.run, t) : state?.kind === 'error' ? extractRejectedText(state.error, t) : state ? t(`rules.extract.${state.kind}`) : ''
  return (
    <>
      <Menu>
        <MenuTrigger asChild>
          <Button variant="ghost" size="icon" aria-label={t('rules.extract.more')} data-testid="session-more"><Ellipsis className="size-4" /></Button>
        </MenuTrigger>
        <MenuContent align="end">
          <MenuItem testId="rule-extract" disabled={!configured || offline || running} hint={hint} onSelect={() => {
            stop.current?.()
            stop.current = startExtract(nodeId, sessionId, setState)
          }}>{t('rules.extract.action')}</MenuItem>
          {!configured ? <a className="block px-2.5 py-2 text-xs text-accent-text hover:underline" href="https://github.com/anycon-ai/anycon/blob/main/docs/spec/daemon.md#25-判断后端" target="_blank" rel="noopener noreferrer">{t('rules.extract.docs')}</a> : null}
        </MenuContent>
      </Menu>
      {state ? (
        <div role="status" data-testid="extract-status" className="fixed right-4 bottom-20 z-40 max-w-sm rounded-lg border border-border bg-surface p-3 text-sm shadow-pop">
          <div className="flex items-center gap-2">{running ? <Spinner /> : null}<span>{message}</span></div>
          {state.kind === 'done' && state.run.status === 'done' && state.run.added.length > 0 ? <Link to="/rules" search={{ tab: 'pending' }} className="mt-2 block text-accent-text hover:underline">{t('rules.extract.view')}</Link> : null}
          <Button size="sm" variant="ghost" onClick={() => { stop.current?.(); setState(null) }}>{t('common.close')}</Button>
        </div>
      ) : null}
    </>
  )
}
