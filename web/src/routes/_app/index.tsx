import { createFileRoute } from '@tanstack/react-router'
import { HomeRedirect } from '@/pages/HomeRedirect'

export const Route = createFileRoute('/_app/')({
  component: HomeRedirect,
})
