// 标了「待处理」的会话停下来时，页面在前台弹一条 toast（web.md § 4.6.1）；正在看着它、没标、还在跑时不弹。
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
  it('tells you when a marked session stops, with a “View” action that opens it', () => {
    const go = vi.fn()
    setNavigator(go)
    seed({ status: 'running', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ follow_up: true }))
    expect(toasts()).toHaveLength(1)
    expect(toasts()[0]!.text).toBe('“Analyse jira” finished and is waiting for you.')
    toasts()[0]!.action!.run()
    expect(go).toHaveBeenCalledWith(`/n/${NODE.slice(0, 8)}/s/${SID}`)
  })

  it('also fires when the mark arrives together with the stop', () => {
    seed({ status: 'running', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ status: 'error' }))
    expect(toasts()).toHaveLength(1)
  })

  it('stays quiet for an unmarked session, a session that is still running, and one that was already stopped', () => {
    seed({ status: 'running' })
    handleNotification(NODE, 'session.changed', changed())
    seed({ status: 'running', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ status: 'running', follow_up: true }))
    seed({ status: 'idle', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ follow_up: true }))
    expect(toasts()).toEqual([])
  })

  it('does not interrupt someone who is looking at that session', () => {
    seed({ status: 'running', follow_up: true })
    useSessions.getState().openStart(NODE, SID, [])
    handleNotification(NODE, 'session.changed', changed({ follow_up: true }))
    expect(toasts()).toEqual([])
  })

  it('stays quiet when the mark was removed or the session archived at the same moment', () => {
    seed({ status: 'running', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ follow_up: false }))
    seed({ status: 'running', follow_up: true })
    handleNotification(NODE, 'session.changed', changed({ follow_up: true, archived: true }))
    expect(toasts()).toEqual([])
  })
})
