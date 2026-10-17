import { describe, expect, it } from 'vitest'
import { backoffDelay, hubReduce, initialHub, initialNode, MAX_HANDSHAKE_ATTEMPTS, nodeConnState, nodeReduce, restartDelay, type HubEvent, type HubMachine, type NodeEvent } from './connection'

const hubRun = (events: HubEvent[], start: HubMachine = initialHub, rng = () => 0.5): HubMachine => events.reduce((m, e) => hubReduce(m, e, rng), start)
const nodeRun = (events: NodeEvent[]) => events.reduce(nodeReduce, initialNode)

describe('hub state machine', () => {
  it('goes disconnected → connecting → authed and clears the backoff after it is stable', () => {
    const m = hubRun([{ type: 'connect' }, { type: 'auth_ok' }])
    expect(m).toMatchObject({ phase: 'hub_authed', halt: null, retryInMs: null })
    const dropped = hubRun([{ type: 'closed', code: 1006 }], m)
    expect(dropped).toMatchObject({ phase: 'hub_disconnected', attempt: 1 })
    expect(hubRun([{ type: 'stable' }], m).attempt).toBe(0)
  })

  it('backs off exponentially from 1 s up to 30 s with jitter', () => {
    expect(backoffDelay(0, () => 0.5)).toBe(1000)
    expect(backoffDelay(1, () => 0.5)).toBe(2000)
    expect(backoffDelay(5, () => 0.5)).toBe(30_000)
    expect(backoffDelay(10, () => 0.5)).toBe(30_000)
    expect(backoffDelay(2, () => 0)).toBe(3000) // −25%
    expect(backoffDelay(2, () => 1)).toBe(5000) // +25%
  })

  it('does not count flapping connections as healthy', () => {
    let m = initialHub
    const delays: number[] = []
    for (let i = 0; i < 3; i++) {
      m = hubReduce(hubReduce(m, { type: 'connect' }), { type: 'auth_ok' })
      m = hubReduce(m, { type: 'closed', code: 1006 }, () => 0.5)
      delays.push(m.retryInMs!)
    }
    expect(delays).toEqual([1000, 2000, 4000])
  })

  it('reconnects in 1–3 s after 4002 (hub restarting) without growing the backoff', () => {
    const m = hubRun([{ type: 'connect' }, { type: 'auth_ok' }, { type: 'closed', code: 4002 }], initialHub, () => 0)
    expect(m.retryInMs).toBe(1000)
    expect(hubRun([{ type: 'closed', code: 4002 }], initialHub, () => 1).retryInMs).toBe(3000)
    expect(restartDelay(() => 0.5)).toBe(2000)
    expect(m.attempt).toBe(0)
  })

  it('stops on 4001 superseded and 4004 revoked', () => {
    expect(hubRun([{ type: 'closed', code: 4001 }])).toMatchObject({ halt: 'superseded', retryInMs: null })
    expect(hubRun([{ type: 'closed', code: 4004 }])).toMatchObject({ halt: 'revoked', retryInMs: null })
  })

  it('maps auth failures to terminal states or a retry', () => {
    expect(hubRun([{ type: 'auth_fail', reason: 'revoked' }])).toMatchObject({ halt: 'revoked', retryInMs: null })
    expect(hubRun([{ type: 'auth_fail', reason: 'invalid' }])).toMatchObject({ halt: 'invalid_token' })
    expect(hubRun([{ type: 'auth_fail', reason: 'version' }])).toMatchObject({ halt: 'version' })
    expect(hubRun([{ type: 'auth_fail', reason: 'protocol' }]).retryInMs).toBe(1000)
  })

  it('visibility: reconnects immediately when disconnected (and takes a superseded connection back), but not from a hard halt', () => {
    const backoff = hubRun([{ type: 'closed', code: 1006 }, { type: 'closed', code: 1006 }])
    expect(hubRun([{ type: 'visible' }], backoff)).toMatchObject({ retryInMs: 0, attempt: 0 })
    const superseded = hubRun([{ type: 'closed', code: 4001 }])
    expect(hubRun([{ type: 'visible' }], superseded)).toMatchObject({ retryInMs: 0, halt: null })
    const revoked = hubRun([{ type: 'closed', code: 4004 }])
    expect(hubRun([{ type: 'visible' }], revoked)).toEqual(revoked)
    const authed = hubRun([{ type: 'connect' }, { type: 'auth_ok' }])
    expect(hubRun([{ type: 'visible' }], authed)).toBe(authed)
  })

  it('suspend (page in the background for 30 s): a clean disconnect without backoff, and coming back reconnects at once', () => {
    const authed = hubRun([{ type: 'connect' }, { type: 'auth_ok' }])
    const parked = hubReduce(authed, { type: 'suspend' })
    expect(parked).toMatchObject({ phase: 'hub_disconnected', attempt: 0, halt: null, retryInMs: 0 })
    expect(hubReduce(parked, { type: 'visible' })).toMatchObject({ phase: 'hub_disconnected', retryInMs: 0, halt: null })
    // 抖动中的连接（已经在退避）进后台：失败计数清零，不带着旧的退避回来
    const flapping = hubRun([{ type: 'closed', code: 1006 }, { type: 'closed', code: 1006 }, { type: 'closed', code: 1006 }])
    expect(flapping.attempt).toBe(3)
    expect(hubReduce(flapping, { type: 'suspend' })).toMatchObject({ attempt: 0, retryInMs: 0 })
    // 终态不会因为进后台被解除
    const revoked = hubRun([{ type: 'closed', code: 4004 }])
    expect(hubReduce(revoked, { type: 'suspend' })).toMatchObject({ halt: 'revoked', retryInMs: null })
    expect(hubReduce(hubReduce(revoked, { type: 'suspend' }), { type: 'visible' })).toMatchObject({ halt: 'revoked', retryInMs: null })
    // 被别的标签页顶掉：回到前台照旧可以自动夺回
    const superseded = hubRun([{ type: 'closed', code: 4001 }])
    expect(hubReduce(hubReduce(superseded, { type: 'suspend' }), { type: 'visible' })).toMatchObject({ halt: null, retryInMs: 0 })
  })
})

