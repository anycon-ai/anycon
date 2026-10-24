// 引导页的入口（`index.html` 里的 `<script type="module">`）。流程在 `boot/flow.ts`，这里把它接到真实的浏览器上。
import './boot.css'
import { loadLaunch, saveLaunch } from './shared/launch'
import { pickLang } from './shared/i18n'
import { createReloadGuard, runBoot, waitForActivation, type BootEnv } from './boot/flow'
import { renderView } from './boot/view'

const SW_URL = '/__anycon/sw.js'

function serviceWorkerRegistrar(): (() => Promise<void>) | undefined {
  if (!('serviceWorker' in navigator)) return undefined
  return async () => {
    const registration = await navigator.serviceWorker.register(SW_URL, { scope: '/' })
    await waitForActivation(registration)
  }
}

function sessionStorageOrUndefined(): Storage | undefined {
  try {
    return window.sessionStorage
  } catch {
    return undefined
  }
}

const root = document.getElementById('root')
if (root) {
  const lang = pickLang(navigator.language)
  const env: BootEnv = {
    location: window.location,
    now: () => Date.now(),
    replaceState: (url) => window.history.replaceState(null, '', url),
    navigate: (url) => window.location.replace(url),
    reload: () => window.location.reload(),
    registerServiceWorker: serviceWorkerRegistrar(),
    saveLaunch,
    loadLaunch,
    allowReload: createReloadGuard(sessionStorageOrUndefined()),
    show: (view) => renderView(document, root, lang, view),
  }
  void runBoot(env)
}
