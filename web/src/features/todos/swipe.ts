// 手机上清单行向左滑 = 完成（spec/web.md § 4.12）。判定是纯函数：只认触摸（鼠标没有滑动，那是按钮的事）、
// 主要是横向、向左、够远。每个滑动动作在长按菜单和详情页里都有对应的按钮，滑动不是唯一入口。权限请求没有任何滑动操作。

/** 向左滑多远算数（像素）。 */
export const SWIPE_DISTANCE = 72
/** 横向位移要比纵向大这么多倍，免得竖着滚动被当成滑动。 */
export const SWIPE_RATIO = 1.6

export type SwipeVerdict = 'done' | 'none'

export function swipeVerdict(dx: number, dy: number, pointerType: string): SwipeVerdict {
  if (pointerType !== 'touch') return 'none'
  if (dx > -SWIPE_DISTANCE) return 'none'
  if (Math.abs(dx) < Math.abs(dy) * SWIPE_RATIO) return 'none'
  return 'done'
}

/** 跟手时行偏移多少（只向左，封顶）。 */
export function swipeOffset(dx: number, dy: number): number {
  if (dx >= 0 || Math.abs(dx) < Math.abs(dy) * SWIPE_RATIO) return 0
  return Math.max(dx, -SWIPE_DISTANCE * 1.6)
}

/** 长按多久算（毫秒）。 */
export const LONG_PRESS_MS = 500
/** 按下之后移动超过这么远就不是长按了（像素）。 */
export const LONG_PRESS_SLOP = 10
