import { hubHostKey, resolveProfile, type SdkStorage } from './storage.js'

/** 用到的 fs / os / path 的最小形状；动态导入，避免浏览器打包器去解析 node 内置模块。 */
interface NodeFs {
  mkdir(path: string, options: { recursive: boolean; mode: number }): Promise<unknown>
  readFile(path: string, encoding: 'utf8'): Promise<string>
  writeFile(path: string, data: string, options: { mode: number }): Promise<void>
  rename(from: string, to: string): Promise<void>
  chmod(path: string, mode: number): Promise<void>
  rm(path: string, options: { force: boolean }): Promise<void>
}

async function nodeFs(): Promise<NodeFs> {
  const specifier = 'node:fs/promises'
  return (await import(/* @vite-ignore */ specifier)) as NodeFs
}

const KEY_RE = /^[A-Za-z0-9_.-]+$/

/**
 * 文件存储：每个键一个 `<dir>/<key>.json`，目录 0700、文件 0600，写入走临时文件 + rename（原子）。
 * 默认目录见 `defaultSdkDir()`：`$ANYCON_HOME/sdk/<hub-host>/<profile>/`（`ANYCON_HOME` 缺省 `~/.anycon`）。
 */
export class FileStorage implements SdkStorage {
  readonly dir: string

  constructor(dir: string) {
    this.dir = dir
  }

  async get(key: string): Promise<string | undefined> {
    const fs = await nodeFs()
    try {
      return await fs.readFile(this.#path(key), 'utf8')
    } catch (err) {
      if ((err as { code?: string }).code === 'ENOENT') return undefined
      throw err
    }
  }

  async set(key: string, value: string): Promise<void> {
    const fs = await nodeFs()
    const path = this.#path(key)
    await fs.mkdir(this.dir, { recursive: true, mode: 0o700 })
    const tmp = `${path}.${Math.random().toString(36).slice(2)}.tmp`
    await fs.writeFile(tmp, value, { mode: 0o600 })
    await fs.chmod(tmp, 0o600)
    await fs.rename(tmp, path)
  }

  async delete(key: string): Promise<void> {
    const fs = await nodeFs()
    await fs.rm(this.#path(key), { force: true })
  }

  #path(key: string): string {
    if (!KEY_RE.test(key)) throw new Error(`invalid storage key: ${key}`)
    return `${this.dir.replace(/[\\/]+$/, '')}/${key}.json`
  }
}

/** `$ANYCON_HOME/sdk`，缺省 `~/.anycon/sdk`。`ANYCON_HOME` 是空串时当作没设（否则会变成根目录下的 `/sdk`）。 */
export async function defaultSdkDir(): Promise<string> {
  const osSpecifier = 'node:os'
  const os = (await import(/* @vite-ignore */ osSpecifier)) as { homedir(): string }
  const env = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process
    ?.env
  const home = (env?.ANYCON_HOME || `${os.homedir()}/.anycon`).replace(/[\\/]+$/, '')
  return `${home}/sdk`
}

/**
 * 默认的 profile 目录：`<sdk-dir>/<hub-host>/<profile>`。`profile` 必须通过 `assertSafeProfile`
 * （无路径分隔符、无 `..`），hub 主机名里的非法字符也会被替换，所以结果一定在 `sdkDir` 之内。
 */
export function profileDir(sdkDir: string, hubUrl: string, profile?: string): string {
  return `${sdkDir.replace(/[\\/]+$/, '')}/${hubHostKey(hubUrl)}/${resolveProfile(profile)}`
}
