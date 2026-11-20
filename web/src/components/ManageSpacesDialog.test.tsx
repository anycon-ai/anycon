// 管理空间（spec/web.md § 4.13）：给节点设分组、给空间改名；改名只改在线的节点，不在线的列为没改；校验和 daemon 一样；
// 显示 daemon 的 invalid / forbidden（env_override）；改完刷新 node.info，空间切换才会跟着变。
import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, afterEach, describe, expect, it } from 'vitest'
import type { NodeInfo } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { setRpc } from '@/transport/instance'
import { useManageSpaces } from '@/stores/manageSpaces'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { ManageSpacesDialog } from './ManageSpacesDialog'

const HOME = 'h'.repeat(32)
const WORK1 = 'w'.repeat(32)
const WORK2 = 'x'.repeat(32)
const info = (group?: string): NodeInfo => ({ group, name: 'n', capabilities: { todo: true } }) as unknown as NodeInfo

let rpc: FakeRpc
const groups: Record<string, string> = { [HOME]: 'Home', [WORK1]: 'Work', [WORK2]: 'Work' }

beforeEach(() => {
  rpc = new FakeRpc()
  setRpc(rpc)
  Object.assign(groups, { [HOME]: 'Home', [WORK1]: 'Work', [WORK2]: 'Work' })
  rpc.handlers['node.set_config'] = (p, nodeId) => {
    const change = (p as { changes: Array<{ key: string; value: string }> }).changes[0]!
    groups[nodeId] = change.value
    return { items: [] }
  }
  rpc.handlers['node.info'] = (_p, nodeId) => info(groups[nodeId])
  useNodes.setState({
    byId: {
      [HOME]: nodeEntry({ id: HOME, name: 'house', info: info('Home') }),
      [WORK1]: nodeEntry({ id: WORK1, name: 'office', info: info('Work') }),
      [WORK2]: nodeEntry({ id: WORK2, name: 'lab', info: info('Work') }),
    },
    order: [HOME, WORK1, WORK2],
    loaded: true,
  })
  useManageSpaces.setState({ open: true })
})
afterEach(() => {
  setRpc(null)
  useManageSpaces.setState({ open: false })
})

const rowOf = (id: string) => screen.getAllByTestId('manage-node').find((r) => r.getAttribute('data-node') === id)!

describe('setting a node’s group', () => {
  it('sends hub.group to that node, refreshes node.info, and the switcher’s data follows', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    const input = within(rowOf(HOME)).getByTestId('manage-group-input')
    await user.clear(input)
    await user.type(input, 'Garage')
    await user.click(within(rowOf(HOME)).getByTestId('manage-group-save'))
    await waitFor(() => expect(useNodes.getState().byId[HOME]!.info!.group).toBe('Garage'))
    expect(rpc.calls.filter((c) => c.method === 'node.set_config')).toEqual([{ nodeId: HOME, method: 'node.set_config', params: { changes: [{ key: 'hub.group', value: 'Garage' }] } }])
    expect(rpc.methods()).toEqual(['node.set_config', 'node.info'])
    expect(within(rowOf(HOME)).getByTestId('manage-result')).toHaveTextContent('Changed.')
  })

  it('an empty value clears the group (no group)', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    await user.clear(within(rowOf(HOME)).getByTestId('manage-group-input'))
    await user.click(within(rowOf(HOME)).getByTestId('manage-group-save'))
    await waitFor(() => expect(useNodes.getState().byId[HOME]!.info!.group).toBe(''))
    expect(rpc.calls[0]!.params).toEqual({ changes: [{ key: 'hub.group', value: '' }] })
  })

  it('validates like the daemon before sending: too long (41 code points), control characters; nothing is sent', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    const input = within(rowOf(HOME)).getByTestId('manage-group-input')
    await user.clear(input)
    await user.click(input)
    await user.paste('x'.repeat(41))
    expect(within(rowOf(HOME)).getByRole('alert')).toHaveTextContent('too long')
    expect(within(rowOf(HOME)).getByTestId('manage-group-save')).toBeDisabled()
    await user.clear(input)
    await user.click(input)
    await user.paste('a b')
    expect(within(rowOf(HOME)).getByRole('alert')).toHaveTextContent('single line')
    expect(rpc.calls).toEqual([])
  })

  it('shows the daemon’s rejection: invalid, and forbidden / env_override (set by an environment variable)', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    rpc.handlers['node.set_config'] = () => {
      throw rpcError('invalid', 'hub.group', 'bad')
    }
    let input = within(rowOf(HOME)).getByTestId('manage-group-input')
    await user.clear(input)
    await user.type(input, 'X')
    await user.click(within(rowOf(HOME)).getByTestId('manage-group-save'))
    expect(await within(rowOf(HOME)).findByTestId('manage-result')).toHaveTextContent('did not accept')
    rpc.handlers['node.set_config'] = () => {
      throw rpcError('forbidden', 'hub.group', 'env_override')
    }
    input = within(rowOf(HOME)).getByTestId('manage-group-input')
    await user.type(input, 'Y')
    await user.click(within(rowOf(HOME)).getByTestId('manage-group-save'))
    await waitFor(() => expect(within(rowOf(HOME)).getByTestId('manage-result')).toHaveTextContent('environment variable'))
    expect(useNodes.getState().byId[HOME]!.info!.group).toBe('Home') // 没改
  })

  it('an offline node cannot be changed from here and says so', () => {
    useNodes.getState().patch(HOME, { conn: 'peer_offline' })
    render(<ManageSpacesDialog />)
    expect(within(rowOf(HOME)).getByTestId('manage-group-save')).toBeDisabled()
    expect(within(rowOf(HOME)).getByText(/Offline: not changed/)).toBeInTheDocument()
  })
})

