// 新建会话时带图片（ADR-35）：输入框里的「＋」菜单（添加文件或照片）跟着选中的 agent 走；图片随第一条消息发出（可以没有文字）；创建失败时文字和图片都放回输入框。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AgentInfo, SessionCreateParams } from '@/protocol'
import { resetDb } from '@/db'
import { resetBlobCache } from '@/features/attachments/cache'
import { setImageCodec, type ImageCodec } from '@/features/attachments/normalize'
import { blobId, installBlobDaemon } from '@/test/blobDaemon'
import { gitProject, mockAgent, nodeEntry, nodeInfo, resetSeq, summary } from '@/test/factories'
import { FakeRpc, rpcError } from '@/test/fakeRpc'
import { RpcCallError } from '@/transport/rpcClient'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts, useDrafts } from '@/stores/drafts'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { NewSessionForm } from './NewSessionForm'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

const NODE = 'n'.repeat(32)
const DRAFT = `new:${NODE}:app`
const PNG_HEAD = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
let rpc: FakeRpc

const codec: ImageCodec = { decode: async () => ({ width: 40, height: 30, close: () => {} }), encode: async () => null }
const png = (name = 'shot.png', size = 1500): File => {
  const bytes = new Uint8Array(size)
  bytes.set(PNG_HEAD)
  return new File([bytes], name, { type: 'image/png' })
}
const caps = (attachments?: string[]) => ({ env: false, interject: false, interrupt: true, resume: true, rewind: false, structured_output: false, thinking_stream: true, usage: true, attachments })

function mount(agents: AgentInfo[] = [mockAgent({ capabilities: caps(['image/png']) })], blobs = true, conn: 'ready' | 'peer_offline' = 'ready') {
  const info = nodeInfo({ capabilities: { audit: true, blobs, fs: true, search: false, workspace: true }, worktree: null })
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents, projects: [gitProject('app')], info, conn }) }, order: [NODE], loaded: true })
  return render(<NewSessionForm nodeId={NODE} project="app" />)
}
const createParams = () => rpc.calls.find((c) => c.method === 'session.create')?.params as SessionCreateParams | undefined
/** 在 agent 下拉里选一个（下拉只在有多个可用 agent 时才有）。 */
async function pickAgent(id: string) {
  await userEvent.click(screen.getByTestId('new-session-agent'))
  await userEvent.click(await screen.findByTestId(`new-session-agent-${id}`))
  await waitFor(() => expect(screen.getByTestId('new-session-agent')).toHaveAttribute('data-value', id))
}

beforeEach(async () => {
  resetSeq()
  navigate.mockReset()
  await resetDb()
  resetBlobCache()
  resetDrafts()
  rpc = new FakeRpc()
  rpc.handlers['session.create'] = () => summary({ id: 's1', project: 'app' })
  rpc.handlers['workspace.branches'] = () => ({ current: 'main', default_base: 'main', local: ['main'], remote: [] })
  setRpc(rpc)
  setImageCodec(codec)
  usePrefs.setState({ lastUsed: {}, lastWorkspace: {}, customModels: {}, enterBehavior: 'send' })
  useSessions.setState({ list: {}, pending: {}, open: {} })
  useUi.setState({ toasts: [] })
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})
afterEach(() => {
  setRpc(null)
  setImageCodec(null)
})

