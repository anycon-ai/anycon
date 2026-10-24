import { describe, expect, it } from 'vitest'
import { configuredProviders, isMultiMode, isOAuthProvider, oauthStartUrl, onPublicOrigin } from './oauth'

describe('configuredProviders', () => {
  it('keeps only github / google, in the order the hub listed them', () => {
    expect(configuredProviders(['github', 'google', 'email', 'token'])).toEqual(['github', 'google'])
    expect(configuredProviders(['google', 'email'])).toEqual(['google'])
    expect(configuredProviders(['email', 'token'])).toEqual([])
    expect(configuredProviders(undefined)).toEqual([])
    expect(configuredProviders(['gitlab'])).toEqual([])
    expect(isOAuthProvider('github')).toBe(true)
    expect(isOAuthProvider('email')).toBe(false)
  })
})

describe('isMultiMode', () => {
  it('is true only for multi; anything else (including a missing config) behaves like single', () => {
    expect(isMultiMode({ mode: 'multi' })).toBe(true)
    expect(isMultiMode({ mode: 'single' })).toBe(false)
    expect(isMultiMode({ mode: 'something-new' })).toBe(false)
    expect(isMultiMode(undefined)).toBe(false)
  })
})

describe('onPublicOrigin', () => {
  it('compares origins, ignoring the path of public_url', () => {
    expect(onPublicOrigin('https://hub.example.com', 'https://hub.example.com')).toBe(true)
    expect(onPublicOrigin('https://hub.example.com/', 'https://hub.example.com')).toBe(true)
    expect(onPublicOrigin('https://hub.example.com', 'http://192.168.1.5:8080')).toBe(false)
    expect(onPublicOrigin('https://hub.example.com', 'https://hub.example.com:8443')).toBe(false)
    expect(onPublicOrigin('http://hub.example.com', 'https://hub.example.com')).toBe(false)
  })

  it('cannot tell without a usable public_url, so it allows', () => {
    expect(onPublicOrigin(undefined, 'http://localhost:5173')).toBe(true)
    expect(onPublicOrigin('', 'http://localhost:5173')).toBe(true)
    expect(onPublicOrigin('not a url', 'http://localhost:5173')).toBe(true)
  })
})

describe('oauthStartUrl', () => {
  it('is a plain path with only the parameters that were given', () => {
    expect(oauthStartUrl('github')).toBe('/auth/oauth/github/start')
    expect(oauthStartUrl('google', { next: '/pair' })).toBe('/auth/oauth/google/start?next=%2Fpair')
    expect(oauthStartUrl('github', { link: true })).toBe('/auth/oauth/github/start?link=1')
    expect(oauthStartUrl('github', { invite: 'abc_DEF-1' })).toBe('/auth/oauth/github/start?invite=abc_DEF-1')
  })

  it('percent-encodes next, which can carry its own query', () => {
    const url = oauthStartUrl('github', { next: '/n/abc?tab=agent&x=1' })
    expect(new URL(url, 'http://h').searchParams.get('next')).toBe('/n/abc?tab=agent&x=1')
  })

  it('does not send a pointless next=/', () => {
    expect(oauthStartUrl('github', { next: '/' })).toBe('/auth/oauth/github/start')
  })
})
