/**
 * `@anycon/protocol`：anycon 协议的 TypeScript 侧。
 *
 * - 生成的类型（`generated.ts`，来自 `crates/protocol` 的 JSON Schema）
 * - 外层信封 / 控制消息的 msgpack 编解码（`envelope.ts`）
 * - 内层 JSON-RPC 编解码与 `RpcPeer`（`rpc.ts`）
 * - Noise wasm 封装（`crypto.ts`）
 * - 常量（`constants.ts`）
 * - dev server 预览：启动参数、label、隧道编解码（`preview.ts`，ADR-39）
 */
export * from './constants.js'
export * from './crypto.js'
export * from './envelope.js'
export * from './errors.js'
export * from './method-types.js'
export * from './preview.js'
export * from './rpc.js'

// 生成的类型。`Control`、`RpcMessage` 等已经由上面的模块导出，这里只补充其余的。
export type * from './generated.js'
