//! 用户输入 → stream-json 用户消息（写进 stdin 的一行）。
//!
//! - 没有附件：`content` 是字符串，和附件功能出现之前完全一样。
//! - 有附件（ADR-35）：`content` 是数组——`image` block 在前（按附件顺序），然后是列出其它文件路径的 `text` block
//!   （[`file_reference_text`]），最后是用户的文字（为空就不放）。图片由 adapter 读 daemon 落盘的文件
//!   （`UserInput.attachment_paths`，与 `attachments` 一一对应）再转 base64；其它文件不读，只把路径告诉 Claude，
//!   由它用 Read 等工具自己读（文件在 daemon 的数据目录里，不在 workspace 里，读的时候可能要审批）。
//!
//! 一行可能有好几 MiB：stdin 这一侧没有行长限制（写任务把整行 `write_all`），只有 stdout 读取有上限。
//! 但一条输入里的图片 base64 合计不能超过 [`MAX_IMAGES_BASE64`]：Claude 的单次请求有总大小上限，超了的图一旦写进
//! Claude Code 的会话历史，之后每个 turn 都会带着它失败（ADR-35「2026-10 修订」）。daemon 已经按每条消息 / 每个 turn
//! 限了总量，这里是兜底。

use anycon_agent_api::{AdapterError, Result, UserInput};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};

use crate::common::{file_reference_text, is_inline_image};

/// 一条输入里所有图片 base64 之后的合计上限（Anthropic 的请求体上限是 32 MB，留出文字和历史的余量）。
pub const MAX_IMAGES_BASE64: u64 = 30 * 1024 * 1024;

/// 已经读出来、转好 base64 的一张图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub mime: String,
    /// 标准 base64（带填充）。
    pub data: String,
}

/// 一条输入的附件，准备好放进用户消息：图片的 base64，和列出其它文件路径的一段文字。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attachments {
    pub images: Vec<Image>,
    /// 不是图片的附件（[`file_reference_text`]）；没有就是 `None`。
    pub files: Option<String>,
}

impl Attachments {
    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.files.is_none()
    }
}

/// 校验 `input` 里的附件，读出图片。没有附件时什么都不做。
///
/// - `attachments` 和 `attachment_paths` 数量不一致 → `Protocol`（daemon 与 adapter 的约定被破坏）
/// - 文件不在 / 读不了 → `Io`（保留 `ErrorKind`，信息里带附件名），不会 panic；不是图片的附件也要是个普通文件
/// - 图片的 base64 合计超过 [`MAX_IMAGES_BASE64`] → `Unsupported`（按文件大小算，先于读内容）
///
/// 所有检查都先于读文件内容，出错时 adapter 的状态一概不变（调用方应在改状态之前调用）。
pub async fn load_attachments(input: &UserInput) -> Result<Attachments> {
    load_attachments_within(input, MAX_IMAGES_BASE64).await
}

async fn load_attachments_within(input: &UserInput, max_base64: u64) -> Result<Attachments> {
    if input.attachments.len() != input.attachment_paths.len() {
        return Err(AdapterError::Protocol(format!(
            "{} attachments but {} attachment paths",
            input.attachments.len(),
            input.attachment_paths.len()
        )));
    }
    let read_err = |name: &str, path: &std::path::Path, e: std::io::Error| {
        tracing::warn!(path = %path.display(), error = %e, "claude: cannot read an attachment");
        AdapterError::Io(std::io::Error::new(e.kind(), format!("cannot read attachment {name}: {e}")))
    };
    let mut total = 0u64;
    for (a, path) in input.attachments.iter().zip(&input.attachment_paths) {
        let meta = tokio::fs::metadata(path).await.map_err(|e| read_err(&a.name, path, e))?;
        if !meta.is_file() {
            return Err(read_err(&a.name, path, std::io::Error::other("not a regular file")));
        }
        if is_inline_image(&a.mime) {
            total += meta.len().div_ceil(3) * 4;
        }
    }
    if total > max_base64 {
        return Err(AdapterError::Unsupported(format!(
            "the images in this message add up to {} MiB once encoded, more than one Claude request can carry ({} MiB); send fewer or smaller images",
            total.div_ceil(1024 * 1024),
            max_base64 / (1024 * 1024)
        )));
    }
    let mut images = Vec::new();
    for (a, path) in input.attachments.iter().zip(&input.attachment_paths) {
        if is_inline_image(&a.mime) {
            let bytes = tokio::fs::read(path).await.map_err(|e| read_err(&a.name, path, e))?;
            images.push(Image { mime: a.mime.clone(), data: STANDARD.encode(&bytes) });
        }
    }
    let files = file_reference_text(input.attachments.iter().zip(input.attachment_paths.iter().map(|p| p.as_path())));
    Ok(Attachments { images, files })
}

