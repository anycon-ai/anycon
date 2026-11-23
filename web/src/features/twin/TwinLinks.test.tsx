// 「使用别的节点」：列表、添加的筛选和确认、两步调用的顺序、失败说明、断开的说明；短码从不出现在任何渲染结果里。
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { FakeTwinDaemon, LINK_CODE, peerStatus, twinLink } from '@/test/fakeTwinDaemon'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { TwinLinks } from './TwinLinks'
import { handleNotification } from '@/features/sync/notifications'
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), vi.fn()))
const A = 'a'.repeat(32)
const B = 'b'.repeat(32)
const C = 'c'.repeat(32)
const D = 'd'.repeat(32)
const E = 'e'.repeat(32)
const info = (over: Partial<NodeInfo> = {}, twin = true): NodeInfo => ({ capabilities: { twin, todo: true }, twin_accept_links: true, group: 'home', ...over }) as unknown as NodeInfo
let rpc: FakeRpc
let daemon: FakeTwinDaemon
let warn: ReturnType<typeof vi.spyOn>
let log: ReturnType<typeof vi.spyOn>
let error: ReturnType<typeof vi.spyOn>
const nodes = () => ({
  [A]: nodeEntry({ id: A, name: 'alpha', info: info() }),
  [B]: nodeEntry({ id: B, name: 'beta', info: info() }),
  [C]: nodeEntry({ id: C, name: 'old', info: info({}, false) }),
  [D]: nodeEntry({ id: D, name: 'closed', info: info({ twin_accept_links: false }) }),
  [E]: nodeEntry({ id: E, name: 'sleepy', info: info(), conn: 'peer_offline' }),
})
beforeEach(() => {
  rpc = new FakeRpc()
  daemon = new FakeTwinDaemon(rpc)
  setRpc(rpc)
  useNodes.setState({ byId: nodes(), order: [A, B, C, D, E], loaded: true })
  warn = vi.spyOn(console, 'warn')
  log = vi.spyOn(console, 'log')
  error = vi.spyOn(console, 'error')
})
afterEach(() => vi.restoreAllMocks())
const calls = (method: string) => rpc.calls.filter((c) => c.method === method)
const renderLinks = async () => {
  render(<TwinLinks node={useNodes.getState().byId[A]!} />)
  await waitFor(() => expect(calls('twin.links')).toHaveLength(1))
}
/** 界面、日志和存储里都没有短码。 */
function expectNoCode(): void {
  expect(document.body.innerHTML).not.toContain(LINK_CODE)
  expect(document.body.textContent).not.toContain(LINK_CODE.replaceAll('-', ''))
  for (const spy of [warn, log, error]) expect(JSON.stringify(spy.mock.calls)).not.toContain(LINK_CODE)
  expect(JSON.stringify({ ...localStorage })).not.toContain(LINK_CODE)
  expect(JSON.stringify({ ...sessionStorage })).not.toContain(LINK_CODE)
}
async function openAdd() {
  await userEvent.click(screen.getByTestId('twin-link-add'))
  return screen.findByTestId('twin-link-add-dialog')
}

