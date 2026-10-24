// 会话页的「预览」视图（ADR-39，web.md § 13.1）：端口列表与轮询、打开流程拼出的 URL、iframe 的 sandbox、新标签页、停止、错误提示。
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { resetDb } from '@/db'
import { decodePreviewLaunch, PREVIEW_ENTER_PATH, type PreviewLaunch, type PreviewOpenParams, type PreviewPortsParams } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { errorBody, installFakeApi } from '@/test/fakeApi'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { DEVICE_TOKEN, HOUR_MS, LABEL, NODE_KEY, NODE_KEY_HEX, openResult, PREVIEW_NODE, PREVIEW_TEMPLATE, previewNodeInfo, previewPort, PSK, seedDeviceToken, TICKET_ID } from '@/test/preview'
import { useNodes } from '@/stores/nodes'
import { useUi } from '@/stores/ui'
import { setRpc } from '@/transport/instance'
import { RpcCallError } from '@/transport/rpcClient'
import { PreviewView } from './PreviewView'
import { PORTS_POLL_MS } from './usePreviewPorts'
import { clearTickets } from './ticket'

const NODE = PREVIEW_NODE
const SESSION = 'sess_a'
let rpc: FakeRpc
let api: ReturnType<typeof installFakeApi>

const openCalls = () => rpc.calls.filter((c) => c.method === 'preview.open')
const portsCalls = () => rpc.calls.filter((c) => c.method === 'preview.ports')

function serve(ports = [previewPort(3000, { process: 'node' }), previewPort(5173, { process: 'vite' })], detection: 'ok' | 'unavailable' = 'ok'): void {
  rpc.handlers['preview.ports'] = () => ({ ports, detection })
  rpc.handlers['preview.open'] = (p) => openResult({ port: (p as PreviewOpenParams).port })
  rpc.handlers['preview.close'] = () => ({})
  api.set('POST /api/preview/tokens', () => ({ token: 'hub-token', origin: `https://${LABEL}.preview.test`, expires_at_ms: Date.now() + 24 * HOUR_MS }))
}

function view(over: { offline?: boolean; sessionId?: string } = {}) {
  return <PreviewView nodeId={NODE} sessionId={over.sessionId ?? SESSION} template={PREVIEW_TEMPLATE} offline={over.offline ?? false} />
}

/** iframe 的 src 里的启动参数。 */
function launchOf(frame: HTMLElement): PreviewLaunch {
  return decodePreviewLaunch(new URL(frame.getAttribute('src')!).hash)
}

beforeEach(async () => {
  await resetDb()
  clearTickets()
  rpc = new FakeRpc()
  setRpc(rpc)
  api = installFakeApi({})
  useUi.setState({ toasts: [] })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, publicKey: NODE_KEY, info: previewNodeInfo() }) }, order: [NODE], loaded: true })
  await seedDeviceToken()
})
afterEach(() => {
  setRpc(null)
  vi.useRealTimers()
})

