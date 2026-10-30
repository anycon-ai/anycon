import { describe, expect, it } from 'vitest'
import { mockAgent, nodeInfo } from '@/test/factories'
import { acceptsAttachment, attachmentSupport } from './support'

const info = (blobs: boolean) => nodeInfo({ capabilities: { audit: true, blobs, fs: true, search: false, workspace: true } })
const agent = (attachments?: string[]) => mockAgent({ capabilities: { env: false, interject: false, interrupt: true, resume: true, rewind: false, structured_output: false, thinking_stream: true, usage: true, attachments } })

describe('attachmentSupport', () => {
  it('needs both: a node with blobs and an agent that takes attachments', () => {
    expect(attachmentSupport(info(true), agent(['image/png', 'image/jpeg']))).toEqual({ images: ['image/png', 'image/jpeg'], files: false })
    expect(attachmentSupport(info(false), agent(['image/png']))).toBeNull()
    expect(attachmentSupport(info(false), agent(['*/*']))).toBeNull()
    expect(attachmentSupport(info(true), agent())).toBeNull()
    expect(attachmentSupport(info(true), agent([]))).toBeNull()
    expect(attachmentSupport(info(true), undefined)).toBeNull()
    expect(attachmentSupport(undefined, agent(['image/png']))).toBeNull()
  })

  it('only image formats the node can take count as images (the agent list ∩ png / jpeg / gif / webp)', () => {
    expect(attachmentSupport(info(true), agent(['application/pdf', 'image/webp', 'image/bmp']))).toEqual({ images: ['image/webp'], files: false })
    expect(attachmentSupport(info(true), agent(['application/pdf']))).toBeNull()
  })

  it('“*/*” means any file; it is enough on its own, and combines with the named image formats', () => {
    expect(attachmentSupport(info(true), agent(['*/*']))).toEqual({ images: [], files: true })
    expect(attachmentSupport(info(true), agent(['image/png', 'image/jpeg', 'image/gif', 'image/webp', '*/*']))).toEqual({ images: ['image/png', 'image/jpeg', 'image/gif', 'image/webp'], files: true })
  })

  it('an agent without any capabilities object takes nothing', () => {
    expect(attachmentSupport(info(true), { capabilities: undefined })).toBeNull()
  })
})

describe('acceptsAttachment', () => {
  it('a named image format, or anything when the agent takes any file', () => {
    const imagesOnly = { images: ['image/png'], files: false }
    expect(acceptsAttachment(imagesOnly, 'image/png')).toBe(true)
    expect(acceptsAttachment(imagesOnly, 'image/jpeg')).toBe(false)
    expect(acceptsAttachment(imagesOnly, 'application/pdf')).toBe(false)
    const any = { images: ['image/png'], files: true }
    expect(acceptsAttachment(any, 'application/pdf')).toBe(true)
    expect(acceptsAttachment(any, 'image/jpeg')).toBe(true)
    expect(acceptsAttachment(null, 'image/png')).toBe(false)
  })
})
