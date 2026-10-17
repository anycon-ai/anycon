import type { TFunction } from 'i18next'
import type { Tone } from '@/ui/Misc'
import type { NodeConnState } from '@/transport/connection'

export function connectionTone(conn: NodeConnState): Tone {
  switch (conn) {
    case 'ready':
      return 'success'
    case 'noise_handshaking':
    case 'hub_connecting':
      return 'warning'
    case 'peer_offline':
    case 'hub_disconnected':
      return 'neutral'
    case 'noise_failed':
      return 'danger'
    case 'hub_authed':
      return 'warning'
  }
}

export function connectionLabel(conn: NodeConnState, t: TFunction): string {
  return t(`status.node.${conn}`)
}
