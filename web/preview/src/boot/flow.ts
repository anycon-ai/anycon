// 引导页的流程（spec/web.md § 13.2「引导页」）。没有被 service worker 接管的页面请求都落到引导页：
//
// 1. `/__anycon/enter#<launch>`：解出启动参数、校验、存进 IndexedDB、**立刻**把 fragment 从地址栏抹掉，
//    注册 service worker、等它激活，然后跳到 `launch.path`；
// 2. 别的路径：读 IndexedDB 里的启动参数，有且没过期就确保 service worker 已激活，然后重新加载
//    （10 秒内重载 3 次仍没被接管就停下，显示错误）；没有或过期就说明「要从 Anycon 打开」；
// 3. 不支持 service worker（或第三方 iframe 里不让注册）：显示说明和「在新标签页打开」。
//
// 所有副作用（地址栏、存储、service worker、画面）都从 `BootEnv` 传进来，所以整个流程可以直接测。
import { PREVIEW_ENTER_PATH, decodePreviewLaunch, encodePreviewLaunch, type PreviewLaunch } from '@anycon/protocol'
import { launchMatchesHost, type LaunchLookup } from '../shared/launch'

export type BootView =
  | { kind: 'starting' }
  | { kind: 'needs_launch' }
  | { kind: 'expired' }
  | { kind: 'invalid' }
  /** 这个环境不能用 service worker：给一个在新标签页打开的链接。 */
  | { kind: 'unsupported'; newTabUrl: string | undefined }
  | { kind: 'failed'; newTabUrl: string | undefined }
  | { kind: 'loop' }

export interface BootEnv {
  location: { origin: string; hostname: string; pathname: string; search: string; hash: string }
  now(): number
  /** `history.replaceState(null, '', url)`。 */
  replaceState(url: string): void
  /** `location.replace(url)`。 */
  navigate(url: string): void
  reload(): void
  /** 注册 `/__anycon/sw.js`（scope `/`）并等它激活；浏览器不支持就是 `undefined`。 */
  registerServiceWorker: (() => Promise<void>) | undefined
  saveLaunch(launch: PreviewLaunch): Promise<void>
  loadLaunch(now: number): Promise<LaunchLookup>
  /** 记一次「为了让 service worker 接管而重载」；10 秒内第 4 次返回 false。 */
  allowReload(now: number): boolean
  show(view: BootView): void
}

/** 10 秒内最多重载几次。 */
export const RELOAD_LIMIT = 3
export const RELOAD_WINDOW_MS = 10_000

/**
 * 启动后要跳到的路径：必须和自己同源（浏览器会把 `\` 当 `/`、去掉 tab 和换行，`/\evil.example` 这样的写法会跳到别处），
 * 也不能是 `/__anycon/` 下的路径（那是 hub 的文件，不经 service worker，会回到引导页）。不合格就回 `/`。
 * 返回浏览器规范化之后的形式。
 */
export function safeLaunchPath(path: string, origin: string): string {
  try {
    const url = new URL(path, origin)
    if (url.origin !== origin || url.pathname.startsWith('/__anycon/')) return '/'
    return url.pathname + url.search + url.hash
  } catch {
    return '/'
  }
}

/** 「在新标签页打开」的链接：带着启动参数（新标签页是另一个存储分区，读不到这里的 IndexedDB）。 */
function newTabUrl(env: BootEnv, launch: PreviewLaunch, path: string): string {
  return `${env.location.origin}${PREVIEW_ENTER_PATH}#${encodePreviewLaunch({ ...launch, path: safeLaunchPath(path, env.location.origin) })}`
}

export async function runBoot(env: BootEnv): Promise<void> {
  const { pathname, search, hash } = env.location
  const isEnter = pathname === PREVIEW_ENTER_PATH
  if (isEnter && hash.length > 1) {
    // 先抹掉再做别的：fragment 里有票据密钥和令牌
    env.replaceState(pathname + search)
    return enter(env, hash)
  }
  return recover(env, isEnter)
}

