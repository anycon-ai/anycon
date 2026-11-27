// Twin 页的几样小图形：区块标题、额度进度条、唤醒结果的分段条和图例色块。都是普通的 div，不引入图表库。
// 配色只用 token：动手 = accent-text（深色下 accent 本身太暗，做色块也用文字那一档），失败 = danger，其余 = fg-subtle（退到背景）。
// 这三个颜色按「动手、失败、其余」的顺序上下相邻时，浅色和深色下色觉异常也分得开；换顺序或加颜色前要重新校验。
import type { ReactNode } from 'react'
import { cn } from '@/ui/cn'

/** 唤醒结果在图上的三类。预算用完和被取消并进「其余」，和「无事」一起退到背景。 */
export type WakeKind = 'done' | 'failed' | 'other'
export const WAKE_KINDS: readonly WakeKind[] = ['done', 'failed', 'other']
export const WAKE_FILL: Record<WakeKind, string> = { done: 'bg-accent-text', failed: 'bg-danger', other: 'bg-fg-subtle' }

/** 区块标题：DESIGN.md「字号」里的区块标题档（0.75rem / 600 / 大写 / 字距），右边可以放这一块的控件。 */
export function SectionTitle({ children, aside, id }: { children: ReactNode; aside?: ReactNode; id?: string }) {
  return (
    <div className="flex min-h-8 flex-wrap items-center justify-between gap-2">
      <h3 id={id} className="text-xs font-semibold uppercase tracking-wider text-fg-subtle">
        {children}
      </h3>
      {aside}
    </div>
  )
}

/** 图例里的色块（和图上的块同色同形）。只在文字旁边用，颜色不单独承载信息。 */
export function Swatch({ kind }: { kind: WakeKind }) {
  return <span className={cn('inline-block size-2 shrink-0 rounded-[2px]', WAKE_FILL[kind])} aria-hidden />
}

/**
 * 用量对上限的进度条：轨道是同一色系的浅一档，填充在 80% 以下是 accent，到 80% 变 warning，用满是 danger。
 * 数字由旁边的文字给出，这条本身对读屏隐藏。
 */
export function Meter({ value, max }: { value: number; max: number }) {
  const ratio = max > 0 ? Math.min(1, value / max) : 0
  const tone = ratio >= 1 ? 'danger' : ratio >= 0.8 ? 'warning' : 'accent'
  const track = { accent: 'bg-accent-subtle', warning: 'bg-warning-subtle', danger: 'bg-danger-subtle' }[tone]
  const fill = { accent: 'bg-accent-text', warning: 'bg-warning', danger: 'bg-danger' }[tone]
  return (
    <div className={cn('h-1.5 w-full overflow-hidden rounded-full', track)} aria-hidden data-tone={tone}>
      {/* 有用量就至少露出一点，免得 1 / 100 看起来像没用 */}
      <div className={cn('h-full rounded-full', fill)} style={{ width: value > 0 ? `max(${ratio * 100}%, 0.375rem)` : 0 }} />
    </div>
  )
}

/** 一整条按比例分段的细条（今天的唤醒按结果分开）。段之间留 2px 底色的缝，不画边框；为 0 的段不画。 */
export function SplitBar({ parts }: { parts: ReadonlyArray<{ kind: WakeKind; value: number }> }) {
  const total = parts.reduce((a, p) => a + p.value, 0)
  return (
    <div className="flex h-1.5 w-full gap-0.5 overflow-hidden rounded-full bg-bg-muted" aria-hidden>
      {total > 0 ? parts.filter((p) => p.value > 0).map((p) => <div key={p.kind} className={cn('h-full', WAKE_FILL[p.kind])} style={{ flexGrow: p.value, flexBasis: 0 }} />) : null}
    </div>
  )
}

/** 一次运行结果的小圆点：完成 = success、失败 = danger、预算用完 = warning、进行中 = accent（缓慢脉冲），其余中性。旁边总有结果的文字。 */
export function ResultDot({ run }: { run: { result: string; finished_ms?: number | null } }) {
  const tone = run.finished_ms == null ? 'bg-accent-text animate-pulse-soft' : run.result === 'done' ? 'bg-success' : run.result === 'failed' ? 'bg-danger' : run.result === 'budget' ? 'bg-warning' : 'bg-fg-subtle'
  return <span className={cn('inline-block size-1.5 shrink-0 rounded-full', tone)} aria-hidden />
}
