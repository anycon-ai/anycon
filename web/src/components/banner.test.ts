import { describe, expect, it } from 'vitest'
import { pickBanner, type BannerInput } from './banner'

const base: BannerInput = { hubPhase: 'hub_authed', hubHalt: null, started: true, nodeConn: 'ready', unsent: 0, notificationsPrompt: false, updateReady: false }

describe('pickBanner', () => {
  it('shows nothing when everything is fine', () => {
    expect(pickBanner(base)).toBeNull()
  })
  it('puts hub disconnection above everything else', () => {
    expect(pickBanner({ ...base, hubPhase: 'hub_connecting', nodeConn: 'peer_offline', unsent: 3, updateReady: true })).toBe('hub_reconnecting')
  })
  it('does not flash the red banner during the very first connection attempt', () => {
    expect(pickBanner({ ...base, hubPhase: 'hub_connecting', hubAttempt: 0 })).toBeNull()
    expect(pickBanner({ ...base, hubPhase: 'hub_connecting', hubAttempt: 2 })).toBe('hub_reconnecting')
  })
  it('does not flash reconnecting before the transport starts', () => {
    expect(pickBanner({ ...base, started: false, hubPhase: 'hub_disconnected' })).toBeNull()
  })
  it('node offline beats unsent beats update', () => {
    expect(pickBanner({ ...base, nodeConn: 'peer_offline', unsent: 2, updateReady: true })).toBe('node_offline')
    expect(pickBanner({ ...base, unsent: 2, updateReady: true })).toBe('unsent')
    expect(pickBanner({ ...base, updateReady: true })).toBe('update')
  })
  it('a failed local write (draft / cache) is flagged below node offline but above unsent', () => {
    expect(pickBanner({ ...base, storageError: true, unsent: 2 })).toBe('storage')
    expect(pickBanner({ ...base, storageError: true, nodeConn: 'peer_offline' })).toBe('node_offline')
    expect(pickBanner({ ...base, storageError: false, unsent: 2 })).toBe('unsent')
  })
  it('surfaces terminal hub states', () => {
    expect(pickBanner({ ...base, hubPhase: 'hub_disconnected', hubHalt: 'superseded' })).toBe('hub_superseded')
    expect(pickBanner({ ...base, hubPhase: 'hub_disconnected', hubHalt: 'revoked' })).toBe('hub_revoked')
    expect(pickBanner({ ...base, hubPhase: 'hub_disconnected', hubHalt: 'version' })).toBe('hub_version')
  })
  it('puts the notification prompt between unsent and update', () => {
    expect(pickBanner({ ...base, notificationsPrompt: true, updateReady: true })).toBe('notifications')
    expect(pickBanner({ ...base, notificationsPrompt: true, unsent: 1 })).toBe('unsent')
  })
  it('a device asking to join a node (ADR-36) sits below hub disconnection and node offline, above everything else', () => {
    expect(pickBanner({ ...base, enrollRequests: 1, unsent: 2, storageError: true, notificationsPrompt: true, updateReady: true })).toBe('enroll_request')
    expect(pickBanner({ ...base, enrollRequests: 1, nodeConn: 'peer_offline' })).toBe('node_offline')
    expect(pickBanner({ ...base, enrollRequests: 1, hubPhase: 'hub_connecting', hubAttempt: 3 })).toBe('hub_reconnecting')
    expect(pickBanner({ ...base, enrollRequests: 0 })).toBeNull()
  })
})