describe('new-session form: images', () => {
  it('offers the “+” menu (add files or photos) only when the node has blobs and the selected agent takes images; it follows the agent selection', async () => {
    mount([mockAgent({ capabilities: caps(['image/png']) }), mockAgent({ id: 'plain', label: 'Plain', capabilities: caps(undefined) })])
    expect(screen.getByTestId('composer-plus')).toBeEnabled()
    expect(screen.getByTestId('composer-file-input')).toBeEnabled()
    await userEvent.click(screen.getByTestId('composer-plus'))
    expect(await screen.findByTestId('composer-attach')).not.toHaveAttribute('data-disabled')
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(screen.queryByTestId('composer-attach')).toBeNull())
    await pickAgent('plain')
    expect(screen.queryByTestId('composer-plus')).toBeNull()
    expect(screen.queryByTestId('composer-file-input')).toBeNull()
    await pickAgent('mock')
    expect(screen.getByTestId('composer-plus')).toBeEnabled()
    expect(screen.getByTestId('composer-file-input')).toBeInTheDocument()
  })

  it('no “+” menu on a node without the blobs capability; “Add files or photos” is disabled with a reason when the node is offline', async () => {
    const { unmount } = mount(undefined, false)
    expect(screen.queryByTestId('composer-plus')).toBeNull()
    expect(screen.queryByTestId('composer-file-input')).toBeNull()
    unmount()
    mount(undefined, true, 'peer_offline')
    // 「＋」本身始终能点开；离线时里面的「添加文件或照片」禁用并写明原因，选文件的 input 也禁用
    expect(screen.getByTestId('composer-plus')).toBeEnabled()
    expect(screen.getByTestId('composer-file-input')).toBeDisabled()
    await userEvent.click(screen.getByTestId('composer-plus'))
    const item = await screen.findByTestId('composer-attach')
    expect(item).toHaveAttribute('data-disabled')
    expect(item).toHaveAttribute('aria-disabled', 'true')
    expect(item).toHaveTextContent("The node is offline. Images can't be attached right now.")
  })

  it('creates the session with the first message’s images — and works with no text at all', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    mount()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png())
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    await userEvent.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(createParams()).toBeDefined())
    const p = createParams()!
    expect(p).toMatchObject({ project: 'app', agent: 'mock', attachments: [{ blob_id: blobId(1), name: 'shot.png', mime: 'image/png', bytes: 1500, width: 40, height: 30 }] })
    expect(p.prompt).toBeUndefined()
    expect(typeof p.message_id).toBe('string')
    await waitFor(() => expect(navigate).toHaveBeenCalled())
    expect(screen.queryAllByTestId('composer-attachment')).toEqual([])
  })

  it('text and images together', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    mount()
    await userEvent.upload(screen.getByTestId('composer-file-input'), [png('a.png'), png('b.png')])
    await waitFor(() => expect(screen.getAllByTestId('composer-attachment').map((e) => e.getAttribute('data-state'))).toEqual(['ready', 'ready']))
    await userEvent.type(screen.getByTestId('composer-input'), 'fix this{Enter}')
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()).toMatchObject({ prompt: 'fix this', attachments: [{ name: 'a.png' }, { name: 'b.png' }] })
  })

  it('when creating fails, the text and the images go back into the box', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    rpc.handlers['session.create'] = () => {
      throw rpcError('node_busy')
    }
    mount()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('keep.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    await userEvent.type(screen.getByTestId('composer-input'), 'please{Enter}')
    await waitFor(() => expect(screen.getByTestId('composer-input')).toHaveValue('please'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-name', 'keep.png'))
    expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready')
    expect(useDrafts.getState().byKey[DRAFT]!.attachments).toMatchObject([{ blob_id: blobId(1) }])
    expect(navigate).not.toHaveBeenCalled()
  })

  it('when the node says an attachment is gone, it says so and re-uploads the images from the local copy', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    rpc.handlers['session.create'] = () => {
      throw rpcError('not_found', undefined, 'blob')
    }
    mount()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('keep.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    await userEvent.type(screen.getByTestId('composer-input'), 'please{Enter}')
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.text)).toContain('An attachment is no longer on the node. Edit the message and add the images again.'))
    await waitFor(() => expect(d.uploads.size).toBe(2))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(useDrafts.getState().byKey[DRAFT]!.attachments).toMatchObject([{ blob_id: blobId(2), name: 'keep.png' }])
  })
  it('shows a hint — and sends no images — after switching to an agent that takes none, and drops the hint when switching back', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    mount([mockAgent({ capabilities: caps(['image/png']) }), mockAgent({ id: 'plain', label: 'Plain', capabilities: caps(undefined) })])
    expect(screen.queryByTestId('composer-attachments-unsupported')).toBeNull()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('keep.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(screen.queryByTestId('composer-attachments-unsupported')).toBeNull()

    await pickAgent('plain')
    expect(screen.getByTestId('composer-attachments-unsupported')).toHaveTextContent("The selected agent doesn't accept images, so the images you added won't be sent.")
    expect(screen.queryByTestId('composer-plus')).toBeNull()
    await userEvent.type(screen.getByTestId('composer-input'), 'hello{Enter}')
    await waitFor(() => expect(createParams()).toBeDefined())
    expect(createParams()!.attachments).toBeUndefined()
    expect(createParams()).toMatchObject({ agent: 'plain', prompt: 'hello' })
  })

  it('the hint goes away and the images are back when the agent that takes images is selected again', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    mount([mockAgent({ capabilities: caps(['image/png']) }), mockAgent({ id: 'plain', label: 'Plain', capabilities: caps(undefined) })])
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('keep.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    await pickAgent('plain')
    expect(screen.getByTestId('composer-attachments-unsupported')).toBeInTheDocument()
    await pickAgent('mock')
    expect(screen.queryByTestId('composer-attachments-unsupported')).toBeNull()
    expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-name', 'keep.png')
  })

  it('a create that timed out is retried with the same message_id, so the node hands back the session it already made', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    const created: SessionCreateParams[] = []
    let attempt = 0
    rpc.handlers['session.create'] = (p) => {
      created.push(p as SessionCreateParams)
      attempt += 1
      // 第一次：节点其实建好了，只是答复没回来
      if (attempt === 1) throw new RpcCallError('timeout', 'session.create: timed out')
      return summary({ id: 's1', project: 'app' })
    }
    mount()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('keep.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    await userEvent.type(screen.getByTestId('composer-input'), 'please{Enter}')
    await waitFor(() => expect(screen.getByTestId('composer-input')).toHaveValue('please'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(navigate).not.toHaveBeenCalled()

    await userEvent.click(screen.getByTestId('composer-send'))
    await waitFor(() => expect(navigate).toHaveBeenCalled())
    expect(created).toHaveLength(2)
    expect(typeof created[0]!.message_id).toBe('string')
    expect(created[1]!.message_id).toBe(created[0]!.message_id)
    expect(created[1]!.attachments).toEqual(created[0]!.attachments)
    expect(created[1]!.attachments).toMatchObject([{ blob_id: blobId(1) }]) // 没有重新上传
  })

  it('a changed request is a different request: after editing the text the retry gets a new message_id', async () => {
    installBlobDaemon(rpc, { chunkSize: 1024 })
    const created: SessionCreateParams[] = []
    rpc.handlers['session.create'] = (p) => {
      created.push(p as SessionCreateParams)
      if (created.length === 1) throw new RpcCallError('timeout', 'session.create: timed out')
      return summary({ id: 's1', project: 'app' })
    }
    mount()
    await userEvent.type(screen.getByTestId('composer-input'), 'first{Enter}')
    await waitFor(() => expect(screen.getByTestId('composer-input')).toHaveValue('first'))
    await userEvent.type(screen.getByTestId('composer-input'), ' and more{Enter}')
    await waitFor(() => expect(navigate).toHaveBeenCalled())
    expect(created.map((c) => c.prompt)).toEqual(['first', 'first and more'])
    expect(created[1]!.message_id).not.toBe(created[0]!.message_id)
  })

  it('invalid / blob_in_use says the images belong to another session, and the images are re-uploaded from the local copy', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    rpc.handlers['session.create'] = () => {
      throw rpcError('invalid', undefined, 'blob_in_use')
    }
    mount()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('keep.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    await userEvent.type(screen.getByTestId('composer-input'), 'please{Enter}')
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.text)).toContain('These images already belong to another session (for example one that was just created while the connection dropped). Edit the message and add the images again.'))
    await waitFor(() => expect(d.uploads.size).toBe(2))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    expect(useDrafts.getState().byKey[DRAFT]!.attachments).toMatchObject([{ blob_id: blobId(2), name: 'keep.png' }])
  })

  it('invalid / too_large says the images add up to too much, and keeps them in the box to remove some', async () => {
    const d = installBlobDaemon(rpc, { chunkSize: 1024 })
    rpc.handlers['session.create'] = () => {
      throw rpcError('invalid', undefined, 'too_large')
    }
    mount()
    await userEvent.upload(screen.getByTestId('composer-file-input'), png('keep.png'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-state', 'ready'))
    await userEvent.type(screen.getByTestId('composer-input'), 'please{Enter}')
    await waitFor(() => expect(useUi.getState().toasts.map((t) => t.text)).toContain('The images on this message add up to more than the node accepts. Edit the message and remove some.'))
    await waitFor(() => expect(screen.getByTestId('composer-attachment')).toHaveAttribute('data-name', 'keep.png'))
    expect(d.uploads.size).toBe(1) // 不是丢了的问题，不重传
    expect(useDrafts.getState().byKey[DRAFT]!.attachments).toMatchObject([{ blob_id: blobId(1) }])
  })
})
