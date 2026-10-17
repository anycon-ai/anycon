import i18n from 'i18next'
import LanguageDetector from 'i18next-browser-languagedetector'
import { initReactI18next } from 'react-i18next'
import en from './en.json'
import zh from './zh.json'

export const LANGUAGES = ['en', 'zh'] as const
export type Language = (typeof LANGUAGES)[number]

// 英文是默认语言（公共 hub 面向全球用户），按浏览器语言自动选；设置里可以强制。
void i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources: { en: { translation: en }, zh: { translation: zh } },
    fallbackLng: 'en',
    supportedLngs: LANGUAGES,
    nonExplicitSupportedLngs: true,
    interpolation: { escapeValue: false },
    detection: { order: ['navigator'], caches: [] },
    returnNull: false,
  })

/** 应用语言偏好：auto 回到浏览器语言。返回实际生效的语言。 */
export function applyLanguagePref(pref: 'auto' | Language): Language {
  const target: Language = pref === 'auto' ? (navigator.language.toLowerCase().startsWith('zh') ? 'zh' : 'en') : pref
  if (i18n.resolvedLanguage !== target) void i18n.changeLanguage(target)
  document.documentElement.lang = target
  return target
}

export default i18n
