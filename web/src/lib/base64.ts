// 标准 base64（带 padding）和字节数组互转。blob.put_chunk / blob.get 的 `data` 用它（protocol.md § 5.6）。

/** 一次交给 `String.fromCharCode` 的字节数（参数个数有引擎上限，JavaScriptCore 约 64k）。 */
const STEP = 0x2000

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = ''
  for (let i = 0; i < bytes.length; i += STEP) bin += String.fromCharCode(...bytes.subarray(i, i + STEP))
  return btoa(bin)
}

/** 不合法的 base64 会抛 `InvalidCharacterError`。 */
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/** base64url（无 padding，`-` `_`）→ 字节。VAPID 公钥、`PushSubscription.toJSON().keys` 都是这个编码。 */
export function base64UrlToBytes(b64url: string): Uint8Array<ArrayBuffer> {
  const std = b64url.replace(/-/g, '+').replace(/_/g, '/')
  const bin = atob(std + '='.repeat((4 - (std.length % 4)) % 4))
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/** 字节 → base64url（无 padding）。 */
export function bytesToBase64Url(bytes: Uint8Array): string {
  return bytesToBase64(bytes).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}