describe('PreviewView: ports', () => {
  it('lists the ports sorted by number, marks the ones from the config, and ends with “Other port…”', async () => {
    serve([previewPort(5173, { process: 'vite' }), previewPort(80, { source: 'config', process: null }), previewPort(3000, { process: 'node' })])
    render(view())
    const select = await screen.findByTestId('preview-port')
    await waitFor(() => expect(within(select).getAllByRole('option').length).toBeGreaterThan(1))
    expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual(['80 · config', '3000 · node', '5173 · vite', 'Other port…'])
  })

  it('opens the lowest listening port by itself (a configured port is not necessarily running)', async () => {
    serve([previewPort(80, { source: 'config', process: null }), previewPort(5173, { process: 'vite' }), previewPort(3000)])
    render(view())
    const frame = await screen.findByTestId('preview-frame')
    expect(launchOf(frame).port).toBe(3000)
    expect(screen.getByTestId('preview-port')).toHaveValue('3000')
  })

  it('asks for the session’s ports when it opens, and again every 5 seconds while it is visible', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] })
    serve()
    const { unmount } = render(view())
    await waitFor(() => expect(portsCalls()).toHaveLength(1))
    expect(portsCalls()[0]!.params).toEqual({ session_id: SESSION } satisfies PreviewPortsParams)
    await act(async () => void vi.advanceTimersByTime(PORTS_POLL_MS))
    expect(portsCalls()).toHaveLength(2)
    await act(async () => void vi.advanceTimersByTime(PORTS_POLL_MS))
    expect(portsCalls()).toHaveLength(3)
    unmount()
    await act(async () => void vi.advanceTimersByTime(PORTS_POLL_MS * 3))
    expect(portsCalls()).toHaveLength(3)
  })

  it('a new port shows up in the list on the next poll without touching what is open', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] })
    serve([previewPort(3000)])
    render(view())
    await screen.findByTestId('preview-frame')
    rpc.handlers['preview.ports'] = () => ({ ports: [previewPort(3000), previewPort(8080, { process: 'python' })], detection: 'ok' })
    await act(async () => void vi.advanceTimersByTime(PORTS_POLL_MS))
    await waitFor(() => expect(within(screen.getByTestId('preview-port')).getAllByRole('option').map((o) => o.textContent)).toContain('8080 · python'))
    expect(screen.getByTestId('preview-port')).toHaveValue('3000')
    expect(openCalls()).toHaveLength(1)
  })

  it('skips the poll while the page is hidden and catches up when it is shown again', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] })
    serve()
    const hidden = vi.spyOn(document, 'hidden', 'get').mockReturnValue(false)
    render(view())
    await waitFor(() => expect(portsCalls()).toHaveLength(1))
    hidden.mockReturnValue(true)
    await act(async () => void vi.advanceTimersByTime(PORTS_POLL_MS * 2))
    expect(portsCalls()).toHaveLength(1)
    hidden.mockReturnValue(false)
    await act(async () => void document.dispatchEvent(new Event('visibilitychange')))
    await waitFor(() => expect(portsCalls()).toHaveLength(2))
  })

  it('keeps the last list when a poll fails', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] })
    serve()
    render(view())
    await screen.findByTestId('preview-frame')
    rpc.handlers['preview.ports'] = () => {
      throw rpcError('internal')
    }
    await act(async () => void vi.advanceTimersByTime(PORTS_POLL_MS))
    expect(within(screen.getByTestId('preview-port')).getAllByRole('option').map((o) => o.textContent)).toContain('3000 · node')
    expect(screen.queryByTestId('preview-ports-error')).toBeNull()
  })

  it('says so when the machine cannot detect ports (only preview.allow_ports can be previewed)', async () => {
    serve([previewPort(8080, { source: 'config', process: null })], 'unavailable')
    render(view())
    expect(await screen.findByTestId('preview-detection-hint')).toHaveTextContent('can’t detect listening ports')
    expect(screen.getByTestId('preview-detection-hint')).toHaveTextContent('preview.allow_ports')
  })

  it('shows the empty state when no port is listening, and “Other port…” still works', async () => {
    serve([])
    const user = userEvent.setup()
    render(view())
    expect(await screen.findByTestId('preview-empty')).toHaveTextContent('No ports yet')
    expect(screen.queryByTestId('preview-frame')).toBeNull()
    expect(screen.getByTestId('preview-open-tab')).toBeDisabled()
    await user.selectOptions(screen.getByTestId('preview-port'), 'Other port…')
    await user.type(screen.getByTestId('preview-other-port'), '4000{Enter}')
    expect((await screen.findByTestId('preview-frame')).getAttribute('src')).toContain(PREVIEW_ENTER_PATH)
    expect(openCalls().map((c) => c.params)).toEqual([{ port: 4000, session_id: SESSION }])
  })

  it('cannot list the ports → says why', async () => {
    rpc.handlers['preview.ports'] = () => {
      throw rpcError('internal')
    }
    render(view())
    expect(await screen.findByTestId('preview-ports-error')).toHaveTextContent('Something went wrong on the node.')
  })

  it('a node that is offline is not asked for ports, and its controls that need the node are off', async () => {
    serve()
    render(view({ offline: true }))
    expect(await screen.findByTestId('preview-offline')).toBeInTheDocument()
    await act(async () => {})
    expect(portsCalls()).toHaveLength(0)
    expect(screen.getByTestId('preview-port')).toBeDisabled()
    expect(screen.getByTestId('preview-stop')).toBeDisabled()
  })
})

