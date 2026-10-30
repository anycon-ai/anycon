// 显示附件用的两个 hook：进入视口才开始拉（IntersectionObserver），拉到的字节变成 `blob:` URL（卸载时 revoke）。
import { useCallback, useEffect, useState } from 'react'
import { isAborted } from './async'
import { acquireBlob } from './download'
import { isImageMime } from './normalize'

/**
 * 元素第一次进入（或接近）视口后变成 true，之后保持。没有 IntersectionObserver 的环境直接是 true。
 * 用法：`const [ref, seen] = useInView<HTMLDivElement>()`，把 `ref` 挂到元素上。
 */
export function useInView<T extends Element>(rootMargin = '300px'): [(el: T | null) => void, boolean] {
  const [el, setEl] = useState<T | null>(null)
  const [seen, setSeen] = useState(() => typeof IntersectionObserver === 'undefined')
  useEffect(() => {
    if (seen || !el) return
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          setSeen(true)
          io.disconnect()
        }
      },
      { rootMargin },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [el, seen, rootMargin])
  return [useCallback((node: T | null) => setEl(node), []), seen]
}

export type BlobUrlStatus = 'idle' | 'loading' | 'ready' | 'error'

export interface BlobUrl {
  status: BlobUrlStatus
  url?: string
  mime?: string
  /** 失败后重新拉。 */
  retry: () => void
}

/**
 * 把一个图片 blob 变成可以放进 `<img src>` 的 URL：查缓存 → 没有就 `blob.get` 拉 → 写缓存。
 * `enabled` 为 false（还没进视口 / 不需要）时什么都不做。卸载（或换了 blob）时 revoke 并取消还没开始的请求。
 * 只给四种栅格图片（`isImageMime`）生成 URL：别的类型（`text/html`、`image/svg+xml` …）的 `blob:` URL 在应用的源里打开就能跑脚本，
 * 拉到的不是这四种一律当失败。文件只能下载（`saveAttachment`）。
 */
export function useBlobUrl(nodeId: string | undefined, blobId: string | undefined, enabled: boolean): BlobUrl {
  const [state, setState] = useState<{ status: BlobUrlStatus; url?: string; mime?: string }>({ status: 'idle' })
  const [attempt, setAttempt] = useState(0)
  const retry = useCallback(() => setAttempt((n) => n + 1), [])

  useEffect(() => {
    if (!enabled || !blobId) {
      setState({ status: 'idle' })
      return
    }
    let cancelled = false
    let url: string | undefined
    setState({ status: 'loading' })
    const handle = acquireBlob(nodeId ?? '', { blob_id: blobId })
    handle.promise.then(
      ({ bytes, mime }) => {
        if (cancelled) return
        if (!isImageMime(mime)) {
          console.warn('anycon: not showing an attachment that is not a raster image inline', mime)
          setState({ status: 'error' })
          return
        }
        url = URL.createObjectURL(new Blob([bytes as Uint8Array<ArrayBuffer>], { type: mime }))
        setState({ status: 'ready', url, mime })
      },
      (e: unknown) => {
        if (cancelled || isAborted(e)) return
        console.warn('anycon: could not load the attachment image', e)
        setState({ status: 'error' })
      },
    )
    return () => {
      cancelled = true
      handle.release()
      if (url) URL.revokeObjectURL(url)
    }
  }, [nodeId, blobId, enabled, attempt])

  return { ...state, retry }
}
