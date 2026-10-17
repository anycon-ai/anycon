// 界面层测试用的假 AnyconClient（只有接入相关的两个方法）：测试自己决定 `enroll` 什么时候走到哪一步、怎么结束。
import type { PairResult, EnrollRequest } from '@/transport/client'
import { EnrollError, type EnrollHooks } from '@/transport/enroll'

export interface EnrollCall {
  req: EnrollRequest
  hooks: EnrollHooks
  resolve: (nodeId: string) => void
  reject: (e: EnrollError) => void
  /** 已经因 `cancelEnroll` 结束。 */
  cancelled: boolean
}

export class FakeEnrollClient {
  deviceName = 'Chrome · macOS · localhost'
  calls: EnrollCall[] = []
  cancels = 0
  removed: string[] = []

  /** 最近一次 `enroll` 调用。 */
  get last(): EnrollCall {
    const c = this.calls.at(-1)
    if (!c) throw new Error('enroll has not been called')
    return c
  }

  /** 和真客户端一样：同一时刻只允许一个接入，还没结束时再调用会被拒绝。 */
  private active: EnrollCall | null = null
  /** 被「已有进行中的接入」拒绝的调用次数（界面层不该并发地开始两个）。 */
  overlaps = 0

  enroll = (req: EnrollRequest, hooks: EnrollHooks): Promise<PairResult> => {
    if (this.active) {
      this.overlaps += 1
      return Promise.reject(new EnrollError('cancelled', 'another pairing or enrollment is in progress'))
    }
    return new Promise<PairResult>((resolve, reject) => {
      const call: EnrollCall = {
        req,
        hooks,
        cancelled: false,
        resolve: (nodeId) => {
          if (this.active === call) this.active = null
          resolve({ nodeId, publicKey: new Uint8Array(32) })
        },
        reject: (e) => {
          if (this.active === call) this.active = null
          reject(e)
        },
      }
      this.active = call
      this.calls.push(call)
    })
  }

  cancelEnroll = (): void => {
    this.cancels += 1
    const c = this.active
    if (c && !c.cancelled) {
      c.cancelled = true
      c.reject(new EnrollError('cancelled'))
    }
  }

  removeNode = async (id: string): Promise<void> => {
    this.removed.push(id)
  }
}