describe('PreviewView: the other-port box', () => {
  it('accepts only whole numbers from 1 to 65535', async () => {
    serve([previewPort(3000)])
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    await user.selectOptions(screen.getByTestId('preview-port'), 'Other port…')
    for (const bad of ['abc', '0', '65536', '12.5', '-3']) {
      await user.clear(screen.getByTestId('preview-other-port'))
      await user.type(screen.getByTestId('preview-other-port'), `${bad}{Enter}`)
      expect(await screen.findByTestId('preview-other-invalid')).toHaveTextContent('between 1 and 65535')
    }
    expect(openCalls()).toHaveLength(1)
    await user.clear(screen.getByTestId('preview-other-port'))
    await user.type(screen.getByTestId('preview-other-port'), '65535{Enter}')
    await waitFor(() => expect(openCalls()).toHaveLength(2))
    expect(openCalls()[1]!.params).toEqual({ port: 65535, session_id: SESSION })
  })

  it('a port that is not in the list stays selectable once it was opened, and the daemon’s refusal is shown', async () => {
    serve([previewPort(3000)])
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    rpc.handlers['preview.open'] = (p) => {
      if ((p as PreviewOpenParams).port === 22) throw rpcError('forbidden', undefined, 'port_not_allowed')
      return openResult({ port: (p as PreviewOpenParams).port })
    }
    await user.selectOptions(screen.getByTestId('preview-port'), 'Other port…')
    await user.type(screen.getByTestId('preview-other-port'), '22{Enter}')
    expect(await screen.findByTestId('preview-error')).toHaveTextContent('isn’t allowed for preview')
    expect(screen.getByTestId('preview-port')).toHaveValue('22')
    expect(within(screen.getByTestId('preview-port')).getAllByRole('option').map((o) => o.textContent)).toEqual(['22', '3000 · node', 'Other port…'])
  })
})

