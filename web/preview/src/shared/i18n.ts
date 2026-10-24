// 预览运行时自带的文案（en / zh）：引导页和 service worker 的错误页共用。不依赖 Web UI 的 i18n。

export type Lang = 'en' | 'zh'

/** 按 `navigator.language` 选语言：中文（含 zh-CN / zh-TW）用中文，其余英文。 */
export function pickLang(language: string | undefined): Lang {
  return (language ?? '').toLowerCase().startsWith('zh') ? 'zh' : 'en'
}

export type TextKey =
  | 'title'
  | 'starting'
  | 'reopen'
  | 'openInTab'
  | 'needsLaunch.title'
  | 'needsLaunch.body'
  | 'expired.title'
  | 'expired.body'
  | 'invalid.title'
  | 'invalid.body'
  | 'unsupported.title'
  | 'unsupported.body'
  | 'loop.title'
  | 'loop.body'
  | 'failed.title'
  | 'failed.body'
  | 'revoked.title'
  | 'revoked.body'
  | 'closed.title'
  | 'closed.body'
  | 'disabled.title'
  | 'disabled.body'
  | 'portNotAllowed.title'
  | 'portNotAllowed.body'
  | 'shutdown.title'
  | 'shutdown.body'
  | 'unauthorized.title'
  | 'unauthorized.body'
  | 'rejected.title'
  | 'rejected.body'
  | 'nodeOffline.title'
  | 'nodeOffline.body'
  | 'connecting.title'
  | 'connecting.body'
  | 'handshakeFailed.title'
  | 'handshakeFailed.body'
  | 'cryptoUnavailable.title'
  | 'cryptoUnavailable.body'
  | 'version.title'
  | 'version.body'
  | 'upstreamRefused.title'
  | 'upstreamRefused.body'
  | 'upstreamTimeout.title'
  | 'upstreamTimeout.body'
  | 'upstreamError.title'
  | 'upstreamError.body'
  | 'tooLarge.title'
  | 'tooLarge.body'

const en: Record<TextKey, string> = {
  title: 'Anycon Preview',
  starting: 'Starting the preview…',
  reopen: 'Go back to Anycon and open the preview again.',
  openInTab: 'Open in a new tab',
  'needsLaunch.title': 'Open this preview from Anycon',
  'needsLaunch.body':
    'This address only works when it is opened from the Preview view in Anycon. Go back to Anycon and open it again.',
  'expired.title': 'This preview has expired',
  'expired.body': 'Previews are only valid for a limited time. Open it again from Anycon.',
  'invalid.title': 'This preview link is not valid',
  'invalid.body': 'Open the preview again from Anycon.',
  'unsupported.title': 'This browser cannot run the preview here',
  'unsupported.body':
    'The preview needs a service worker, which this browser does not allow in this context (for example inside an embedded frame). Open it in a new tab instead.',
  'loop.title': 'The preview could not start',
  'loop.body': 'This page was reloaded several times without the preview taking over. Open the preview again from Anycon.',
  'failed.title': 'The preview could not start',
  'failed.body': 'The preview service worker did not start. Open the preview again from Anycon, or open it in a new tab.',
  'revoked.title': 'This preview was closed',
  'revoked.body': 'The preview was stopped or the device that opened it is no longer allowed to use it.',
  'closed.title': 'This preview was stopped',
  'closed.body': 'It was stopped from Anycon.',
  'disabled.title': 'Preview is turned off on this machine',
  'disabled.body': 'The machine running the dev server no longer allows previews.',
  'portNotAllowed.title': 'This port can no longer be previewed',
  'portNotAllowed.body': 'The machine running the dev server no longer allows previewing this port.',
  'shutdown.title': 'The machine is shutting down',
  'shutdown.body': 'The Anycon daemon that serves this preview is exiting.',
  'unauthorized.title': 'This preview is no longer authorized',
  'unauthorized.body': 'The preview token was rejected, most likely because it expired.',
  'rejected.title': 'The machine refused this preview',
  'rejected.body': 'The Anycon daemon did not accept the preview ticket.',
  'nodeOffline.title': 'The machine is offline',
  'nodeOffline.body': 'The machine running the dev server is not connected to the hub right now. This page retries automatically.',
  'connecting.title': 'Still connecting…',
  'connecting.body': 'The preview could not reach the machine yet. This page retries automatically.',
  'handshakeFailed.title': 'The secure connection failed',
  'handshakeFailed.body': 'The machine did not prove its identity, so the preview was not started.',
  'cryptoUnavailable.title': 'The preview runtime is incomplete',
  'cryptoUnavailable.body': 'The encryption module could not be loaded.',
  'version.title': 'This preview needs an update',
  'version.body': 'The hub and this preview use incompatible protocol versions.',
  'upstreamRefused.title': 'Nothing is listening on that port',
  'upstreamRefused.body': 'The dev server is not running, or it stopped. Start it again and reload this page.',
  'upstreamTimeout.title': 'The dev server did not respond',
  'upstreamTimeout.body': 'It took too long to answer. Reload this page to try again.',
  'upstreamError.title': 'The preview request failed',
  'upstreamError.body': 'The connection to the dev server was interrupted. Reload this page to try again.',
  'tooLarge.title': 'The request is too large',
  'tooLarge.body': 'Request bodies larger than 16 MiB cannot be sent through the preview.',
}

