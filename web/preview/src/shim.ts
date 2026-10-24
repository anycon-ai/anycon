// `shim.js` 的入口：作为页面的第一个脚本执行（service worker 把 `<script src="/__anycon/shim.js" data-cookies="…">`
// 注入到导航响应的 `<head>` 里）。
import { installShim } from './shim/install'

const script = document.currentScript
installShim(window, { cookies: script instanceof HTMLScriptElement ? (script.dataset.cookies ?? '') : '' })
