import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { SignOutDialog } from './SignOutDialog'

describe('SignOutDialog', () => {
  it('says that the device key and cached sessions stay in this browser, and offers the remove-device alternative', async () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined)
    const onRemoveInstead = vi.fn()
    const user = userEvent.setup()
    render(<SignOutDialog open onOpenChange={() => {}} onConfirm={onConfirm} onRemoveInstead={onRemoveInstead} />)
    expect(screen.getByRole('dialog')).toHaveTextContent(/keeps its device key|设备密钥/)
    // 退出登录也会关掉本浏览器的推送，确认框里要讲清楚
    expect(screen.getByRole('dialog')).toHaveTextContent(/Push notifications for this browser are turned off|推送通知会关掉/)
    await user.click(screen.getByTestId('logout-remove-instead'))
    expect(onRemoveInstead).toHaveBeenCalled()
    expect(onConfirm).not.toHaveBeenCalled()
    await user.click(screen.getByTestId('logout-confirm'))
    await waitFor(() => expect(onConfirm).toHaveBeenCalled())
  })
})
