// 分享目标（PWA `share_target`，spec/web.md § 8、§ 4.12.1）：从别的应用「分享到 anycon」会向 `/capture` POST 一份 multipart 表单
// （`title` / `text` / `url`）。分享的内容是用户的东西，**不能进 URL**（URL 会留在 hub 和它前面反代的访问日志里），
// 所以 service worker 在页面之前拦下这个 POST：把三个字段存进它自己的小库（`anycon-share`，只有这一份、页面读完就删），
// 回 303 到 `/capture`（没有 query），页面取出来填进输入条，用户按了发送才会记下来；没有任何东西被自动添加。
//
// 这个文件只有纯逻辑：库和 `Response` 都由调用方（`install.ts`）注入，可以在 jsdom 里用假对象测试。

export const CAPTURE_PATH = '/capture'

/** 这是不是分享目标的 POST：同源、`POST`、路径恰好是 `/capture`。 */
export function isShareRequest(method: string, url: string, origin: string): boolean {
  if (method.toUpperCase() !== 'POST') return false
  try {
    const u = new URL(url)
    return u.origin === origin && u.pathname === CAPTURE_PATH
  } catch {
    return false
  }
}

/** 暂存的三个字段。 */
export interface SharedFields {
  title: string
  text: string
  url: string
  /** 收到的时刻（毫秒）：页面只认不太久以前的，旧的忽略并删掉。 */
  at: number
}

/** 每个字段最多存这么多个 UTF-16 码元：一次分享不该把库塞满（正文本身的上限在记一项时再校验）。 */
export const FIELD_MAX = 32 * 1024
/** 暂存多久以内页面还认（毫秒）。 */
export const STASH_TTL_MS = 60 * 60 * 1000

export interface FormDataLike {
  get(name: string): unknown
}

/** 截到 `FIELD_MAX` 个码元，不把一个代理对（emoji 之类）从中间劈开：落在高位代理上就多去掉那一个码元。 */
export function cut(v: string): string {
  if (v.length <= FIELD_MAX) return v
  const end = FIELD_MAX
  const last = v.charCodeAt(end - 1)
  return v.slice(0, last >= 0xd800 && last <= 0xdbff ? end - 1 : end)
}

/** 表单里的字符串字段（文件之类的不收）；超长截断。 */
export function readFields(form: FormDataLike, now: number): SharedFields {
  const str = (name: string): string => {
    const v = form.get(name)
    return typeof v === 'string' ? cut(v) : ''
  }
  return { title: str('title'), text: str('text'), url: str('url'), at: now }
}

export interface ShareDeps {
  /** 请求里的表单（读不出来就抛）。 */
  formData(): Promise<FormDataLike>
  /** 存进 `anycon-share`。 */
  store(fields: SharedFields): Promise<void>
  now(): number
}

/**
 * 处理一次分享：读表单、存起来。**不管成败都回 `/capture`**：读不出来或存不下，这一次分享的内容就丢了，页面打开的是空的输入条
 * （比回一个错误页好：用户看到的是能立刻记一项的界面）。内容不进日志、不进 URL。
 */
export async function handleShare(deps: ShareDeps): Promise<string> {
  try {
    const fields = readFields(await deps.formData(), deps.now())
    // 三个都是空的：没有东西可存，也不要留一条空的把页面上正在用的暂存盖掉
    if (fields.title !== '' || fields.text !== '' || fields.url !== '') await deps.store(fields)
  } catch (e) {
    console.warn('anycon sw: could not stash the shared content', e instanceof Error ? e.name : 'error')
  }
  return CAPTURE_PATH
}

/** 页面这一边：把暂存的三个字段拼成输入条里的文字。标题（没有就是文字的第一行）在最前，其余是正文；链接不重复。 */
export function composeSharedText(f: Pick<SharedFields, 'title' | 'text' | 'url'>): string {
  const title = f.title.trim()
  const text = f.text.trim()
  const url = f.url.trim()
  const parts: string[] = []
  if (title) parts.push(title)
  if (text && text !== title) parts.push(text)
  if (url && !parts.some((p) => p.includes(url))) parts.push(url)
  return parts.length <= 1 ? (parts[0] ?? '') : `${parts[0]}\n${parts.slice(1).join('\n')}`
}
