// 删账号成功后整页跳到 `/login`，页面重载会丢掉内存里的 toast；用 sessionStorage 带一个一次性标记过去，登录页读到就提示一次。
// 必须在清空本地数据**之后**写（清空会连 sessionStorage 一起清掉）。
const KEY = 'anycon.account_deleted'

export function flagAccountDeleted(): void {
  try {
    sessionStorage.setItem(KEY, '1')
  } catch {
    /* 没有 sessionStorage 就没有提示，不影响删除本身 */
  }
}

/** 读出并清掉标记。 */
export function consumeAccountDeleted(): boolean {
  try {
    const flagged = sessionStorage.getItem(KEY) === '1'
    sessionStorage.removeItem(KEY)
    return flagged
  } catch {
    return false
  }
}
