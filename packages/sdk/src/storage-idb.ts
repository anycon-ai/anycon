import type { SdkStorage } from './storage.js'

const STORE = 'kv'

/**
 * 浏览器存储：一个 IndexedDB 数据库（`anycon-sdk:<scope>`），一个 `kv` object store。
 * 注意：私钥以可导出形式存放，同源脚本都读得到；浏览器里想更强的保护应换成 WebCrypto 不可导出密钥的方案。
 */
export class IndexedDBStorage implements SdkStorage {
  readonly #name: string
  readonly #factory: IDBFactory | undefined
  #db: Promise<IDBDatabase> | undefined

  /**
   * @param scope 数据库后缀，通常是 `<hub-host>/<profile>`
   * @param factory 测试时可注入 `fake-indexeddb` 的 `IDBFactory`
   */
  constructor(scope = 'default', factory?: IDBFactory) {
    this.#name = `anycon-sdk:${scope}`
    this.#factory = factory
  }

  async get(key: string): Promise<string | undefined> {
    const value = await this.#request('readonly', (store) => store.get(key))
    return typeof value === 'string' ? value : undefined
  }

  async set(key: string, value: string): Promise<void> {
    await this.#request('readwrite', (store) => store.put(value, key))
  }

  async delete(key: string): Promise<void> {
    await this.#request('readwrite', (store) => store.delete(key))
  }

  /** 关闭数据库连接（测试 / 卸载时用）。 */
  async close(): Promise<void> {
    if (!this.#db) return
    const db = await this.#db
    db.close()
    this.#db = undefined
  }

  #open(): Promise<IDBDatabase> {
    this.#db ??= new Promise<IDBDatabase>((resolve, reject) => {
      const factory = this.#factory ?? (globalThis as { indexedDB?: IDBFactory }).indexedDB
      if (!factory) {
        reject(new Error('IndexedDB is not available in this environment'))
        return
      }
      const req = factory.open(this.#name, 1)
      req.onupgradeneeded = () => {
        if (!req.result.objectStoreNames.contains(STORE)) req.result.createObjectStore(STORE)
      }
      req.onsuccess = () => resolve(req.result)
      req.onerror = () => reject(req.error ?? new Error('failed to open IndexedDB'))
    })
    return this.#db
  }

  async #request<T>(
    mode: IDBTransactionMode,
    run: (store: IDBObjectStore) => IDBRequest<T>,
  ): Promise<T> {
    const db = await this.#open()
    return new Promise<T>((resolve, reject) => {
      const tx = db.transaction(STORE, mode)
      const req = run(tx.objectStore(STORE))
      tx.oncomplete = () => resolve(req.result)
      tx.onerror = () => reject(tx.error ?? new Error('IndexedDB transaction failed'))
      tx.onabort = () => reject(tx.error ?? new Error('IndexedDB transaction aborted'))
    })
  }
}
