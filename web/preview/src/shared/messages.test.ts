import { describe, expect, it } from 'vitest'
import { parsePageMessage, parseWorkerMessage } from './messages'
import { pickLang, text, textKeys } from './i18n'

describe('page → worker messages', () => {
  it('accepts the messages the shim sends', () => {
    const ok = [
      { type: 'keepalive' },
      { type: 'cookies.sync' },
      { type: 'cookie.set', value: 'a=1; path=/' },
      { type: 'ws.open', id: 1, url: 'wss://x.example/ws', protocols: ['a', 'b'] },
      { type: 'ws.send', id: 1, binary: false, data: 'hi' },
      { type: 'ws.send', id: 1, binary: true, data: new ArrayBuffer(2) },
      { type: 'ws.close', id: 1, code: 1000, reason: 'bye' },
      { type: 'ws.close', id: 2 },
    ]
    for (const m of ok) expect(parsePageMessage(m)).toMatchObject({ type: m.type })
    expect(parsePageMessage({ type: 'ws.close', id: 2, junk: 1 })).toEqual({ type: 'ws.close', id: 2, code: undefined, reason: undefined })
  })

  it('rejects everything else, including extra fields on simple messages', () => {
    for (const bad of [
      null,
      'keepalive',
      42,
      {},
      { type: 'nope' },
      { type: 'cookie.set', value: 1 },
      { type: 'ws.open', id: -1, url: 'x', protocols: [] },
      { type: 'ws.open', id: 1.5, url: 'x', protocols: [] },
      { type: 'ws.open', id: 1, url: 1, protocols: [] },
      { type: 'ws.open', id: 1, url: 'x', protocols: [1] },
      { type: 'ws.open', id: 1, url: 'x' },
      { type: 'ws.send', id: 1, binary: 'no', data: 'x' },
      { type: 'ws.send', id: 1, binary: true, data: [1, 2] },
      { type: 'ws.close', id: 'a' },
    ]) {
      expect(parsePageMessage(bad), JSON.stringify(bad)).toBeUndefined()
    }
    expect(parsePageMessage({ type: 'keepalive', x: 1 })).toEqual({ type: 'keepalive' })
  })
})

describe('worker → page messages', () => {
  it('accepts the messages the worker sends', () => {
    expect(parseWorkerMessage({ type: 'ws.open', id: 1, protocol: '' })).toEqual({ type: 'ws.open', id: 1, protocol: '' })
    expect(parseWorkerMessage({ type: 'ws.message', id: 1, binary: false, data: 'x' })).toMatchObject({ data: 'x' })
    expect(parseWorkerMessage({ type: 'ws.close', id: 1, code: 1006, reason: '', wasClean: false })).toEqual({ type: 'ws.close', id: 1, code: 1006, reason: '', wasClean: false })
    expect(parseWorkerMessage({ type: 'ws.error', id: 1 })).toEqual({ type: 'ws.error', id: 1 })
  })

  it('keeps only well-formed cookies in a snapshot', () => {
    const good = { name: 'a', value: '1', path: '/', expiresAt: null, createdAt: 1, httpOnly: false }
    const parsed = parseWorkerMessage({ type: 'cookies', cookies: [good, { name: 'b' }, 'x', { ...good, expiresAt: 'soon' }] })
    expect(parsed).toEqual({ type: 'cookies', cookies: [good] })
    expect(parseWorkerMessage({ type: 'cookies', cookies: 'no' })).toBeUndefined()
  })

  it('rejects malformed messages', () => {
    for (const bad of [null, {}, { type: 'ws.open', id: 1 }, { type: 'ws.message', id: 1, binary: true, data: 5 }, { type: 'ws.close', id: 1, code: '1', reason: '' }, { type: 'ws.error' }]) {
      expect(parseWorkerMessage(bad), JSON.stringify(bad)).toBeUndefined()
    }
  })
})

describe('i18n', () => {
  it('picks Chinese for any zh locale and English for the rest', () => {
    for (const l of ['zh', 'zh-CN', 'zh-TW', 'ZH-hans']) expect(pickLang(l)).toBe('zh')
    for (const l of ['en', 'en-US', 'de', 'ja', '', undefined]) expect(pickLang(l)).toBe('en')
  })

  it('has every string in both languages, none empty', () => {
    expect(textKeys('zh')).toEqual(textKeys('en'))
    for (const key of textKeys('en')) {
      expect(text('en', key as never), key).not.toBe('')
      expect(text('zh', key as never), key).not.toBe('')
    }
  })
})
