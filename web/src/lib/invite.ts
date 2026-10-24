// 邀请码（`single`）在 `/invite/:code` 落地后暂存在 sessionStorage，登录页读它：「接受邀请」或带上它走 OAuth。
// 只在这个标签页里、只有同源脚本读得到；用完（登录成功）就删。
const KEY = 'invite'

export function readInvite(): string | null {
  try {
    return sessionStorage.getItem(KEY)
  } catch {
    return null
  }
}

/** 隐私模式写不进去时返回 false：邀请码丢了，调用方让用户重新点链接。 */
export function stashInvite(code: string): boolean {
  try {
    sessionStorage.setItem(KEY, code)
    return true
  } catch (e) {
    console.warn('anycon: could not keep the invite code in sessionStorage', e)
    return false
  }
}

export function clearInvite(): void {
  try {
    sessionStorage.removeItem(KEY)
  } catch {
    /* ignore */
  }
}
