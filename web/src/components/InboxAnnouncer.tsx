import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useInboxNowCount } from '@/features/inbox/useInbox'

/**
 * 「待处理」的数字一变，用状态播报区读出完整的一句话（「3 件事在等你」），不只是读一个数字（spec/web.md § 14.10）。
 * 常驻在外壳里（读屏用户不一定在「待处理」页上）。第一次渲染不播；连着变几次（同步时请求一条条到）合并成最后一句。
 */
export function InboxAnnouncer({ delayMs = 600 }: { delayMs?: number }) {
  const { t } = useTranslation()
  const count = useInboxNowCount()
  const [text, setText] = useState('')
  const prev = useRef<number | null>(null)
  useEffect(() => {
    if (prev.current === null) {
      prev.current = count
      return
    }
    if (prev.current === count) return
    prev.current = count
    const id = setTimeout(() => setText(count > 0 ? t('inbox.announce', { count }) : t('inbox.announceNone')), delayMs)
    return () => clearTimeout(id)
  }, [count, delayMs, t])
  return (
    <div role="status" aria-live="polite" aria-atomic="true" className="sr-only" data-testid="inbox-live">
      {text}
    </div>
  )
}