/// stdin 上的一条用户消息。没有附件时 `content` 就是字符串。
pub fn user_message(text: &str, attachments: Attachments) -> Value {
    let content = if attachments.is_empty() {
        Value::String(text.to_string())
    } else {
        let mut blocks: Vec<Value> = attachments
            .images
            .into_iter()
            .map(|i| json!({ "type": "image", "source": { "type": "base64", "media_type": i.mime, "data": i.data } }))
            .collect();
        if let Some(files) = attachments.files {
            blocks.push(json!({ "type": "text", "text": files }));
        }
        if !text.is_empty() {
            blocks.push(json!({ "type": "text", "text": text }));
        }
        Value::Array(blocks)
    };
    json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "parent_tool_use_id": null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::AttachmentRef;

    fn att(name: &str, mime: &str) -> AttachmentRef {
        AttachmentRef { blob_id: "b".into(), name: name.into(), mime: mime.into(), bytes: 1, width: None, height: None }
    }

    fn input(text: &str, attachments: Vec<AttachmentRef>, paths: Vec<std::path::PathBuf>) -> UserInput {
        UserInput {
            message_id: "m".into(),
            content: text.into(),
            attachments,
            attachment_paths: paths,
            interject: false,
        }
    }

    #[test]
    fn plain_text_keeps_the_string_content() {
        assert_eq!(
            user_message("hello", Attachments::default()),
            json!({"type": "user", "message": {"role": "user", "content": "hello"}, "parent_tool_use_id": null})
        );
        // 空文字也是字符串，不变
        assert_eq!(user_message("", Attachments::default())["message"]["content"], json!(""));
    }

    #[test]
    fn images_come_first_then_the_text() {
        let msg = user_message(
            "what is this?",
            Attachments {
                images: vec![
                    Image { mime: "image/png".into(), data: "AAAA".into() },
                    Image { mime: "image/webp".into(), data: "BBBB".into() },
                ],
                files: None,
            },
        );
        assert_eq!(
            msg,
            json!({
                "type": "user",
                "message": {"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "BBBB"}},
                    {"type": "text", "text": "what is this?"},
                ]},
                "parent_tool_use_id": null,
            })
        );
    }

    #[test]
    fn no_text_block_when_the_text_is_empty() {
        let msg = user_message(
            "",
            Attachments { images: vec![Image { mime: "image/gif".into(), data: "AAAA".into() }], files: None },
        );
        assert_eq!(
            msg["message"]["content"],
            json!([{"type": "image", "source": {"type": "base64", "media_type": "image/gif", "data": "AAAA"}}])
        );
    }

    #[tokio::test]
    async fn load_attachments_reads_and_encodes_images_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.png"), dir.path().join("b.jpg"));
        std::fs::write(&a, [0x89, b'P', b'N', b'G']).unwrap();
        std::fs::write(&b, [0xFF, 0xD8, 0xFF]).unwrap();
        let inp = input("t", vec![att("a.png", "image/png"), att("b.jpg", "image/jpeg")], vec![a, b]);
        let loaded = load_attachments(&inp).await.unwrap();
        assert_eq!(
            loaded.images,
            vec![
                Image { mime: "image/png".into(), data: "iVBORw==".into() },
                Image { mime: "image/jpeg".into(), data: "/9j/".into() },
            ]
        );
        assert_eq!(loaded.files, None);
        assert!(load_attachments(&input("t", vec![], vec![])).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn other_files_are_listed_by_path_and_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let (img, pdf, txt) = (dir.path().join("a.png"), dir.path().join("b.pdf"), dir.path().join("c.bin"));
        std::fs::write(&img, [0x89, b'P', b'N', b'G']).unwrap();
        std::fs::write(&pdf, b"%PDF-1.7").unwrap();
        std::fs::write(&txt, b"hello").unwrap();
        let pdf_ref = AttachmentRef { bytes: 8, ..att("spec.pdf", "application/pdf") };
        let inp = input(
            "summarise",
            vec![att("a.png", "image/png"), pdf_ref, att("notes", "application/octet-stream")],
            vec![img, pdf.clone(), txt.clone()],
        );
        let loaded = load_attachments(&inp).await.unwrap();
        assert_eq!(loaded.images, vec![Image { mime: "image/png".into(), data: "iVBORw==".into() }]);
        let files = loaded.files.clone().unwrap();
        assert!(files.contains(&format!("- spec.pdf (application/pdf, 8 bytes): {}", pdf.display())), "{files}");
        assert!(files.contains(&format!("- notes (application/octet-stream, 1 bytes): {}", txt.display())), "{files}");
        assert!(!files.contains("a.png"), "{files}");

        let msg = user_message("summarise", loaded);
        let blocks = msg["message"]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[1], json!({"type": "text", "text": files}));
        assert_eq!(blocks[2], json!({"type": "text", "text": "summarise"}));

        // 只有文件、没有文字：一个 text block
        let only = input("", vec![att("b.pdf", "application/pdf")], vec![pdf]);
        let msg = user_message("", load_attachments(&only).await.unwrap());
        let blocks = msg["message"]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0]["text"].as_str().unwrap().starts_with("The user attached a file."));
    }

    #[tokio::test]
    async fn load_attachments_rejects_bad_input_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.png");
        std::fs::write(&ok, b"x").unwrap();

        // 数量对不上（两个方向）
        let e = load_attachments(&input("t", vec![att("a.png", "image/png")], vec![])).await.unwrap_err();
        assert!(matches!(e, AdapterError::Protocol(_)), "{e:?}");
        let e = load_attachments(&input("t", vec![], vec![ok.clone()])).await.unwrap_err();
        assert!(matches!(e, AdapterError::Protocol(_)), "{e:?}");

        // 读不了：文件不存在 / 是目录 → Io，信息里有附件名，不泄露目录；不是图片的附件也一样
        for (name, mime) in [("gone.png", "image/png"), ("gone.pdf", "application/pdf")] {
            let inp = input("t", vec![att(name, mime)], vec![dir.path().join(name)]);
            match load_attachments(&inp).await.unwrap_err() {
                AdapterError::Io(e) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
                    assert!(e.to_string().contains(name), "{e}");
                }
                other => panic!("{other:?}"),
            }
            let inp = input("t", vec![att(name, mime)], vec![dir.path().to_path_buf()]);
            assert!(matches!(load_attachments(&inp).await, Err(AdapterError::Io(_))));
        }
    }

    #[tokio::test]
    async fn the_encoded_total_of_one_message_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.png"), dir.path().join("b.png"));
        std::fs::write(&a, vec![0u8; 300]).unwrap(); // base64 400
        std::fs::write(&b, vec![0u8; 301]).unwrap(); // base64 404
        let inp = input("t", vec![att("a.png", "image/png"), att("b.png", "image/png")], vec![a.clone(), b.clone()]);
        assert_eq!(load_attachments_within(&inp, 804).await.unwrap().images.len(), 2);
        match load_attachments_within(&inp, 803).await.unwrap_err() {
            AdapterError::Unsupported(m) => assert!(m.contains("Claude request"), "{m}"),
            other => panic!("{other:?}"),
        }
        // 不是图片的附件不算进去（它们不进请求）
        let inp = input("t", vec![att("a.png", "image/png"), att("b.bin", "application/octet-stream")], vec![a, b]);
        assert_eq!(load_attachments_within(&inp, 400).await.unwrap().images.len(), 1);
        assert_eq!(MAX_IMAGES_BASE64, 30 * 1024 * 1024);
    }
}
