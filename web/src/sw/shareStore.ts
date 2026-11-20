// 分享暂存的库（`anycon-share`）：service worker 写、页面读完就删。和主库（`anycon`，service worker 只读、绝不创建）分开：
// 主库里是会话和清单的缓存，service worker 从不写它；这里是 service worker 自己拿到的、要交给页面的那一次分享。
// 里面只有一条记录（最新的一次分享），页面取走时删掉；「退出并移除本设备」连库一起删。明文，和别的本地缓存同样的暴露面。
import { deleteDB, openDB, type IDBPDatabase } from 'idb'
import { STASH_TTL_MS, type SharedFields } from './share'

export const SHARE_DB = 'anycon-share'
const STORE = 'stash'
const KEY = 'latest'

async function open(create: boolean): Promise<IDBPDatabase | null> {
  try {
    const db = await openDB(SHARE_DB, 1, {
      upgrade(d, _old, _new, tx) {
        if (!create) {
          void tx.done.catch(() => undefined)
          tx.abort() // 页面读的时候库不存在就是没有分享：别把它建出来
          return
        }
        d.createObjectStore(STORE)
      },
    })
    return db
  } catch {
    return null
  }
}

/** service worker：存下这一次分享（覆盖上一次没被取走的）。 */
export async function storeShared(fields: SharedFields): Promise<void> {
  const db = await open(true)
  if (!db) throw new Error('could not open the share stash')
  try {
    await db.put(STORE, fields, KEY)
  } finally {
    db.close()
  }
}

function narrow(v: unknown): SharedFields | null {
  if (typeof v !== 'object' || v === null) return null
  const o = v as Record<string, unknown>
  if (typeof o.title !== 'string' || typeof o.text !== 'string' || typeof o.url !== 'string' || typeof o.at !== 'number') return null
  return { title: o.title, text: o.text, url: o.url, at: o.at }
}

/** 页面：取走暂存的分享（取出就删；太旧的、格式不对的也删，返回 null）。没有就返回 null，不创建库。 */
export async function takeShared(now: number = Date.now()): Promise<SharedFields | null> {
  const db = await open(false)
  if (!db) return null
  try {
    const tx = db.transaction(STORE, 'readwrite')
    const raw: unknown = await tx.store.get(KEY)
    await tx.store.delete(KEY)
    await tx.done
    const f = narrow(raw)
    return f && now - f.at >= 0 && now - f.at <= STASH_TTL_MS ? f : null
  } catch {
    return null
  } finally {
    db.close()
  }
}

/** 「退出并移除本设备」：连库一起删。 */
export async function deleteShareDb(): Promise<void> {
  await deleteDB(SHARE_DB)
}
