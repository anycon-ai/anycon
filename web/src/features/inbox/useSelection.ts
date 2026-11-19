// 「待处理」页的选中和焦点（spec/web.md § 14.2、§ 14.4）：
//   split（≥ 1200px）  选中的条目显示在右半边，默认选中第一条；
//   overlay（平板）     选中 = 详情从右侧滑出，默认不选；
//   inline（手机）      选中 = 展开，默认展开第一条，再点一下收起。
// 选中的条目处理完（从列表里消失）之后，选中和焦点都落到下一条，不用回到列表再点。
import { useCallback, useEffect, useRef, useState, type RefObject } from 'react'

export type InboxMode = 'split' | 'overlay' | 'inline'

/** 弹出详情的容器带这个 class：焦点在里面时，条目消失后焦点也要接着落到下一条的详情里。 */
export const OVERLAY_CLASS = 'inbox-overlay'

const rowButton = (key: string): HTMLElement | null => document.querySelector<HTMLElement>(`[data-inbox-row="${CSS.escape(key)}"] [data-inbox-focus]`)

/** 详情里第一个能聚焦的东西（按钮、链接、输入框）。 */
export function detailFocusable(): HTMLElement | null {
  return document.querySelector<HTMLElement>('[data-inbox-detail] :is(button:not([disabled]), a[href], input:not([disabled]), textarea:not([disabled]))')
}

function clamp(i: number, len: number): number {
  return Math.min(Math.max(i, 0), len - 1)
}

export interface InboxSelection {
  selected: string | null
  /** 点一下一行：split / overlay 选中它；inline 在展开和收起之间切换。 */
  select: (key: string) => void
  /** 收起 / 关掉详情。 */
  clear: () => void
  /** 选中并（可选）把焦点放到那一行。 */
  choose: (key: string, opts?: { focus?: boolean }) => void
  /** J / K：上一条、下一条。 */
  move: (delta: 1 | -1) => void
  /** 把焦点移进详情（Enter）。 */
  focusDetail: () => void
}

/**
 * `keys`：列表里条目的 key，按显示顺序。`active`：页面此刻是否在眼前（手机上标签页常驻、只是藏起来，藏着的时候不抢焦点）。
 * `empty`：一条都没有时，焦点落到的元素（「没有等你的事」那个标题）。
 */
export function useInboxSelection({ keys, mode, active, root, empty }: { keys: readonly string[]; mode: InboxMode; active: boolean; root: RefObject<HTMLElement | null>; empty: RefObject<HTMLElement | null> }): InboxSelection {
  const [selected, setSelected] = useState<string | null>(null)
  const current = useRef<string | null>(null)
  const prevKeys = useRef<readonly string[]>(keys)
  // 用户自己收起过 / 选过之后，不再替他默认展开第一条
  const touched = useRef(false)
  const pendingFocus = useRef<string | 'empty' | null>(null)

  const apply = useCallback((next: string | null) => {
    current.current = next
    setSelected(next)
  }, [])

  useEffect(() => {
    const prev = prevKeys.current
    prevKeys.current = keys
    if (keys.length === 0) touched.current = false
    const cur = current.current
    if (cur !== null && !keys.includes(cur)) {
      const next = keys.length > 0 ? keys[clamp(prev.indexOf(cur), keys.length)]! : null
      const ae = document.activeElement
      // 被处理掉的那一行（或详情里的按钮）正有焦点：它消失后焦点会掉到 body 上
      const had = !ae || ae === document.body || !!root.current?.contains(ae) || !!ae.closest?.(`.${OVERLAY_CLASS}`)
      apply(next)
      if (had && active) pendingFocus.current = next ?? 'empty'
      return
    }
    if (cur === null && !touched.current && keys.length > 0 && mode !== 'overlay') apply(keys[0]!)
  }, [keys, mode, active, apply, root])

  useEffect(() => {
    const target = pendingFocus.current
    if (target === null) return
    pendingFocus.current = null
    const focus = (): void => {
      if (target === 'empty') empty.current?.focus()
      else if (mode === 'overlay') (detailFocusable() ?? rowButton(target))?.focus()
      else rowButton(target)?.focus()
    }
    // 弹出详情关掉时 Radix 会在下一个 tick 把焦点还给触发它的元素（已经不在了）：等它做完再放
    if (mode === 'overlay') setTimeout(focus, 50)
    else focus()
  }, [selected, keys, mode, empty])

  const choose = useCallback(
    (key: string, opts: { focus?: boolean } = {}) => {
      touched.current = true
      apply(key)
      if (opts.focus) rowButton(key)?.focus()
    },
    [apply],
  )
  const select = useCallback(
    (key: string) => {
      touched.current = true
      apply(mode === 'inline' && current.current === key ? null : key)
    },
    [apply, mode],
  )
  const clear = useCallback(() => {
    touched.current = true
    apply(null)
  }, [apply])
  const move = useCallback(
    (delta: 1 | -1) => {
      if (keys.length === 0) return
      const i = current.current === null ? (delta === 1 ? 0 : keys.length - 1) : clamp(keys.indexOf(current.current) + delta, keys.length)
      const key = keys[i]!
      choose(key, { focus: true })
      rowButton(key)?.scrollIntoView?.({ block: 'nearest' })
    },
    [keys, choose],
  )
  const focusDetail = useCallback(() => {
    detailFocusable()?.focus()
  }, [])

  return { selected, select, clear, choose, move, focusDetail }
}
