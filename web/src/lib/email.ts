/**
 * 够用的邮箱格式检查：有本地部分、一个 `@`、带点的域名，没有空白。真正的校验在 hub 和邮件投递——
 * 这里只是别把明显打错的东西发出去（比如没填 `@`）。
 */
export function isPlausibleEmail(address: string): boolean {
  return /^[^\s@]+@[^\s@.]+(\.[^\s@.]+)+$/.test(address) && address.length <= 254
}
