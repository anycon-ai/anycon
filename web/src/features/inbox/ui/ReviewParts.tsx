// 「待处理」里的「待验收」段（spec/web.md § 4.11、§ 4.12）：行上的结果摘要和改动统计、详情里的结论和三个动作（通过、再改改、打开改动）。
// 这一段的条目不是在等人答复：不自动选中、不自动展开、不计入数字；动作都是普通按钮，不需要「解锁」。
import { useEffect, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { ExternalLink } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { create } from 'zustand'
import { Markdown } from '@/components/Markdown'
import { sendMessage } from '@/features/session/actions'
import { setTodoState } from '@/features/todos/actions'
import { latestResult, loadHistory, useHistoryEntry } from '@/features/todos/history'
import { isSessionActive, loadChangeSummary, useLinkedSession, type ChangeSummary } from '@/features/todos/review'
import { useReworkDraft } from '@/features/todos/reworkDraft'
import type { ViewTodo } from '@/features/todos/types'
import { cleanOneLine } from '@/features/todos/validate'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Textarea } from '@/ui/Field'
import { holdHandled, unmarkHandled } from '@/features/session/inflight'
import type { ReviewInboxItem } from '../derive'

/** 哪一条的「再改改」输入框开着（行上的按钮把它打开，详情里显示）。 */
export const useReviewUi = create<{ reworkKey: string | null; open: (key: string | null) => void }>()((set) => ({ reworkKey: null, open: (reworkKey) => set({ reworkKey }) }))

/** 改动统计的缓存：同一个会话、同一个 `last_seq` 只问一次（行和详情共用；agent 又改了东西 `last_seq` 变了才重新问）。 */
const briefCache = new Map<string, Promise<ChangeSummary | null>>()
function summaryFor(nodeId: string, session: { id: string; last_seq: number; worktree?: unknown }): Promise<ChangeSummary | null> {
  const key = `${nodeId}:${session.id}:${session.last_seq}`
  let p = briefCache.get(key)
  if (!p) {
    p = loadChangeSummary(nodeId, session as Parameters<typeof loadChangeSummary>[1]).catch(() => null)
    briefCache.set(key, p)
    if (briefCache.size > 200) briefCache.delete(briefCache.keys().next().value as string)
  }
  return p
}

export interface ReviewBrief {
  /** 最近一轮的结果（agent 的最后一段话），还没读到 / 没有是 `undefined`。 */
  text?: string
  summary?: ChangeSummary | null
}

