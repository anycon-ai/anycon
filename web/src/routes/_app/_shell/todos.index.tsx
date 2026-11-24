import { createFileRoute } from '@tanstack/react-router'
import { KeptRoute } from '@/components/PageHosts'

export const Route = createFileRoute('/_app/_shell/todos/')({ component: () => <KeptRoute page="todos" /> })