describe('per-node state machine', () => {
  it('derives the spec states from hub phase + node machine', () => {
    expect(nodeConnState('hub_disconnected', initialNode)).toBe('hub_disconnected')
    expect(nodeConnState('hub_connecting', initialNode)).toBe('hub_connecting')
    expect(nodeConnState('hub_authed', initialNode)).toBe('peer_offline')
    const online = nodeRun([{ type: 'peer_online' }])
    expect(nodeConnState('hub_authed', online)).toBe('noise_handshaking')
    expect(nodeConnState('hub_authed', nodeRun([{ type: 'peer_online' }, { type: 'handshake_started' }]))).toBe('noise_handshaking')
    expect(nodeConnState('hub_authed', nodeRun([{ type: 'peer_online' }, { type: 'handshake_started' }, { type: 'handshake_ok' }]))).toBe('ready')
    expect(nodeConnState('hub_authed', nodeRun([{ type: 'peer_online' }, { type: 'handshake_failed' }]))).toBe('noise_failed')
  })

  it('retries once on handshake timeout, then fails', () => {
    expect(MAX_HANDSHAKE_ATTEMPTS).toBe(2)
    const first = nodeRun([{ type: 'peer_online' }, { type: 'handshake_started' }, { type: 'handshake_timeout' }])
    expect(first).toMatchObject({ noise: 'none', attempts: 1 })
    const second = nodeReduce(nodeReduce(first, { type: 'handshake_started' }), { type: 'handshake_timeout' })
    expect(second.noise).toBe('failed')
  })

  it('peer_online after a failure starts over; peer_offline and hub_down reset everything', () => {
    const failed = nodeRun([{ type: 'peer_online' }, { type: 'handshake_failed' }])
    expect(nodeReduce(failed, { type: 'peer_online' })).toEqual({ peerOnline: true, noise: 'none', attempts: 0 })
    expect(nodeReduce(failed, { type: 'peer_offline' })).toEqual(initialNode)
    expect(nodeRun([{ type: 'peer_online' }, { type: 'handshake_ok' }, { type: 'hub_down' }])).toEqual(initialNode)
  })

  it('ignores handshake progress for an offline peer and supports a noise reset while online', () => {
    expect(nodeRun([{ type: 'handshake_started' }])).toEqual(initialNode)
    const ready = nodeRun([{ type: 'peer_online' }, { type: 'handshake_ok' }])
    expect(nodeReduce(ready, { type: 'reset' }).noise).toBe('none')
  })
})
