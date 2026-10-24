/**
 * dev server 预览（ADR-39）用例共用的小工具：预览 origin 的写法、从 iframe 的 `src` 里取启动参数、
 * 以预览域名的 `Host` 向 hub 要静态文件。
 *
 * 预览域名是 `<label>.preview.localhost`：Chromium 自己把 `*.localhost` 解析到回环地址，别的程序（Node 的 `fetch`）不一定会，
 * 所以 Node 侧要访问预览域名上的东西时，连 127.0.0.1、只把 `Host` 头写成预览域名（`fetchPreviewHost`）。
 */
import { request as httpRequest } from 'node:http'

import { hubPort } from './env.js'

/** 满足 hub 的 label 格式（26 位小写 base32）的占位 label，只用来打到预览域名上的静态文件。 */
export const PROBE_LABEL = 'a'.repeat(26)

/** `http://<label>.preview.localhost:<hub 端口>`。 */
export function previewOriginOf(label: string): string {
  return `http://${label}.preview.localhost:${hubPort}`
}

/** 预览 origin（或其下任意 URL）的 label：主机名的第一段。 */
export function labelOf(url: string): string {
  return new URL(url).hostname.split('.')[0] ?? ''
}

/** 预览页的启动参数（`<origin>/__anycon/enter#<base64url(JSON)>`，packages/protocol/src/preview.ts 的 `PreviewLaunch`）。 */
export interface LaunchFragment {
  v: number
  node: string
  node_public: string
  label: string
  port: number
  ticket_id: string
  psk: string
  token: string
  expires_at_ms: number
  path: string
}

/** 从带 fragment 的启动 URL 里解出启动参数（含票据密钥和令牌：只在断言里用，别打印）。 */
export function decodeLaunchUrl(url: string): LaunchFragment {
  const hash = new URL(url).hash.slice(1)
  if (!hash) throw new Error('the URL has no launch fragment')
  return JSON.parse(Buffer.from(hash, 'base64url').toString('utf8')) as LaunchFragment
}

export interface HostResponse {
  status: number
  headers: Record<string, string | string[] | undefined>
  body: string
}

/** 以 `Host: <label>.preview.localhost:<hub 端口>` 向 hub 发一个请求（连的是 127.0.0.1）。 */
export function fetchPreviewHost(
  path: string,
  options: { label?: string; method?: string } = {},
): Promise<HostResponse> {
  const label = options.label ?? PROBE_LABEL
  return new Promise((resolve, reject) => {
    const req = httpRequest(
      {
        host: '127.0.0.1',
        port: hubPort,
        path,
        method: options.method ?? 'GET',
        headers: { Host: `${label}.preview.localhost:${hubPort}` },
        timeout: 5000,
      },
      (res) => {
        const chunks: Buffer[] = []
        res.on('data', (c: Buffer) => chunks.push(c))
        res.on('end', () =>
          resolve({
            status: res.statusCode ?? 0,
            headers: res.headers,
            body: Buffer.concat(chunks).toString('utf8'),
          }),
        )
      },
    )
    req.on('timeout', () => req.destroy(new Error('timed out')))
    req.on('error', reject)
    req.end()
  })
}
