// 「待处理」页的选中和焦点（spec/web.md § 14.2、§ 14.4、§ 4.11）：
//   split（≥ 1200px）  选中的条目显示在右半边，默认选中第一条；
//   overlay（平板）     选中 = 详情从右侧滑出，只在用户操作（点一行、深链接）之后才滑出；
//   inline（手机）      选中 = 展开，页面打开时默认展开第一条，再点一下收起。
// 选中跟着条目的 key，不跟位置：前面插进来一条新的，用户正在看的不变。
// 选中的条目从列表里消失时分两种：
//   在这个页面上处理的（答复、已处理、归档）→ 选中和焦点落到下一条；
//   别的方式离开的（别的设备答复了、在会话页答复了、过期了、标记清掉了）→ 不替用户挑下一条（它的按钮会立刻变成可点的，用户没看过内容），
//   留一个「这一条已经不在等你了」的占位（`ghost`），带「下一条」按钮；用户自己改了节点过滤器导致它离开的，直接清掉选中，不留占位。
import { useCallback, useEffect, useRef, useState, type RefObject } from 'react'
import { consumeHandled } from '@/features/session/inflight'
import type { InboxItem } from './derive'

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

/** 选中的条目不在别处处理掉之后留下的占位。 */
export interface Ghost {
  key: string
  /** 它消失前在列表里的位置：「下一条」取这个位置上现在的条目。 */
  index: number
  /** 它的快照：占位上要写「打开会话」。 */
  item: InboxItem | null
  /** 它是「等待审批（缓存）」，现在同一个会话的真正的请求到了：占位上给「显示请求」（用户自己点了才选中，没看过内容就不能答复）。 */
  replacedBy: string | null
}

export interface InboxSelection {
  /** 每次用户（或深链接）改选中就加一：深链接晚到的定位据此判断用户有没有在这期间自己选过别的。 */
  version: number
  /** 页面在自己放焦点（处理完前进、清空）：平板的抽屉关掉时 Radix 不要把焦点抢回已经不在的触发元素。 */
  focusOwned: RefObject<boolean>
  selected: string | null
  /** 选中是用户自己做的（点了一行、深链接）：平板上只有这时才滑出详情，窗口缩放带来的自动选中不弹。 */
  explicit: boolean
  ghost: Ghost | null
  /** 点一下一行：split / overlay 选中它；inline 在展开和收起之间切换。 */
  select: (key: string) => void
  /** 收起 / 关掉详情。 */
  clear: () => void
  /** 选中并（可选）把焦点放到那一行。 */
  choose: (key: string, opts?: { focus?: boolean }) => void
  /** J / K：上一条、下一条。 */
  move: (delta: 1 | -1) => void
  /** 占位上的「下一条」：选中它原来位置上的条目。 */
  next: () => void
  /** 把焦点移进详情（Enter）。 */
  focusDetail: () => void
  /**
   * 平板抽屉关掉时的焦点归还（接到 `Drawer` 的 `onCloseAutoFocus`）：Radix 默认把焦点还给打开它的那一行，那一行可能早就没了。
   * 这里总是自己放：页面刚把焦点放好了（没有条目了）就不动，否则放到当前选中的那一行，没有选中的就放到列表标题上。
   */
  onDrawerCloseFocus: (e: { preventDefault: () => void }) => void
}

/**
 * `items`：列表里的条目，按显示顺序。`active`：页面此刻是否在眼前（手机上标签页常驻、只是藏起来，藏着的时候不抢焦点）。
 * `empty`：一条都没有时，焦点落到的元素（「没有等你的事」那个标题）。
 */
