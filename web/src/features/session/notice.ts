// `notice` 事件（docs/protocol.md § 6）的文案：认识的 code 用界面语言写，agent 给的原文 `message` 作补充；
// 不认识的 code 按 level 直接显示 message。纯函数，不碰 React。
import type { TFunction } from 'i18next'
import type { NoticeBlock } from './reducer'

/**
 * 有本地化文案的 code → i18n key。`command_output` 不在里面（它整块显示命令输出），
 * `agent_notice` 也不在（agent 的原文就是内容）。
 */
export const NOTICE_TEXT_KEYS = {
  model_fallback: 'notice.modelFallback',
  context_compacted: 'notice.contextCompacted',
  rate_limit: 'notice.rateLimit',
  config_warning: 'notice.configWarning',
  deprecation: 'notice.deprecation',
  mcp_server_failed: 'notice.mcpServerFailed',
  permission_denied: 'notice.permissionDenied',
  auto_review: 'notice.autoReview',
  hook_failed: 'notice.hookFailed',
} as const satisfies Record<string, string>

/** 其它几处要用到的 key（按参数是否齐全选的变体）。 */
export const NOTICE_EXTRA_KEYS = ['notice.modelFallbackTo', 'notice.modelFallbackGeneric', 'notice.rateLimitNoTime', 'notice.commandOutput', 'notice.unknown'] as const

export type LocalizedNoticeCode = keyof typeof NOTICE_TEXT_KEYS
export const isLocalizedNotice = (code: string): code is LocalizedNoticeCode => Object.hasOwn(NOTICE_TEXT_KEYS, code)

export interface NoticeText {
  /** 一行的主文案。 */
  summary: string
  /** 补充：agent 的原文（可能多行）。没有就不能展开。 */
  detail?: string
}

/** 恢复时间的本地时间：当天只写时间，其它日子带日期。 */
export function formatResetTime(ms: number, lang: string, now: number = Date.now()): string {
  const d = new Date(ms)
  const sameDay = d.toDateString() === new Date(now).toDateString()
  return sameDay ? d.toLocaleTimeString(lang, { timeStyle: 'short' }) : d.toLocaleString(lang, { dateStyle: 'medium', timeStyle: 'short' })
}

/** 超过这个长度的单行原文也收起来，点开看全文。 */
const INLINE_MAX = 160

export function noticeText(t: TFunction, lang: string, b: NoticeBlock, modelLabel: (id: string) => string = (id) => id, now: number = Date.now()): NoticeText {
  const message = b.message.trim()
  if (!isLocalizedNotice(b.code)) {
    // agent_notice 和不认识的 code：原文就是内容；很长或多行的收成第一行，点开看全文
    const first = message.split('\n', 1)[0] ?? ''
    if (!first) return { summary: t('notice.unknown') }
    return message.length > INLINE_MAX || message !== first ? { summary: first.length > INLINE_MAX ? `${first.slice(0, INLINE_MAX)}…` : first, detail: message } : { summary: message }
  }
  let summary: string
  switch (b.code) {
    case 'model_fallback':
      summary = b.modelTo ? (b.modelFrom ? t('notice.modelFallback', { from: modelLabel(b.modelFrom), to: modelLabel(b.modelTo) }) : t('notice.modelFallbackTo', { to: modelLabel(b.modelTo) })) : t('notice.modelFallbackGeneric')
      break
    case 'rate_limit':
      summary = b.resetsAtMs != null ? t('notice.rateLimit', { time: formatResetTime(b.resetsAtMs, lang, now) }) : t('notice.rateLimitNoTime')
      break
    default:
      summary = t(NOTICE_TEXT_KEYS[b.code])
  }
  return message && message !== summary ? { summary, detail: message } : { summary }
}