describe('renaming a space', () => {
  async function rename(user: ReturnType<typeof userEvent.setup>, from: string, to: string): Promise<void> {
    await user.selectOptions(screen.getByTestId('rename-space'), from)
    await user.type(screen.getByTestId('rename-to'), to)
    await user.click(screen.getByTestId('rename-apply'))
  }

  it('sends the new name to every online node of the space and refreshes their info', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    await rename(user, 'Work', 'Office')
    await waitFor(() => expect(screen.getAllByTestId('rename-results')[0]!.querySelectorAll('li')).toHaveLength(2))
    expect(rpc.calls.filter((c) => c.method === 'node.set_config').map((c) => c.nodeId).sort()).toEqual([WORK1, WORK2].sort())
    await waitFor(() => expect([useNodes.getState().byId[WORK1]!.info!.group, useNodes.getState().byId[WORK2]!.info!.group]).toEqual(['Office', 'Office']))
    expect(useNodes.getState().byId[HOME]!.info!.group).toBe('Home')
  })

  it('with one node offline: the online one is changed, the offline one is listed as not changed — never reported as done — and can be retried when it is back', async () => {
    const user = userEvent.setup()
    useNodes.getState().patch(WORK2, { conn: 'peer_offline' })
    render(<ManageSpacesDialog />)
    await rename(user, 'Work', 'Office')
    const list = await screen.findByTestId('rename-results')
    await waitFor(() => expect(within(list).getAllByRole('listitem')).toHaveLength(2))
    const items = within(list).getAllByRole('listitem')
    expect(items.map((li) => li.getAttribute('data-status')).sort()).toEqual(['changed', 'offline'])
    expect(items.find((li) => li.getAttribute('data-status') === 'offline')).toHaveTextContent('lab: Offline: not changed')
    expect(rpc.calls.filter((c) => c.method === 'node.set_config').map((c) => c.nodeId)).toEqual([WORK1])
    expect(useNodes.getState().byId[WORK2]!.info!.group).toBe('Work')
    // 回来之后再试一次：只剩它没改
    useNodes.getState().patch(WORK2, { conn: 'ready' })
    await user.click(screen.getByTestId('rename-apply'))
    // 「Office」已经是（改成功的那个节点的）空间：再改一次就是并进去，先确认
    expect(screen.getByTestId('merge-confirm')).toBeInTheDocument()
    await user.click(screen.getByTestId('rename-apply'))
    await waitFor(() => expect(useNodes.getState().byId[WORK2]!.info!.group).toBe('Office'))
  })

  it('validates the new name and shows a node’s rejection per node', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    await user.selectOptions(screen.getByTestId('rename-space'), 'Work')
    await user.click(screen.getByTestId('rename-to'))
    await user.paste('x'.repeat(41))
    expect(screen.getByTestId('rename-apply')).toBeDisabled()
    expect(screen.getByRole('alert')).toHaveTextContent('too long')
    await user.clear(screen.getByTestId('rename-to'))
    await user.type(screen.getByTestId('rename-to'), 'Office')
    rpc.handlers['node.set_config'] = (_p, nodeId) => {
      if (nodeId === WORK1) throw rpcError('forbidden', 'hub.group', 'env_override')
      return { items: [] }
    }
    await user.click(screen.getByTestId('rename-apply'))
    const list = await screen.findByTestId('rename-results')
    await waitFor(() => expect(within(list).getAllByRole('listitem')).toHaveLength(2))
    expect(within(list).getByText(/office: .*environment variable/)).toBeInTheDocument()
  })
})

