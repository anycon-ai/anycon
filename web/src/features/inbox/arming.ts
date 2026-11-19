import { useEffect, useState } from 'react'

/**
 * 详情里显示的请求换了（点了另一行、处理完前进到下一条、深链接、重排）之后，答复按钮和 ⌘⇧A / ⌘⇧D 先锁这么久（毫秒）：
 * 用户来不及看清新出现的内容，连点或连按的第二下就不会落到它头上。锁着的时候按钮是禁用的（看得出来），按键什么都不做。
 */
export const ARM_DELAY_MS = 600

/** 正在显示的条目 key；换了就重新计时。`null`（没有显示）时返回 false。 */
export function useArmed(shownKey: string | null, delayMs: number = ARM_DELAY_MS): boolean {
  // 解锁的是哪个 key：从 key 推出来而不是单独的布尔，换 key 的那一次渲染就已经是锁着的，不会有一帧可点
  const [armedKey, setArmedKey] = useState<string | null>(null)
  // 换回刚离开的那个 key（A → B → A）也要重新计时：渲染期间发现 key 变了就把解锁记录清掉
  const [lastKey, setLastKey] = useState<string | null>(shownKey)
  if (lastKey !== shownKey) {
    setLastKey(shownKey)
    setArmedKey(null)
  }
  useEffect(() => {
    if (shownKey === null) return
    const id = setTimeout(() => setArmedKey(shownKey), delayMs)
    return () => clearTimeout(id)
  }, [shownKey, delayMs])
  return shownKey !== null && armedKey === shownKey
}
