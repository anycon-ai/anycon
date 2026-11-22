// 通知与整表交叉、离线写入、忘记节点时的在途请求。
import { beforeEach, expect, it } from 'vitest'
import type { NodeInfo, TwinGetResult } from '@/protocol'
import { nodeEntry } from '@/test/factories'
import { FakeRpc } from '@/test/fakeRpc'
import { FakeTwinDaemon, twinQuestion, twinValue } from '@/test/fakeTwinDaemon'
import { setRpc } from '@/transport/instance'
import { useNodes } from '@/stores/nodes'
import { useTwin } from './store'
import { answerTwin, forgetTwin, handleTwinQuestion, setTwin, syncTwin } from './service'
const A = 'a'.repeat(32)
let rpc: FakeRpc
beforeEach(() => {
  rpc = new FakeRpc()
  new FakeTwinDaemon(rpc)
  setRpc(rpc)
  forgetTwin(A)
  useNodes.setState({ byId: { [A]: nodeEntry({ id: A, info: { capabilities: { twin: true } } as unknown as NodeInfo }) }, order: [A] })
})
it('does not send writes offline or read unsupported nodes', async () => {
  useNodes.getState().patch(A, { conn: 'peer_offline' })
  await expect(setTwin(A, { paused: true })).rejects.toThrow()
  await expect(answerTwin(A, { id: 'q1', answer: 'web' })).rejects.toThrow()
  expect(rpc.calls).toHaveLength(0)
  useNodes.getState().patch(A, { conn: 'ready', info: { capabilities: { twin: false } } as unknown as NodeInfo })
  await syncTwin(A)
  expect(rpc.calls).toHaveLength(0)
})
it('an older list cannot overwrite a question notification, or resurrect it after being forgotten', async () => {
  let finish!: (value: TwinGetResult) => void
  rpc.handlers['twin.get'] = () =>
    new Promise<TwinGetResult>((resolve) => {
      finish = resolve
    })
  const syncing = syncTwin(A)
  handleTwinQuestion(A, { question: twinQuestion({ status: 'answered', answer: 'Web' }) })
  finish(twinValue())
  await syncing
  expect(useTwin.getState().nodes[A]?.questions[0]?.status).toBe('answered')
  const second = syncTwin(A)
  forgetTwin(A)
  finish(twinValue())
  await second
  expect(useTwin.getState().nodes[A]).toBeUndefined()
})
it('does not reopen an answered question on a duplicate older notice', () => {
  handleTwinQuestion(A, { question: twinQuestion({ status: 'answered', answer: 'Web' }) })
  handleTwinQuestion(A, { question: twinQuestion() })
  expect(useTwin.getState().nodes[A]?.questions[0]?.status).toBe('answered')
})

it('a late successful write cannot restore a forgotten node', async () => {
  let finish!: (value: TwinGetResult) => void
  rpc.handlers['twin.set'] = () =>
    new Promise<TwinGetResult>((resolve) => {
      finish = resolve
    })
  const writing = setTwin(A, { paused: true })
  forgetTwin(A)
  finish(twinValue())
  await expect(writing).rejects.toThrow()
  expect(useTwin.getState().nodes[A]).toBeUndefined()
})