describe('PreviewView: opening', () => {
  it('preview.open → token request → the iframe points at <origin>/__anycon/enter#<launch> with every field right', async () => {
    serve()
    const expires = Date.now() + 2 * HOUR_MS
    rpc.handlers['preview.open'] = (p) => openResult({ port: (p as PreviewOpenParams).port, expires_at_ms: expires })
    render(view())
    const frame = await screen.findByTestId('preview-frame')

    expect(openCalls()[0]!.params).toEqual({ port: 3000, session_id: SESSION })
    const token = api.callsTo('POST /api/preview/tokens')
    expect(token).toHaveLength(1)
    expect(token[0]!.body).toEqual({ node: NODE, label: LABEL })
    expect(token[0]!.headers.Authorization).toBe(`Bearer ${DEVICE_TOKEN}`)

    const src = frame.getAttribute('src')!
    expect(src.startsWith(`https://${LABEL}.preview.test${PREVIEW_ENTER_PATH}#`)).toBe(true)
    expect(launchOf(frame)).toEqual({
      v: 1,
      node: NODE,
      node_public: NODE_KEY_HEX,
      label: LABEL,
      port: 3000,
      ticket_id: TICKET_ID,
      psk: PSK,
      token: 'hub-token',
      expires_at_ms: expires,
      path: '/',
    })
  })

  it('the iframe is sandboxed without allow-top-navigation, sends no referrer, and has a title', async () => {
    serve()
    render(view())
    const frame = await screen.findByTestId('preview-frame')
    expect(frame.tagName).toBe('IFRAME')
    const sandbox = frame.getAttribute('sandbox')!
    expect(sandbox.split(' ').sort()).toEqual(['allow-downloads', 'allow-forms', 'allow-modals', 'allow-popups', 'allow-popups-to-escape-sandbox', 'allow-same-origin', 'allow-scripts'])
    expect(sandbox).not.toContain('allow-top-navigation')
    expect(frame.getAttribute('referrerpolicy')).toBe('no-referrer')
    expect(frame).toHaveAttribute('title', 'Preview of port 3000')
  })

  it('never listens for messages from the page it embeds', async () => {
    const added = vi.spyOn(window, 'addEventListener')
    serve()
    render(view())
    await screen.findByTestId('preview-frame')
    expect(added.mock.calls.filter(([type]) => type === 'message')).toEqual([])
  })

  it('switching the port opens the new one from “/”; going back reuses the cached ticket', async () => {
    serve()
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    await user.clear(screen.getByTestId('preview-path'))
    await user.type(screen.getByTestId('preview-path'), '/deep{Enter}')
    await user.selectOptions(screen.getByTestId('preview-port'), '5173 · vite')
    await waitFor(() => expect(launchOf(screen.getByTestId('preview-frame')).port).toBe(5173))
    expect(launchOf(screen.getByTestId('preview-frame')).path).toBe('/')
    expect(screen.getByTestId('preview-path')).toHaveValue('/')
    await user.selectOptions(screen.getByTestId('preview-port'), '3000 · node')
    await waitFor(() => expect(launchOf(screen.getByTestId('preview-frame')).port).toBe(3000))
    expect(openCalls().map((c) => (c.params as PreviewOpenParams).port)).toEqual([3000, 5173])
  })

  it('the address bar: Enter navigates to that path (a pasted URL keeps only its path; // is not a host)', async () => {
    serve()
    const user = userEvent.setup()
    render(view())
    const first = await screen.findByTestId('preview-frame')
    await user.clear(screen.getByTestId('preview-path'))
    await user.type(screen.getByTestId('preview-path'), 'docs/intro?x=1{Enter}')
    await waitFor(() => expect(screen.getByTestId('preview-frame')).not.toBe(first))
    expect(launchOf(screen.getByTestId('preview-frame')).path).toBe('/docs/intro?x=1')
    expect(screen.getByTestId('preview-path')).toHaveValue('/docs/intro?x=1')

    await user.clear(screen.getByTestId('preview-path'))
    await user.type(screen.getByTestId('preview-path'), 'http://localhost:3000/pasted?a=b{Enter}')
    await waitFor(() => expect(launchOf(screen.getByTestId('preview-frame')).path).toBe('/pasted?a=b'))
    await user.clear(screen.getByTestId('preview-path'))
    await user.type(screen.getByTestId('preview-path'), '//evil.example{Enter}')
    await waitFor(() => expect(launchOf(screen.getByTestId('preview-frame')).path).toBe('/evil.example'))
    // 同一张票据：没有再向 daemon 和 hub 要
    expect(openCalls()).toHaveLength(1)
    expect(api.callsTo('POST /api/preview/tokens')).toHaveLength(1)
  })

  it('reload mounts a fresh iframe at the address-bar path, with the same cached ticket', async () => {
    serve()
    const user = userEvent.setup()
    render(view())
    const first = await screen.findByTestId('preview-frame')
    await user.click(screen.getByTestId('preview-reload'))
    await waitFor(() => expect(screen.getByTestId('preview-frame')).not.toBe(first))
    expect(screen.getByTestId('preview-frame').getAttribute('src')).toBe(first.getAttribute('src'))
    expect(openCalls()).toHaveLength(1)
  })

  it('a ticket with less than 10 minutes left is replaced when the preview is reloaded', async () => {
    serve()
    rpc.handlers['preview.open'] = (p) => openResult({ port: (p as PreviewOpenParams).port, expires_at_ms: Date.now() + 5 * 60_000 })
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    rpc.handlers['preview.open'] = (p) => openResult({ port: (p as PreviewOpenParams).port, ticket_id: 'a'.repeat(32) })
    await user.click(screen.getByTestId('preview-reload'))
    await waitFor(() => expect(launchOf(screen.getByTestId('preview-frame')).ticket_id).toBe('a'.repeat(32)))
    expect(openCalls()).toHaveLength(2)
  })

  it('does not put the URL (it carries the keys) anywhere it could leak: no log line mentions the secrets', async () => {
    const spies = [vi.spyOn(console, 'log'), vi.spyOn(console, 'warn'), vi.spyOn(console, 'error'), vi.spyOn(console, 'info'), vi.spyOn(console, 'debug')]
    serve()
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    await user.click(screen.getByTestId('preview-reload'))
    const logged = JSON.stringify(spies.flatMap((s) => s.mock.calls))
    expect(logged).not.toContain(PSK)
    expect(logged).not.toContain('hub-token')
    expect(window.location.href).not.toContain(PSK)
  })

  it('remembers the port per session for next time the view is opened', async () => {
    serve()
    const user = userEvent.setup()
    const first = render(view({ sessionId: 'sess_remember' }))
    await screen.findByTestId('preview-frame')
    await user.selectOptions(screen.getByTestId('preview-port'), '5173 · vite')
    await waitFor(() => expect(launchOf(screen.getByTestId('preview-frame')).port).toBe(5173))
    first.unmount()
    render(view({ sessionId: 'sess_remember' }))
    await waitFor(() => expect(screen.getByTestId('preview-port')).toHaveValue('5173'))
    expect(launchOf(await screen.findByTestId('preview-frame')).port).toBe(5173)
  })
})

