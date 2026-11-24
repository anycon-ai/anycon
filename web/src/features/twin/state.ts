// 分身页的状态头和节点设置「分身」标签共用同一套状态优先级：
// 离线 → 读取中 / 读取失败 → 本机关闭 → 没配置（压过已暂停）→ 守护进程报告的状态。
// 没配好的分身暂停与否没有意义，用户要先补上缺的部分，所以「没配置」排在「已暂停」前面。
import type { TFunction } from 'i18next'
import type { NodeEntry } from '@/stores/nodes'
import type { NodeTwin } from './store'

export type TwinHead = 'offline' | 'loading' | 'failed' | 'disabled' | 'not_configured' | 'paused' | 'budget' | 'running' | 'idle' | 'unknown'

export function twinHead(node: Pick<NodeEntry, 'conn'>, entry: NodeTwin | undefined): TwinHead {
  const value = entry?.value
  if (node.conn !== 'ready') return 'offline'
  if (!value) return entry?.failed ? 'failed' : 'loading'
  if (!value.enabled) return 'disabled'
  if (!value.configured) return 'not_configured'
  if (value.state === 'paused' || value.state === 'budget' || value.state === 'running' || value.state === 'idle') return value.state
  // 守护进程报告 disabled / not_configured 但与上面的字段不一致时，按「未知」显示，不猜
  return 'unknown'
}

export function twinHeadLabel(t: TFunction, head: TwinHead): string {
  switch (head) {
    case 'offline':
      return t('twin.offline')
    case 'loading':
      return t('twin.loading')
    case 'failed':
      return t('twin.loadFailed')
    case 'disabled':
      return t('twin.machineDisabled')
    default:
      return t(`twin.state.${head}`)
  }
}

/** 没有可用的分流后端：没选、选的不存在，或选的有问题。状态头和节点设置「分身」标签用同一个判断。 */
export function missingTriage(node: Pick<NodeEntry, 'info'>): boolean {
  const judge = node.info?.judge
  const triage = judge?.backends.find((b) => b.name === judge.triage)
  return !triage || !!triage.problem
}
