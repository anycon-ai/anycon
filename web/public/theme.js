// 在首帧前套用主题 / 字号，避免闪白。偏好在 localStorage（zustand persist 的 anycon.prefs）。
// 单独成文件而不是内联在 index.html 里，这样 hub 的 CSP（script-src 'self'）不需要 'unsafe-inline' 或 hash。
try {
  var p = JSON.parse(localStorage.getItem('anycon.prefs') || '{}').state || {}
  var dark = p.theme === 'dark' || ((!p.theme || p.theme === 'system') && matchMedia('(prefers-color-scheme: dark)').matches)
  document.documentElement.classList.toggle('dark', dark)
  if (p.fontSize) document.documentElement.dataset.font = p.fontSize
  if (p.compact) document.documentElement.dataset.compact = '1'
} catch (e) {}
