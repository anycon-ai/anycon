import { useEffect, useState } from 'react'

/** 每 `intervalMs` 毫秒刷新一次的当前时间（倒计时用）。`active = false` 时不起定时器。 */
export function useNow(intervalMs = 1000, active = true): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    setNow(Date.now())
    const id = setInterval(() => setNow(Date.now()), intervalMs)
    return () => clearInterval(id)
  }, [intervalMs, active])
  return now
}
