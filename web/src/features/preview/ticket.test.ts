// 预览票据（ADR-39，web.md § 13.1）：preview.open → 令牌请求 → 启动参数 → URL，缓存与刷新，本地条件，停止。
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { ApiError } from '@/api/client'
import { resetDb } from '@/db'
import { decodePreviewLaunch, PREVIEW_ENTER_PATH, type PreviewCloseParams, type PreviewOpenParams } from '@/protocol'
import { DEVICE_TOKEN, HOUR_MS, LABEL, NODE_KEY, NODE_KEY_HEX, openResult, PREVIEW_NODE, PREVIEW_TEMPLATE, previewNodeInfo, PSK, seedDeviceToken, TICKET_ID } from '@/test/preview'
import { nodeEntry } from '@/test/factories'
import { errorBody, installFakeApi, type ApiCall } from '@/test/fakeApi'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { useNodes } from '@/stores/nodes'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { clearTickets, closePreview, ensureTicket, forgetLabel, knownTicket, launchUrl, normalizePreviewPath, PreviewLocalError, REFRESH_MARGIN_MS, usableTicket } from './ticket'

const NODE = PREVIEW_NODE
let rpc: FakeRpc
let api: ReturnType<typeof installFakeApi>

const args = { nodeId: NODE, port: 5173, sessionId: 'sess_a', template: PREVIEW_TEMPLATE }
const tokenCalls = (): ApiCall[] => api.callsTo('POST /api/preview/tokens')
const openCalls = () => rpc.calls.filter((c) => c.method === 'preview.open')

function serve(opts: { open?: Parameters<typeof openResult>[0]; tokenExpiresIn?: number } = {}): void {
  rpc.handlers['preview.open'] = () => openResult(opts.open)
  api.set('POST /api/preview/tokens', () => ({ token: 'hub-token', origin: `https://${LABEL}.preview.test`, expires_at_ms: Date.now() + (opts.tokenExpiresIn ?? 24 * HOUR_MS) }))
}

beforeEach(async () => {
  await resetDb()
  clearTickets()
  rpc = new FakeRpc()
  setRpc(rpc)
  api = installFakeApi({})
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, publicKey: NODE_KEY, info: previewNodeInfo() }) }, order: [NODE], loaded: true })
  await seedDeviceToken()
})
afterEach(() => setRpc(null))

