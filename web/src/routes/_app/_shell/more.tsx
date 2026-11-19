import { createFileRoute } from '@tanstack/react-router'
import { TabRoute } from '@/components/TabHost'

export const Route = createFileRoute('/_app/_shell/more')({ component: () => <TabRoute tab="more" /> })