/** 一个待验收条目的摘要：结果文字（读「经过」）和改动统计（读最近一个关联会话的文件列表）。节点不在线时不去读。 */
export function useReviewBrief(todo: ViewTodo): ReviewBrief {
  const nodeId = todo.node_id
  const online = useNodes((s) => s.byId[nodeId]?.conn === 'ready')
  const history = useHistoryEntry(nodeId, todo.id)
  const { session } = useLinkedSession(nodeId, todo.sessions)
  const sessionId = session?.id
  const lastSeq = session?.last_seq
  useEffect(() => {
    if (online) void loadHistory(nodeId, todo.id)
  }, [online, nodeId, todo.id])
  const [summary, setSummary] = useState<ChangeSummary | null | undefined>(undefined)
  useEffect(() => {
    if (!online || !session) return
    let live = true
    void summaryFor(nodeId, session).then((s) => live && setSummary(s))
    return () => {
      live = false
    }
    // 会话对象每次都是新的：用 id 和 last_seq 当依赖
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [online, nodeId, sessionId, lastSeq])
  const r = latestResult(history.events)
  return { ...(r ? { text: r.text } : {}), summary }
}

/** 行上的一行字：结果的第一行 + 改了几个文件和行数。都读不到时什么都不画。 */
export function ReviewBriefLine({ todo }: { todo: ViewTodo }) {
  const { t } = useTranslation()
  const b = useReviewBrief(todo)
  const first = b.text?.split('\n').find((l) => l.trim() !== '')
  const counts = b.summary ? (b.summary.files.length === 0 ? t('todos.reviewPage.noChanges') : `${t('todos.reviewPage.changes', { count: b.summary.files.length })} +${b.summary.additions} −${b.summary.deletions}`) : undefined
  if (!first && !counts) return null
  return (
    <span className="mt-0.5 block truncate text-xs text-fg-muted" data-testid="inbox-review-brief">
      {[first ? cleanOneLine(first, 120) : undefined, counts].filter(Boolean).join(' · ')}
    </span>
  )
}

/** 行上的三个动作。「再改改…」只是打开详情里的输入框（要写点什么才发得出去）。 */
export function ReviewRowActions({ item, online, selected, onSelect, small }: { item: ReviewInboxItem; online: boolean; /** 这一条已经选中（手机上已经展开）：再点「再改改…」只开输入框，不能因为 `select` 是开关而把它收起来。 */ selected: boolean; onSelect: (key: string) => void; small: string }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const { session, resolving } = useLinkedSession(item.nodeId, item.todo.sessions)
  const busy = !!session && isSessionActive(session)
  return (
    <div className="flex flex-wrap gap-2 px-3 pb-2.5 pl-10">
      <Button size="sm" variant="primary" className={small} disabled={!online || resolving} onClick={() => approve(item.todo)} data-testid="inbox-approve">
        {t('todos.reviewPage.approve')}
      </Button>
      <Button
        size="sm"
        variant="secondary"
        className={small}
        disabled={!online || !session || session.archived === true || busy || resolving}
        onClick={() => {
          if (!selected) onSelect(item.key)
          useReviewUi.getState().open(item.key)
        }}
        data-testid="inbox-rework"
      >
        {t('todos.reviewPage.rework')}
      </Button>
      {session ? (
        <Link to="/n/$node/s/$session" params={{ node: shortNodeId(item.nodeId, order), session: session.id }} search={{ view: 'changes' } as never} className={`inline-flex h-8 select-none items-center rounded-md border border-border-strong px-3 text-[0.8125rem] font-medium text-fg hover:bg-bg-hover ${small}`} data-testid="inbox-open-changes">
          {t('inbox.openChanges')}
        </Link>
      ) : null}
    </div>
  )
}

/** 通过：先标「在这里处理的」（条目消失后选中和焦点落到下一条），再改状态。 */
function approve(todo: ViewTodo): void {
  holdHandled(`review:${todo.node_id}:${todo.id}`)
  void setTodoState(todo, 'done')
}

/** 详情：结论（agent 最后一段话，消毒的 Markdown）、改动统计、三个动作，和验收页的链接。 */
export function ReviewDetail({ item, online }: { item: ReviewInboxItem; online: boolean }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const b = useReviewBrief(item.todo)
  const { session, resolving } = useLinkedSession(item.nodeId, item.todo.sessions)
  const busy = !!session && isSessionActive(session)
  const reworkKey = useReviewUi((s) => s.reworkKey)
  const [text, setText, clearText] = useReworkDraft(item.nodeId, item.todo.id)
  const [sending, setSending] = useState(false)
  // 写了一半的话还在（切到别的条目再回来）时输入框也开着
  const open = reworkKey === item.key || text.trim() !== ''
  const node = shortNodeId(item.nodeId, order)
  const send = async (): Promise<void> => {
    if (!session || text.trim() === '' || sending) return
    setSending(true)
    // 发出去之后这一项会离开「待验收」：先标「在这里处理的」，选中和焦点落到这一段的下一条，不留「已经不在等你了」的占位
    holdHandled(item.key)
    try {
      const r = await sendMessage(item.nodeId, session.id, text.trim())
      // 没发出去（被拒绝、或链路断了记成「未发送」）：留着输入框和文字，说清楚，不当成已发送
      if (r.state === 'failed') {
        unmarkHandled(item.key)
        toast.error(r.reason === 'archived' ? t('session.archivedHint') : t('todos.reviewPage.reworkFailed'))
      } else if (r.state === 'unsent') {
        unmarkHandled(item.key)
        toast.error(t('todos.reviewPage.reworkUnsent'))
      } else {
        toast.info(r.state === 'queued' ? t('todos.reviewPage.reworkQueued') : t('todos.reviewPage.reworkSent'))
        clearText()
        useReviewUi.getState().open(null)
      }
    } finally {
      setSending(false)
    }
  }
  return (
    <div className="space-y-3" data-testid="inbox-review-detail">
      <section className="space-y-1">
        <h4 className="text-sm font-semibold">{t('todos.reviewPage.conclusion')}</h4>
        {b.text ? (
          <div className="rounded-md border border-border p-3" data-testid="inbox-review-result">
            <Markdown text={b.text} />
          </div>
        ) : (
          <p className="text-sm text-fg-muted">{t('todos.reviewPage.noResult')}</p>
        )}
        {b.summary ? (
          <p className="text-sm" data-testid="inbox-review-counts">
            {b.summary.files.length === 0 ? t('todos.reviewPage.noChanges') : t('todos.reviewPage.changes', { count: b.summary.files.length })}
            {b.summary.files.length > 0 ? (
              <>
                {' '}
                <span className="text-success">+{b.summary.additions}</span> <span className="text-danger">−{b.summary.deletions}</span>
              </>
            ) : null}
          </p>
        ) : null}
      </section>
      <div className="flex flex-wrap items-center gap-2">
        <Button variant="primary" disabled={!online || resolving} onClick={() => approve(item.todo)} data-testid="inbox-detail-approve">
          {t('todos.reviewPage.approve')}
        </Button>
        <Button disabled={!online || !session || session.archived === true || busy || resolving} onClick={() => useReviewUi.getState().open(open && text.trim() === '' ? null : item.key)} aria-expanded={open} data-testid="inbox-detail-rework">
          {t('todos.reviewPage.rework')}
        </Button>
        {session ? (
          <Link to="/n/$node/s/$session" params={{ node, session: session.id }} search={{ view: 'changes' } as never} className="inline-flex min-h-9 items-center gap-1 rounded-md border border-border-strong px-3 text-sm font-medium hover:bg-bg-hover max-md:min-h-11" data-testid="inbox-detail-open-changes">
            {t('inbox.openChanges')}
            <ExternalLink className="size-3.5" aria-hidden />
          </Link>
        ) : null}
        <Link to="/todos/$node/$todo/review" params={{ node, todo: item.todo.id }} className="inline-flex min-h-9 items-center text-sm font-medium text-accent-text hover:underline max-md:min-h-11" data-testid="inbox-detail-review-page">
          {t('inbox.openReviewPage')}
        </Link>
      </div>
      {resolving || busy ? (
        <p className="text-xs text-fg-muted" role="status" data-testid="inbox-rework-why">
          {resolving ? t('todos.reviewPage.resolving') : t('todos.reviewPage.reworkActive')}
        </p>
      ) : null}
      {open ? (
        <div className="space-y-2" data-testid="inbox-rework-box">
          <Textarea rows={3} value={text} onChange={(e) => setText(e.target.value)} placeholder={t('todos.reviewPage.reworkPlaceholder')} aria-label={t('todos.reviewPage.reworkPlaceholder')} data-testid="inbox-rework-input" />
          <div className="flex justify-end gap-2">
            <Button
              onClick={() => {
                // 取消 = 不发了：草稿一起丢掉
                clearText()
                useReviewUi.getState().open(null)
              }}
              data-testid="inbox-rework-cancel"
            >
              {t('common.cancel')}
            </Button>
            <Button variant="primary" disabled={sending || text.trim() === ''} onClick={() => void send()} data-testid="inbox-rework-send">
              {t('todos.reviewPage.reworkSend')}
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  )
}
