// 节点设置 →「限制」的纯逻辑（docs/spec/web.md § 4.8，ADR-43）：表单草稿 ↔ `node.set_config` 的值、本地校验、环境变量名。
// 约束（范围、选项）来自 `node.get_config`，这里不写死任何键的规则；daemon 才是最终的校验者，本地校验只是少跑一趟。
import type { ConfigItem } from '@/protocol'

/** 表单里一项的草稿：开关是布尔，其余都是输入框里的文字。 */
export type Draft = boolean | string

/** 校验没过的原因，和 `node.set_config` 错误里的 `data.reason` 对应（`type` 和 `format` 合并成 `format`）。 */
export type LimitProblem = 'format' | 'range'

/** 按段分组用：`sessions.max_concurrent` → `sessions`。 */
export function sectionOf(key: string): string {
  return key.split('.')[0] ?? key
}

/** 被环境变量覆盖时，该去哪里改：覆盖这个键的环境变量名。`log.level` 还能被 `ANYCON_LOG` / `RUST_LOG` 接管。 */
export function envNames(key: string): string {
  const own = `ANYCON_${key.replace('.', '__').toUpperCase()}`
  return key === 'log.level' ? `${own}, ANYCON_LOG, RUST_LOG` : own
}

/** 节点上给出的当前值 → 表单草稿。 */
export function toDraft(item: ConfigItem): Draft {
  if (item.kind === 'bool') return item.value === true
  return item.value === null || item.value === undefined ? '' : String(item.value)
}

export function initialDrafts(items: ConfigItem[]): Record<string, Draft> {
  return Object.fromEntries(items.map((i) => [i.key, toDraft(i)]))
}

const UNITS_MS: Record<string, number> = { '': 1000, s: 1000, sec: 1000, secs: 1000, ms: 1, m: 60_000, min: 60_000, mins: 60_000, h: 3_600_000, hr: 3_600_000, hrs: 3_600_000, d: 86_400_000, w: 604_800_000 }

/** 和 daemon 的 `parse_duration` 同一套写法：`30s` `20m` `2h` `7d` `1w` `500ms`，纯数字按秒。解析不了返回 null。 */
export function parseDurationMs(s: string): number | null {
  const m = /^(\d+(?:\.\d+)?|\.\d+)\s*([a-z]*)$/.exec(s.trim())
  if (!m) return null
  const unit = UNITS_MS[m[2]!]
  if (unit === undefined) return null
  return Math.round(Number(m[1]) * unit)
}

/** 毫秒 → 最短的整数写法：300000 → `5m`，604800000 → `7d`，0 → `0`。 */
export function formatLimitDuration(ms: number): string {
  if (ms === 0) return '0'
  for (const [suffix, unit] of [['d', 86_400_000], ['h', 3_600_000], ['m', 60_000], ['s', 1000]] as const) {
    if (ms % unit === 0) return `${ms / unit}${suffix}`
  }
  return `${ms}ms`
}

/** 草稿 → 要发给 `node.set_config` 的值；填的东西连类型都不对（比如整数框里是 `abc`）时返回 undefined。 */
export function toWire(item: ConfigItem, draft: Draft): boolean | number | string | undefined {
  switch (item.kind) {
    case 'bool':
      return draft === true
    case 'int': {
      const s = String(draft).trim()
      return /^\d+$/.test(s) ? Number(s) : undefined
    }
    case 'duration':
      return String(draft).trim()
    default:
      return String(draft)
  }
}

/** 草稿和节点上的当前值不一样（要写才会变）。 */
export function isChanged(item: ConfigItem, draft: Draft): boolean {
  const wire = toWire(item, draft)
  // 填的东西不合法也算「改过」：要让用户看到错误，而不是悄悄当成没改
  return wire === undefined || wire !== item.value
}

/** 本地能判断的问题：数字不是整数 / 越界，时长写不出来 / 越界，文本太长。没问题返回 null。 */
export function localProblem(item: ConfigItem, draft: Draft): LimitProblem | null {
  switch (item.kind) {
    case 'int': {
      const n = toWire(item, draft)
      if (typeof n !== 'number') return 'format'
      return (item.min != null && n < item.min) || (item.max != null && n > item.max) ? 'range' : null
    }
    case 'duration': {
      const ms = parseDurationMs(String(draft))
      if (ms === null) return 'format'
      return (item.min != null && ms < item.min) || (item.max != null && ms > item.max) ? 'range' : null
    }
    case 'text':
      return item.max != null && [...String(draft)].length > item.max ? 'range' : null
    case 'enum':
      return item.options?.includes(String(draft)) || String(draft) === item.value ? null : 'format'
    default:
      return null
  }
}
