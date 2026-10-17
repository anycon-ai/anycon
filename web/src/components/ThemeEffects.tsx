import { useEffect } from 'react'
import { applyLanguagePref } from '@/i18n'
import { syncSwLanguage } from '@/sw/directory'
import { usePrefs } from '@/stores/prefs'

/** 把外观偏好落到 <html>：主题 class、字号档、紧凑模式、语言。 */
export function ThemeEffects() {
  const theme = usePrefs((s) => s.theme)
  const fontSize = usePrefs((s) => s.fontSize)
  const compact = usePrefs((s) => s.compact)
  const language = usePrefs((s) => s.language)

  useEffect(() => {
    const root = document.documentElement
    const mq = window.matchMedia('(prefers-color-scheme: dark)')
    const apply = (): void => void root.classList.toggle('dark', theme === 'dark' || (theme === 'system' && mq.matches))
    apply()
    if (theme !== 'system') return
    mq.addEventListener('change', apply)
    return () => mq.removeEventListener('change', apply)
  }, [theme])

  useEffect(() => {
    document.documentElement.dataset.font = fontSize
  }, [fontSize])

  useEffect(() => {
    if (compact) document.documentElement.dataset.compact = '1'
    else delete document.documentElement.dataset.compact
  }, [compact])

  useEffect(() => {
    applyLanguagePref(language)
    // service worker 读不到 localStorage：手动选的语言记到它能读的地方，推送通知才用同一种语言（「自动」时它自己跟浏览器语言）
    void syncSwLanguage(language)
  }, [language])

  return null
}
