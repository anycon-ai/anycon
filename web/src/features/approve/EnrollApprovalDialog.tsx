import { useTranslation } from 'react-i18next'
import { enrollKey, pendingApprovals, useEnroll } from '@/stores/enroll'
import { useNodes } from '@/stores/nodes'
import { useNow } from '@/lib/useNow'
import { cleanHintText } from '@/lib/hint'
import { Dialog } from '@/ui/Overlay'
import { EnrollRequestCard } from './EnrollRequestCard'

/**
 * 批准对话框（spec/web.md § 4.3.2）：`node.enroll_requested` → 横幅（§ 3.4）→「查看」打开这里。
 * 批准后卡片换成 6 位码；别的设备先处理了就收成一行；关掉对话框，码就看不到了。
 */
export function EnrollApprovalDialog() {
  const { t } = useTranslation()
  const dialog = useEnroll((s) => s.dialog)
  const close = useEnroll((s) => s.closeDialog)
  const items = useEnroll((s) => s.items)
  const resolved = useEnroll((s) => s.resolved)
  const nodeName = useNodes((s) => (dialog ? s.byId[dialog.nodeId]?.name : undefined))
  const now = useNow(1000, !!dialog)
  const open = dialog !== null
  const others = dialog ? pendingApprovals({ items, resolved }, now, dialog.nodeId).filter((it) => enrollKey(it.nodeId, it.info.enroll_id) !== enrollKey(dialog.nodeId, dialog.enrollId)) : []
  return (
    <Dialog open={open} onOpenChange={(o) => !o && close()} title={t('approve.title', { node: cleanHintText(nodeName) || t('security.thisNode') })} closeLabel={t('common.close')} testId="approve-dialog">
      {dialog ? (
        <div className="space-y-4">
          <EnrollRequestCard key={enrollKey(dialog.nodeId, dialog.enrollId)} nodeId={dialog.nodeId} enrollId={dialog.enrollId} onScan={() => {
            close()
            useEnroll.getState().openScanner()
          }} />
          {others.length > 0 ? (
            <button type="button" className="text-xs text-accent-text hover:underline" onClick={() => useEnroll.getState().openDialog(dialog.nodeId, others[0]!.info.enroll_id)} data-testid="approve-next">
              {t('approve.more', { count: others.length })}
            </button>
          ) : null}
        </div>
      ) : null}
    </Dialog>
  )
}
