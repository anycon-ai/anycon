/** 节点口令至少几个字符（NFC 之后按 Unicode 字符计，和 daemon 一致；不做复杂度规则——在线猜测有硬上限）。 */
export const MIN_PASSWORD_CHARS = 8

export function passwordLongEnough(password: string): boolean {
  return [...password.normalize('NFC')].length >= MIN_PASSWORD_CHARS
}
