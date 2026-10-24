// 预览启动参数（`PreviewLaunch`）的存取。引导页写，service worker 读；两边都不信任库里读出来的东西
// （同 origin 的 dev app 也能写这个库），所以读出来要重新校验。
import { decodePreviewLaunch, encodePreviewLaunch, isValidPreviewLabel, type PreviewLaunch } from '@anycon/protocol'
import { STORES, idbDelete, idbGet, idbPut } from './idb'

const KEY = 'current'

export type LaunchLookup =
  | { ok: true; launch: PreviewLaunch }
  | { ok: false; reason: 'none' | 'invalid' | 'expired' }

export async function saveLaunch(launch: PreviewLaunch): Promise<void> {
  await idbPut(STORES.launch, KEY, launch)
}

export async function clearLaunch(): Promise<void> {
  await idbDelete(STORES.launch, KEY)
}

/** 校验库里读出来的值：和 URL fragment 走同一套规则（版本、label、id、密钥格式、path）。 */
export function validateLaunch(value: unknown): PreviewLaunch | undefined {
  if (typeof value !== 'object' || value === null) return undefined
  try {
    return decodePreviewLaunch(encodePreviewLaunch(value as PreviewLaunch))
  } catch {
    return undefined
  }
}

export async function loadLaunch(now: number = Date.now()): Promise<LaunchLookup> {
  const raw = await idbGet<unknown>(STORES.launch, KEY)
  if (raw === undefined) return { ok: false, reason: 'none' }
  const launch = validateLaunch(raw)
  if (!launch) return { ok: false, reason: 'invalid' }
  if (launch.expires_at_ms <= now) return { ok: false, reason: 'expired' }
  return { ok: true, launch }
}

/** 主机名的第一段（预览 origin 是 `<label>.<预览域名>`）。 */
export function hostLabel(hostname: string): string {
  return hostname.split('.')[0] ?? ''
}

/** 启动参数的 label 必须等于自己主机名的第一段：否则这份参数是给别的预览 origin 的。 */
export function launchMatchesHost(launch: PreviewLaunch, hostname: string): boolean {
  const label = hostLabel(hostname)
  return isValidPreviewLabel(label) && launch.label === label
}