const zh: Record<TextKey, string> = {
  title: 'Anycon 预览',
  starting: '正在启动预览…',
  reopen: '回到 Anycon 重新打开预览。',
  openInTab: '在新标签页打开',
  'needsLaunch.title': '请从 Anycon 打开这个预览',
  'needsLaunch.body': '这个地址只能从 Anycon 的「预览」视图打开。回到 Anycon 重新打开即可。',
  'expired.title': '这个预览已过期',
  'expired.body': '预览只在一段时间内有效。请从 Anycon 重新打开。',
  'invalid.title': '预览链接无效',
  'invalid.body': '请从 Anycon 重新打开预览。',
  'unsupported.title': '这个浏览器在此处无法运行预览',
  'unsupported.body': '预览需要 service worker，而当前环境（例如嵌入的 iframe）不允许。请改用新标签页打开。',
  'loop.title': '预览没能启动',
  'loop.body': '页面重新加载了好几次，预览仍没有接管。请从 Anycon 重新打开预览。',
  'failed.title': '预览没能启动',
  'failed.body': '预览的 service worker 没有启动。请从 Anycon 重新打开，或用新标签页打开。',
  'revoked.title': '这个预览已被关闭',
  'revoked.body': '预览被停止，或者打开它的设备不再有权使用。',
  'closed.title': '这个预览已被停止',
  'closed.body': '它在 Anycon 里被停止了。',
  'disabled.title': '这台机器上的预览已关闭',
  'disabled.body': '运行 dev server 的机器不再允许预览。',
  'portNotAllowed.title': '这个端口不能再预览',
  'portNotAllowed.body': '运行 dev server 的机器不再允许预览这个端口。',
  'shutdown.title': '机器正在关闭',
  'shutdown.body': '提供这个预览的 Anycon daemon 正在退出。',
  'unauthorized.title': '这个预览已失去授权',
  'unauthorized.body': '预览令牌被拒绝，多半是过期了。',
  'rejected.title': '机器拒绝了这个预览',
  'rejected.body': 'Anycon daemon 没有接受预览票据。',
  'nodeOffline.title': '机器不在线',
  'nodeOffline.body': '运行 dev server 的机器现在没有连上 hub。本页会自动重试。',
  'connecting.title': '仍在连接…',
  'connecting.body': '预览暂时连不上那台机器。本页会自动重试。',
  'handshakeFailed.title': '安全连接失败',
  'handshakeFailed.body': '对方没能证明自己的身份，预览没有启动。',
  'cryptoUnavailable.title': '预览运行时不完整',
  'cryptoUnavailable.body': '加密模块没能加载。',
  'version.title': '这个预览需要更新',
  'version.body': 'hub 和这个预览使用的协议版本不兼容。',
  'upstreamRefused.title': '这个端口上没有服务在监听',
  'upstreamRefused.body': 'dev server 没有运行，或者已经停了。重新启动后刷新本页。',
  'upstreamTimeout.title': 'dev server 没有响应',
  'upstreamTimeout.body': '它响应得太慢。刷新本页重试。',
  'upstreamError.title': '预览请求失败',
  'upstreamError.body': '到 dev server 的连接被中断。刷新本页重试。',
  'tooLarge.title': '请求太大',
  'tooLarge.body': '超过 16 MiB 的请求体无法经预览发送。',
}

const TEXTS: Record<Lang, Record<TextKey, string>> = { en, zh }

export function text(lang: Lang, key: TextKey): string {
  return TEXTS[lang][key]
}

/** 测试用：两种语言的键必须一致（类型已经保证，这里给运行时检查留个入口）。 */
export function textKeys(lang: Lang): string[] {
  return Object.keys(TEXTS[lang])
}
