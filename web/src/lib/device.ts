interface NavigatorUAData {
  brands?: { brand: string; version: string }[]
  platform?: string
}

export function detectBrowser(ua: string = navigator.userAgent): string {
  if (/Edg\//.test(ua)) return 'Edge'
  if (/OPR\/|Opera/.test(ua)) return 'Opera'
  if (/Firefox\//.test(ua)) return 'Firefox'
  if (/Chrome\/|CriOS\//.test(ua)) return 'Chrome'
  if (/Safari\//.test(ua)) return 'Safari'
  return 'Browser'
}

export function detectOS(ua: string = navigator.userAgent): string {
  if (/iPhone|iPad|iPod/.test(ua)) return 'iOS'
  if (/Android/.test(ua)) return 'Android'
  if (/Mac OS X|Macintosh/.test(ua)) return 'macOS'
  if (/Windows/.test(ua)) return 'Windows'
  if (/CrOS/.test(ua)) return 'ChromeOS'
  if (/Linux/.test(ua)) return 'Linux'
  const data = (navigator as Navigator & { userAgentData?: NavigatorUAData }).userAgentData
  return data?.platform ?? 'Unknown'
}

/** iPhone / iPad。iPadOS 13+ 的 Safari 自称 Mac，靠触点数分辨。 */
export function isIos(): boolean {
  return /iPad|iPhone|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1)
}

export function isAndroid(): boolean {
  return /Android/.test(navigator.userAgent)
}

/** 默认设备名：`浏览器 · 系统 · origin 主机名`（spec/web.md § 4.3）。 */
export function defaultDeviceName(): string {
  return `${detectBrowser()} · ${detectOS()} · ${location.hostname}`
}
