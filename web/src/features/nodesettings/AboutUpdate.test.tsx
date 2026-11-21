// 节点设置「关于」里的「更新」一行（ADR-57）：节点的更新检查知道有更新的版本时才显示，链接只能是那个版本的 GitHub 发布页。
import { render, screen } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { NodeUpdateAvailable } from '@/protocol'
import { nodeEntry, nodeInfo } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'

const h = vi.hoisted(() => ({ navigate: vi.fn() }))
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), h.navigate, { node: 'nnnnnnnn' }))
vi.mock('@/transport/instance', () => ({ getClient: () => ({}), rpc: () => ({}) }))

import { AboutTab } from './LimitsAndAbout'

const NODE = 'n'.repeat(32)

function mount(update?: NodeUpdateAvailable) {
  // 节点离线：不去取 node.stats，只看 node.info 里已有的内容
  useNodes.setState({
    byId: { [NODE]: nodeEntry({ id: NODE, conn: 'peer_offline', info: nodeInfo({ version: '0.3.1', update_available: update }) }) },
    order: [NODE],
    loaded: true,
  })
  return render(
    <QueryClientProvider client={new QueryClient()}>
      <AboutTab nodeId={NODE} />
    </QueryClientProvider>,
  )
}

const good: NodeUpdateAvailable = { latest: '0.4.0', url: 'https://github.com/anycon-ai/anycon/releases/tag/v0.4.0', checked_at_ms: 1 }

beforeEach(() => {
  h.navigate.mockReset()
})

describe('about: update notice', () => {
  it('shows the newer version and links to its release page', () => {
    mount(good)
    expect(screen.getByTestId('node-update')).toHaveTextContent('v0.4.0 is available.')
    const link = screen.getByTestId('node-update-link')
    expect(link).toHaveAttribute('href', 'https://github.com/anycon-ai/anycon/releases/tag/v0.4.0')
    expect(link).toHaveAttribute('target', '_blank')
    expect(link).toHaveAttribute('rel', 'noopener noreferrer')
  })

  it('shows nothing when the node knows of no newer version', () => {
    mount(undefined)
    expect(screen.queryByTestId('node-update')).toBeNull()
    // 以前的占位按钮「检查更新」不再有
    expect(screen.queryByText('Check for updates')).toBeNull()
  })

  it('ignores a notice whose version or link is not what the daemon would build', () => {
    for (const bad of [
      { ...good, url: 'https://evil.example/releases/tag/v0.4.0' },
      { ...good, url: 'https://github.com/anycon-ai/anycon/releases/tag/v0.5.0' },
      { ...good, latest: '0.4.0 <b>now</b>', url: 'https://github.com/anycon-ai/anycon/releases/tag/v0.4.0 <b>now</b>' },
      { ...good, latest: 'v0.4.0', url: 'https://github.com/anycon-ai/anycon/releases/tag/vv0.4.0' },
    ]) {
      const { unmount } = mount(bad)
      expect(screen.queryByTestId('node-update')).toBeNull()
      unmount()
    }
  })
})
