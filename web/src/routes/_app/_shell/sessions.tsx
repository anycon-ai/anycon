import { createFileRoute } from '@tanstack/react-router'
import { ToInbox } from '@/components/PageHosts'

export const Route = createFileRoute('/_app/_shell/sessions')({ component: ToInbox })
