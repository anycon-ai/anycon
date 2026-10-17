// 类型收窄小工具。协议里 `serde_json::Value` 字段（`tool_call.input`、`tool_result.output`、`ConfigOption.default`、
// `ToolPermissionDetail.input` 等）在生成的类型里是 `unknown`：用之前必须显式收窄，不要用 `as` 硬转。

/** 普通对象（不含 null 和数组）。 */
export function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

/** 非空字符串，否则 undefined。 */
export function nonEmptyString(v: unknown): string | undefined {
  return typeof v === 'string' && v !== '' ? v : undefined
}
