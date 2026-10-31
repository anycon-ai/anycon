// 文件浏览的数据 hook：目录列表缓存（树和目录视图共用）、单次异步读取、图片字节 → `blob:` URL。
// 不进 zustand：这些数据只属于当前这一页，离开就丢；节点上的文件随时会变，也不该缓存到下次打开。
import { useCallback, useEffect, useRef, useState } from 'react'
import type { FsEntry } from '@/protocol'
import { RpcCallError } from '@/transport/rpcClient'
import { listDir } from './api'

export interface DirState {
  status: 'loading' | 'ready' | 'error'
  /** `ready` 时是最新的列表；刷新（`loading`）期间保留上一次的，免得整棵树闪成空的。 */
  entries?: FsEntry[]
  error?: unknown
}

export interface DirCache {
  dirs: Record<string, DirState>
  /** 拉一个目录；已经拉过（或在拉）的不重复拉。 */
  load: (path: string) => void
  /** 重新拉所有拉过的目录，期间继续显示旧列表。 */
  reload: () => void
}

/** 链路上的暂时性失败（节点掉线、超时）：恢复之后值得自动重试；`not_found` / `forbidden` 之类不会因为重试变好。 */
export function isTransient(e: unknown): boolean {
  return e instanceof RpcCallError && (e.kind === 'offline' || e.kind === 'undeliverable' || e.kind === 'timeout' || e.kind === 'closed')
}

/**
 * 目录列表缓存。`online` 从 false 变 true 时，因为链路问题失败的目录自动重拉一次。
 * 用的时候给组件加 `key={nodeId}`：换节点就是一个新的缓存。
 */
export function useDirCache(nodeId: string, online: boolean): DirCache {
  const [dirs, setDirs] = useState<Record<string, DirState>>({})
  const requested = useRef(new Set<string>())
  // 每次 reload 加一：reload 之前发出、还没回来的请求，结果不再写进来
  const epoch = useRef(0)
  const dirsRef = useRef(dirs)
  useEffect(() => {
    dirsRef.current = dirs
  })

  const fetchDir = useCallback(
    (path: string) => {
      requested.current.add(path)
      const mine = epoch.current
      setDirs((d) => ({ ...d, [path]: { status: 'loading', entries: d[path]?.entries } }))
      listDir(nodeId, path).then(
        (entries) => {
          if (epoch.current === mine) setDirs((d) => ({ ...d, [path]: { status: 'ready', entries } }))
        },
        (error: unknown) => {
          if (epoch.current !== mine) return
          requested.current.delete(path) // 失败的可以再 `load`
          setDirs((d) => ({ ...d, [path]: { status: 'error', error } }))
        },
      )
    },
    [nodeId],
  )
  const load = useCallback(
    (path: string) => {
      if (!requested.current.has(path)) fetchDir(path)
    },
    [fetchDir],
  )
  const reload = useCallback(() => {
    epoch.current += 1
    const paths = [...requested.current, ...Object.keys(dirsRef.current).filter((p) => dirsRef.current[p]?.status === 'error')]
    for (const p of new Set(paths)) fetchDir(p)
  }, [fetchDir])

  useEffect(() => {
    if (!online) return
    for (const [path, s] of Object.entries(dirsRef.current)) if (s.status === 'error' && isTransient(s.error)) fetchDir(path)
  }, [online, fetchDir])

  return { dirs, load, reload }
}

export type Async<T> = { status: 'loading' } | { status: 'ready'; data: T } | { status: 'error'; error: unknown }

/**
 * 按 `key` 做一次异步读取：`key` 变了重新读，旧的结果不会短暂显示在新的 key 下面；`retry` 重读。
 * `enabled` 为 false 时不读（保持 `loading`）。`run` 不用进依赖：它由 `key` 决定。
 */
export function useAsync<T>(key: string, run: () => Promise<T>, enabled = true): { state: Async<T>; retry: () => void } {
  const [attempt, setAttempt] = useState(0)
  const [result, setResult] = useState<{ key: string; value: Async<T> } | null>(null)
  const runRef = useRef(run)
  useEffect(() => {
    runRef.current = run
  })
  const full = `${key}\0${attempt}`
  useEffect(() => {
    if (!enabled) return
    let cancelled = false
    runRef.current().then(
      (data) => {
        if (!cancelled) setResult({ key: full, value: { status: 'ready', data } })
      },
      (error: unknown) => {
        if (!cancelled) setResult({ key: full, value: { status: 'error', error } })
      },
    )
    return () => {
      cancelled = true
    }
  }, [full, enabled])
  const retry = useCallback(() => setAttempt((n) => n + 1), [])
  return { state: result && result.key === full ? result.value : { status: 'loading' }, retry }
}

/** 把字节变成 `blob:` URL（组件卸载或字节变了就 revoke）。`<img>` 用它显示图片；SVG 也只放进 `<img>`，不进 DOM。 */
export function useObjectUrl(bytes: Uint8Array | null, mime: string): string | null {
  const [url, setUrl] = useState<string | null>(null)
  useEffect(() => {
    if (!bytes) {
      setUrl(null)
      return
    }
    const u = URL.createObjectURL(new Blob([bytes as Uint8Array<ArrayBuffer>], { type: mime }))
    setUrl(u)
    return () => URL.revokeObjectURL(u)
  }, [bytes, mime])
  return url
}