describe('Twin links list', () => {
  it('lists the node, online state, open projects and what is left from the last peer status', async () => {
    daemon.links = [twinLink({ status: peerStatus({ projects: ['api', 'docs'], remaining_sessions: 1, remaining_dispatches: 5 }) }), twinLink({ node_id: C, name: 'cold', online: false, status: null, group: 'work' })]
    await renderLinks()
    const rows = await screen.findAllByTestId('twin-link')
    expect(rows).toHaveLength(2)
    expect(within(rows[0]!).getByText('beta')).toBeInTheDocument()
    expect(within(rows[0]!).getByTestId('twin-link-online')).toHaveTextContent('online')
    expect(within(rows[0]!).getByTestId('twin-link-projects')).toHaveTextContent('Open projects: api, docs')
    expect(within(rows[0]!).getByTestId('twin-link-remaining')).toHaveTextContent('1 sessions, 5 dispatches today')
    expect(within(rows[1]!).getByTestId('twin-link-online')).toHaveTextContent('offline')
    expect(within(rows[1]!).getByText('Status not fetched yet.')).toBeInTheDocument()
  })
  it('says when nothing is open there, and when that node paused or stopped accepting', async () => {
    daemon.links = [twinLink({ status: peerStatus({ projects: [], paused: true, accept_links: false }) })]
    await renderLinks()
    expect(await screen.findByTestId('twin-link-projects')).toHaveTextContent('Nothing is open to this node there yet.')
    expect(screen.getByText('That node has paused this connection.')).toBeInTheDocument()
    expect(screen.getByText('That node does not accept twin connections right now.')).toBeInTheDocument()
  })
  it('reads the links again when the node says twin.changed, once for a burst', async () => {
    daemon.links = [twinLink({ online: false, status: null })]
    await renderLinks()
    expect(await screen.findByTestId('twin-link-online')).toHaveTextContent('offline')
    expect(screen.getByText('Status not fetched yet.')).toBeInTheDocument()
    daemon.links = [twinLink({ online: true, status: peerStatus({ projects: ['api'] }) })]
    for (let i = 0; i < 4; i++) handleNotification(A, 'twin.changed', { state: 'idle' })
    await waitFor(() => expect(screen.getByTestId('twin-link-online')).toHaveTextContent('online'))
    expect(screen.getByTestId('twin-link-projects')).toHaveTextContent('Open projects: api')
    expect(calls('twin.links')).toHaveLength(2)
  })
  it('is hidden on a daemon that does not know twin links yet', async () => {
    delete rpc.handlers['twin.links']
    render(<TwinLinks node={useNodes.getState().byId[A]!} />)
    await waitFor(() => expect(calls('twin.links')).toHaveLength(1))
    await waitFor(() => expect(screen.queryByTestId('twin-links')).not.toBeInTheDocument())
  })
})

