import { useNavigate } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { CenterLayout } from '@/components/CenterLayout'
import { PairForm } from '@/components/PairForm'
import { useLinkCode } from '@/features/pairing/useLinkCode'
import { shortNodeId } from '@/lib/ids'
import { useNodes } from '@/stores/nodes'
import { Button } from '@/ui/Button'
import { Card } from '@/ui/Misc'

/** `code`：旧版链接的 `?code=`；现在的链接把短码放在 `#code=`（不发给 hub），由 `useLinkCode` 读取。 */
export function PairPage({ code: legacyCode }: { code?: string }) {
  const { t } = useTranslation()
  const linkCode = useLinkCode('/pair', legacyCode)
  const navigate = useNavigate()
  const hasNodes = useNodes((s) => s.order.length > 0)
  return (
    <CenterLayout>
      <Card className="p-5">
        <h1 className="text-lg font-semibold">{t('pair.title')}</h1>
        <p className="mt-1 mb-5 text-sm text-fg-muted">{t('pair.subtitle')}</p>
        <PairForm
          initialCode={linkCode}
          fromLink={!!linkCode}
          onPaired={(id) => {
            const order = useNodes.getState().order
            void navigate({ to: '/n/$node', params: { node: shortNodeId(id, order) } })
          }}
        />
      </Card>
      {hasNodes ? (
        <div className="mt-4 text-center">
          <Button variant="ghost" onClick={() => void navigate({ to: '/' })}>
            {t('common.cancel')}
          </Button>
        </div>
      ) : null}
    </CenterLayout>
  )
}
