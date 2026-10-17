// service worker 里的文案翻译：只带 `push` 这一小段（不打包整份 en.json / zh.json），用独立的 i18next 实例，不碰页面的那一个。
import i18next, { type TFunction } from 'i18next'
import { push as enPush } from '@/i18n/en.json'
import { push as zhPush } from '@/i18n/zh.json'
import type { SwLanguage } from './lang'

const translators = new Map<SwLanguage, TFunction>()

export function pushTranslator(lang: SwLanguage): TFunction {
  let t = translators.get(lang)
  if (!t) {
    const instance = i18next.createInstance()
    // 资源是内联的，`initAsync: false` 让 init 同步完成
    void instance.init({
      lng: lang,
      fallbackLng: 'en',
      resources: { en: { translation: { push: enPush } }, zh: { translation: { push: zhPush } } },
      interpolation: { escapeValue: false },
      initAsync: false,
      returnNull: false,
    })
    t = instance.getFixedT(lang)
    translators.set(lang, t)
  }
  return t
}