export function useInboxSelection({ items, picked, mode, active, ready, root, empty }: { items: readonly InboxItem[]; /** 左栏节点过滤器勾选的节点。 */ picked: readonly string[]; mode: InboxMode; active: boolean; /** 勾选的节点这一轮同步都定下来了：刚打开页面时条目还在路上，要等它。 */ ready: boolean; root: RefObject<HTMLElement | null>; empty: RefObject<HTMLElement | null> }): InboxSelection {
  const keys = items.map((i) => i.key)
  const keySig = keys.join('\u0000')
  const [state, setState] = useState<{ key: string | null; explicit: boolean; ghost: Ghost | null; version: number }>({ key: null, explicit: false, ghost: null, version: 0 })
  const current = useRef(state)
  current.current = state
  const prev = useRef<{ keys: readonly string[]; items: ReadonlyMap<string, InboxItem> }>({ keys, items: new Map(items.map((i) => [i.key, i])) })
  // 手机上「默认展开第一条」只决定一次：页面在眼前，并且已经有条目、或者勾选的节点都同步完了仍然没有；之后列表怎么变都不替用户展开
  const defaulted = useRef(false)
  const focusOwned = useRef(false)
  // 处理完最后一条「稍后」之后：不再替用户默认选中（不跳进「现在」的请求），直到用户自己点 / 按键选了什么
  const noDefault = useRef(false)
  const prevPicked = useRef(picked)
  // 最近一次选中的条目：抽屉关掉时焦点要回到它那一行
  const lastKey = useRef<string | null>(null)
  const pendingFocus = useRef<string | 'empty' | null>(null)

  const apply = useCallback((next: { key: string | null; explicit: boolean; ghost: Ghost | null }, user = false) => {
    if (user) noDefault.current = false
    const full = { ...next, version: current.current.version + (user ? 1 : 0) }
    if (next.key !== null) lastKey.current = next.key
    current.current = full
    setState(full)
  }, [])

  useEffect(() => {
    const before = prev.current
    prev.current = { keys, items: new Map(items.map((i) => [i.key, i])) }
    const cur = current.current
    const beforePicked = prevPicked.current
    prevPicked.current = picked
    if (cur.ghost && keys.includes(cur.ghost.key)) {
      // 占位上的条目又回来了（比如请求在重连后重新到了）：占位撤掉
      apply({ ...cur, ghost: null })
      return
    }
    if (cur.key !== null && !keys.includes(cur.key)) {
      if (cur.ghost?.key === cur.key) return
      const index = Math.max(before.keys.indexOf(cur.key), 0)
      const gone = before.items.get(cur.key) ?? null
      if (gone && beforePicked.includes(gone.nodeId) && !picked.includes(gone.nodeId)) {
        // 用户自己改了节点过滤器（这一次从勾上变成没勾），它才从列表里离开：不是「没了」，清掉选中就行。
        // 从通知点进来的、本来就在过滤器之外的那一条不算：它和别的条目一样走占位 / 前进
        apply({ key: null, explicit: cur.explicit, ghost: null })
        return
      }
      // 页面藏在后面时什么都不替用户选、不前进、不消费「在这里处理」的记录：一律占位，等用户回来自己点
      if (active && consumeHandled(cur.key)) {
        // 我在这里处理的：落到下一条（原来的位置，最后一条就是前一条）
        // 前进到下一条有内容可看的：「等待审批（缓存）」没有详情；答复之后摘要还没更新的那一会儿它会冒出来，别落到它头上
        // 「稍后」里的条目从来不替用户选中：处理完一条「现在」里的，不会落到「稍后」；处理完一条「稍后」里的，只落到「稍后」里的下一条
        // （没有了就不选，桌面上随后由默认选中落到「现在」的第一条，那时详情换了，答复按钮照常先锁一会儿）
        const readable = items.filter((i) => i.kind !== 'waiting' && (gone?.section === 'later') === (i.section === 'later')).map((i) => i.key)
        const next = readable.length > 0 ? (readable.find((k) => keys.indexOf(k) >= index) ?? readable[readable.length - 1]!) : null
        const ae = document.activeElement
        // 被处理掉的那一行（或详情里的按钮）正有焦点：它消失后焦点会掉到 body 上
        const had = !ae || ae === document.body || !!root.current?.contains(ae) || !!ae.closest?.(`.${OVERLAY_CLASS}`)
        apply({ key: next, explicit: cur.explicit, ghost: null })
        if (next === null && gone?.section === 'later') noDefault.current = true
        if (had && active) pendingFocus.current = next ?? 'empty'
      } else {
        const replacement = gone?.kind === 'waiting' ? items.find((i) => (i.kind === 'permission' || i.kind === 'plan' || i.kind === 'question') && i.sessionId === gone.sessionId) : undefined
        apply({ key: cur.key, explicit: cur.explicit, ghost: { key: cur.key, index, item: gone, replacedBy: replacement?.key ?? null } })
      }
      return
    }
    if (!active) return
    // 默认选中第一条有内容可看的：「等待审批（缓存）」没有详情，不当默认（等真正的请求到了再选，免得先选中占位、再被请求替换）
    // 「稍后」里的条目不当默认（不是在等人答复，不该替用户展开 / 选中）
    const first = items.find((i) => i.kind !== 'waiting' && i.section !== 'later')
    if (cur.key === null && first && !noDefault.current) {
      if (mode === 'split') apply({ key: first.key, explicit: false, ghost: null })
      else if (mode === 'inline' && !defaulted.current) apply({ key: first.key, explicit: false, ghost: null })
    }
    // 手机上这个决定只做一次：页面第一次在眼前、并且数据到了（有条目，或者节点都同步完了仍然没有）的时候。
    // 刚打开页面、同步还没完时先等，不然冷启动时（条目晚到）永远不会默认展开
    if (first || ready) defaulted.current = true
    // keySig 代表 keys 的内容
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keySig, mode, active, ready, apply, root, picked])

  useEffect(() => {
    const target = pendingFocus.current
    if (target === null) return
    pendingFocus.current = null
    // 平板的抽屉关掉时 Radix 默认会把焦点还给触发它的元素（已经不在了）：抽屉的 `onCloseAutoFocus` 看这个标记，由页面自己同步地放
    // 抽屉要关了（没有下一条）才算页面在自己放焦点；前进到下一条时抽屉还开着，不占着
    if (mode === 'overlay' && target === 'empty') {
      focusOwned.current = true
      setTimeout(() => (focusOwned.current = false), 200)
    }
    if (target === 'empty') empty.current?.focus()
    else if (mode === 'overlay') (detailFocusable() ?? rowButton(target))?.focus()
    else rowButton(target)?.focus()
  }, [state, mode, empty])

  const choose = useCallback(
    (key: string, opts: { focus?: boolean } = {}) => {
      apply({ key, explicit: true, ghost: null }, true)
      if (opts.focus) rowButton(key)?.focus()
    },
    [apply],
  )
  const select = useCallback(
    (key: string) => {
      const c = current.current
      if (mode === 'inline' && c.key === key && !c.ghost) apply({ key: null, explicit: true, ghost: null }, true)
      else apply({ key, explicit: true, ghost: null }, true)
    },
    [apply, mode],
  )
  const clear = useCallback(() => apply({ key: null, explicit: true, ghost: null }, true), [apply])
  const next = useCallback(() => {
    const g = current.current.ghost
    if (!g || keys.length === 0) {
      apply({ key: null, explicit: true, ghost: null }, true)
      return
    }
    const key = keys[clamp(g.index, keys.length)]!
    apply({ key, explicit: true, ghost: null }, true)
    pendingFocus.current = key
  }, [apply, keys])
  const move = useCallback(
    (delta: 1 | -1) => {
      if (keys.length === 0) return
      const c = current.current
      let i: number
      if (c.ghost) i = clamp(delta === 1 ? c.ghost.index : c.ghost.index - 1, keys.length)
      else i = c.key === null || !keys.includes(c.key) ? (delta === 1 ? 0 : keys.length - 1) : clamp(keys.indexOf(c.key) + delta, keys.length)
      const key = keys[i]!
      choose(key, { focus: true })
      rowButton(key)?.scrollIntoView?.({ block: 'nearest' })
    },
    [keys, choose],
  )
  const focusDetail = useCallback(() => {
    detailFocusable()?.focus()
  }, [])

  const onDrawerCloseFocus = useCallback(
    (e: { preventDefault: () => void }) => {
      e.preventDefault()
      if (focusOwned.current) {
        focusOwned.current = false
        return
      }
      const key = current.current.key ?? lastKey.current
      const row = key ? rowButton(key) : null
      ;(row ?? empty.current)?.focus()
    },
    [empty],
  )

  return { onDrawerCloseFocus, version: state.version, focusOwned, selected: state.key, explicit: state.explicit, ghost: state.ghost, select, clear, choose, move, next, focusDetail }
}