describe('PreviewView: open in a new tab', () => {
  it('opens <origin>/__anycon/enter#<launch> with noopener,noreferrer — straight from the click when the ticket is cached', async () => {
    serve()
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    await user.clear(screen.getByTestId('preview-path'))
    await user.type(screen.getByTestId('preview-path'), '/settings{Enter}')
    await waitFor(() => expect(launchOf(screen.getByTestId('preview-frame')).path).toBe('/settings'))

    await user.click(screen.getByTestId('preview-open-tab'))
    expect(open).toHaveBeenCalledTimes(1)
    const [url, target, features] = open.mock.calls[0]!
    expect(target).toBe('_blank')
    expect(features).toBe('noopener,noreferrer')
    expect(String(url).startsWith(`https://${LABEL}.preview.test${PREVIEW_ENTER_PATH}#`)).toBe(true)
    expect(decodePreviewLaunch(new URL(String(url)).hash)).toMatchObject({ label: LABEL, port: 3000, ticket_id: TICKET_ID, psk: PSK, token: 'hub-token', path: '/settings' })
    // 用的是缓存里的票据，没有再要一张
    expect(openCalls()).toHaveLength(1)
  })

  it('asks for a ticket first when none is cached', async () => {
    serve()
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    clearTickets()
    await user.click(screen.getByTestId('preview-open-tab'))
    await waitFor(() => expect(open).toHaveBeenCalledTimes(1))
    expect(openCalls()).toHaveLength(2)
  })

  it('shows why when the ticket cannot be had, and opens nothing', async () => {
    serve()
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    clearTickets()
    api.set('POST /api/preview/tokens', () => ({ status: 404, body: errorBody('preview_disabled') }))
    await user.click(screen.getByTestId('preview-open-tab'))
    expect(await screen.findByTestId('preview-error')).toHaveTextContent('This hub has preview turned off.')
    expect(open).not.toHaveBeenCalled()
    // 已经开着的预览不受影响
    expect(screen.getByTestId('preview-frame')).toBeInTheDocument()
  })
})

describe('PreviewView: stopping', () => {
  it('asks first, explaining that it disconnects every device; then closes the label and drops the iframe', async () => {
    serve()
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    await user.click(screen.getByTestId('preview-stop'))
    const dialog = await screen.findByTestId('preview-stop-dialog')
    expect(dialog).toHaveTextContent('Stop the preview of port 3000?')
    expect(dialog).toHaveTextContent('all your devices')
    expect(rpc.methods()).not.toContain('preview.close')

    await user.click(screen.getByTestId('preview-stop-confirm'))
    await waitFor(() => expect(screen.queryByTestId('preview-frame')).toBeNull())
    expect(rpc.calls.find((c) => c.method === 'preview.close')!.params).toEqual({ label: LABEL })
    expect(await screen.findByTestId('preview-stopped')).toHaveTextContent('Preview stopped')
    expect(useUi.getState().toasts.map((x) => x.text)).toContain('Stopped the preview of port 3000')
    expect(screen.getByTestId('preview-stop')).toBeDisabled()
    expect(screen.getByTestId('preview-port')).toHaveValue('3000')
  })

  it('cancelling sends nothing, and a refusal from the node keeps the preview', async () => {
    serve()
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    await user.click(screen.getByTestId('preview-stop'))
    await user.click(within(await screen.findByTestId('preview-stop-dialog')).getByRole('button', { name: 'Cancel' }))
    expect(rpc.methods()).not.toContain('preview.close')
    expect(screen.getByTestId('preview-frame')).toBeInTheDocument()

    rpc.handlers['preview.close'] = () => {
      throw rpcError('internal')
    }
    await user.click(screen.getByTestId('preview-stop'))
    await user.click(await screen.findByTestId('preview-stop-confirm'))
    await waitFor(() => expect(useUi.getState().toasts.some((x) => x.kind === 'error')).toBe(true))
    expect(screen.getByTestId('preview-frame')).toBeInTheDocument()
  })

  it('“Start again” after a stop asks for a fresh ticket', async () => {
    serve()
    const user = userEvent.setup()
    render(view())
    await screen.findByTestId('preview-frame')
    await user.click(screen.getByTestId('preview-stop'))
    await user.click(await screen.findByTestId('preview-stop-confirm'))
    await user.click(await screen.findByTestId('preview-restart'))
    expect(await screen.findByTestId('preview-frame')).toBeInTheDocument()
    expect(openCalls()).toHaveLength(2)
  })
})