async function enter(env: BootEnv, fragment: string): Promise<void> {
  let launch: PreviewLaunch
  try {
    launch = decodePreviewLaunch(fragment)
  } catch {
    return env.show({ kind: 'invalid' })
  }
  if (!launchMatchesHost(launch, env.location.hostname)) return env.show({ kind: 'invalid' })
  if (launch.expires_at_ms <= env.now()) return env.show({ kind: 'expired' })
  env.show({ kind: 'starting' })
  const tab = newTabUrl(env, launch, launch.path)
  try {
    await env.saveLaunch(launch)
  } catch {
    // 存储被禁（第三方 iframe 里常见）：新标签页里就没有这个限制
    return env.show({ kind: 'unsupported', newTabUrl: tab })
  }
  if (!env.registerServiceWorker) return env.show({ kind: 'unsupported', newTabUrl: tab })
  try {
    await env.registerServiceWorker()
  } catch {
    return env.show({ kind: 'unsupported', newTabUrl: tab })
  }
  env.navigate(safeLaunchPath(launch.path, env.location.origin))
}

async function recover(env: BootEnv, isEnter: boolean): Promise<void> {
  let lookup: LaunchLookup
  try {
    lookup = await env.loadLaunch(env.now())
  } catch {
    return env.show({ kind: 'failed', newTabUrl: undefined })
  }
  if (!lookup.ok) {
    return env.show({ kind: lookup.reason === 'expired' ? 'expired' : lookup.reason === 'invalid' ? 'invalid' : 'needs_launch' })
  }
  const { launch } = lookup
  if (!launchMatchesHost(launch, env.location.hostname)) return env.show({ kind: 'invalid' })
  const here = isEnter ? launch.path : env.location.pathname + env.location.search
  const tab = newTabUrl(env, launch, here)
  if (!env.registerServiceWorker) return env.show({ kind: 'unsupported', newTabUrl: tab })
  env.show({ kind: 'starting' })
  try {
    await env.registerServiceWorker()
  } catch {
    return env.show({ kind: 'unsupported', newTabUrl: tab })
  }
  // 刷新过 `/__anycon/enter`（fragment 已经没了）：直接去启动参数里的路径，不是重载这个页面
  if (isEnter) return env.navigate(safeLaunchPath(launch.path, env.location.origin))
  if (!env.allowReload(env.now())) return env.show({ kind: 'loop' })
  env.reload()
}

/** 10 秒内的重载次数记在 sessionStorage 里（页面重载后仍在）。存储不可用时退回到只在内存里计，仍然能防住同一个页面的循环之外的情况。 */
export function createReloadGuard(storage: Pick<Storage, 'getItem' | 'setItem'> | undefined): (now: number) => boolean {
  const KEY = 'anycon-preview-reloads'
  let memory: number[] = []
  return (now) => {
    let times: number[] = memory
    try {
      const raw = storage?.getItem(KEY)
      if (raw) {
        const parsed: unknown = JSON.parse(raw)
        if (Array.isArray(parsed)) times = parsed.filter((t): t is number => typeof t === 'number')
      }
    } catch {
      // 用内存里的
    }
    times = times.filter((t) => now - t < RELOAD_WINDOW_MS)
    if (times.length >= RELOAD_LIMIT) return false
    times.push(now)
    memory = times
    try {
      storage?.setItem(KEY, JSON.stringify(times))
    } catch {
      // 内存里已经记了
    }
    return true
  }
}

/** 注册后等 service worker 激活。`timeoutMs` 内没激活（或变成 redundant）就抛错。 */
export async function waitForActivation(registration: Pick<ServiceWorkerRegistration, 'active' | 'waiting' | 'installing'>, timeoutMs = 10_000): Promise<void> {
  const worker = registration.active ?? registration.waiting ?? registration.installing
  if (!worker) throw new Error('service worker registration has no worker')
  if (worker.state === 'activated') return
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('service worker did not activate')), timeoutMs)
    worker.addEventListener('statechange', () => {
      if (worker.state === 'activated') {
        clearTimeout(timer)
        resolve()
      } else if (worker.state === 'redundant') {
        clearTimeout(timer)
        reject(new Error('service worker became redundant'))
      }
    })
  })
}
