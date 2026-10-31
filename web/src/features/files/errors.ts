// 文件浏览的错误文案：节点离线、路径不存在、daemon 拒绝访问各有各的说法。
import type { TFunction } from 'i18next'
import { describeErrorIn } from '@/lib/errors'
import { RpcCallError } from '@/transport/rpcClient'
import { FsDataError } from './api'

/** 链路没有就绪（节点离线、请求没送到）。页面据此换成「节点离线」的整块提示。 */
export function isOfflineError(e: unknown): boolean {
  return e instanceof RpcCallError && (e.kind === 'offline' || e.kind === 'undeliverable')
}

/** `fs.list` / `fs.read` 失败的一句话。daemon 的 `not_found` / `forbidden` / `invalid` 在这里有文件浏览自己的说法。 */
export function describeFsError(e: unknown, t: TFunction): string {
  if (e instanceof FsDataError) return t(`files.errors.${e.code}`)
  if (isOfflineError(e)) return t('files.errors.offline')
  return describeErrorIn(e, t, 'files.errors')
}
