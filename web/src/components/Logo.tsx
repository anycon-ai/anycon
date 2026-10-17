import { useId } from 'react'
import { cn } from '@/ui/cn'

/** 主标外形：A 的轮廓 + 中间的 C（evenodd 挖空）。和 `public/logo.svg`、`public/favicon.svg` 是同一条路径。 */
const BODY =
  'M119 38 C125 25 137 25 143 39 L211 190 C218 205 227 216 238 222 C219 231 198 226 187 211 L164 175 H94 L74 218 C69 228 59 232 48 230 L23 225 C37 218 47 208 53 194 Z'
const HOLE =
  'M156.870058 99.129942 A38 38 0 1 0 156.870058 152.870058 A8 8 0 0 0 145.556349 141.556349 A22 22 0 1 1 145.556349 110.443651 A8 8 0 0 0 156.870058 99.129942 Z'
export const MARK_D = `${BODY} ${HOLE}`

/**
 * 品牌标：彩色主标 + 字标 Anycon。
 * 图形内联自 `public/logo.svg`（设计稿原件，见 DESIGN.md「品牌与图标」），`Logo.test.tsx` 保证外形路径和它一致。
 * 渐变 id 用 `useId` 加前缀，页面上同时有几个 Logo 也不会互相引用错。
 */
export function Logo({ className, withWord = true }: { className?: string; withWord?: boolean }) {
  const id = useId().replace(/[^a-zA-Z0-9_-]/g, '')
  const ref = (name: string) => `url(#${id}-${name})`
  return (
    <span className={cn('inline-flex items-center gap-2 font-semibold tracking-tight', className)}>
      <svg viewBox="0 0 256 256" className="size-7 shrink-0" aria-hidden>
        <defs>
          <linearGradient id={`${id}-base`} x1="35" y1="225" x2="170" y2="53" gradientUnits="userSpaceOnUse">
            <stop stopColor="#294CEB" />
            <stop offset=".48" stopColor="#426BFF" />
            <stop offset="1" stopColor="#6258F5" />
          </linearGradient>
          <linearGradient id={`${id}-fold`} x1="135" y1="55" x2="232" y2="226" gradientUnits="userSpaceOnUse">
            <stop stopColor="#9C8AFF" />
            <stop offset=".4" stopColor="#6655EF" />
            <stop offset=".72" stopColor="#4935CC" />
            <stop offset="1" stopColor="#B19AFF" />
          </linearGradient>
          <linearGradient id={`${id}-light`} x1="46" y1="214" x2="130" y2="43" gradientUnits="userSpaceOnUse">
            <stop stopColor="#43C7FA" />
            <stop offset=".65" stopColor="#557FFF" />
            <stop offset="1" stopColor="#BBB8FF" />
          </linearGradient>
          <clipPath id={`${id}-clip`}>
            <path fillRule="evenodd" clipRule="evenodd" d={MARK_D} />
          </clipPath>
        </defs>
        <g clipPath={ref('clip')}>
          <path d={BODY} fill={ref('base')} />
          <path
            d="M131 28 C127 51 139 79 151 106 L185 182 C198 212 216 224 238 222 L251 245 H202 L174 208 L138 131 L112 62 Z"
            fill={ref('fold')}
          />
          <path
            d="M119 38 C123 29 127 27 132 28 C120 43 113 63 105 84 L58 204 C54 216 41 224 23 225 C37 218 47 208 53 194 Z"
            fill={ref('light')}
          />
        </g>
      </svg>
      {withWord ? <span>Anycon</span> : null}
    </span>
  )
}
