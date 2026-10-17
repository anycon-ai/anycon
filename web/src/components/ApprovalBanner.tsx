import { ShieldQuestion } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/ui/Button'

/**
 * 手机上的待审批横幅（spec/web.md § 3.3）：固定在输入区上方，不遮消息。点「查看」跳到那条请求（卡片里才有完整内容和按钮，
 * 横幅本身不提供一键允许——看不到内容就批准是最危险的做法）。≥768px 用不到：右栏 / 左栏已经常驻显示待审批。
 */
export function ApprovalBanner({ count, title, onReview, reviewLabel }: { count: number; title: string; onReview: () => void; reviewLabel?: string }) {
  const { t } = useTranslation()
  return (
    <div role="status" className="flex items-center gap-2 border-t border-warning/40 bg-warning-subtle px-3 py-2 text-sm text-warning md:hidden" data-testid="approval-banner">
      <ShieldQuestion className="size-4 shrink-0" aria-hidden />
      <span className="min-w-0 flex-1 truncate">{count > 1 ? t('approval.many', { count }) : t('approval.one', { title })}</span>
      <Button size="sm" variant="outline" onClick={onReview} data-testid="approval-banner-review">
        {reviewLabel ?? t('approval.review')}
      </Button>
    </div>
  )
}
