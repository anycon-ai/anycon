import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { ApprovalBanner } from './ApprovalBanner'

describe('ApprovalBanner (phone approval banner)', () => {
  it('names the request, is phone-only (md:hidden) and only offers “Review”, never a one-tap allow', async () => {
    const onReview = vi.fn()
    const user = userEvent.setup()
    render(<ApprovalBanner count={1} title="Run rm -rf build" onReview={onReview} />)
    const banner = screen.getByTestId('approval-banner')
    expect(banner).toHaveTextContent('Run rm -rf build')
    expect(banner.className).toContain('md:hidden')
    expect(screen.getAllByRole('button')).toHaveLength(1)
    await user.click(screen.getByTestId('approval-banner-review'))
    expect(onReview).toHaveBeenCalled()
  })

  it('summarises several waiting requests', () => {
    render(<ApprovalBanner count={3} title="x" onReview={() => {}} />)
    expect(screen.getByTestId('approval-banner')).toHaveTextContent('3')
  })
})
