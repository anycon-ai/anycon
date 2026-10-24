// 会话页的「预览」（ADR-39，web.md § 13.1）：什么时候有这个分段、选了之后主区换成预览视图、选择只对当前会话有效。
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Event, SessionSummary } from '@/protocol'
import { resetDb } from '@/db'
import { clearTickets } from '@/features/preview/ticket'
import { ev, gitProject, gitStatus, mockAgent, nodeEntry, nodeInfo, project, resetSeq, summary, worktree } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { installFakeApi } from '@/test/fakeApi'
import { openResult, PREVIEW_NODE, PREVIEW_TEMPLATE, previewNodeInfo, previewPort, seedDeviceToken } from '@/test/preview'
import { mockRouter } from '@/test/routerMock'
import { setRpc } from '@/transport/instance'
import { resetDrafts } from '@/stores/drafts'
import { useGit } from '@/stores/git'
import { useNodes } from '@/stores/nodes'
import { usePrefs } from '@/stores/prefs'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'

const h = vi.hoisted(() => ({ navigate: vi.fn(), template: null as string | null }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, { node: 'bbbbbbbb', session: 'sess_a' }))
// hub 的 /api/config 在 react-query 里；这里直接给预览 origin 模板（null = hub 没配预览）
vi.mock('@/features/preview/availability', async (orig) => ({ ...(await orig<object>()), usePreviewTemplate: () => h.template }))
// 不真的去 daemon 拉会话：事件直接放进 store
vi.mock('@/features/session/actions', async (orig) => ({
  ...(await orig<object>()),
  openSession: vi.fn(async () => {}),
  closeSession: vi.fn(async () => {}),
}))

import { SessionPage } from './SessionPage'

const NODE = PREVIEW_NODE
const SID = 'sess_a'
let rpc: FakeRpc

interface Seed {
  summary?: Partial<SessionSummary>
  events?: Event[]
  git?: boolean
  preview?: boolean
  conn?: 'ready' | 'peer_offline'
}

function seed({ summary: over = {}, events = [], git = true, preview = true, conn = 'ready' }: Seed = {}): void {
  rpc.handlers['session.git_status'] = () => gitStatus()
  rpc.handlers['preview.ports'] = () => ({ ports: [previewPort(5173, { process: 'vite' })], detection: 'ok' })
  rpc.handlers['preview.open'] = () => openResult()
  const base = git ? nodeInfo() : nodeInfo({ capabilities: { audit: true, blobs: false, fs: true, search: false, workspace: true }, worktree: null })
  const info = preview ? { ...base, capabilities: { ...base.capabilities, preview: true } } : base
  useNodes.setState({ byId: { [NODE]: nodeEntry({ id: NODE, agents: [mockAgent()], projects: [git ? gitProject('app', { branch: 'main' }) : project({ name: 'app' })], info, conn, publicKey: new Uint8Array(32).fill(0xab) }) }, order: [NODE], loaded: true })
  useSessions.setState({ list: { [SID]: { ...summary({ id: SID, project: 'app', ...(git ? { worktree: worktree() } : {}), ...over }), node_id: NODE, cached_seq: 0 } }, pending: {}, open: {} })
  useSessions.getState().openStart(NODE, SID, events.map((e) => ev(e)))
  useSessions.getState().openPatch(SID, { phase: 'ready', watching: true })
}

beforeEach(async () => {
  await resetDb()
  await seedDeviceToken()
  resetSeq()
  h.navigate.mockReset()
  h.template = PREVIEW_TEMPLATE
  clearTickets()
  rpc = new FakeRpc()
  setRpc(rpc)
  installFakeApi({ 'POST /api/preview/tokens': () => ({ token: 'hub-token', origin: 'https://x', expires_at_ms: Date.now() + 86_400_000 }) })
  useUi.setState({ toasts: [] })
  useGit.setState({ bySession: {} })
  usePrefs.setState({ infoPanelOpen: true })
  resetDrafts()
})
afterEach(() => setRpc(null))

