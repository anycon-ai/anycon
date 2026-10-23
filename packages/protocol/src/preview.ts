/**
 * dev server 预览（ADR-39，docs/protocol.md § 11）的 TS 侧：启动参数的编解码、label 校验、预览 origin。
 * 隧道消息 / 帧的编解码与 `crates/protocol/src/preview.rs` 的 `tunnel` 模块逐字节一致
 * （`tests/vectors/preview.json`）。
 */

import { ProtocolError } from './errors.js'
import type { PreviewLaunch } from './generated.js'

/** 预览 label：26 个字符的小写 base32（RFC 4648 字母表，无填充）。 */
export const PREVIEW_LABEL_LEN = 26
export const PREVIEW_LABEL_ALPHABET = 'abcdefghijklmnopqrstuvwxyz234567'
/** `PreviewLaunch.v` 的当前版本。 */
export const PREVIEW_LAUNCH_VERSION = 1
/** 预览页的入口路径：hub 在这里回引导页，启动参数放在 fragment 里。 */
export const PREVIEW_ENTER_PATH = '/__anycon/enter'

const LABEL_RE = /^[a-z2-7]{26}$/

export function isValidPreviewLabel(label: string): boolean {
  return LABEL_RE.test(label)
}

/**
 * 按 hub 的模板（`/api/config.preview_origin_template`，如 `https://{label}.aiplace.app`）算出预览 origin。
 * label 不合法时抛错：它会成为主机名的一段。
 */
export function previewOrigin(template: string, label: string): string {
  if (!isValidPreviewLabel(label)) throw new ProtocolError(`invalid preview label: ${label}`)
  if (!template.includes('{label}'))
    throw new ProtocolError('preview origin template has no {label}')
  return template.replace('{label}', label)
}

function base64urlEncode(bytes: Uint8Array): string {
  let bin = ''
  for (const b of bytes) bin += String.fromCharCode(b)
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

function base64urlDecode(text: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]*$/.test(text)) throw new ProtocolError('invalid base64url')
  const std = text.replace(/-/g, '+').replace(/_/g, '/')
  const padded = std + '='.repeat((4 - (std.length % 4)) % 4)
  const bin = atob(padded)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/** `PreviewLaunch` → URL fragment（不含 `#`）：JSON 的 UTF-8 字节做 base64url，无填充。 */
export function encodePreviewLaunch(launch: PreviewLaunch): string {
  return base64urlEncode(new TextEncoder().encode(JSON.stringify(launch)))
}

const HEX32 = /^[0-9a-f]{32}$/
const HEX64 = /^[0-9a-f]{64}$/

/**
 * URL fragment（可以带或不带开头的 `#`）→ `PreviewLaunch`。格式不对一律抛 `ProtocolError`：
 * 版本、label、id 和密钥的格式、端口范围、`path` 以 `/` 开头且不是 `//`（协议相对 URL）。
 */
export function decodePreviewLaunch(fragment: string): PreviewLaunch {
  const text = fragment.startsWith('#') ? fragment.slice(1) : fragment
  let value: unknown
  try {
    value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(base64urlDecode(text)))
  } catch (err) {
    if (err instanceof ProtocolError) throw err
    throw new ProtocolError('invalid preview launch')
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new ProtocolError('invalid preview launch')
  }
  const v = value as Record<string, unknown>
  const str = (k: string): string => {
    const x = v[k]
    if (typeof x !== 'string') throw new ProtocolError(`preview launch: ${k} must be a string`)
    return x
  }
  const int = (k: string, min: number, max: number): number => {
    const x = v[k]
    if (typeof x !== 'number' || !Number.isInteger(x) || x < min || x > max) {
      throw new ProtocolError(`preview launch: ${k} out of range`)
    }
    return x
  }
  const launch: PreviewLaunch = {
    v: int('v', 0, Number.MAX_SAFE_INTEGER),
    node: str('node'),
    node_public: str('node_public'),
    label: str('label'),
    port: int('port', 1, 65535),
    ticket_id: str('ticket_id'),
    psk: str('psk'),
    token: str('token'),
    expires_at_ms: int('expires_at_ms', 0, Number.MAX_SAFE_INTEGER),
    path: str('path'),
  }
  if (launch.v !== PREVIEW_LAUNCH_VERSION)
    throw new ProtocolError(`unsupported preview launch version ${launch.v}`)
  if (!isValidPreviewLabel(launch.label)) throw new ProtocolError('preview launch: invalid label')
  if (!HEX32.test(launch.node) || !HEX32.test(launch.ticket_id))
    throw new ProtocolError('preview launch: invalid id')
  if (!HEX64.test(launch.node_public) || !HEX64.test(launch.psk))
    throw new ProtocolError('preview launch: invalid key')
  if (launch.token.length === 0) throw new ProtocolError('preview launch: empty token')
  if (!launch.path.startsWith('/') || launch.path.startsWith('//')) {
    throw new ProtocolError('preview launch: path must start with a single /')
  }
  return launch
}
