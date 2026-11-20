import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useInboxNowCount } from '@/features/inbox/useInbox'
import { SETTLE_TIMEOUT_MS, useNodesSettled } from '@/features/inbox/useSettled'

/**
 * 「待处理」的数字一变，用状态播报区读出完整的一句话（「3 件事在等你」），不只是读一个数字（spec/web.md § 14.10）。
 * 常驻在外壳里（读屏用户不一定在「待处理」页上）。**启动和重连同步时不播**：勾选的节点都同步完（或确认离线）之后才取基线，
 * 同步进行中数字怎么变都不播，同步完再重新取基线；基线之后的真实变化才播。连着变几次合并成最后一句。
 */
export function InboxAnnouncer({ delayMs = 600 }: { delayMs?: number }) {
  const { t } = useTranslation()
  const count = useInboxNowCount()
  const settled = useNodesSettled()
  const [text, setText] = useState('')
  const prev = useRef<number | null>(null)
  useEffect(() => {
    if (!settled) {
      prev.current = null
      return
    }
    if (prev.current === null) {
      prev.current = count
      return
    }
    if (prev.current === count) return
    prev.current = count
    const id = setTimeout(() => setText(count > 0 ? t('inbox.announce', { count }) : t('inbox.announceNone')), delayMs)
    return () => clearTimeout(id)
  }, [count, settled, delayMs, t])
  return (
    <div role="status" aria-live="polite" aria-atomic="true" className="sr-only" data-testid="inbox-live">
      {text}
    </div>
  )
}

export { SETTLE_TIMEOUT_MS }