describe('renaming and the current selection', () => {
  async function rename(user: ReturnType<typeof userEvent.setup>, from: string, to: string): Promise<void> {
    await user.selectOptions(screen.getByTestId('rename-space'), from)
    await user.clear(screen.getByTestId('rename-to'))
    await user.type(screen.getByTestId('rename-to'), to)
    await user.click(screen.getByTestId('rename-apply'))
  }

  it('renaming the selected space moves the selection to the new name (it does not fall back to “All”)', async () => {
    const user = userEvent.setup()
    usePrefs.setState({ sidebarSpace: 'Work', sidebarNodes: null })
    render(<ManageSpacesDialog />)
    await rename(user, 'Work', 'Office')
    await waitFor(() => expect(usePrefs.getState().sidebarSpace).toBe('Office'))
  })

  it('renaming a space that is not the selected one leaves the selection alone', async () => {
    const user = userEvent.setup()
    usePrefs.setState({ sidebarSpace: 'Home', sidebarNodes: null })
    render(<ManageSpacesDialog />)
    await rename(user, 'Work', 'Office')
    await waitFor(() => expect(useNodes.getState().byId[WORK1]!.info!.group).toBe('Office'))
    expect(usePrefs.getState().sidebarSpace).toBe('Home')
  })

  it('a partial rename: the selection follows the renamed nodes and the dialog says which nodes are still under the old name', async () => {
    const user = userEvent.setup()
    useNodes.getState().patch(WORK2, { conn: 'peer_offline' })
    usePrefs.setState({ sidebarSpace: 'Work', sidebarNodes: null })
    render(<ManageSpacesDialog />)
    await rename(user, 'Work', 'Office')
    await waitFor(() => expect(usePrefs.getState().sidebarSpace).toBe('Office'))
    expect(await screen.findByTestId('rename-note')).toHaveTextContent('Still under the old name “Work”: lab')
  })

  it('nothing renamed (everything offline or refused): the selection is left alone', async () => {
    const user = userEvent.setup()
    useNodes.getState().patch(WORK1, { conn: 'peer_offline' })
    useNodes.getState().patch(WORK2, { conn: 'peer_offline' })
    usePrefs.setState({ sidebarSpace: 'Work' })
    render(<ManageSpacesDialog />)
    await rename(user, 'Work', 'Office')
    await screen.findByTestId('rename-results')
    expect(usePrefs.getState().sidebarSpace).toBe('Work')
  })

  it('renaming into the name of another existing space asks first, naming who ends up where; nothing is sent until it is confirmed', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    await user.selectOptions(screen.getByTestId('rename-space'), 'Work')
    await user.type(screen.getByTestId('rename-to'), 'Home')
    await user.click(screen.getByTestId('rename-apply'))
    expect(screen.getByTestId('merge-confirm')).toHaveTextContent('merges “Work” into “Home”: office, lab, house will all be in “Home”')
    expect(rpc.calls).toEqual([])
    expect(screen.getByTestId('rename-apply')).toHaveTextContent('Merge')
    await user.click(screen.getByTestId('rename-apply'))
    await waitFor(() => expect([useNodes.getState().byId[WORK1]!.info!.group, useNodes.getState().byId[WORK2]!.info!.group]).toEqual(['Home', 'Home']))
  })

  it('changing the typed name takes the merge confirmation back', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    await user.selectOptions(screen.getByTestId('rename-space'), 'Work')
    await user.type(screen.getByTestId('rename-to'), 'Home')
    await user.click(screen.getByTestId('rename-apply'))
    expect(screen.getByTestId('merge-confirm')).toBeInTheDocument()
    await user.type(screen.getByTestId('rename-to'), 's')
    expect(screen.queryByTestId('merge-confirm')).toBeNull()
  })

  it('a space that disappeared while the dialog was open says so instead of doing nothing', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    await user.selectOptions(screen.getByTestId('rename-space'), 'Work')
    await user.type(screen.getByTestId('rename-to'), 'Office')
    // 别的设备把 Work 的两个节点都改到 Home 去了
    useNodes.getState().setInfo(WORK1, info('Home'))
    useNodes.getState().setInfo(WORK2, info('Home'))
    await user.click(screen.getByTestId('rename-apply'))
    expect(await screen.findByTestId('rename-note')).toHaveTextContent('no longer exists')
    expect(rpc.calls.filter((c) => c.method === 'node.set_config')).toEqual([])
  })
})