describe('ensureTicket / launchUrl', () => {
  it('asks the node for a ticket, trades the device token for a hub token, and builds <origin>/__anycon/enter#<launch>', async () => {
    const tokenExpiry = Date.now() + 5 * HOUR_MS
    serve({ open: { expires_at_ms: Date.now() + 24 * HOUR_MS }, tokenExpiresIn: 5 * HOUR_MS })
    const ticket = await ensureTicket(args)

    expect(openCalls()).toHaveLength(1)
    expect(openCalls()[0]!.params).toEqual({ port: 5173, session_id: 'sess_a' } satisfies PreviewOpenParams)
    expect(tokenCalls()).toHaveLength(1)
    expect(tokenCalls()[0]!.body).toEqual({ node: NODE, label: LABEL })

    const url = launchUrl(ticket, '/dashboard?tab=1')
    expect(url.startsWith(`https://${LABEL}.preview.test${PREVIEW_ENTER_PATH}#`)).toBe(true)
    const launch = decodePreviewLaunch(new URL(url).hash)
    expect(launch).toMatchObject({
      v: 1,
      node: NODE,
      node_public: NODE_KEY_HEX,
      label: LABEL,
      port: 5173,
      ticket_id: TICKET_ID,
      psk: PSK,
      token: 'hub-token',
      path: '/dashboard?tab=1',
    })
    // expires_at_ms 取票据和令牌里较早的那个
    expect(Math.abs(launch.expires_at_ms - tokenExpiry)).toBeLessThan(5_000)
  })

  it('uses the earlier of the two expiry times when the ticket is the shorter one', async () => {
    const ticketExpiry = Date.now() + 2 * HOUR_MS
    serve({ open: { expires_at_ms: ticketExpiry } })
    const ticket = await ensureTicket(args)
    expect(decodePreviewLaunch(new URL(launchUrl(ticket, '/')).hash).expires_at_ms).toBe(ticketExpiry)
  })

  it('sends the token request with the device token as Bearer, X-Anycon, and no cookie', async () => {
    serve()
    await ensureTicket(args)
    const call = tokenCalls()[0]!
    expect(call.headers.Authorization).toBe(`Bearer ${DEVICE_TOKEN}`)
    expect(call.headers['X-Anycon']).toBe('1')
    expect((globalThis.fetch as unknown as { mock: { calls: [string, RequestInit][] } }).mock.calls[0]![1].credentials).toBe('omit')
  })

  it('keeps the secrets in the fragment only: the path part of the URL carries no key material', async () => {
    serve()
    const ticket = await ensureTicket(args)
    const url = new URL(launchUrl(ticket, '/'))
    expect(url.pathname).toBe(PREVIEW_ENTER_PATH)
    expect(url.search).toBe('')
    expect(url.href.split('#')[0]).not.toContain(PSK)
    expect(url.href.split('#')[0]).not.toContain('hub-token')
  })

  it('reuses the cached ticket per (node, port) while it has more than 10 minutes left', async () => {
    serve()
    const a = await ensureTicket(args)
    const b = await ensureTicket(args)
    expect(b).toBe(a)
    expect(openCalls()).toHaveLength(1)
    expect(tokenCalls()).toHaveLength(1)
    expect(usableTicket(NODE, 5173)).toBe(a)
    // 另一个端口是另一份缓存
    await ensureTicket({ ...args, port: 3000 })
    expect(openCalls()).toHaveLength(2)
  })

  it('asks again when less than 10 minutes are left', async () => {
    serve({ open: { expires_at_ms: Date.now() + REFRESH_MARGIN_MS - 60_000 } })
    const first = await ensureTicket(args)
    expect(usableTicket(NODE, 5173)).toBeUndefined()
    expect(knownTicket(NODE, 5173)).toBe(first)
    serve()
    const second = await ensureTicket(args)
    expect(second).not.toBe(first)
    expect(openCalls()).toHaveLength(2)
    expect(tokenCalls()).toHaveLength(2)
  })

  it('merges concurrent requests for the same (node, port) into one', async () => {
    serve()
    const [a, b] = await Promise.all([ensureTicket(args), ensureTicket(args)])
    expect(a).toBe(b)
    expect(openCalls()).toHaveLength(1)
    expect(tokenCalls()).toHaveLength(1)
  })

  it('does not ask the node for a ticket when the browser has no device token or no node key', async () => {
    serve()
    await seedDeviceToken(null)
    await expect(ensureTicket(args)).rejects.toMatchObject({ code: 'no_device_token' })
    await seedDeviceToken()
    useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, info: previewNodeInfo() }) } })
    await expect(ensureTicket(args)).rejects.toMatchObject({ code: 'no_node_key' })
    await expect(ensureTicket({ ...args, template: 'https://nolabel.example' })).rejects.toBeInstanceOf(PreviewLocalError)
    expect(openCalls()).toHaveLength(0)
    expect(tokenCalls()).toHaveLength(0)
  })

  it('a label the hub cannot use as a hostname is a local error, before any token is requested', async () => {
    serve({ open: { label: 'Not-A-Label' } })
    await expect(ensureTicket(args)).rejects.toMatchObject({ code: 'bad_label' })
    expect(tokenCalls()).toHaveLength(0)
  })

  it('passes the daemon’s and the hub’s errors through, and caches nothing', async () => {
    rpc.handlers['preview.open'] = () => {
      throw rpcError('forbidden', undefined, 'port_not_allowed')
    }
    const a = await ensureTicket(args).catch((e: unknown) => e)
    expect(a).toBeInstanceOf(RpcCallError)
    expect((a as RpcCallError).reason).toBe('port_not_allowed')

    serve()
    api.set('POST /api/preview/tokens', () => ({ status: 409, body: errorBody('label_taken') }))
    const b = await ensureTicket(args).catch((e: unknown) => e)
    expect(b).toBeInstanceOf(ApiError)
    expect(b).toMatchObject({ status: 409, code: 'label_taken' })
    expect(knownTicket(NODE, 5173)).toBeUndefined()
  })

  it('a 401 from the token endpoint does not send the user to the login page (it is about the device, not the cookie)', async () => {
    serve()
    api.set('POST /api/preview/tokens', () => ({ status: 401, body: errorBody('unauthorized') }))
    await expect(ensureTicket(args)).rejects.toMatchObject({ status: 401 })
  })
})

describe('closePreview', () => {
  it('sends preview.close with the label and forgets every cached ticket of it', async () => {
    serve()
    rpc.handlers['preview.close'] = () => ({})
    await ensureTicket(args)
    await closePreview(NODE, LABEL)
    expect(rpc.calls.find((c) => c.method === 'preview.close')!.params).toEqual({ label: LABEL } satisfies PreviewCloseParams)
    expect(knownTicket(NODE, 5173)).toBeUndefined()
  })

  it('keeps the cache when the node refuses', async () => {
    serve()
    await ensureTicket(args)
    rpc.handlers['preview.close'] = () => {
      throw rpcError('internal')
    }
    await expect(closePreview(NODE, LABEL)).rejects.toBeInstanceOf(RpcCallError)
    expect(knownTicket(NODE, 5173)).toBeDefined()
    forgetLabel(NODE, LABEL)
    expect(knownTicket(NODE, 5173)).toBeUndefined()
  })
})

describe('normalizePreviewPath', () => {
  it.each([
    ['', '/'],
    ['  ', '/'],
    ['/', '/'],
    ['/a/b?c=1', '/a/b?c=1'],
    ['a/b', '/a/b'],
    ['?x=1', '/?x=1'],
    ['//evil.example/x', '/evil.example/x'],
    ['/\\evil.example', '/evil.example'],
    ['\\\\evil.example', '/evil.example'],
    ['/\t/evil.example', '/evil.example'],
    ['http://localhost:5173/app?x=1#top', '/app?x=1#top'],
    ['http://localhost:5173', '/'],
    ['https://example.com//double', '/double'],
  ])('%j → %j', (input, want) => {
    expect(normalizePreviewPath(input)).toBe(want)
  })
})
