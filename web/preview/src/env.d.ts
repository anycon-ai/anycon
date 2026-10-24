/** 构建时注入（`vite.preview.config.ts` 的 `define`）。没注入（测试、wasm 没构建）时用默认值。 */
declare const __PREVIEW_WASM_URL__: string
declare const __PREVIEW_VERSION__: string
