//! 附件 / blob 通道（ADR-35）：客户端把文件（图片或任意文件）分块传到 daemon，daemon 落盘并校验；会话消息只带 [`AttachmentRef`]。
//!
//! 流程：`blob.put_begin` → 若干 `blob.put_chunk`（可乱序、可并发、可重发）→ `blob.put_end` 得到 `AttachmentRef`
//! → 放进 `session.send` / `session.create` 的 `attachments`。没被消息引用的 blob 过一段时间由 daemon 回收；
//! `blob.discard` 让客户端主动丢掉（用户在输入区移除了附件）。下载用 `blob.get` 按偏移分页拉。
//!
//! 分块在内层，hub 看到的只是若干个 `data` 信封。

use serde::{Deserialize, Serialize};

use crate::{AttachmentRef, TimestampMs};

/// `blob.put_begin`：开始一次上传。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobPutBeginParams {
    /// 展示用的文件名（1–255 字节，不能含 `/` `\` 和控制字符）。daemon 不用它拼路径。
    pub name: String,
    /// `type/subtype`（不带参数，≤ 127 字节，大小写不敏感，daemon 记成小写）；不知道类型时用 `application/octet-stream`。
    /// `image/png` `image/jpeg` `image/gif` `image/webp` 四种会核对文件头、读宽高，其它类型不看内容。
    pub mime: String,
    /// 总字节数，`1 ..= sessions.max_attachment_bytes`。
    pub size: u64,
    /// 整个文件的 SHA-256，64 位小写 hex；`put_end` 校验。
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobPutBeginResult {
    /// 32 位小写 hex。
    pub blob_id: String,
    /// 分块字节数（目前 512 KiB）。除最后一块外每块恰好这么大；块数 = `ceil(size / chunk_size)`。
    pub chunk_size: u32,
    /// 这次上传在此之前没有 `put_end` 就作废（已收的分块丢弃）。每收到一块顺延。
    pub expires_at_ms: TimestampMs,
}

/// `blob.put_chunk`：一块数据。偏移 = `index * chunk_size`。同一块重发是幂等的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobPutChunkParams {
    pub blob_id: String,
    pub index: u32,
    /// 标准 base64（带 padding）。解码后的长度必须正好是这一块应有的长度。
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobIdParams {
    pub blob_id: String,
}

/// `blob.get`：按偏移读一段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobGetParams {
    pub blob_id: String,
    #[serde(default)]
    pub offset: u64,
    /// 缺省和上限都是 512 KiB。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobGetResult {
    /// daemon 记录的元数据（和 `user_message.attachments` 里的一致）。
    pub blob: AttachmentRef,
    pub offset: u64,
    /// 标准 base64（带 padding）。
    pub data: String,
    /// 这一段读到了文件末尾。
    pub eof: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_params_defaults() {
        let p: BlobGetParams = serde_json::from_str(r#"{"blob_id":"ab"}"#).unwrap();
        assert_eq!(p.offset, 0);
        assert_eq!(p.max_bytes, None);
        assert_eq!(serde_json::to_string(&p).unwrap(), r#"{"blob_id":"ab","offset":0}"#);
    }
}
