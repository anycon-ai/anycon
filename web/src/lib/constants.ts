export { CLOSE_CODE, ENVELOPE_VERSION, METHODS, PAIR_CODE_ALPHABET, PAIR_CODE_LEN, PAIR_CODE_LEN_LONG, PROTOCOL_VERSION } from '@anycon/protocol'

export const APP_VERSION: string = typeof __APP_VERSION__ === 'string' ? __APP_VERSION__ : '0.0.0'

export const RPC_TIMEOUT_MS = 30_000
export const HANDSHAKE_TIMEOUT_MS = 8_000
export const PAIR_STEP_TIMEOUT_MS = 20_000
/** PairDone 之后等 daemon 的 `pair.ack` 的最长时间；超时配对失败、不落盘。 */
export const PAIR_ACK_TIMEOUT_MS = 30_000
/** 握手发出后这么久之内又收到 peer_online，视为同一次上线的重复通知（重新配对时 attachNode 和 hub 各一次）。 */
export const DUPLICATE_PEER_ONLINE_MS = 1_000
