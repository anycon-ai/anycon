import { afterEach, describe, expect, it, vi } from 'vitest'
import { copyText } from './clipboard'

const original = Object.getOwnPropertyDescriptor(navigator, 'clipboard')

function setClipboard(value: unknown): void {
  Object.defineProperty(navigator, 'clipboard', { value, configurable: true })
}

function stubExecCommand(result: boolean): { copied: () => string | undefined } {
  let copied: string | undefined
  const exec = vi.fn((cmd: string) => {
    if (cmd === 'copy' && document.activeElement instanceof HTMLTextAreaElement) copied = document.activeElement.value
    return result
  })
  Object.defineProperty(document, 'execCommand', { value: exec, configurable: true })
  return { copied: () => copied }
}

afterEach(() => {
  if (original) Object.defineProperty(navigator, 'clipboard', original)
  else Reflect.deleteProperty(navigator, 'clipboard')
  Reflect.deleteProperty(document, 'execCommand')
})

describe('copyText', () => {
  it('uses navigator.clipboard when there is one', async () => {
    const writeText = vi.fn(() => Promise.resolve())
    setClipboard({ writeText })
    await copyText('hi')
    expect(writeText).toHaveBeenCalledWith('hi')
  })

  it('falls back to execCommand when the page is not a secure context (no navigator.clipboard), and cleans up', async () => {
    setClipboard(undefined)
    const exec = stubExecCommand(true)
    const before = document.createElement('button')
    document.body.appendChild(before)
    before.focus()
    await copyText('ls -la')
    expect(exec.copied()).toBe('ls -la')
    expect(document.querySelector('textarea')).toBeNull()
    expect(document.activeElement).toBe(before)
    before.remove()
  })

  it('falls back to execCommand when writeText is refused', async () => {
    setClipboard({ writeText: () => Promise.reject(new Error('denied')) })
    const exec = stubExecCommand(true)
    await copyText('x')
    expect(exec.copied()).toBe('x')
  })

  it('rejects when neither way works', async () => {
    setClipboard(undefined)
    stubExecCommand(false)
    await expect(copyText('x')).rejects.toThrow()
  })
})
