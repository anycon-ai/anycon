// 解锁页的接入流程：`AnyconClient.enroll` 的 React 封装。同一时刻只进行一个入口；换入口、离开页面都会取消
// （向 hub 发 `enroll_cancel`）。扫码入口只在用户点了「显示二维码」之后才开始；二维码到期（hub 5 分钟）后自己换一个新的，
// 页面进了后台就取消它（没人看得见的二维码不该占着节点的名额），回到前台也不自动重开。
import { useCallback, useEffect, useRef, useState } from 'react'
import type { EnrollMethod } from '@/protocol'
import { localExpiry } from '@/lib/expiry'
import { getClient } from '@/transport/instance'
import { EnrollError, type EnrollHooks, type EnrollStage } from '@/transport/enroll'
import { restartsByItself, type UnlockEntry } from './errors'

export type UnlockPhase = 'idle' | EnrollStage | 'error'

export interface UnlockState {
  /** 正在进行（或刚失败）的入口。 */
  entry: UnlockEntry | null
  phase: UnlockPhase
  /** 扫码入口的二维码内容（含扫码密钥，只放内存，不进 URL / 存储）。 */
  qr: string | null
  /** 这个接入会话到期的时刻（本机时钟）。 */
  expiresAt: number | null
  /** 接入码还剩几次机会（输错之后才知道）。 */
  attemptsLeft: number | null
  /** 刚才输的码不对：输入框要重新可用。 */
  wrongCode: boolean
  error: EnrollError | null
  /** 出错的时刻（`backoff` 的倒计时从这里算起）。 */
  errorAt: number | null
  nodeId: string | null
  /** 扫码入口因为页面进了后台被取消了：回到前台不自动重开，由用户点「重新显示二维码」。 */
  paused: boolean
}

const IDLE: UnlockState = { entry: null, phase: 'idle', qr: null, expiresAt: null, attemptsLeft: null, wrongCode: false, error: null, errorAt: null, nodeId: null, paused: false }

/** 扫码入口里「还没有人批准」的阶段：此时页面进了后台就取消。批准之后（验证、握手…）几百毫秒就走完，让它走完。 */
const WAITING_FOR_SCAN: ReadonlySet<UnlockPhase> = new Set<UnlockPhase>(['connecting', 'starting', 'waiting'])

export type UnlockStart =
  | { entry: 'scan' }
  | { entry: 'approve' }
  | { entry: 'code'; code: string }
  | { entry: 'password'; password: string }

const METHOD: Record<UnlockEntry, { method: EnrollMethod; approval: boolean }> = {
  scan: { method: 'scan', approval: false },
  approve: { method: 'code', approval: true },
  code: { method: 'code', approval: false },
  password: { method: 'password', approval: false },
}

/** 二维码连续自动更换的上限（每个 5 分钟，约一小时）：页面被忘在那里时不要无限期占着 hub 的名额。 */
const MAX_AUTO_RESTARTS = 12

