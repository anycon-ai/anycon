// daemon 消费 `follow_up` 标记（`follow_up_fired`）时，页面在前台弹一条 toast（web.md § 4.6.1）；
// 只认这个信号，正在看着那个会话时不弹。
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import i18n from '@/i18n'
import { setNavigator } from '@/lib/navigate'
import { resetSeq, summary } from '@/test/factories'
import { useNodes } from '@/stores/nodes'
import { useSessions } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { handleNotification } from './notifications'

const NODE = 'n'.repeat(32)
const SID = 'sess_a'

const changed = (over: Record<string, unknown> = {}) => ({ session_id: SID, title: 'Analyse jira', status: 'idle', pending_permissions: 0, updated_at_ms: 9, last_seq: 9, ...over })
const toasts = () => useUi.getState().toasts

beforeEach(async () => {
  resetSeq()
  await i18n.changeLanguage('en')
  useUi.setState({ toasts: [] })
  useNodes.getState().hydrate([{ node_id: NODE, name: 'node', public_key: new Uint8Array(32), paired_at: 1, last_seen: 1 }])
  useSessions.setState({ list: {}, pending: {}, open: {} })
})
afterEach(() => setNavigator(null))

function seed(over: Parameters<typeof summary>[0]): void {
  useSessions.getState().upsertSummaries(NODE, [summary({ id: SID, title: 'Analyse jira', ...over })])
}

describe('follow-up toast', () => {
  it('tells you when the daemon consumed the mark (follow_up_fired), with a “View” action that opens it', () => {
    const go = vi.fn()
    setNavigator(go)
    seed({ status: 'running', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ follow_up: false, follow_up_fired: true }))
    expect(toasts()).toHaveLength(1)
    expect(toasts()[0]!.text).toBe('“Analyse jira” finished.')
    toasts()[0]!.action!.run()
    expect(go).toHaveBeenCalledWith(`/n/${NODE.slice(0, 8)}/s/${SID}`)
  })

  it('stays quiet when the mark went from true to false without the fired signal (cancelled on another device)', () => {
    seed({ status: 'idle', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ follow_up: false }))
    expect(toasts()).toEqual([])
  })

  it('does not infer a finish from the status: a marked session that stops without the signal stays quiet', () => {
    seed({ status: 'running', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ follow_up: true }))
    handleNotification(NODE, 'session.changed', changed({ follow_up: false, status: 'error' }))
    expect(toasts()).toEqual([])
  })

  it('does not interrupt someone who is looking at that session', () => {
    seed({ status: 'running', follow_up: true })
    useSessions.getState().openStart(NODE, SID, [])
    handleNotification(NODE, 'session.changed', changed({ follow_up: false, follow_up_fired: true }))
    expect(toasts()).toEqual([])
  })

  it('never stores the fired signal on the session summary', () => {
    seed({ status: 'running', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ follow_up: false, follow_up_fired: true }))
    expect(useSessions.getState().list[SID]).not.toHaveProperty('follow_up_fired')
    expect(useSessions.getState().list[SID]!.follow_up).toBe(false)
  })
})
