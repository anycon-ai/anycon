import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { RemoveDeviceError } from '@/app/removeDevice'
import { useNodes } from '@/stores/nodes'

const signOutAndRemoveDevice = vi.fn()
vi.mock('@/app/session', () => ({ signOutAndRemoveDevice: (o: unknown) => signOutAndRemoveDevice(o) }))

import { RemoveDeviceDialog } from './RemoveDeviceDialog'

beforeEach(() => {
  signOutAndRemoveDevice.mockReset()
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})

describe('RemoveDeviceDialog', () => {
  it('explains what is deleted and, once done, leaves the page', async () => {
    signOutAndRemoveDevice.mockResolvedValue({ revokedOnNodes: ['n1'], skippedNodes: [] })
    const onDone = vi.fn()
    const user = userEvent.setup()
    render(<RemoveDeviceDialog open onOpenChange={() => {}} onDone={onDone} />)
    expect(screen.getByRole('dialog')).toHaveTextContent(/revokes this browser|撤销这个浏览器/)
    await user.click(screen.getByTestId('remove-device-confirm'))
    await waitFor(() => expect(onDone).toHaveBeenCalled())
    expect(signOutAndRemoveDevice).toHaveBeenCalledWith({ force: false })
  })

  it('shows the error and keeps the local data when the hub refuses; “remove anyway” forces it', async () => {
    signOutAndRemoveDevice.mockRejectedValueOnce(new RemoveDeviceError('hub', new Error('network error')))
    const onDone = vi.fn()
    const user = userEvent.setup()
    render(<RemoveDeviceDialog open onOpenChange={() => {}} onDone={onDone} />)
    await user.click(screen.getByTestId('remove-device-confirm'))
    expect(await screen.findByTestId('remove-device-error')).toHaveTextContent('network error')
    expect(onDone).not.toHaveBeenCalled()
    signOutAndRemoveDevice.mockResolvedValueOnce({ revokedOnNodes: [], skippedNodes: [] })
    await user.click(screen.getByTestId('remove-device-force'))
    await waitFor(() => expect(onDone).toHaveBeenCalled())
    expect(signOutAndRemoveDevice).toHaveBeenLastCalledWith({ force: true })
  })

  it('names the nodes that could not revoke and keeps everything', async () => {
    useNodes.setState({ byId: { n1: { id: 'n1', name: 'mac-mini' } as never }, order: ['n1'] })
    signOutAndRemoveDevice.mockRejectedValueOnce(new RemoveDeviceError('nodes', new Error('offline'), ['n1']))
    const user = userEvent.setup()
    render(<RemoveDeviceDialog open onOpenChange={() => {}} onDone={() => {}} />)
    await user.click(screen.getByTestId('remove-device-confirm'))
    expect(await screen.findByTestId('remove-device-error')).toHaveTextContent('mac-mini')
    expect(screen.getByTestId('remove-device-force')).toBeInTheDocument()
  })

  it('tells the user about nodes that were skipped (force) before leaving', async () => {
    signOutAndRemoveDevice.mockResolvedValue({ revokedOnNodes: [], skippedNodes: ['n1', 'n2'] })
    const onDone = vi.fn()
    const user = userEvent.setup()
    render(<RemoveDeviceDialog open onOpenChange={() => {}} onDone={onDone} />)
    await user.click(screen.getByTestId('remove-device-confirm'))
    expect(await screen.findByTestId('remove-device-report')).toHaveTextContent('2')
    expect(onDone).not.toHaveBeenCalled()
    await user.click(screen.getByTestId('remove-device-continue'))
    expect(onDone).toHaveBeenCalled()
  })
})
