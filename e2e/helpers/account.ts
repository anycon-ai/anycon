/**
 * 不经页面、用已登录的 `APIRequestContext`（比如 `context.request`，带着会话 cookie）改账号的邮箱 / 读推送订阅。
 * 写操作要 `X-Anycon: 1`；hub 对发码有限速（每 IP 每小时 5 次，登录和绑定各一个桶，同一邮箱同一用途 60 秒一次），
 * 所以用例里的发码次数要算好（见 e2e/README.md）。
 */
import { expect, type APIRequestContext } from '@playwright/test'

import type { E2EState } from './env.js'
import { mailsTo, waitForCode } from './smtp.js'

const CSRF = { 'X-Anycon': '1' }

export interface PushSubscriptionRow {
  id: string
  device_id: string
  device_name?: string
  endpoint_host: string
  created_at_ms: number
}

/** 绑定邮箱：`POST /api/me/email` → 从假 SMTP 取绑定码 → `POST /api/me/email/verify`。 */
export async function bindEmailViaApi(
  request: APIRequestContext,
  state: E2EState,
  address: string,
): Promise<void> {
  const after = mailsTo(state.smtpDir, address).length
  const sent = await request.post(`${state.hubUrl}/api/me/email`, {
    data: { email: address },
    headers: CSRF,
  })
  expect(sent.status(), `POST /api/me/email → ${sent.status()}`).toBe(200)
  const code = await waitForCode(state.smtpDir, address, { after })
  const verified = await request.post(`${state.hubUrl}/api/me/email/verify`, {
    data: { email: address, code },
    headers: CSRF,
  })
  expect(verified.status(), `POST /api/me/email/verify → ${verified.status()}`).toBe(200)
}

/** 解绑邮箱（没绑定过也是 204）。 */
export async function unbindEmailViaApi(
  request: APIRequestContext,
  state: E2EState,
): Promise<void> {
  const res = await request.delete(`${state.hubUrl}/api/me/email`, { headers: CSRF })
  expect(res.status(), `DELETE /api/me/email → ${res.status()}`).toBe(204)
}

/** 本账号在 hub 里登记的推送订阅（endpoint 本身不回显，只有主机名）。 */
export async function listPushSubscriptions(
  request: APIRequestContext,
  state: E2EState,
): Promise<PushSubscriptionRow[]> {
  const res = await request.get(`${state.hubUrl}/api/push/subscriptions`)
  expect(res.ok(), `GET /api/push/subscriptions → ${res.status()}`).toBe(true)
  return (await res.json()) as PushSubscriptionRow[]
}

/** 删掉本账号的全部推送订阅（用例失败后的清理：留着订阅，之后别的用例触发 `notify` 时 hub 会真的去连推送服务）。 */
export async function deleteAllPushSubscriptions(
  request: APIRequestContext,
  state: E2EState,
): Promise<void> {
  for (const row of await listPushSubscriptions(request, state)) {
    await request.delete(`${state.hubUrl}/api/push/subscriptions/${row.id}`, { headers: CSRF })
  }
}

/** 账号里名叫 `name` 的 web 设备的 id（`GET /api/devices`）。 */
export async function webDeviceId(
  request: APIRequestContext,
  state: E2EState,
  name: string,
): Promise<string> {
  const res = await request.get(`${state.hubUrl}/api/devices`)
  expect(res.ok(), `GET /api/devices → ${res.status()}`).toBe(true)
  const list = (await res.json()) as Array<{ id: string; name: string; kind: string }>
  const found = list.find((d) => d.kind === 'web' && d.name === name)
  if (!found) throw new Error(`no web device named ${name}: ${JSON.stringify(list)}`)
  return found.id
}

/** hub 眼里账号的设备 `id` 此刻有没有在线连接（`GET /api/devices` 的 `online`）；设备不在列表里（吊销了）是 false。 */
export async function deviceOnline(
  request: APIRequestContext,
  state: E2EState,
  id: string,
): Promise<boolean> {
  const res = await request.get(`${state.hubUrl}/api/devices`)
  expect(res.ok(), `GET /api/devices → ${res.status()}`).toBe(true)
  const list = (await res.json()) as Array<{ id: string; online: boolean }>
  return list.find((d) => d.id === id)?.online === true
}