describe('PreviewView: errors', () => {
  const cases: Array<[string, () => void, string]> = [
    [
      'forbidden / port_not_allowed',
      () => {
        rpc.handlers['preview.open'] = () => {
          throw rpcError('forbidden', undefined, 'port_not_allowed')
        }
      },
      'preview.allow_ports',
    ],
    [
      'forbidden / disabled',
      () => {
        rpc.handlers['preview.open'] = () => {
          throw rpcError('forbidden', undefined, 'disabled')
        }
      },
      'Preview is turned off on this node',
    ],
    ['404 preview_disabled', () => api.set('POST /api/preview/tokens', () => ({ status: 404, body: errorBody('preview_disabled') })), 'This hub has preview turned off.'],
    ['404 not_found', () => api.set('POST /api/preview/tokens', () => ({ status: 404, body: errorBody('not_found') })), 'doesn’t know this node'],
    ['409 label_taken', () => api.set('POST /api/preview/tokens', () => ({ status: 409, body: errorBody('label_taken') })), 'already used by another node'],
    ['409 too_many_labels', () => api.set('POST /api/preview/tokens', () => ({ status: 409, body: errorBody('too_many_labels') })), 'maximum number of preview addresses'],
    ['401', () => api.set('POST /api/preview/tokens', () => ({ status: 401, body: errorBody('unauthorized') })), 'doesn’t accept this browser as a paired device'],
    [
      'the node drops off while opening',
      () => {
        rpc.handlers['preview.open'] = () => {
          throw new RpcCallError('offline', 'preview.open: node link not ready')
        }
      },
      'The node is offline.',
    ],
    ['the hub is unreachable', () => api.set('POST /api/preview/tokens', () => Promise.reject(new TypeError('fetch failed'))), 'Could not reach the hub.'],
  ]

  it.each(cases)('%s → a readable message and a retry', async (_name, arrange, text) => {
    serve()
    arrange()
    const user = userEvent.setup()
    render(view())
    const box = await screen.findByTestId('preview-error')
    expect(box).toHaveTextContent(text)
    expect(screen.queryByTestId('preview-frame')).toBeNull()
    // 重试：问题解决后能打开
    serve()
    await user.click(screen.getByTestId('preview-retry'))
    expect(await screen.findByTestId('preview-frame')).toBeInTheDocument()
    expect(screen.queryByTestId('preview-error')).toBeNull()
  })

  it('a browser that has no device token says so, without asking the node for a ticket', async () => {
    serve()
    await seedDeviceToken(null)
    render(view())
    expect(await screen.findByTestId('preview-error')).toHaveTextContent('no device token')
    expect(openCalls()).toHaveLength(0)
  })

  it('the latest choice wins when two opens overlap', async () => {
    serve()
    let release: (() => void) | undefined
    rpc.handlers['preview.open'] = async (p) => {
      const port = (p as PreviewOpenParams).port
      if (port === 3000) await new Promise<void>((r) => (release = r))
      return openResult({ port })
    }
    const user = userEvent.setup()
    render(view())
    await waitFor(() => expect(openCalls()).toHaveLength(1))
    await user.selectOptions(screen.getByTestId('preview-port'), '5173 · vite')
    await waitFor(() => expect(launchOf(screen.getByTestId('preview-frame')).port).toBe(5173))
    await act(async () => release?.())
    expect(launchOf(screen.getByTestId('preview-frame')).port).toBe(5173)
  })
})

describe('PreviewView: phone layout', () => {
  const narrow = () =>
    vi.spyOn(window, 'matchMedia').mockImplementation(
      (query: string) => ({ matches: false, media: query, onchange: null, addEventListener: () => {}, removeEventListener: () => {}, addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false }) as unknown as MediaQueryList,
    )

  it('keeps the toolbar in one row and makes “Open in new tab” the prominent labelled button', async () => {
    narrow()
    serve()
    render(view())
    await screen.findByTestId('preview-frame')
    const toolbar = screen.getByTestId('preview-toolbar')
    expect(toolbar).toHaveClass('flex', 'items-center')
    expect(toolbar).not.toHaveClass('flex-wrap')
    const open = screen.getByTestId('preview-open-tab')
    expect(open).toHaveTextContent('Open')
    expect(open).toHaveClass('bg-accent')
    expect(open).toHaveAccessibleName('Open in new tab')
    // 其余操作还在同一行
    for (const id of ['preview-port', 'preview-path', 'preview-reload', 'preview-stop']) expect(toolbar).toContainElement(screen.getByTestId(id))
  })

  it('on a wide screen the same button is a quiet icon', async () => {
    serve()
    render(view())
    await screen.findByTestId('preview-frame')
    const open = screen.getByTestId('preview-open-tab')
    expect(open).not.toHaveClass('bg-accent')
    expect(open.textContent).toBe('')
    expect(open).toHaveAccessibleName('Open in new tab')
  })
})
