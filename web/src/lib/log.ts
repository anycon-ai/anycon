import { useUi } from '@/stores/ui'

/**
 * 失败不能被吞掉：后台操作失败至少要在控制台留一条 warn。
 * 用法：`promise.catch(warnOnError('save the draft'))`。
 */
export function warnOnError(what: string): (e: unknown) => void {
  return (e) => console.warn(`anycon: could not ${what}`, e)
}

/**
 * 写本地存储（IndexedDB）失败：除了 warn，还要让用户知道——未发送的消息、会话缓存可能在关掉标签页后丢失。
 * 状态栏会显示一条可关闭的提示（banner.storage）。
 */
export function storageFailed(what: string): (e: unknown) => void {
  return (e) => {
    console.warn(`anycon: could not ${what}; it only lives in memory`, e)
    useUi.getState().setStorageError(true)
  }
}
