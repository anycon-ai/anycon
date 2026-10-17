// service worker 的界面语言：页面记下的优先，没有就跟 `navigator.language`（和页面「自动」的规则一致：`zh*` 是中文，其余英文）。
export type SwLanguage = 'en' | 'zh'

export function resolveLanguage(stored: string | undefined, navigatorLanguage: string | undefined): SwLanguage {
  if (stored === 'en' || stored === 'zh') return stored
  return (navigatorLanguage ?? '').toLowerCase().startsWith('zh') ? 'zh' : 'en'
}
