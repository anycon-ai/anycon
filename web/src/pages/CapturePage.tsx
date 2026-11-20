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
 * 用户按了发送才记下。分享的内容从来不在地址里。没有这个处理的 service worker 接管时那次 POST 经过 hub、内容丢了，这里就是空的输入条。
 */
// 取分享暂存只取一次：React StrictMode（开发）里 effect 会跑两遍，第一遍取走并删掉之后结果要给第二遍用，不能丢。
// 同一时刻所有挂载共用一个 promise，最后一个卸载之后下一个任务里清掉，下次进页面重新取。
let pending: ReturnType<typeof takeShared> | null = null
let users = 0
function acquire(): ReturnType<typeof takeShared> {
  users++
  pending ??= takeShared()
  return pending
}
function release(): void {
  users--
  setTimeout(() => {
    if (users === 0) pending = null
  }, 0)
}

export function CapturePage() {
  const { t } = useTranslation()
  const [ready, setReady] = useState(false)
  // 页面自己带着表单：期间别的入口（`C`、「＋」）不再另开条 / 弹层，免得两个表单绑着同一段文字
  useEffect(() => {
    useCapture.getState().setInline(true)
    return () => useCapture.getState().setInline(false)
  }, [])
  useEffect(() => {
    let live = true
    void acquire().then((shared) => {
      if (!live) return
      if (shared) {
        const text = composeSharedText(shared)
        if (text) useCapture.getState().setText(text)
      }
      setReady(true)
    })
    return () => {
      live = false
      release()
    }
  }, [])
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="capture-page">
      <PageHeader crumbs={[{ label: t('todos.addTitle') }]} showNodeStatus={false} />
      <div className="mx-auto w-full max-w-2xl p-4">{ready ? <CaptureForm /> : null}</div>
    </div>
  )
}
