// service worker 里的隧道持有者：第一次有请求时读启动参数、建隧道；启动参数换了（用户从 Anycon 重新打开了预览，
// 票据和令牌都是新的）就换一条新隧道——终态的隧道也因此能被新的启动参数救活。
import type { PreviewLaunch } from '@anycon/protocol'
import type { LaunchLookup } from '../shared/launch'
import { TunnelUnavailable, type PreviewTunnel } from './tunnel/link'
import type { TunnelSession } from './tunnel/session'

export interface HostDeps {
  loadLaunch(): Promise<LaunchLookup>
  createTunnel(launch: PreviewLaunch): PreviewTunnel
}

const launchKey = (l: PreviewLaunch): string => `${l.ticket_id}:${l.token}`

export class TunnelHost {
  private tunnel: PreviewTunnel | undefined
  private key = ''

  constructor(private readonly deps: HostDeps) {}

  /**
   * 当前该用的隧道。`reread`：重新读一遍启动参数看有没有换（导航请求时做，子资源请求直接用内存里的）；
   * 没有隧道或隧道已经是终态时总是重读。
   */
  async current(reread: boolean): Promise<PreviewTunnel> {
    const alive = this.tunnel && this.tunnel.state.phase !== 'terminal'
    if (this.tunnel && alive && !reread) return this.tunnel
    const lookup = await this.deps.loadLaunch()
    if (!lookup.ok) {
      // 库里读不到（或过期了）：还活着的隧道继续用，它自己会在过期时进入终态
      if (this.tunnel && alive) return this.tunnel
      throw new TunnelUnavailable(lookup.reason === 'expired' ? 'expired' : 'no_launch')
    }
    const key = launchKey(lookup.launch)
    if (this.tunnel && this.key === key) return this.tunnel
    this.tunnel?.stop()
    this.tunnel = this.deps.createTunnel(lookup.launch)
    this.key = key
    return this.tunnel
  }

  /** 等隧道就绪。 */
  async whenReady(reread: boolean): Promise<TunnelSession> {
    return (await this.current(reread)).whenReady()
  }

  /** 隧道已经进入终态时的原因（请求中途断开时，用它决定错误页该说什么）。 */
  terminalReason(): TunnelUnavailable | undefined {
    const state = this.tunnel?.state
    return state?.phase === 'terminal' ? new TunnelUnavailable(state.kind, state.detail) : undefined
  }

  /** 诊断 / 测试。 */
  get state(): PreviewTunnel['state'] | undefined {
    return this.tunnel?.state
  }
}
