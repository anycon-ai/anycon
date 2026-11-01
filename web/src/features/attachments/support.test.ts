import { describe, expect, it } from 'vitest'
import { mockAgent, nodeInfo } from '@/test/factories'
import { acceptsAttachment, attachmentSupport } from './support'

const info = (blobs: boolean, files = false) => nodeInfo({ capabilities: { audit: true, blobs, fs: true, search: false, workspace: true, ...(files ? { file_attachments: true } : {}) } })
const limits = { max_bytes: 4096, max_per_message: 20, max_message_bytes: 8192 }
const agent = (attachments?: string[]) => mockAgent({ capabilities: { env: false, interject: false, interrupt: true, resume: true, rewind: false, structured_output: false, thinking_stream: true, usage: true, attachments } })

describe('attachmentSupport', () => {
  it('on a node that does not take any file (no file_attachments): needs blobs and an agent that takes images', () => {
    expect(attachmentSupport(info(true), agent(['image/png', 'image/jpeg']))).toEqual({ images: ['image/png', 'image/jpeg'], files: false })
    expect(attachmentSupport(info(false), agent(['image/png']))).toBeNull()
    expect(attachmentSupport(info(true), agent())).toBeNull()
    expect(attachmentSupport(info(true), agent([]))).toBeNull()
    expect(attachmentSupport(info(true), undefined)).toBeNull()
    expect(attachmentSupport(undefined, agent(['image/png']))).toBeNull()
  })

  it('only image formats the node can take count as images (the agent list ∩ png / jpeg / gif / webp)', () => {
    expect(attachmentSupport(info(true), agent(['application/pdf', 'image/webp', 'image/bmp']))).toEqual({ images: ['image/webp'], files: false })
    expect(attachmentSupport(info(true), agent(['application/pdf']))).toBeNull()
  })

  it('a node with file_attachments takes any file for every agent, whatever the agent takes natively', () => {
    // 不原生收任何附件的 agent 也行：daemon 把附件落到会话的工作目录
    expect(attachmentSupport(info(true, true), agent())).toEqual({ images: [], files: true })
    expect(attachmentSupport(info(true, true), agent([]))).toEqual({ images: [], files: true })
    expect(attachmentSupport(info(true, true), agent(['application/pdf']))).toEqual({ images: [], files: true })
    expect(attachmentSupport(info(true, true), { capabilities: undefined })).toEqual({ images: [], files: true })
    expect(attachmentSupport(info(true, true), undefined)).toEqual({ images: [], files: true })
    expect(attachmentSupport(info(true, true), agent(['image/png', 'image/jpeg', 'image/gif', 'image/webp']))).toEqual({ images: ['image/png', 'image/jpeg', 'image/gif', 'image/webp'], files: true })
    // 没有 blob 通道就什么都不能传；`*/*` 不再是一个有意义的值
    expect(attachmentSupport(info(false, true), agent(['image/png']))).toBeNull()
    expect(attachmentSupport(info(true), agent(['*/*']))).toBeNull()
  })

  it('hands on the limits the node reports, and only then', () => {
    expect(attachmentSupport(nodeInfo({ capabilities: { audit: true, blobs: true, fs: true, search: false, workspace: true, file_attachments: true }, attachment_limits: limits }), agent())).toEqual({ images: [], files: true, limits })
    expect(attachmentSupport(info(true, true), agent())).not.toHaveProperty('limits')
  })

  it('an agent without any capabilities object takes nothing on a node that only takes images', () => {
    expect(attachmentSupport(info(true), { capabilities: undefined })).toBeNull()
  })
})

describe('acceptsAttachment', () => {
  it('a named image format, or anything when the node takes any file', () => {
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
