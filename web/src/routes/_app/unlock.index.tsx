import { createFileRoute } from '@tanstack/react-router'
import { UnlockPage } from '@/pages/UnlockPage'

export const Route = createFileRoute('/_app/unlock/')({
  component: UnlockPage,
})