describe('SessionPage: the Preview switch', () => {
  it('is the third part of the top-bar switch (Chat / Changes / Preview) when the hub and the node both support preview', async () => {
    seed()
    render(<SessionPage sessionId={SID} />)
    const toggle = await screen.findByTestId('main-view')
    expect(toggle).toContainElement(screen.getByTestId('main-view-preview'))
    expect([...toggle.querySelectorAll('[data-testid^="main-view-"]')].map((e) => e.getAttribute('data-testid')).filter((id) => id !== 'main-view-changes-count')).toEqual(['main-view-chat', 'main-view-changes', 'main-view-preview'])
    expect(screen.getByTestId('main-view-preview').querySelector('svg')).not.toBeNull()
    expect(screen.getByTestId('main-view-preview')).toHaveTextContent('Preview')
    expect(toggle.closest('header')).not.toBeNull()
  })

  it('a session that is not in a git repository gets just Chat / Preview', async () => {
    seed({ git: false, summary: { worktree: null } })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('main-view')
    expect(screen.getByTestId('main-view-chat')).toBeInTheDocument()
    expect(screen.getByTestId('main-view-preview')).toBeInTheDocument()
    expect(screen.queryByTestId('main-view-changes')).toBeNull()
  })

  it('is not there when the hub has no preview configured', async () => {
    h.template = null
    seed()
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('main-view')
    expect(screen.queryByTestId('main-view-preview')).toBeNull()
    expect(screen.getByTestId('main-view-changes')).toBeInTheDocument()
  })

  it('is not there when the node does not support preview', async () => {
    seed({ preview: false })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('main-view')
    expect(screen.queryByTestId('main-view-preview')).toBeNull()
  })

  it('without git and without preview there is no switch at all, as before', async () => {
    seed({ git: false, preview: false, summary: { worktree: null } })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('composer-box')
    expect(screen.queryByTestId('main-view')).toBeNull()
  })

  it('a node that only reports capabilities.preview = false does not get it even with the hub configured', async () => {
    seed({ preview: false })
    useNodes.setState({ byId: { [NODE]: { ...useNodes.getState().byId[NODE]!, info: { ...previewNodeInfo(), capabilities: { ...previewNodeInfo().capabilities, preview: false } } } } })
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('main-view')
    expect(screen.queryByTestId('main-view-preview')).toBeNull()
  })
})

describe('SessionPage: the Preview view', () => {
  it('replaces the chat and the input box with the preview, and Chat brings them back', async () => {
    seed()
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await screen.findByTestId('composer-box')
    expect(screen.queryByTestId('preview-view')).toBeNull()
    expect(rpc.methods()).not.toContain('preview.ports')

    await user.click(screen.getByTestId('main-view-preview'))
    expect(await screen.findByTestId('preview-view')).toBeInTheDocument()
    expect(screen.queryByTestId('composer-box')).toBeNull()
    expect(screen.getByTestId('main-view-preview')).toHaveAttribute('data-state', 'on')
    await waitFor(() => expect(rpc.calls.find((c) => c.method === 'preview.ports')?.params).toEqual({ session_id: SID }))
    expect(await screen.findByTestId('preview-frame')).toBeInTheDocument()

    await user.click(screen.getByTestId('main-view-chat'))
    expect(await screen.findByTestId('composer-box')).toBeInTheDocument()
    expect(screen.queryByTestId('preview-view')).toBeNull()
  })

  it('works in a session that is not in a git repository too', async () => {
    seed({ git: false, summary: { worktree: null } })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('main-view-preview'))
    expect(await screen.findByTestId('preview-frame')).toBeInTheDocument()
  })

  it('the choice is only for this session: another session opens on the chat', async () => {
    seed()
    useSessions.setState({ list: { ...useSessions.getState().list, sess_b: { ...summary({ id: 'sess_b', project: 'app' }), node_id: NODE, cached_seq: 0 } } })
    useSessions.getState().openStart(NODE, 'sess_b', [])
    useSessions.getState().openPatch('sess_b', { phase: 'ready', watching: true })
    const user = userEvent.setup()
    const { rerender } = render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('main-view-preview'))
    await screen.findByTestId('preview-view')
    rerender(<SessionPage sessionId="sess_b" />)
    expect(await screen.findByTestId('composer-box')).toBeInTheDocument()
    expect(screen.queryByTestId('preview-view')).toBeNull()
  })

  it('falls back to the chat when the preview stops being available (the hub turned it off)', async () => {
    seed()
    const user = userEvent.setup()
    const { rerender } = render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('main-view-preview'))
    await screen.findByTestId('preview-view')
    h.template = null
    rerender(<SessionPage sessionId={SID} />)
    expect(await screen.findByTestId('composer-box')).toBeInTheDocument()
    expect(screen.queryByTestId('main-view-preview')).toBeNull()
  })

  it('the approval banner stays, and “Review” goes back to the chat', async () => {
    seed({ events: [{ type: 'permission', request_id: 'r1', kind: 'tool', title: 'Run npm test', detail: { tool: { tool: 'Bash', category: 'shell', input: { command: 'npm test' }, detail: { command: 'npm test' } } }, options: [{ id: 'allow', label: 'Allow', kind: 'allow_once' }] }] })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('main-view-preview'))
    await screen.findByTestId('preview-view')
    await user.click(await screen.findByTestId('approval-banner-review'))
    expect(await screen.findByTestId('composer-box')).toBeInTheDocument()
  })

  it('an offline node: the view still opens, says the node is offline and does not ask it for ports', async () => {
    seed({ conn: 'peer_offline' })
    const user = userEvent.setup()
    render(<SessionPage sessionId={SID} />)
    await user.click(await screen.findByTestId('main-view-preview'))
    expect(await screen.findByTestId('preview-offline')).toBeInTheDocument()
    expect(rpc.methods()).not.toContain('preview.ports')
  })
})
