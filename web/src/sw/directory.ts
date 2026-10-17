// service worker 读页面的 IndexedDB（`anycon` 库，spec/web.md § 6.2）：节点名、会话标题、本设备 id；另有一个只放界面语言的小库。
// 规则：
// - **绝不创建主库**。`openDB` 不带版本号时库不存在会新建一个空的 v1，页面随后的升级就不会跑了（连 `keys` 仓都没有）。
//   所以升级回调里直接中止事务：库不存在 → 打开失败 → 当作「没有缓存」。「退出并移除本设备」删库之后也是这个路径；
// - 连接用完就关（页面要删库 / 升级时不会被这里挡住），别的标签页要删库时（`blocking`）也立刻让出；
// - 读到的东西按不可信数据对待（页面写的，但格式可能是旧版本的）：逐个字段收窄，缺了就当没有。
import { deleteDB, openDB, type IDBPDatabase } from 'idb'

const MAIN_DB = 'anycon'
const SW_DB = 'anycon-sw'

/** 页面缓存里和通知有关的那点数据。 */
export interface PushDirectory {
  /** 载荷里那个节点的名字（本地缓存，不是 hub 说的）。 */
  nodeName?: string
  /** 载荷里那个会话的标题。 */
  sessionTitle?: string
  /** 这个浏览器已配对的全部节点 id（算路由里的节点短 id 要用，冲突时回退全长）。 */
  nodeIds: string[]
}

const EMPTY: PushDirectory = { nodeIds: [] }

/** 打开一个**已经存在**的库；不存在返回 null（绝不新建，见文件开头）。 */
async function openExisting(name: string): Promise<IDBPDatabase | null> {
  let db: IDBPDatabase | null = null
  try {
    db = await openDB(name, undefined, {
      upgrade(_db, _old, _new, tx) {
        void tx.done.catch(() => undefined) // 中止会让 done 以 AbortError 拒绝，没人等它就是一条未处理的拒绝
        tx.abort() // 库还不存在：别建
      },
      blocking() {
        db?.close() // 别的标签页要删库 / 升级：让出连接
      },
    })
    return db
  } catch {
    return null
  }
}

const openMain = (): Promise<IDBPDatabase | null> => openExisting(MAIN_DB)

function str(v: unknown, key: string): string | undefined {
  if (typeof v !== 'object' || v === null) return undefined
  const x = (v as Record<string, unknown>)[key]
  return typeof x === 'string' && x !== '' ? x : undefined
}

/** 读节点名 / 会话标题 / 已配对节点 id。任何一步出错都返回已读到的部分（兜底文案由调用方负责）。 */
export async function readPushDirectory(nodeId: string | null, sessionId: string | null): Promise<PushDirectory> {
  const db = await openMain()
  if (!db) return EMPTY
  try {
    const out: PushDirectory = { nodeIds: [] }
    try {
      out.nodeIds = (await db.getAllKeys('nodes')).filter((k): k is string => typeof k === 'string')
      if (nodeId) {
        const name = str(await db.get('nodes', nodeId), 'name')
        if (name) out.nodeName = name
      }
      // 会话必须属于载荷里的这个节点：hub 说什么都不能让别的节点的会话标题出现在这条通知里（页面一侧的 `sessionTitle(nodeId, sessionId)` 同样核对）
      if (sessionId && nodeId) {
        const rec = await db.get('sessions', sessionId)
        if (str(rec, 'node_id') === nodeId) {
          const title = str(rec, 'title')
          if (title) out.sessionTitle = title
        }
      }
    } catch {
      /* 库是旧版本 / 仓不在：返回读到的部分 */
    }
    return out
  } finally {
    db.close()
  }
}

/** 本设备在 hub 上的正式 web 设备 id（订阅上报要带）；还没配对过是 null。 */
export async function readDeviceId(): Promise<string | null> {
  const db = await openMain()
  if (!db) return null
  try {
    return str(await db.get('keys', 'self'), 'device_id') ?? null
  } catch {
    return null
  } finally {
    db.close()
  }
}

// ---- 界面语言 ----
// 语言偏好在页面的 localStorage 里，service worker 读不到。页面把当前生效的语言写到这个独立的小库里
// （不动主库的 schema），service worker 读它；没有就用 `navigator.language`。

async function openSw(): Promise<IDBPDatabase> {
  return openDB(SW_DB, 1, {
    upgrade(db) {
      db.createObjectStore('kv')
    },
  })
}

/** 页面调用：记下当前界面语言（`en` / `zh`）。失败不影响页面。 */
export async function writeSwLanguage(lang: string): Promise<void> {
  try {
    const db = await openSw()
    try {
      await db.put('kv', lang, 'lang')
    } finally {
      db.close()
    }
  } catch (e) {
    console.debug('anycon: could not store the language for notifications', e)
  }
}

/**
 * 页面调用：让 service worker 的通知语言和页面一致。
 * 页面语言是「自动」时 service worker 自己跟 `navigator.language`（规则一样：`zh*` 中文、其余英文），不用记；
 * 而且这时库也不留——没有任何要存的东西，「退出并移除本设备」之后重新打开登录页也不会又冒出一个库。
 * 只有手动选了语言才记；从手动改回「自动」时把旧的删掉。
 */
export async function syncSwLanguage(pref: 'auto' | 'en' | 'zh'): Promise<void> {
  if (pref !== 'auto') return writeSwLanguage(pref)
  try {
    await deleteSwStore()
  } catch (e) {
    console.debug('anycon: could not clear the language for notifications', e)
  }
}

/** service worker 调用：页面记下的语言，没有就 undefined。 */
export async function readSwLanguage(): Promise<string | undefined> {
  // 页面没记过（「自动」，或从没打开过）就没有这个库：读的时候也别把它建出来
  const db = await openExisting(SW_DB)
  if (!db) return undefined
  try {
    const v: unknown = await db.get('kv', 'lang')
    return typeof v === 'string' ? v : undefined
  } catch {
    return undefined
  } finally {
    db.close()
  }
}

/** 「退出并移除本设备」：把记下的语言也删掉（没有任何敏感内容，只是不留这台浏览器的痕迹）。 */
export async function deleteSwStore(): Promise<void> {
  await deleteDB(SW_DB)
}
