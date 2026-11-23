import type { TFunction } from 'i18next'
import { RpcCallError } from '@/transport/rpcClient'
import { PairError } from '@/transport/client'
import { ApiError } from '@/api/client'

/**
 * daemon 拒绝了危险模式：`allow_dangerous_modes` 关着时直接指定会得到 `forbidden`，
 * 校验失败则是 `invalid_settings { field: "mode" }`。`forbidden` 只有在我们确实请求了危险模式时才算（其它 forbidden 是别的意思）。
 */
export function isDangerousModeRejection(e: unknown, requestedDangerous: boolean): boolean {
  if (!(e instanceof RpcCallError) || e.kind !== 'rpc') return false
  return (e.code === 'invalid_settings' && e.field === 'mode') || (e.code === 'forbidden' && requestedDangerous)
}

/** 把任何错误翻成给用户看的一句话（走 i18n，按稳定错误码）。 */
export function describeError(e: unknown, t: TFunction): string {
  if (e instanceof RpcCallError) {
    switch (e.kind) {
      case 'offline':
      case 'undeliverable':
        return t('errors.nodeOffline')
      case 'timeout':
        return t('errors.timeout')
      case 'closed':
        return t('errors.connectionLost')
      case 'too_large':
        return t('errors.tooLarge')
      case 'rpc': {
        const code = e.code ?? 'internal'
        const key = `errors.rpc.${code}`
        const text = t(key, { defaultValue: '' })
        if (text) return e.field ? `${text} (${e.field})` : text
        return t('errors.rpc.internal')
      }
    }
  }
  if (e instanceof PairError) return t(`pair.errors.${e.code}`, { defaultValue: t('pair.errors.generic') })
  if (e instanceof ApiError) return t(`errors.api.${e.code}`, { defaultValue: e.status === 429 ? t('errors.api.rate_limited') : e.message })
  if (e instanceof Error) return e.message
  return String(e)
}

/**
 * 同一个错误码在不同操作里意思不同（`exists`：重名 / 已关联；`forbidden`：越界 / 功能关闭…）：
 * 先查 `${prefix}.${code}` 这一条专门的说法，没有再走通用的 `describeError`。
 */
export function describeErrorIn(e: unknown, t: TFunction, prefix: string): string {
  if (e instanceof RpcCallError && e.kind === 'rpc' && e.code) {
    const text = t(`${prefix}.${e.code}`, { defaultValue: '' })
    if (text) return text
  }
  return describeError(e, t)
}

/**
 * 这个请求是不是被一个太旧的 daemon 拒绝：旧 daemon 不认识的方法回 JSON-RPC 的 -32601（稳定错误码 `unsupported`）。
 * 只给「新 daemon 绝不会回 `unsupported`」的请求用（比如 `session.set_marks`）：别的请求里 `unsupported` 是「agent 不支持」。
 */
export function isMethodNotFound(e: unknown): boolean {
  return e instanceof RpcCallError && e.kind === 'rpc' && (e.rpc?.code === -32601 || e.code === 'unsupported')
}

/** 标 / 取消「待处理」失败的说法：旧 daemon 不认识 `session.set_marks`，要它更新才能用；别的错误照常。 */
export function describeMarkError(e: unknown, t: TFunction): string {
  return isMethodNotFound(e) ? t('errors.daemonTooOld') : describeError(e, t)
}
