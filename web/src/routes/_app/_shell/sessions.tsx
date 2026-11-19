import { createFileRoute } from '@tanstack/react-router'
import { TabRoute } from '@/components/TabHost'

export const Route = createFileRoute('/_app/_shell/sessions')({ component: () => <TabRoute tab="sessions" /> })
