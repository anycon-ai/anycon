// 协议的唯一导入点：类型 + 信封 / 控制消息 / JSON-RPC 编解码 + Noise 封装，全部来自 workspace 包 @anycon/protocol
// （真源是 crates/protocol）。业务代码一律从 '@/protocol' 导入，不要复制类型，也不要自己写编解码。
export * from '@anycon/protocol'
