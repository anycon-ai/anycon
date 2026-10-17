// Noise 加解密接口与 wasm 加载都来自 @anycon/protocol（`loadCrypto()`）；
// 单元测试用 src/test/fakeCrypto.ts 里实现同一接口的内存假货，快速测试不依赖 wasm。
export { loadCrypto } from '@anycon/protocol'
export type { Cpace, Crypto, EnrollCrypto, FullCrypto, Handshake, Keypair, Role, Transport } from '@anycon/protocol'
