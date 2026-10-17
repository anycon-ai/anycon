import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { toast, useUi } from '@/stores/ui'
import { Toaster } from './Toaster'

beforeEach(() => useUi.setState({ toasts: [] }))

describe('Toaster', () => {
  it('shows a toast with an action button; clicking it runs the action and dismisses the toast', async () => {
    const run = vi.fn()
    const user = userEvent.setup()
    render(<Toaster />)
    act(() => toast.info('Password unlock is locked.', { label: 'Review', run }))
    expect(screen.getByText('Password unlock is locked.')).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Review' }))
    expect(run).toHaveBeenCalledTimes(1)
    expect(useUi.getState().toasts).toHaveLength(0)
  })

  it('plain toasts have no action button', () => {
    render(<Toaster />)
    act(() => toast.success('Saved'))
    expect(screen.queryByRole('button', { name: 'Review' })).toBeNull()
  })
})
