import { useEffect, useRef, useState, type MouseEvent, type PointerEvent, type RefObject } from 'react'

/** 指针停在窄条上多久才滑出：只是划过左边缘不弹。 */
const OPEN_DELAY = 120
/** 指针离开多久才收回：斜着移出一点再回来不闪。 */
const CLOSE_DELAY = 250
/** 浮出左栏右缘外这么宽也算在里面：拖宽度的热区骑在边框外侧。 */
const EDGE_SLACK = 8

/**
 * 页面上开着菜单 / 对话框，或者正在拖左栏宽度：这时不因为指针离开就收回浮出的左栏。
 * 菜单和对话框渲染在 portal 里，指针移过去就离开了左栏的矩形；Radix 的模态层还会给 body 加 `pointer-events: none`。
 */
function engaged(): boolean {
  return document.documentElement.dataset.resizing !== undefined || document.querySelector('[role="menu"], [role="dialog"], [role="alertdialog"]') !== null
}

/**
 * 宽屏左栏收起（没钉住）时的浮出：鼠标移到窄条上滑出，移出窄条和浮出的左栏后收回；
 * 点浮出层外面或按 Esc 也收回。触屏没有悬停，点窄条的空白处滑出。
 * `enabled`：左栏没钉住；`flyout`：浮出的左栏，用它的右缘判断指针还在不在左栏里（窄条在它左边）。
 */
export function useSidebarPeek(enabled: boolean, flyout: RefObject<HTMLElement | null>) {
  const [peek, setPeek] = useState(false)
  // 钉住 / 取消钉住时丢掉浮出状态，下次取消钉住不会直接弹出来
  const [prevEnabled, setPrevEnabled] = useState(enabled)
  if (prevEnabled !== enabled) {
    setPrevEnabled(enabled)
    setPeek(false)
  }
  const timer = useRef<{ id: number; open: boolean } | null>(null)
  const cancel = (): void => {
    if (timer.current) window.clearTimeout(timer.current.id)
    timer.current = null
  }
  const schedule = (open: boolean, ms: number): void => {
    if (timer.current?.open === open) return
    cancel()
    timer.current = {
      open,
      id: window.setTimeout(() => {
        timer.current = null
        setPeek(open)
      }, ms),
    }
  }
  const shown = enabled && peek

  useEffect(() => {
    if (!shown) return
    const inside = (x: number): boolean => {
      const r = flyout.current?.getBoundingClientRect()
      return !!r && x <= r.right + EDGE_SLACK
    }
    const onMove = (e: globalThis.PointerEvent): void => {
      if (e.pointerType === 'touch') return
      if (inside(e.clientX)) {
        if (timer.current && !timer.current.open) cancel()
      } else if (!engaged()) schedule(false, CLOSE_DELAY)
    }
    // 捕获阶段：菜单还开着的时候这一下是关菜单，左栏留着
    const onDown = (e: globalThis.PointerEvent): void => {
      if (!inside(e.clientX) && !engaged()) {
        cancel()
        setPeek(false)
      }
    }
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape' && !engaged()) {
        cancel()
        setPeek(false)
      }
    }
    document.addEventListener('pointermove', onMove)
    document.addEventListener('pointerdown', onDown, true)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('pointermove', onMove)
      document.removeEventListener('pointerdown', onDown, true)
      document.removeEventListener('keydown', onKey)
    }
    // schedule / cancel 只碰 ref 和 setState，不需要跟着重订
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shown, flyout])

  // 钉住 / 取消钉住、卸载时清掉没到点的定时器
  useEffect(() => cancel, [enabled])

  return {
    peek: shown,
    /** 立即浮出（点了窄条上的待审批个数）。 */
    open: (): void => {
      cancel()
      setPeek(true)
    },
    /** 收回（在浮出的左栏里点了链接、打开了搜索等）。 */
    close: (): void => {
      cancel()
      setPeek(false)
    },
    /** 挂在窄条上的事件。 */
    railProps: {
      onPointerEnter: (e: PointerEvent<HTMLElement>): void => {
        if (e.pointerType !== 'touch') schedule(true, OPEN_DELAY)
      },
      onPointerLeave: (): void => {
        // 还没滑出就离开了：取消；已经滑出的由上面的 pointermove 判断
        if (timer.current?.open) cancel()
      },
      // 只认点在窄条空白处（触屏）；窄条上的按钮各干各的
      onClick: (e: MouseEvent<HTMLElement>): void => {
        if (e.target === e.currentTarget) {
          cancel()
          setPeek(true)
        }
      },
    },
  }
}
