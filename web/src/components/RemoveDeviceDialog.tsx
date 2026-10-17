import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { signOutAndRemoveDevice } from '@/app/session'
import { RemoveDeviceError, type RemoveDeviceReport } from '@/app/removeDevice'
import { describeError } from '@/lib/errors'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'

/**
 * 「退出并移除本设备」确认框。撤销（各节点 → hub）→ 退出登录 → 清空 IndexedDB / localStorage，然后整页跳回登录页。
 * 任何一步失败都不删本地数据（清了就没法重试），并说明是哪些节点没撤销成功；错误里给一个「仍然清除本地数据」的出口。
 */
export function RemoveDeviceDialog({ open, onOpenChange, onDone }: { open: boolean; onOpenChange: (v: boolean) => void; onDone: () => void }) {
  const { t } = useTranslation()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [report, setReport] = useState<RemoveDeviceReport | null>(null)

  const run = async (force: boolean): Promise<void> => {
    setBusy(true)
    setError(null)
    try {
      const r = await signOutAndRemoveDevice({ force })
      if (r.skippedNodes.length === 0) return onDone()
      setReport(r) // 有节点当时连不上：让用户看到再离开
    } catch (e) {
      console.warn('anycon: removing this device failed', e)
      if (e instanceof RemoveDeviceError && e.step === 'nodes') {
        const byId = useNodes.getState().byId
        setError(t('removeDevice.errorNodes', { nodes: e.failedNodes.map((id) => byId[id]?.name ?? id.slice(0, 8)).join(', ') }))
      } else {
        const step = e instanceof RemoveDeviceError ? e.step : 'hub'
        setError(t(step === 'logout' ? 'removeDevice.errorLogout' : 'removeDevice.errorHub', { error: describeError(e instanceof RemoveDeviceError ? e.cause : e, t) }))
      }
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (busy) return
        if (!o && report) return onDone()
        onOpenChange(o)
        if (!o) setError(null)
      }}
      title={t('removeDevice.title')}
      description={report ? undefined : t('removeDevice.body')}
      closeLabel={t('common.close')}
      testId="remove-device-dialog"
    >
      {report ? (
        <div className="space-y-4">
          <p className="text-sm" data-testid="remove-device-report">
            {t('removeDevice.reportSkipped', { count: report.skippedNodes.length })}
          </p>
          <div className="flex justify-end">
            <Button variant="primary" onClick={onDone} data-testid="remove-device-continue">
              {t('removeDevice.continue')}
            </Button>
          </div>
        </div>
      ) : (
        <div className="space-y-4">
          {error ? (
            <p className="rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm text-danger" role="alert" data-testid="remove-device-error">
              {error}
            </p>
          ) : null}
          <div className="flex flex-wrap justify-end gap-2">
            <Button variant="outline" disabled={busy} onClick={() => onOpenChange(false)}>
              {t('common.cancel')}
            </Button>
            {error ? (
              <Button variant="outline-danger" disabled={busy} loading={busy} onClick={() => void run(true)} data-testid="remove-device-force">
                {t('removeDevice.force')}
              </Button>
            ) : null}
            <Button variant="danger" loading={busy} onClick={() => void run(false)} data-testid="remove-device-confirm">
              {error ? t('removeDevice.retry') : t('removeDevice.confirm')}
            </Button>
          </div>
        </div>
      )}
    </Dialog>
  )
}
