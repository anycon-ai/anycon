import { useEffect, useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { formatPairCodeInput, pairCodeFromLocation } from '@/lib/code'
import { clearPendingPairCode, peekPendingPairCode } from '@/lib/loginRedirect'

/**
 * 配对链接带进来的短码：读 `location.hash`（`/pair#code=XXXX-XXXX`，fragment 不会发给 hub），
 * 兼容旧的 `?code=`；没有的话看登录跳转前暂存在 `sessionStorage` 里的那份（未登录时打开链接，
 * 跳转前 `loginRedirectTarget` 把短码从 URL 里拿走了，见 docs/protocol.md § 4.1：链接短码不进 query）。
 * 读到后只在内存里留着，并把它从地址栏 / 历史记录 / `sessionStorage` 里擦掉。
 * 来自链接的短码绝不自动开始配对，页面要先让用户确认（docs/spec/web.md § 4.3）。
 */
export function useLinkCode(to: '/pair' | '/setup', legacySearchCode?: string): string {
  const navigate = useNavigate()
  // 渲染期只读（纯函数）；删除放在挂载后的 effect 里
  const [code] = useState(() => pairCodeFromLocation(window.location) || (legacySearchCode ? formatPairCodeInput(legacySearchCode) : '') || formatPairCodeInput(peekPendingPairCode(to)))
  useEffect(() => {
    clearPendingPairCode()
    if (code) void navigate({ to, search: {}, hash: '', replace: true })
    // 只在挂载时擦一次
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  return code
}
