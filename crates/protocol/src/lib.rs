//! anycon 协议类型的唯一真源。
//!
//! 分两层（见 docs/protocol.md）：
//! - 外层 [`envelope::Envelope`]：hub 可见，MessagePack 编码。`kind = control` 的 body 是 [`control::Control`]。
//! - 内层 [`rpc`]：客户端 ↔ daemon 的 JSON-RPC 2.0，Noise 密文里。方法名常量在 [`methods`]，
//!   每个方法的 params / result 类型按域分布在 [`node`]、[`agent`]、[`workspace`]、[`session`]、[`permission`]、[`blob`]、[`enroll`]。
//! - 账号内接入（ADR-36）期间 `handshake` 信封的 body 是 [`enroll::EnrollFrame`]。
//!
//! 所有类型都派生 `serde` 和 `schemars::JsonSchema`；`anycon-gen-schema` 导出到 `schema/protocol.json`，
//! 再生成 TS 和 pydantic。改了这里要跑 `just gen` 并提交生成产物。

pub mod agent;
pub mod blob;
pub mod control;
pub mod enroll;
pub mod envelope;
pub mod error;
pub mod methods;
pub mod node;
pub mod permission;
pub mod rpc;
pub mod schema;
pub mod session;
pub mod workspace;

pub use agent::*;
pub use blob::*;
pub use control::*;
pub use enroll::*;
pub use envelope::*;
pub use error::*;
pub use node::*;
pub use permission::*;
pub use rpc::*;
pub use session::*;
pub use workspace::*;

/// 内层 JSON-RPC 协议版本。相邻 minor 兼容，0.x 阶段可 break。
pub const PROTOCOL_VERSION: u32 = 1;
/// 外层信封版本。极少改动。
pub const ENVELOPE_VERSION: u8 = 1;

/// 设备 id：32 位小写 hex（128 bit 随机）。daemon、web、cli、sdk 都是设备。
pub type DeviceId = String;
/// 会话 id：daemon 生成，`sess_` + 22 位 base62。
pub type SessionId = String;
/// Unix 毫秒时间戳。
pub type TimestampMs = u64;

/// 短码字母表：大写字母数字去掉 0 O 1 I L。8 位 ≈ 40 bit。
pub const PAIR_CODE_ALPHABET: &str = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";
pub const PAIR_CODE_LEN: usize = 8;
