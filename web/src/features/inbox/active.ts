import { createContext, useContext } from 'react'

/**
 * 「待处理」页此刻是不是眼前的页面。手机上它藏在后面时（`inert`）不是：那时候在别的页面（比如会话页）答复同一个请求，
 * 对「待处理」来说是「别处处理的」，回来时要看到占位，而不是已经替用户前进到下一条。
 * 默认是 false：不在 provider 里渲染的卡片永远不会把答复记成「在这里处理的」。
 */
export const InboxActiveContext = createContext(false)

export const useInboxActive = (): boolean => useContext(InboxActiveContext)
