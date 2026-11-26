// 在新标签页里用目录预览打开节点上的一个文件（ADR-62，docs/spec/web.md § 4.6 / § 4.7）：会话消息里指向 HTML 的链接、
// 文件浏览里的「以网页打开」都走这里。
//
// 要新标签页不被弹窗拦截，`window.open` 得在点击事件里同步调用；而第一次打开时票据要先问 daemon、再问 hub（异步）。所以：
// 缓存里有够新的票据就直接同步打开；否则先同步开一个空白标签页（去掉 opener），拿到票据后再让它跳过去，失败就关掉它。
// 空白页被拦了才退回到「等票据、再 `window.open`」（这时可能被拦，至少不比以前差）。
import { previewTargetOf } from '@/features/files/localPaths'
import { ensureTicket, launchUrl, usableTicket, type PreviewTicket } from './ticket'

export interface OpenDirPreviewArgs {
  nodeId: string
  /** 要打开的文件（文件浏览的路径，`<项目名>/<相对路径>`）。 */
  fsPath: string
  /** `/api/config.preview_origin_template`。 */
  template: string
  /** 从哪个会话打开的（daemon 只用于展示和审计）。 */
  sessionId?: string
  /** 空白标签页里在等待时显示的一句话（已翻译）。 */
  waitingText?: string
  /** 取票据失败。 */
  onError: (e: unknown) => void
}

const FEATURES = 'noopener,noreferrer'

/** 打开预览。不抛错：失败交给 `onError`。返回值只用于测试（等异步的部分做完）。 */
export function openDirPreview({ nodeId, fsPath, template, sessionId, waitingText, onError }: OpenDirPreviewArgs): Promise<void> {
  const target = previewTargetOf(fsPath)
  if (target === null) return Promise.resolve()
  const cached = usableTicket(nodeId, { dir: target.dir })
  if (cached) {
    window.open(launchUrl(cached, target.path), '_blank', FEATURES)
    return Promise.resolve()
  }
  const pending: Promise<PreviewTicket> = ensureTicket({ nodeId, target: { dir: target.dir }, sessionId, template })
  // 同步开一个空白页占住用户激活。不能带 `noopener`（那样拿不到窗口），开出来马上切断 opener：预览页拿不到这个页面
  const tab = window.open('', '_blank')
  if (tab) {
    try {
      tab.opener = null
      if (waitingText) {
        tab.document.title = waitingText
        tab.document.body.textContent = waitingText
      }
    } catch {
      /* 写不了空白页（浏览器限制）：不影响跳转 */
    }
    return pending.then(
      (ticket) => {
        // 启动参数在 fragment 里，不会发给 hub；页面自己的 referrer 策略是 no-referrer
        tab.location.replace(launchUrl(ticket, target.path))
      },
      (e: unknown) => {
        tab.close()
        onError(e)
      },
    )
  }
  return pending.then(
    (ticket) => {
      window.open(launchUrl(ticket, target.path), '_blank', FEATURES)
    },
    (e: unknown) => onError(e),
  )
}