describe('Adding a link', () => {
  it('lists the other paired nodes and greys out the ones that cannot be used, with the reason', async () => {
    daemon.links = []
    await renderLinks()
    const dialog = await openAdd()
    const rows = within(dialog).getAllByTestId('twin-link-candidate')
    expect(rows).toHaveLength(4)
    const by = (name: string) => rows.find((r) => r.textContent?.includes(name))!
    expect(by('beta').dataset.blocked).toBe('')
    expect(within(by('beta')).getByRole('radio')).toBeEnabled()
    expect(within(by('old')).getByRole('radio')).toBeDisabled()
    expect(by('old')).toHaveTextContent('does not support being used by a Twin')
    expect(within(by('closed')).getByRole('radio')).toBeDisabled()
    expect(by('closed')).toHaveTextContent('[twin] accept_links = true')
    expect(within(by('sleepy')).getByRole('radio')).toBeDisabled()
    expect(by('sleepy')).toHaveTextContent('Offline')
    expect(within(dialog).queryByText('alpha')).not.toBeInTheDocument()
  })
  it('greys out a node that is already connected', async () => {
    daemon.links = [twinLink({ node_id: B })]
    await renderLinks()
    await screen.findByTestId('twin-link')
    const dialog = await openAdd()
    const row = within(dialog).getAllByTestId('twin-link-candidate').find((r) => r.textContent?.includes('beta'))!
    expect(within(row).getByRole('radio')).toBeDisabled()
    expect(row).toHaveTextContent('Already connected.')
  })
  it('asks the used node for a code, then hands it to this node: two calls in order, nothing else, and the code is never shown', async () => {
    await renderLinks()
    await openAdd()
    expect(screen.getByTestId('twin-link-connect')).toBeDisabled()
    await userEvent.click(screen.getByRole('radio', { name: /beta/ }))
    await userEvent.click(screen.getByTestId('twin-link-connect'))
    await waitFor(() => expect(calls('twin.link')).toHaveLength(1))
    const order = rpc.calls.map((c) => `${c.nodeId === A ? 'A' : 'B'}:${c.method}`).filter((m) => /twin_link_code|twin\.link$/.test(m))
    expect(order).toEqual(['B:node.twin_link_code', 'A:twin.link'])
    expect(calls('node.twin_link_code')[0]).toMatchObject({ nodeId: B, params: {} })
    expect(calls('twin.link')[0]).toMatchObject({ nodeId: A, params: { code: LINK_CODE, node_id: B } })
    // 成功后对话框关闭并重读连接
    await waitFor(() => expect(screen.queryByTestId('twin-link-add-dialog')).not.toBeInTheDocument())
    expect(await screen.findByTestId('twin-link')).toHaveTextContent('beta')
    expect(calls('twin.links').length).toBeGreaterThanOrEqual(2)
    expectNoCode()
  })
  it('connects nodes in the same group without a second confirmation', async () => {
    await renderLinks()
    await openAdd()
    await userEvent.click(screen.getByRole('radio', { name: /beta/ }))
    await userEvent.click(screen.getByTestId('twin-link-connect'))
    await waitFor(() => expect(calls('twin.link')).toHaveLength(1))
    expect(screen.queryByTestId('twin-link-group-confirm')).not.toBeInTheDocument()
  })
  it('asks once more when the groups differ, and calls nothing before that is confirmed; back returns to the list', async () => {
    useNodes.getState().patch(B, { info: info({ group: 'work' }) })
    await renderLinks()
    await openAdd()
    await userEvent.click(screen.getByRole('radio', { name: /beta/ }))
    await userEvent.click(screen.getByTestId('twin-link-connect'))
    const confirm = await screen.findByTestId('twin-link-group-confirm')
    expect(confirm).toHaveTextContent('“home”')
    expect(confirm).toHaveTextContent('“work”')
    expect(confirm).toHaveTextContent('keep, for example, home and work nodes apart')
    expect(calls('node.twin_link_code')).toHaveLength(0)
    await userEvent.click(screen.getByRole('button', { name: 'Back' }))
    expect(screen.getByRole('radio', { name: /beta/ })).toBeInTheDocument()
    await userEvent.click(screen.getByTestId('twin-link-connect'))
    await userEvent.click(await screen.findByRole('button', { name: 'Connect anyway' }))
    await waitFor(() => expect(calls('twin.link')).toHaveLength(1))
    expect(calls('node.twin_link_code')).toHaveLength(1)
  })
  it('treats a missing group on one side as different', async () => {
    useNodes.getState().patch(B, { info: info({ group: null }) })
    await renderLinks()
    await openAdd()
    await userEvent.click(screen.getByRole('radio', { name: /beta/ }))
    await userEvent.click(screen.getByTestId('twin-link-connect'))
    expect(await screen.findByTestId('twin-link-group-confirm')).toHaveTextContent('no group')
  })
  it.each([
    ['code', 'accept_links_off', 'forbidden', 'That node does not accept twin connections. Set [twin] accept_links = true on that machine.'],
    ['link', 'not_found', 'not_found', 'The other node could not be reached for pairing: it must be on the same hub as this node, and the code must be fresh. Try again.'],
    ['link', 'timeout', 'unsupported', 'The other node did not accept the connection. It may have turned off accepting twin connections ([twin] accept_links).'],
    ['link', 'hub_changed', 'unsupported', "This node's twin connections were made on another hub. Disconnect them before connecting on this hub."],
    ['link', 'node_mismatch', 'invalid', 'The node that answered is not the one you chose, so nothing was connected.'],
    ['code', 'quota', 'busy', 'The hub is not opening another pairing right now (daily limit or rate limit). Try again later.'],
  ] as const)('explains a %s step failure with reason %s in plain words and keeps the dialog open', async (step, reason, code, text) => {
    const err = rpcError(code, undefined, reason)
    if (step === 'code') daemon.failCode = err
    else daemon.failLink = err
    await renderLinks()
    await openAdd()
    await userEvent.click(screen.getByRole('radio', { name: /beta/ }))
    await userEvent.click(screen.getByTestId('twin-link-connect'))
    expect(await screen.findByTestId('twin-link-error')).toHaveTextContent(text)
    expect(screen.getByTestId('twin-link-add-dialog')).toBeInTheDocument()
    // 失败后不自动再要短码（再要一个会作废上一个）
    expect(calls('node.twin_link_code')).toHaveLength(1)
    expect(calls('twin.link')).toHaveLength(step === 'code' ? 0 : 1)
    expectNoCode()
  })
  it('falls back to the step and the error code for an unknown reason, never to anything with the code in it', async () => {
    daemon.failLink = rpcError('internal', undefined, 'something_new')
    await renderLinks()
    await openAdd()
    await userEvent.click(screen.getByRole('radio', { name: /beta/ }))
    await userEvent.click(screen.getByTestId('twin-link-connect'))
    expect(await screen.findByTestId('twin-link-error')).toHaveTextContent('Twin could not pair with that node.')
    expectNoCode()
  })
  it('the code only passes through memory: not in the DOM, console or storage while the pairing is in flight either', async () => {
    let release: () => void = () => undefined
    const gate = new Promise<void>((r) => (release = r))
    const inner = rpc.handlers['twin.link']!
    rpc.handlers['twin.link'] = async (p, id) => {
      await gate
      return inner(p, id)
    }
    await renderLinks()
    await openAdd()
    await userEvent.click(screen.getByRole('radio', { name: /beta/ }))
    await userEvent.click(screen.getByTestId('twin-link-connect'))
    expect(await screen.findByText('Connecting…')).toBeInTheDocument()
    await waitFor(() => expect(calls('node.twin_link_code')).toHaveLength(1))
    expectNoCode()
    release()
    await waitFor(() => expect(screen.queryByTestId('twin-link-add-dialog')).not.toBeInTheDocument())
    expectNoCode()
  })
})

