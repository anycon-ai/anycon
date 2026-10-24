// 预览 origin 的 IndexedDB（库 `anycon-preview`）：引导页写启动参数，service worker 读它，并存 cookie jar 和 HTTP 缓存索引。
// 只用到按主键存取，所以不引 `idb` 之类的库。

export const DB_NAME = 'anycon-preview'
const DB_VERSION = 1

/** 对象库名。值都是结构化克隆能存的普通对象。 */
export const STORES = {
  /** 启动参数（键 `current`）。 */
  launch: 'launch',
  /** cookie jar（键 `jar`）。 */
  cookies: 'cookies',
  /** HTTP 缓存索引（键是 URL）。 */
  httpCache: 'httpcache',
} as const
export type StoreName = (typeof STORES)[keyof typeof STORES]

let opened: Promise<IDBDatabase> | undefined

function open(): Promise<IDBDatabase> {
  opened ??= new Promise<IDBDatabase>((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION)
    req.onupgradeneeded = () => {
      const db = req.result
      for (const name of Object.values(STORES)) {
        if (!db.objectStoreNames.contains(name)) db.createObjectStore(name)
      }
    }
    req.onsuccess = () => {
      const db = req.result
      // 别的上下文要升级库时让路；下次访问重新打开
      db.onversionchange = () => {
        db.close()
        opened = undefined
      }
      resolve(db)
    }
    req.onerror = () => reject(req.error ?? new Error('indexedDB open failed'))
    req.onblocked = () => reject(new Error('indexedDB open blocked'))
  }).catch((err: unknown) => {
    opened = undefined
    throw err
  })
  return opened
}

function run<T>(store: StoreName, mode: IDBTransactionMode, body: (s: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  return open().then(
    (db) =>
      new Promise<T>((resolve, reject) => {
        const tx = db.transaction(store, mode)
        const req = body(tx.objectStore(store))
        tx.oncomplete = () => resolve(req.result)
        tx.onerror = () => reject(tx.error ?? new Error('indexedDB transaction failed'))
        tx.onabort = () => reject(tx.error ?? new Error('indexedDB transaction aborted'))
      }),
  )
}

export function idbGet<T>(store: StoreName, key: string): Promise<T | undefined> {
  return run<T | undefined>(store, 'readonly', (s) => s.get(key) as IDBRequest<T | undefined>)
}

export function idbPut(store: StoreName, key: string, value: unknown): Promise<void> {
  return run(store, 'readwrite', (s) => s.put(value, key)).then(() => undefined)
}

export function idbDelete(store: StoreName, key: string): Promise<void> {
  return run(store, 'readwrite', (s) => s.delete(key)).then(() => undefined)
}

export function idbAll<T>(store: StoreName): Promise<Array<{ key: string; value: T }>> {
  return open().then(
    (db) =>
      new Promise((resolve, reject) => {
        const tx = db.transaction(store, 'readonly')
        const out: Array<{ key: string; value: T }> = []
        const cursor = tx.objectStore(store).openCursor()
        cursor.onsuccess = () => {
          const c = cursor.result
          if (!c) return
          out.push({ key: String(c.key), value: c.value as T })
          c.continue()
        }
        tx.oncomplete = () => resolve(out)
        tx.onerror = () => reject(tx.error ?? new Error('indexedDB transaction failed'))
        tx.onabort = () => reject(tx.error ?? new Error('indexedDB transaction aborted'))
      }),
  )
}

export function idbClear(store: StoreName): Promise<void> {
  return run(store, 'readwrite', (s) => s.clear()).then(() => undefined)
}
