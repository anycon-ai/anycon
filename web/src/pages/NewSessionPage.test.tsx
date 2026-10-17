import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mockAgent, nodeEntry, project } from '@/test/factories'
import { mockRouter } from '@/test/routerMock'
import { useNodes } from '@/stores/nodes'

const navigate = vi.hoisted(() => vi.fn())
vi.mock('@tanstack/react-router', async (orig) => mockRouter(await orig<object>(), navigate, { node: 'nnnnnnnn' }))

import { NewSessionPage } from './NewSessionPage'

const NODE = 'n'.repeat(32)

beforeEach(() => {
  navigate.mockReset()
  useNodes.setState({
    byId: {
      [NODE]: nodeEntry({
        id: NODE,
        agents: [mockAgent()],
        projects: [project({ name: 'ok' }), project({ name: 'gone', kind: 'linked', missing: true, modified_at_ms: 0 })],
      }),
    },
    order: [NODE],
    loaded: true,
  })
})

describe('NewSessionPage', () => {
  it('lists a project whose linked folder is missing as disabled, with the reason', async () => {
    const user = userEvent.setup()
    render(<NewSessionPage />)
    expect(screen.getByTestId('new-session-form')).toBeInTheDocument()
    expect(screen.getByTestId('new-session-heading')).toBeInTheDocument()
    // 项目是输入框顶上上下文栏里的下拉：临时会话 + 各个项目，目录不在了的禁用并说明原因
    const trigger = screen.getByTestId('new-session-project')
    expect(screen.getByTestId('composer-context')).toContainElement(trigger)
    expect(screen.getByTestId('option-controls')).not.toContainElement(trigger)
    expect(trigger).toHaveAttribute('data-value', '')
    await user.click(trigger)
    const missing = await screen.findByTestId('new-session-project-gone')
    expect(missing).toHaveTextContent('gone (folder not found)')
    expect(missing).toHaveAttribute('data-disabled')
    expect(missing).toHaveAttribute('aria-disabled', 'true')
    expect(screen.getByTestId('new-session-project-ok')).not.toHaveAttribute('data-disabled')
    expect(screen.getByTestId('new-session-project-scratch')).toBeInTheDocument()
  })

  it('choosing a project navigates to the new-session route of that project', async () => {
    const user = userEvent.setup()
    render(<NewSessionPage />)
    await user.click(screen.getByTestId('new-session-project'))
    await user.click(await screen.findByTestId('new-session-project-ok'))
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ to: '/n/$node/new', search: expect.objectContaining({ project: 'ok' }) }))
  })

  it('shows the selected project in the picker and in the heading', () => {
    render(<NewSessionPage project="ok" />)
    expect(screen.getByTestId('new-session-project')).toHaveAttribute('data-value', 'ok')
    expect(screen.getByTestId('new-session-heading')).toHaveTextContent('ok')
  })

  it('does not offer the form for a missing project', () => {
    render(<NewSessionPage project="gone" />)
    expect(screen.getByTestId('project-missing-notice')).toBeInTheDocument()
    expect(screen.queryByTestId('new-session-form')).toBeNull()
  })
})
