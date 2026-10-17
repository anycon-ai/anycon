import { defaultSdkDir, FileStorage, profileDir } from './storage-file.js'
import { IndexedDBStorage } from './storage-idb.js'
import { hubHostKey, MemoryStorage, resolveProfile, type SdkStorage } from './storage.js'
import { isNodeRuntime } from './runtime.js'

/** 按运行环境选默认存储：Node / Bun → 文件；浏览器 → IndexedDB；都没有 → 内存。 */
export async function defaultStorage(hubUrl: string, profile?: string): Promise<SdkStorage> {
  const scope = `${hubHostKey(hubUrl)}/${resolveProfile(profile)}`
  if (isNodeRuntime()) return new FileStorage(profileDir(await defaultSdkDir(), hubUrl, profile))
  if (typeof (globalThis as { indexedDB?: unknown }).indexedDB !== 'undefined') {
    return new IndexedDBStorage(scope)
  }
  return new MemoryStorage()
}
