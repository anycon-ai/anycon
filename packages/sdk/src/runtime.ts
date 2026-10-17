/** 是否 Node / Bun（有 `process.versions.node`）。浏览器和 Web Worker 返回 false。 */
export function isNodeRuntime(): boolean {
  const p = (globalThis as { process?: { versions?: { node?: string } } }).process
  return typeof p?.versions?.node === 'string'
}

/** 设备平台串，如 `macos-aarch64`、`linux-x86_64`、`browser`。 */
export function detectPlatform(): string {
  const p = (globalThis as { process?: { platform?: string; arch?: string } }).process
  if (!isNodeRuntime() || !p?.platform) return 'browser'
  const os = p.platform === 'darwin' ? 'macos' : p.platform === 'win32' ? 'windows' : p.platform
  const arch = p.arch === 'arm64' ? 'aarch64' : p.arch === 'x64' ? 'x86_64' : (p.arch ?? 'unknown')
  return `${os}-${arch}`
}
