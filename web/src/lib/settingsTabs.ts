// 节点设置的标签（`/n/:node/settings?tab=`）。单独一个小模块：路由文件的 `validateSearch` 要用它，
// 但不能因此把整个节点设置页（和它的所有标签）拽进主包。
export const SETTINGS_TABS = ['agent', 'devices', 'enroll', 'workspace', 'limits', 'about'] as const
export type SettingsTab = (typeof SETTINGS_TABS)[number]

/** `?tab=` 里认得的标签（其余当没写）。安全提醒的 toast 和推送点进来时带 `tab=enroll`（ADR-36）。 */
export function parseSettingsTab(v: unknown): SettingsTab | undefined {
  return typeof v === 'string' && (SETTINGS_TABS as readonly string[]).includes(v) ? (v as SettingsTab) : undefined
}
