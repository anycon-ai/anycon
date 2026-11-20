import { useEffect, useMemo, useRef, useState, type Ref } from 'react'
import { Link } from '@tanstack/react-router'
import { CheckCircle2, ChevronRight, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { PageHeader } from '@/components/PageHeader'
import { SessionDot } from '@/components/StatusDot'
import { InboxActiveContext } from '@/features/inbox/active'
import { useArmed } from '@/features/inbox/arming'
import { SECTIONS, deriveInbox, isArmed, nowItems, type InboxItem, type InboxSectionId, type RequestItem } from '@/features/inbox/derive'
import { GhostDetail, InboxDetail, InboxRow } from '@/features/inbox/ui/InboxItems'
import { useMinute } from '@/features/inbox/ui/info'
import { useInboxActions } from '@/features/inbox/ui/useInboxActions'
import { OVERLAY_CLASS, useInboxSelection, type InboxMode } from '@/features/inbox/useSelection'
import { useInbox } from '@/features/inbox/useInbox'
import { useNodesSettled } from '@/features/inbox/useSettled'
import { useInboxKeys } from '@/features/inbox/useInboxKeys'
import { formatRelative } from '@/lib/format'
import { resolveNodeParam, shortNodeId } from '@/lib/ids'
import { useNodeFilter } from '@/lib/nodeFilter'
import { useMediaQuery } from '@/lib/useMediaQuery'
import { useNodes } from '@/stores/nodes'
import { useSessions, type SessionItem } from '@/stores/sessions'
import { useUi } from '@/stores/ui'
import { Chip } from '@/ui/Misc'
import { Drawer } from '@/ui/Overlay'
import { cn } from '@/ui/cn'

/** 每一段的标题（i18n key）。加新的段时在这里加一行，页面骨架不用动。 */
const SECTION_TITLE: Record<InboxSectionId, string> = { now: 'inbox.sections.now' }

/** 从通知点进来：定位这个会话最早的一条待答复请求（推送只带类别、节点和会话，不带请求 id）。 */
export interface InboxLocate {
  node: string
  session: string
}

/** ≥ 1200px 左右分栏；平板详情从右边滑出；手机单栏、行内展开。 */
function useInboxMode(): InboxMode {
  const xl = useMediaQuery('(min-width: 1200px)', true)
  const md = useMediaQuery('(min-width: 768px)', true)
  return xl ? 'split' : md ? 'overlay' : 'inline'
}

const isRequest = (i: InboxItem): i is RequestItem => i.kind === 'permission' || i.kind === 'plan' || i.kind === 'question'

/**
 * 「待处理」（`/inbox`，应用的首页，spec/web.md § 4.11）：所有等用户的事，跨左栏节点过滤器勾选的节点。
 * `active`：页面此刻是否在眼前。手机上三个标签页常驻、只是藏起来（保留各自的滚动位置和没发出去的输入），藏着时不处理快捷键和焦点。
 * `locate`：从通知点进来的目标；页面读到它就调 `onLocateDone`（把地址里的参数去掉），之后靠自己的状态跟着走。
 */
export function InboxPage({ active = true, locate, onLocateDone }: { active?: boolean; locate?: InboxLocate; onLocateDone?: () => void }) {
  const { t } = useTranslation()
  const model = useInbox()
  const mainItems = useMemo(() => nowItems(model), [model])
  const mode = useInboxMode()
  const root = useRef<HTMLDivElement>(null)
  const empty = useRef<HTMLHeadingElement>(null)
  const { actions, dialogs } = useInboxActions()
  const { selected: pickedNodes, toggle } = useNodeFilter()
  const order = useNodes((s) => s.order)
  const setDefer = useUi((s) => s.setDeferHomeNotice)

  // ---- 通知的目标：不受节点过滤器限制（§ 14.3：不在当前勾选里的内容照常显示，写明它属于哪里，不自动切换） ----
  const [target, setTarget] = useState<InboxLocate | null>(null)
  const [gone, setGone] = useState<InboxLocate | null>(null)
  const seen = useRef<string | null>(null)
  const resolvedKey = useRef<string | null>(null)
  // 链接打开时的选中版本：节点晚点同步完时，用户在这期间自己选过别的，就不再抢走他的选中
  const versionAtLink = useRef(0)
  const versionNow = useRef(0)
  useEffect(() => {
    if (!locate) {
      seen.current = null
      return
    }
    const id = `${locate.node}:${locate.session}`
    if (!active || seen.current === id) return
    seen.current = id
    resolvedKey.current = null
    versionAtLink.current = versionNow.current
    setTarget(locate)
    setGone(null)
    // 参数一读到就从地址里去掉，不管请求有没有到
    onLocateDone?.()
  }, [locate, active, onLocateDone])

  const nodeId = target ? resolveNodeParam(target.node, order) : undefined
  const targetNode = useNodes((s) => (nodeId ? s.byId[nodeId] : undefined))
  const sessions = useSessions((s) => s.list)
  const pending = useSessions((s) => s.pending)
  const located = useMemo<RequestItem | null>(() => {
    if (!target || !nodeId) return null
    const m = deriveInbox({ sessions, pending, enroll: { items: {}, resolved: {} }, selected: [nodeId], now: Date.now() })
    return nowItems(m).filter(isRequest).filter((i) => i.sessionId === target.session).sort((a, b) => a.sinceMs - b.sinceMs)[0] ?? null
  }, [target, nodeId, sessions, pending])
  // 过滤器之外的节点上的那条：放在列表最前面，带说明
  const outside = located && !mainItems.some((i) => i.key === located.key) ? located : null
  const items = useMemo(() => (outside ? [outside, ...mainItems] : mainItems), [outside, mainItems])

  const ready = useNodesSettled()
  const sel = useInboxSelection({ items, picked: pickedNodes, mode, active, ready, root, empty })
  const { choose } = sel
  versionNow.current = sel.version
  const shown = sel.ghost ? null : (items.find((i) => i.key === sel.selected) ?? null)
  // 平板上详情只在用户操作（点一行、深链接）之后才滑出；窗口缩放带来的自动选中不弹
  const overlayOpen = mode === 'overlay' && sel.explicit && sel.selected !== null
  const detailShown = mode === 'overlay' ? overlayOpen : mode === 'split' ? !!shown : !!shown
  // 页面藏在后面（手机上切到别的标签页）时不计时：回到眼前也算「显示的请求换了」，重新锁一小会儿
  const armed = useArmed(active && detailShown && shown ? shown.key : null)

  useInboxKeys({ active, enter: mode !== 'inline', item: shown, armed, selectedKey: sel.selected, move: sel.move, focusDetail: sel.focusDetail, choose })

  useEffect(() => {
    if (!target || resolvedKey.current !== null || !nodeId) return
    if (located) {
      resolvedKey.current = located.key
      // 用户在链接打开之后自己选了别的：不抢走他的选中（条目还在列表里，他想看随时能点）
      if (versionNow.current !== versionAtLink.current) {
        setTarget(null)
        return
      }
      choose(located.key)
      requestAnimationFrame(() => document.querySelector(`[data-inbox-row="${CSS.escape(located.key)}"]`)?.scrollIntoView?.({ block: 'nearest' }))
    } else if (targetNode?.synced) {
      resolvedKey.current = 'gone'
      setGone(target)
      setTarget(null)
    }
  }, [target, nodeId, located, targetNode?.synced, choose])
  // 用户离开了那条请求（选了别的、它没了）：目标放掉。要先看到它被选中过（`choose` 到选中之间隔着一次渲染）
  const reached = useRef(false)
  useEffect(() => {
    const key = resolvedKey.current
    if (!target || !key || key === 'gone') {
      reached.current = false
      return
    }
    if (sel.selected === key) reached.current = true
    else if (reached.current) setTarget(null)
  }, [target, sel.selected])
  // 升级说明先不弹，免得盖住从通知点进来的请求
  useEffect(() => {
    setDefer(!!target || !!gone)
    return () => setDefer(false)
  }, [target, gone, setDefer])

  useMinute()
  const showNode = pickedNodes.length > 1
  const total = mainItems.length
  const outsideNote = outside ? { onShow: () => toggle(outside.nodeId, true) } : undefined
  const detail = sel.ghost ? <GhostDetail item={sel.ghost.item} hasNext={items.length > 0} onNext={sel.next} replacedBy={!!sel.ghost.replacedBy} onShow={() => sel.ghost?.replacedBy && choose(sel.ghost.replacedBy)} /> : shown ? <InboxDetail key={shown.key} item={shown} armed={armed} outside={shown.key === outside?.key ? outsideNote : undefined} /> : null

  const renderRow = (item: InboxItem) => (
    <InboxRow key={item.key} item={item} mode={mode} selected={sel.selected === item.key && !sel.ghost} showNode={showNode || item.key === outside?.key} actions={actions} onSelect={sel.select}>
      {mode === 'inline' && sel.selected === item.key && !sel.ghost ? (
        <div className="border-t border-border px-3 py-3" data-testid="inbox-inline-detail">
          {detail}
        </div>
      ) : null}
    </InboxRow>
  )

  return (
    <InboxActiveContext.Provider value={active}>
    <div ref={root} className="flex min-h-0 flex-1 flex-col" data-testid="inbox-page" data-mode={mode}>
      <PageHeader root crumbs={[{ label: t('inbox.title') }]} showNodeStatus={false} />
      <div className="flex min-h-0 flex-1">
        <div className={cn('scroll-thin min-h-0 min-w-0 flex-1 overflow-y-auto', mode === 'split' && 'max-w-[26rem] shrink-0 border-r border-border xl:w-[360px] xl:flex-none')} data-testid="inbox-list">
          <div className="mx-auto max-w-3xl space-y-4 p-3 sm:p-4">
            {gone ? <GoneNotice locate={gone} onDismiss={() => setGone(null)} /> : null}
            {target && !located ? <PendingTarget target={target} nodeId={nodeId} onDismiss={() => setTarget(null)} /> : null}
            {outside ? (
              <section aria-label={t('inbox.title')} data-testid="inbox-located">
                <ul className="space-y-2">{renderRow(outside)}</ul>
              </section>
            ) : null}
            {SECTIONS.map((id) => {
              const section = model.sections.find((s) => s.id === id)
              const rows = section?.items ?? []
              const ghostAt = mode === 'inline' && sel.ghost ? Math.min(Math.max(sel.ghost.index - (outside ? 1 : 0), 0), rows.length) : -1
              if (rows.length === 0 && ghostAt < 0) return null
              return (
                <section key={id} aria-labelledby={`inbox-${id}`} data-testid={`inbox-section-${id}`}>
                  <h2 id={`inbox-${id}`} className="mb-2 flex items-center gap-2 px-1 text-xs font-semibold tracking-wide text-fg-muted uppercase">
                    {t(SECTION_TITLE[id])}
                    <span className="rounded-full bg-bg-muted px-1.5 py-0.5 text-[0.6875rem] font-medium normal-case text-fg-muted" data-testid={`inbox-count-${id}`}>
                      {rows.length}
                    </span>
                  </h2>
                  <ul className="space-y-2" aria-label={t('inbox.list')}>
                    {rows.flatMap((item, n) => [...(ghostAt === n && sel.ghost ? [<li key="ghost" className="rounded-lg border border-border">{detail}</li>] : []), renderRow(item)])}
                    {ghostAt >= rows.length && sel.ghost ? <li key="ghost" className="rounded-lg border border-border">{detail}</li> : null}
                  </ul>
                </section>
              )
            })}
            {total === 0 && !outside && !(mode === 'inline' && sel.ghost) ? <EmptyInbox ref={empty} /> : null}
            <InProgress sessions={model.inProgress} showNode={showNode} />
          </div>
        </div>
        {mode === 'split' ? (
          <div className="scroll-thin min-w-0 flex-1 overflow-y-auto p-6" data-testid="inbox-detail-pane">
            <div className="mx-auto max-w-2xl">{detail ?? <p className="text-sm text-fg-subtle">{items.length > 0 ? t('inbox.pickOne') : ''}</p>}</div>
          </div>
        ) : null}
      </div>
      {mode === 'overlay' ? (
        <Drawer
          side="right"
          open={overlayOpen}
          onOpenChange={(open) => !open && sel.clear()}
          title={t('inbox.title')}
          closeLabel={t('inbox.closeDetail')}
          className={OVERLAY_CLASS}
          width="w-[min(36rem,92vw)]"
          // 处理完之后焦点由页面自己放（下一条、「没有等你的事」）：别让 Radix 把它还给已经不在的那一行
          onCloseAutoFocus={(e) => {
            if (sel.focusOwned.current) e.preventDefault()
          }}
        >
          <div className="scroll-thin min-h-0 flex-1 overflow-y-auto p-4 pt-12" data-testid="inbox-overlay">
            {detail}
          </div>
        </Drawer>
      ) : null}
      {dialogs}
    </div>
    </InboxActiveContext.Provider>
  )
}

function EmptyInbox({ ref }: { ref: Ref<HTMLHeadingElement> }) {
  const { t } = useTranslation()
  return (
    <div className="flex flex-col items-center gap-2 px-4 py-10 text-center" data-testid="inbox-empty">
      <CheckCircle2 className="size-8 text-success" aria-hidden />
      <h2 ref={ref} tabIndex={-1} className="text-base font-semibold focus:outline-none focus-visible:underline">
        {t('inbox.empty')}
      </h2>
      <p className="max-w-sm text-sm text-fg-subtle">{t('inbox.emptyBody')}</p>
    </div>
  )
}

function DismissButton({ onClick }: { onClick: () => void }) {
  const { t } = useTranslation()
  return (
    <button type="button" onClick={onClick} aria-label={t('common.dismiss')} className="grid size-8 shrink-0 place-items-center rounded-md text-fg-subtle hover:bg-bg-hover max-md:size-11">
      <X className="size-4" aria-hidden />
    </button>
  )
}

function GoneNotice({ locate, onDismiss }: { locate: InboxLocate; onDismiss: () => void }) {
  const { t } = useTranslation()
  const order = useNodes((s) => s.order)
  const nodeId = resolveNodeParam(locate.node, order)
  return (
    <div className="flex items-start gap-2 rounded-lg border border-border bg-bg-subtle px-3 py-2.5 text-sm" role="status" data-testid="inbox-gone">
      <div className="min-w-0 flex-1">
        <p className="font-medium">{t('inbox.requestGone')}</p>
        <p className="text-fg-muted">{t('inbox.requestGoneBody')}</p>
        {nodeId ? (
          <Link to="/n/$node/s/$session" params={{ node: shortNodeId(nodeId, order), session: locate.session }} className="mt-1 inline-block font-medium text-accent-text hover:underline">
            {t('inbox.openSession')}
          </Link>
        ) : null}
      </div>
      <DismissButton onClick={onDismiss} />
    </div>
  )
}

/**
 * 通知的目标还没找到：节点这个浏览器不认识 / 还没解锁（给解锁和配对的入口，不是「已处理」）；认识但还没连上、没同步完（等它，
 * 同步完之后要么找到、要么说已经处理了）。
 */
function PendingTarget({ target, nodeId, onDismiss }: { target: InboxLocate; nodeId: string | undefined; onDismiss: () => void }) {
  const { t } = useTranslation()
  const name = useNodes((s) => (nodeId ? s.byId[nodeId]?.name : undefined))
  const conn = useNodes((s) => (nodeId ? s.byId[nodeId]?.conn : undefined))
  return (
    <div className="flex items-start gap-2 rounded-lg border border-border bg-bg-subtle px-3 py-2.5 text-sm" role="status" data-testid={nodeId ? 'inbox-connecting' : 'inbox-unknown-node'}>
      <div className="min-w-0 flex-1">
        {nodeId ? (
          <>
            <p>{conn === 'peer_offline' ? t('inbox.nodeOffline', { node: name ?? target.node }) : conn === 'noise_failed' ? t('inbox.nodeNeedsPairing', { node: name ?? target.node }) : t('inbox.connecting', { node: name ?? target.node })}</p>
            {conn === 'noise_failed' ? (
              <Link to="/unlock/$node" params={{ node: target.node }} className="inline-flex min-h-8 items-center font-medium text-accent-text hover:underline max-md:min-h-11">
                {t('inbox.unlockNode')}
              </Link>
            ) : null}
          </>
        ) : (
          <>
            <p>{t('inbox.unknownNode')}</p>
            <p className="mt-1 flex flex-wrap gap-x-4">
              <Link to="/unlock/$node" params={{ node: target.node }} className="inline-flex min-h-8 items-center font-medium text-accent-text hover:underline max-md:min-h-11" data-testid="inbox-unlock">
                {t('inbox.unlockNode')}
              </Link>
              <Link to="/pair" className="inline-flex min-h-8 items-center font-medium text-accent-text hover:underline max-md:min-h-11">
                {t('inbox.pairNode')}
              </Link>
            </p>
          </>
        )}
      </div>
      <DismissButton onClick={onDismiss} />
    </div>
  )
}

/** 「进行中」：平时折叠成一行（数量），不和等用户的事抢位置；展开列出还在跑的会话。标了「完成后提醒」的写明。 */
function InProgress({ sessions, showNode }: { sessions: SessionItem[]; showNode: boolean }) {
  const { t, i18n } = useTranslation()
  useMinute()
  const [open, setOpen] = useState(false)
  const nodes = useNodes((s) => s.byId)
  const order = useNodes((s) => s.order)
  if (sessions.length === 0) return null
  return (
    <section data-testid="inbox-in-progress" aria-label={t('inbox.inProgress')}>
      <button
        type="button"
        aria-expanded={open}
        aria-controls={open ? 'inbox-in-progress-list' : undefined}
        onClick={() => setOpen((v) => !v)}
        className="flex min-h-11 w-full items-center gap-2 rounded-lg px-2 text-left text-sm text-fg-muted hover:bg-bg-hover"
        data-testid="inbox-in-progress-toggle"
      >
        <ChevronRight className={cn('size-4 shrink-0 transition-transform', open && 'rotate-90')} aria-hidden />
        <span className="font-medium">{t('inbox.inProgress')}</span>
        <span className="rounded-full bg-bg-muted px-1.5 py-0.5 text-xs" data-testid="inbox-in-progress-count">
          {sessions.length}
        </span>
      </button>
      {open ? (
        <ul id="inbox-in-progress-list" className="mt-1 space-y-1">
          {sessions.map((s) => (
            <li key={s.id}>
              <Link
                to="/n/$node/s/$session"
                params={{ node: shortNodeId(s.node_id, order), session: s.id }}
                className="flex min-h-11 items-center gap-2 rounded-lg px-2 py-1.5 text-sm hover:bg-bg-hover"
                data-testid="inbox-in-progress-row"
                data-armed={isArmed(s) ? 'true' : undefined}
              >
                <SessionDot status={s.status} />
                <span className="min-w-0 flex-1">
                  <span className="block truncate">{s.title || s.id}</span>
                  <span className="block truncate text-xs text-fg-subtle">{[s.project, showNode ? nodes[s.node_id]?.name : undefined, t(`status.session.${s.status}`), formatRelative(s.updated_at_ms, i18n.language)].filter(Boolean).join(' · ')}</span>
                </span>
                {isArmed(s) ? <Chip tone="info">{t('inbox.willRemind')}</Chip> : null}
              </Link>
            </li>
          ))}
        </ul>
      ) : null}
    </section>
  )
}
