import { Dot } from '@/ui/Misc'
import type { SessionStatus } from '@/protocol'

export function sessionTone(status: SessionStatus) {
  switch (status) {
    case 'running':
    case 'starting':
      return 'accent' as const
    case 'waiting_permission':
      return 'warning' as const
    case 'error':
      return 'danger' as const
    case 'queued':
    case 'interrupted':
      return 'warning' as const
    default:
      return 'neutral' as const
  }
}

export function SessionDot({ status }: { status: SessionStatus }) {
  return <Dot tone={sessionTone(status)} pulse={status === 'running' || status === 'starting'} />
}
