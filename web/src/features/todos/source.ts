// 一项是谁记的（docs/spec/daemon.md § 21.2）。`kind` 之后会多出新的值（更新的 daemon、分身）：不认识的值一律按「不是人加的」处理，
// 解析时不能失败，也不能被当成「我记的」，更不能走 agent 专属的「收下 / 丢掉」那一套（除非它自己带了 `unreviewed`）。
import type { TodoSource } from '@/protocol'

/** 人记的：经 hub 来的已配对设备，或本机 CLI。 */
export const isHumanSource = (source: Pick<TodoSource, 'kind'>): boolean => source.kind === 'device' || source.kind === 'local'

/** agent 记的。 */
export const isAgentSource = (source: Pick<TodoSource, 'kind'>): boolean => source.kind === 'agent'

/** 不认识的来源（既不是人，也不是 agent）：显示成「自动记的」。 */
export const isOtherSource = (source: Pick<TodoSource, 'kind'>): boolean => !isHumanSource(source) && !isAgentSource(source)
