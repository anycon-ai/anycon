// 会话页交给消息的本地文件处理（docs/spec/web.md § 4.6「本地文件链接」，ADR-62）。只有 context，不引路由和组件：
// 消息组件只需要读它，具体的链接组件在 `LocalFiles.tsx`，由会话页引入。
import { createContext, useContext } from 'react'
import type { LocalFiles } from '@/components/Markdown'

export const SessionFilesContext = createContext<LocalFiles | undefined>(undefined)

/** 会话页给消息的本地文件处理；不在会话页里（或会话还没加载出来）是 undefined，Markdown 照旧。 */
export function useSessionLocalFiles(): LocalFiles | undefined {
  return useContext(SessionFilesContext)
}
