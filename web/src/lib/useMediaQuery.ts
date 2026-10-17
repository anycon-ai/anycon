import { useSyncExternalStore } from 'react'

/** 订阅 CSS 媒体查询。服务端 / 不支持 matchMedia 的环境返回 fallback。 */
export function useMediaQuery(query: string, fallback = false): boolean {
  return useSyncExternalStore(
    (cb) => {
      if (typeof window === 'undefined' || !window.matchMedia) return () => {}
      const mq = window.matchMedia(query)
      mq.addEventListener('change', cb)
      return () => mq.removeEventListener('change', cb)
    },
    () => (typeof window !== 'undefined' && window.matchMedia ? window.matchMedia(query).matches : fallback),
    () => fallback,
  )
}

/** ≥768px：桌面 / 平板，选项栏内联；否则选项收进「⚙」底部弹层。 */
export function useIsWide(): boolean {
  return useMediaQuery('(min-width: 768px)', true)
}

/** 粗指针（手机 / 平板触屏）：Enter 默认换行。 */
export function useIsTouch(): boolean {
  return useMediaQuery('(pointer: coarse)', false)
}
