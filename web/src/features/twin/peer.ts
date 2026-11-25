// Bot 连接里「另一个节点」的显示：这台设备配过对时用节点名，没配过对时用 id 的前 8 位。
import { useTranslation } from 'react-i18next'
import { useNodes } from '@/stores/nodes'

export function usePeerName(id: string): string {
  const { t } = useTranslation()
  const name = useNodes((s) => s.byId[id]?.name)
  return name || t('twin.remote.unknownNode', { id: id.slice(0, 8) })
}
