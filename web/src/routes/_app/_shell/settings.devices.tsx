import { createFileRoute } from '@tanstack/react-router'
import { DevicesPage } from '@/pages/settings/DevicesPage'

export const Route = createFileRoute('/_app/_shell/settings/devices')({ component: DevicesPage })
