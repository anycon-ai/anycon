import { useState } from 'react'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { resolveDefaults, type Selection, type SettingsField } from '@/features/settings/resolve'
import type { ConfigOption } from '@/protocol'
import { mockAgent } from '@/test/factories'
import { SettingsControls } from './SettingsControls'

function Harness({ onChange, initial, ...rest }: { onChange?: (s: Selection) => void; initial?: Partial<Selection>; dangerousForbidden?: boolean; disabled?: boolean; pending?: ReadonlySet<SettingsField> }) {
  const agent = mockAgent()
  const [sel, setSel] = useState<Selection>({ ...resolveDefaults(agent, []), ...initial })
  return (
    <SettingsControls
      agent={agent}
      value={sel}
      onChange={(n) => {
        setSel(n)
        onChange?.(n)
      }}
      customKey="n:mock"
      idPrefix="option"
      {...rest}
    />
  )
}

describe('SettingsControls', () => {
  it('marks queued (not yet applied) controls in a subtler color with a tooltip, without adding any line', () => {
    const { container } = render(<Harness pending={new Set<SettingsField>(['model'])} />)
    const model = screen.getByTestId('option-model')
    expect(model).toHaveAttribute('data-pending', 'true')
    expect(model).toHaveClass('text-fg-subtle')
    expect(model).toHaveAttribute('title', 'Change will apply when this turn ends.')
    expect(screen.getByTestId('option-thinking')).not.toHaveAttribute('data-pending')
    expect(screen.getByTestId('option-thinking')).not.toHaveClass('text-fg-subtle')
    expect(container.querySelector('p')).toBeNull()
  })
  it('the offline reason wins over the queued hint', () => {
    render(<Harness disabled pending={new Set<SettingsField>(['model'])} />)
    expect(screen.getByTestId('option-model')).not.toHaveAttribute('data-pending')
  })
  it('shows model, thinking (for the selected model), mode and more-options, all driven by AgentInfo', async () => {
    const user = userEvent.setup()
    render(<Harness />)
    expect(screen.getByTestId('option-model')).toHaveTextContent('Mock Large')
    expect(screen.getByTestId('option-thinking')).toHaveAttribute('data-value', 'high')
    expect(screen.getByTestId('option-mode')).toHaveTextContent('Ask')
    expect(screen.getByTestId('option-more')).toBeInTheDocument()
    // 思考等级是下拉：打开之后才有各个等级，当前的那个是选中状态
    expect(screen.queryByTestId('option-thinking-low')).toBeNull()
    await user.click(screen.getByTestId('option-thinking'))
    expect(await screen.findByTestId('option-thinking-low')).toHaveAttribute('aria-checked', 'false')
    expect(screen.getByTestId('option-thinking-high')).toHaveAttribute('aria-checked', 'true')
  })

  it('hides the thinking control for a model without thinking levels', () => {
    render(<Harness initial={{ model: 'mock-small', thinking: null }} />)
    expect(screen.queryByTestId('option-thinking')).toBeNull()
  })

  it('switching model reconciles the thinking level and reports it', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Harness onChange={onChange} />)
    await user.click(screen.getByTestId('option-model'))
    await user.click(await screen.findByTestId('option-model-mock-small'))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ model: 'mock-small', thinking: null }))
    expect(screen.queryByTestId('option-thinking')).toBeNull()
    await user.click(screen.getByTestId('option-model'))
    await user.click(await screen.findByTestId('option-model-mock-large'))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ model: 'mock-large', thinking: 'high' }))
  })

  it('changes the thinking level via the dropdown', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Harness onChange={onChange} />)
    await user.click(screen.getByTestId('option-thinking'))
    await user.click(await screen.findByTestId('option-thinking-low'))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ thinking: 'low' }))
    expect(screen.getByTestId('option-thinking')).toHaveAttribute('data-value', 'low')
  })

  it('asks for confirmation before enabling a dangerous mode, and shows it in red', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Harness onChange={onChange} />)
    await user.click(screen.getByTestId('option-mode'))
    await user.click(await screen.findByTestId('option-mode-yolo'))
    expect(onChange).not.toHaveBeenCalled()
    expect(await screen.findByTestId('danger-confirm')).toBeInTheDocument()
    await user.click(screen.getByTestId('danger-confirm-submit'))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ mode: 'yolo' }))
    expect(screen.getByTestId('option-mode')).toHaveAttribute('data-risk', 'dangerous')
  })

  it('cancelling the confirmation keeps the previous mode', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Harness onChange={onChange} />)
    await user.click(screen.getByTestId('option-mode'))
    await user.click(await screen.findByTestId('option-mode-yolo'))
    await user.click(await screen.findByRole('button', { name: 'Cancel' }))
    expect(onChange).not.toHaveBeenCalled()
    expect(screen.getByTestId('option-mode')).toHaveAttribute('data-value', 'ask')
  })

  it('applies a safe mode immediately and disables dangerous ones when the node forbids them', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Harness onChange={onChange} dangerousForbidden />)
    await user.click(screen.getByTestId('option-mode'))
    expect(await screen.findByTestId('option-mode-yolo')).toHaveAttribute('aria-disabled', 'true')
    await user.click(screen.getByTestId('option-mode-plan'))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ mode: 'plan' }))
  })

  it('renders generic agent options in the More popover', async () => {
    const onChange = vi.fn()
    const user = userEvent.setup()
    render(<Harness onChange={onChange} />)
    await user.click(screen.getByTestId('option-more'))
    await user.click(await screen.findByRole('switch', { name: 'Verbose' }))
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ options: { verbose: true } }))
  })

  it('disables everything when locked', () => {
    render(<Harness disabled />)
    expect(screen.getByTestId('option-model')).toBeDisabled()
    expect(screen.getByTestId('option-mode')).toBeDisabled()
  })

  describe('text options with choices', () => {
    const advisor: ConfigOption = {
      id: 'advisor_model',
      label: 'Advisor model',
      kind: 'text',
      default: 'off',
      choices: [
        { id: 'off', label: 'Off' },
        { id: 'opus', label: 'Opus' },
      ],
    }
    function Opts({ option, initial, onChange }: { option: ConfigOption; initial?: unknown; onChange?: (s: Selection) => void }) {
      const agent = mockAgent({ options: [option] })
      const [sel, setSel] = useState<Selection>({ ...resolveDefaults(agent, []), options: { [option.id]: initial ?? option.default } })
      return (
        <SettingsControls
          agent={agent}
          value={sel}
          onChange={(n) => {
            setSel(n)
            onChange?.(n)
          }}
          customKey="n:mock"
          idPrefix="option"
        />
      )
    }
    async function openMore(user: ReturnType<typeof userEvent.setup>) {
      await user.click(screen.getByTestId('option-more'))
      return screen.findByLabelText('Advisor model')
    }

    it('renders a pick list and reports the chosen id', async () => {
      const onChange = vi.fn()
      const user = userEvent.setup()
      render(<Opts option={advisor} onChange={onChange} />)
      const select = await openMore(user)
      expect(select.tagName).toBe('SELECT')
      expect([...select.querySelectorAll('option')].map((o) => o.textContent)).toEqual(['Off', 'Opus'])
      await user.selectOptions(select, 'opus')
      expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ options: expect.objectContaining({ advisor_model: 'opus' }) }))
    })

    it('keeps a stored value that is no longer in the list, shown and selected', async () => {
      const user = userEvent.setup()
      render(<Opts option={advisor} initial="claude-gone-1" />)
      const select = (await openMore(user)) as HTMLSelectElement
      expect(select.value).toBe('claude-gone-1')
      expect([...select.querySelectorAll('option')].map((o) => o.textContent)).toEqual(['claude-gone-1', 'Off', 'Opus'])
    })

    it('stays a plain input without choices', async () => {
      const user = userEvent.setup()
      render(<Opts option={{ ...advisor, choices: [] }} />)
      const input = await openMore(user)
      expect(input.tagName).toBe('INPUT')
    })
  })
})