export function useUnlock(node: string, deviceName: string, onDone: (nodeId: string) => void) {
  const [state, setState] = useState<UnlockState>(IDLE)
  const run = useRef(0)
  /** 每次 `start` / `cancel` / 卸载领一个号：`await stop()` 期间又来了新的，旧的就放弃（不然两个 `enroll` 会同时开始，后一个被「已有进行中的接入」拒绝）。 */
  const ticket = useRef(0)
  const inflight = useRef<Promise<unknown> | null>(null)
  const answer = useRef<((code: string | null) => void) | null>(null)
  const mounted = useRef(true)
  const autoRestarts = useRef(0)
  const latest = useRef({ node, deviceName, onDone })
  latest.current = { node, deviceName, onDone }
  /** 让正在进行的那一次失效：它之后的回调（阶段、结果）一律忽略。 */
  const invalidate = useCallback((): number => ++run.current, [])
  const drop = useCallback((): number => ++ticket.current, [])

  /** 取消正在进行的接入并等它收尾（`AnyconClient` 同一时刻只允许一个）。 */
  const stop = useCallback(async (): Promise<void> => {
    invalidate()
    answer.current?.(null)
    answer.current = null
    getClient().cancelEnroll()
    await inflight.current?.catch(() => {})
  }, [invalidate])

  const begin = useCallback(
    async (req: UnlockStart, auto: boolean): Promise<void> => {
      const mine = ++ticket.current
      await stop()
      if (!mounted.current || ticket.current !== mine) return
      if (!auto) autoRestarts.current = 0
      const id = ++run.current
      const alive = (): boolean => mounted.current && run.current === id
      setState({ ...IDLE, entry: req.entry, phase: 'connecting' })
      let presetCode = req.entry === 'code' ? req.code : null
      const hooks: EnrollHooks = {
        stage: (stage) => alive() && setState((s) => ({ ...s, phase: stage })),
        started: ({ expiresAtMs }) => alive() && setState((s) => ({ ...s, expiresAt: localExpiry(expiresAtMs, Date.now()) })),
        qr: (payload) => alive() && setState((s) => ({ ...s, qr: payload })),
        askCode: (ctx) => {
          if (presetCode !== null) {
            // 「我有接入码」：码在发起之前就输好了，第一轮直接用
            const code = presetCode
            presetCode = null
            return Promise.resolve(code)
          }
          if (alive()) setState((s) => ({ ...s, attemptsLeft: ctx.attemptsLeft, wrongCode: ctx.wrong }))
          return new Promise<string | null>((resolve) => {
            answer.current = resolve
          })
        },
      }
      const { node: n, deviceName: name } = latest.current
      const p = getClient().enroll(
        { node: n, ...METHOD[req.entry], deviceName: name, ...(req.entry === 'password' ? { password: req.password } : {}) },
        hooks,
      )
      inflight.current = p
      try {
        const res = await p
        if (!alive()) return
        setState((s) => ({ ...s, phase: 'done', nodeId: res.nodeId, qr: null }))
        latest.current.onDone(res.nodeId)
      } catch (e) {
        if (!alive()) return
        const error = e instanceof EnrollError ? e : new EnrollError('hub_unavailable', e instanceof Error ? e.message : String(e))
        if (!(e instanceof EnrollError)) console.error('anycon: enrollment failed unexpectedly', e)
        // 二维码到期了 / 连接断了重连：换一个新的，用户不用点什么。页面在后台就不换了（没人看得见），回来由用户点
        if (req.entry === 'scan' && restartsByItself(error.code) && autoRestarts.current < MAX_AUTO_RESTARTS) {
          if (document.visibilityState === 'hidden') {
            setState({ ...IDLE, entry: 'scan', paused: true })
            return
          }
          autoRestarts.current++
          void begin(req, true)
          return
        }
        setState((s) => ({ ...s, phase: 'error', error, errorAt: Date.now(), qr: null }))
      }
    },
    [stop],
  )

  const start = useCallback((req: UnlockStart) => begin(req, false), [begin])

  /** 用户在「输入 6 位码」这一步提交了码。 */
  const submitCode = useCallback((code: string) => {
    const resolve = answer.current
    answer.current = null
    resolve?.(code)
  }, [])

  /** 放弃当前入口，回到什么都没选的状态。 */
  const cancel = useCallback(async () => {
    const mine = ++ticket.current
    await stop()
    if (mounted.current && ticket.current === mine) setState(IDLE)
  }, [stop])

  /** 页面进了后台：取消没人批准的扫码会话，状态变成「已暂停」。 */
  const pause = useCallback(async () => {
    const mine = ++ticket.current
    await stop()
    if (mounted.current && ticket.current === mine) setState({ ...IDLE, entry: 'scan', paused: true })
  }, [stop])
  const live = useRef(state)
  live.current = state
  useEffect(() => {
    const onVisibility = (): void => {
      if (document.visibilityState !== 'hidden') return
      // 只对扫码入口：「请求批准」等批准时用户就是要切到手机上去批准，不能取消；接入码 / 口令没有等待的阶段
      const s = live.current
      if (s.entry === 'scan' && WAITING_FOR_SCAN.has(s.phase)) void pause()
    }
    document.addEventListener('visibilitychange', onVisibility)
    return () => document.removeEventListener('visibilitychange', onVisibility)
  }, [pause])

  useEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
      drop()
      invalidate()
      answer.current?.(null)
      answer.current = null
      // 离开页面：向 hub 发 enroll_cancel（别让接入会话占着 daemon 的名额到过期）
      getClient().cancelEnroll()
    }
  }, [invalidate, drop])

  return { state, start, submitCode, cancel }
}
