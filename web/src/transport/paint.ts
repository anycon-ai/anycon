// 在做会卡住主线程的同步计算（配对的 Argon2id，0.1–1 s）之前，让浏览器先把进度画出来。

/**
 * 等下一帧画完再继续。标签页在后台时 rAF 不会触发，所以另有一个超时兜底。
 */
export function nextPaint(): Promise<void> {
  return new Promise((resolve) => {
    let done = false
    const finish = (): void => {
      if (done) return
      done = true
      resolve()
    }
    setTimeout(finish, 100)
    if (typeof requestAnimationFrame === 'function') requestAnimationFrame(() => setTimeout(finish, 0))
    else setTimeout(finish, 0)
  })
}