describe('Disconnecting', () => {
  it('calls twin.unlink on this node and says the device must be revoked on the other node', async () => {
    daemon.links = [twinLink({ node_id: B })]
    await renderLinks()
    await userEvent.click(await screen.findByTestId('twin-link-remove'))
    const dialog = await screen.findByTestId('twin-link-remove-dialog')
    expect(within(dialog).getByTestId('twin-link-revoke-hint')).toHaveTextContent('revoke this node’s twin device in the device list of beta')
    // 这台设备也和 B 配过对：给一个去 B 的设备列表的入口
    const link = within(dialog).getByRole('link', { name: 'Open the device list of beta' })
    expect(link.getAttribute('href')).toMatch(/^\/n\/.+\/settings\?tab=devices$/)
    expect(calls('twin.unlink')).toHaveLength(0)
    await userEvent.click(within(dialog).getByTestId('twin-link-remove-confirm'))
    await waitFor(() => expect(calls('twin.unlink')).toHaveLength(1))
    expect(calls('twin.unlink')[0]).toMatchObject({ nodeId: A, params: { node_id: B } })
    expect(await screen.findByTestId('twin-link-revoke')).toHaveTextContent('revoke this node’s twin device on beta')
    await waitFor(() => expect(screen.queryByTestId('twin-link')).not.toBeInTheDocument())
  })
  it('without a pairing with the other node there is no link, only the instruction', async () => {
    daemon.links = [twinLink({ node_id: 'f'.repeat(32), name: 'faraway' })]
    await renderLinks()
    await userEvent.click(await screen.findByTestId('twin-link-remove'))
    const dialog = await screen.findByTestId('twin-link-remove-dialog')
    expect(within(dialog).queryByRole('link')).not.toBeInTheDocument()
    expect(within(dialog).getByTestId('twin-link-revoke-hint')).toHaveTextContent('faraway')
    await userEvent.click(within(dialog).getByTestId('twin-link-remove-confirm'))
    expect(await screen.findByTestId('twin-link-revoke')).toHaveTextContent('This device is not paired with faraway')
  })
  it('keeps the dialog and shows the error when the unlink fails', async () => {
    daemon.links = [twinLink({ node_id: B })]
    rpc.handlers['twin.unlink'] = () => {
      throw rpcError('not_found')
    }
    await renderLinks()
    await userEvent.click(await screen.findByTestId('twin-link-remove'))
    await userEvent.click(await screen.findByTestId('twin-link-remove-confirm'))
    expect(await within(screen.getByTestId('twin-link-remove-dialog')).findByRole('alert')).toBeInTheDocument()
  })
})
