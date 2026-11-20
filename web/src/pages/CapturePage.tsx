import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { useCapture } from '@/features/todos/capture'
import { CaptureForm } from '@/features/todos/ui/CaptureForm'
import { composeSharedText } from '@/sw/share'
import { takeShared } from '@/sw/shareStore'

/**
 * `/capture`（spec/web.md § 14.5）：主屏图标的「记一项」快捷方式、从别的应用分享、hub 回的 303 都落在这里。
 * 分享的内容由 service worker 存在它自己的库里（`src/sw/share.ts`），这里取出来（取出就删）填进输入框——**不会自动添加**，
 * 用户按了发送才记下。分享的内容从来不在地址里。service worker 没接管时那次 POST 经过 hub、内容丢了，这里就是空的输入条。
 */
export function CapturePage() {
  const { t } = useTranslation()
  const [ready, setReady] = useState(false)
  useEffect(() => {
    let live = true
    void takeShared().then((shared) => {
      if (!live) return
      if (shared) {
        const text = composeSharedText(shared)
        if (text) useCapture.getState().setText(text)
      }
      setReady(true)
    })
    return () => {
      live = false
    }
  }, [])
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="capture-page">
      <PageHeader crumbs={[{ label: t('todos.addTitle') }]} showNodeStatus={false} />
      <div className="mx-auto w-full max-w-2xl p-4">{ready ? <CaptureForm /> : null}</div>
    </div>
  )
}
