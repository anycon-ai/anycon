export { CLOSE_CODE, ENVELOPE_VERSION, METHODS, PAIR_CODE_ALPHABET, PAIR_CODE_LEN, PAIR_CODE_LEN_LONG, PROTOCOL_VERSION } from '@anycon/protocol'

export const APP_VERSION: string = typeof __APP_VERSION__ === 'string' ? __APP_VERSION__ : '0.0.0'

export const RPC_TIMEOUT_MS = 30_000
export const HANDSHAKE_TIMEOUT_MS = 8_000
export const PAIR_STEP_TIMEOUT_MS = 20_000
/** PairDone 之后等 daemon 的 `pair.ack` 的最长时间；超时配对失败、不落盘。 */
export const PAIR_ACK_TIMEOUT_MS = 30_000
/** 握手发出后这么久之内又收到 peer_online，视为同一次上线的重复通知（重新配对时 attachNode 和 hub 各一次）。 */
export const DUPLICATE_PEER_ONLINE_MS = 1_000

// device token 轮换（spec/web.md § 6.4，docs/spec/hub.md § 5）
/** device token 签发超过这么久就请求 hub 轮换。 */
export const TOKEN_MAX_AGE_MS = 30 * 24 * 3600_000
/** 发了 `rotate_token` 还没拿到新 token，等这么久再发一次：hub 同一设备 1 分钟内只轮换一次、多余的请求不回复，这个间隔要比它长。重连之后重发没用：用旧 token 认证的连接不能轮换（docs/spec/hub.md § 5）。 */
export const ROTATE_RETRY_MS = 75_000
/** 一条连接上头几次 `rotate_token` 各隔 `ROTATE_RETRY_MS`；这几次之后改成每隔 `ROTATE_RECHECK_MS` 看一次。 */
export const ROTATE_QUICK_SENDS = 3
/** 不需要发（或头几次已经发完）时，连着 hub 的标签页每隔这么久再看一次 token 是否到期，到期就再发。 */
export const ROTATE_RECHECK_MS = 3600_000
