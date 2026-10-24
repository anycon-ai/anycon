// 删除自己的账号（`DELETE /api/me`，两种模式都有）。
//
// - 要求登录会话是 10 分钟内建立的：否则 `403 reauth_required`，说明原因并给「重新登录」（退出登录，回到登录页，登录后回到资料页再点一次）；
// - `single` 的引导管理员删不掉：`409 bootstrap_admin`，说明就好；
// - 成功：账号在 hub 上已经没了（设备、配对、订阅都级联删除，cookie 也清了），所以不再走「退出并移除本设备」里撤销节点 / 设备那几步，
//   只清这个浏览器里的本地数据，再整页跳到 `/login`（整页，内存里的 store、查询缓存、WebSocket 一起丢；toast 靠 sessionStorage 标记带过去）。
import { useState } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { useDeleteMyAccount } from '@/api/accounts'
import { ApiError } from '@/api/client'
import { resetClientState, signOut, wipeLocalData } from '@/app/session'
import { describeError } from '@/lib/errors'
import { flagAccountDeleted } from '@/lib/accountDeleted'
import { toast } from '@/stores/ui'
import { Button } from '@/ui/Button'
import { Dialog } from '@/ui/Overlay'

type Problem = { kind: 'reauth' } | { kind: 'bootstrap' } | { kind: 'other'; text: string }

/** 整页跳到登录页。 */
const goToLogin = (): void => location.replace('/login')

/** `onDeleted`：账号删掉、本地数据清完之后做什么（默认整页跳 `/login`；测试里换成桩）。 */
export function DeleteAccountDialog({ open, onOpenChange, onDeleted = goToLogin }: { open: boolean; onOpenChange: (v: boolean) => void; onDeleted?: () => void }) {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const qc = useQueryClient()
  const del = useDeleteMyAccount()
  const [problem, setProblem] = useState<Problem | null>(null)
  const [wiping, setWiping] = useState(false)
  const [signingOut, setSigningOut] = useState(false)
  const busy = del.isPending || wiping || signingOut

  const close = (o: boolean): void => {
    if (busy) return
    onOpenChange(o)
    if (!o) setProblem(null)
  }

  const run = (): void => {
    setProblem(null)
    del.mutate(undefined, {
      onSuccess: async () => {
        setWiping(true)
        try {
          // 账号已经没了：停掉还在飞的请求（它们只会得到 401），再清本地数据
          await qc.cancelQueries()
          await wipeLocalData()
        } catch (e) {
          console.warn('anycon: could not clear the local data after deleting the account', e)
        }
        flagAccountDeleted() // 清本地数据会清 sessionStorage，所以放在它之后
        onDeleted()
      },
      onError: (e) => {
        if (e instanceof ApiError && e.code === 'reauth_required') setProblem({ kind: 'reauth' })
        else if (e instanceof ApiError && e.code === 'bootstrap_admin') setProblem({ kind: 'bootstrap' })
        else setProblem({ kind: 'other', text: describeError(e, t) })
      },
    })
  }

  // 「重新登录」：普通退出（保留本浏览器的设备密钥），登录后回到资料页
  const signInAgain = async (): Promise<void> => {
    setSigningOut(true)
    try {
      await signOut()
    } catch (e) {
      setSigningOut(false)
      toast.error(t('sidebar.signOutFailed', { error: describeError(e, t) }))
      return
    }
    await resetClientState()
    qc.clear()
    onOpenChange(false)
    await navigate({ to: '/login', search: { redirect: '/settings' } })
  }

  return (
    <Dialog open={open} onOpenChange={close} title={t('settings.deleteAccountTitle')} description={problem ? undefined : t('settings.deleteAccountIntro')} closeLabel={t('common.close')} testId="delete-account-dialog">
      {problem?.kind === 'reauth' ? (
        <div className="space-y-4">
          <p className="rounded-md border border-warning/40 bg-warning-subtle p-3 text-sm text-warning" role="alert" data-testid="delete-account-reauth">
            {t('settings.deleteAccountReauth')}
          </p>
          <div className="flex flex-wrap justify-end gap-2">
            <Button variant="outline" disabled={busy} onClick={() => close(false)}>
              {t('common.cancel')}
            </Button>
            <Button variant="primary" loading={signingOut} onClick={() => void signInAgain()} data-testid="delete-account-signin-again">
              {t('settings.deleteAccountSignInAgain')}
            </Button>
          </div>
        </div>
      ) : problem?.kind === 'bootstrap' ? (
        <div className="space-y-4">
          <p className="rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm text-danger" role="alert" data-testid="delete-account-bootstrap">
            {t('settings.deleteAccountBootstrap')}
          </p>
          <div className="flex justify-end">
            <Button variant="outline" onClick={() => close(false)}>
              {t('common.close')}
            </Button>
          </div>
        </div>
      ) : (
        <div className="space-y-4">
          <ul className="list-disc space-y-1.5 pl-5 text-sm text-fg-muted">
            <li>{t('settings.deleteAccountDevices')}</li>
            <li>{t('settings.deleteAccountHistory')}</li>
            <li>{t('settings.deleteAccountRepair')}</li>
            <li>{t('settings.deleteAccountLocal')}</li>
          </ul>
          {problem?.kind === 'other' ? (
            <p className="rounded-md border border-danger/40 bg-danger-subtle p-3 text-sm text-danger" role="alert" data-testid="delete-account-error">
              {problem.text}
            </p>
          ) : null}
          <div className="flex flex-wrap justify-end gap-2">
            <Button variant="outline" disabled={busy} onClick={() => close(false)}>
              {t('common.cancel')}
            </Button>
            <Button variant="danger" loading={busy} onClick={run} data-testid="delete-account-confirm">
              {t('settings.deleteAccountConfirm')}
            </Button>
          </div>
        </div>
      )}
    </Dialog>
  )
}
