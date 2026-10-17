export type ClassValue = string | false | null | undefined | ClassValue[]

/** 极简 class 拼接。 */
export function cn(...parts: ClassValue[]): string {
  const out: string[] = []
  const walk = (p: ClassValue): void => {
    if (!p) return
    if (Array.isArray(p)) p.forEach(walk)
    else out.push(p)
  }
  parts.forEach(walk)
  return out.join(' ')
}