describe('telling similar spaces apart', () => {
  it('lists each space with the names of its nodes, so identical-looking names can be told apart', () => {
    render(<ManageSpacesDialog />)
    const items = screen.getAllByTestId('space-overview-item').map((li) => li.textContent)
    expect(items).toEqual([expect.stringContaining('Home: house'), expect.stringContaining('Work: office, lab')])
  })

  it('a name with invisible characters is flagged, and the suggestion shows the cleaned text while its value stays the raw name', () => {
    useNodes.getState().setInfo(HOME, info(`Ho${String.fromCharCode(0x200b)}me`))
    const { container } = render(<ManageSpacesDialog />)
    expect(screen.getAllByTestId('space-overview-odd')).toHaveLength(1)
    const option = [...document.querySelectorAll('datalist option')].find((o) => o.getAttribute('value')?.includes(String.fromCharCode(0x200b)))!
    expect(option.getAttribute('label')).toBe('Home')
    void container
  })
})

describe('renaming into a name that only looks like an existing space', () => {
  const ZWSP = String.fromCharCode(0x200b)

  it('warns that it would create a second space that looks the same, and offers the existing name (which then goes through the merge confirmation)', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    await user.selectOptions(screen.getByTestId('rename-space'), 'Work')
    await user.type(screen.getByTestId('rename-to'), `Ho${ZWSP}me`)
    const warn = await screen.findByTestId('lookalike-warning')
    expect(warn).toHaveTextContent('looks the same as the existing space “Home”')
    await user.click(screen.getByTestId('lookalike-use-existing'))
    expect(screen.getByTestId('rename-to')).toHaveValue('Home')
    expect(screen.queryByTestId('lookalike-warning')).toBeNull()
    await user.click(screen.getByTestId('rename-apply'))
    expect(screen.getByTestId('merge-confirm')).toHaveTextContent('merges “Work” into “Home”')
    expect(rpc.calls).toEqual([])
  })

  it('an unrelated new name, and the exact name of an existing space, show no look-alike warning', async () => {
    const user = userEvent.setup()
    render(<ManageSpacesDialog />)
    await user.selectOptions(screen.getByTestId('rename-space'), 'Work')
    await user.type(screen.getByTestId('rename-to'), 'Office')
    expect(screen.queryByTestId('lookalike-warning')).toBeNull()
    await user.clear(screen.getByTestId('rename-to'))
    await user.type(screen.getByTestId('rename-to'), 'Home')
    expect(screen.queryByTestId('lookalike-warning')).toBeNull()
  })

  it('the odd-name flag also fires for an unusual encoding or spacing, and its text says so', () => {
    useNodes.getState().setInfo(HOME, info('Cafe\u0301 Bar'))
    render(<ManageSpacesDialog />)
    expect(screen.getAllByTestId('space-overview-odd')[0]).toHaveTextContent(/unusual spacing or a non-standard encoding/)
  })
})
