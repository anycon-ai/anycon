//! 生成 `packages/protocol/tests/vectors/`：
//! - 每个向量一个 `<name>.hex`（Rust 编码器的真实输出，小写 hex，一行）；
//! - 一个 `manifest.json`：向量名、类型（envelope / control / rpc）、逻辑值（供 TS 解码后比对）；
//! - 一个 `frames.json`：`data` 信封 body 框架（`envelope::frame`，ADR-30）的编码 / 解码 / 重组向量。
//!   大消息的密文不落盘：约定 `ciphertext = (i % 251) as u8` 的确定性图样，只存各片的头和长度，TS 侧用同样的图样还原。
//!
//! 用法（在仓库根）：
//!   CARGO_TARGET_DIR=$PWD/target cargo run -q --manifest-path packages/protocol/tools/gen-vectors/Cargo.toml
//! 或 `just gen-vectors`。输出确定性（无随机、无时间），重复运行 git 不应有 diff。

use std::fs;
use std::path::PathBuf;

use anycon_protocol::*;
use serde_json::{json, Value};

const DEVICE_A: &str = "0123456789abcdef0123456789abcdef";
const DEVICE_B: &str = "fedcba9876543210fedcba9876543210";

struct Vector {
    name: String,
    kind: &'static str,
    bytes: Vec<u8>,
    value: Value,
}

fn control(name: &str, c: Control) -> Vector {
    let bytes = rmp_serde::to_vec_named(&c).expect("encode control");
    Vector { name: format!("control_{name}"), kind: "control", bytes, value: serde_json::to_value(&c).expect("json") }
}

fn envelope(name: &str, from: &str, to: &str, kind: Kind, seq: u64, body: Vec<u8>) -> Vector {
    let e = Envelope { v: ENVELOPE_VERSION, from: from.into(), to: to.into(), kind, seq, body: body.clone() };
    let bytes = e.encode().expect("encode envelope");
    let value = json!({
        "v": e.v,
        "from": e.from,
        "to": e.to,
        "kind": serde_json::to_value(kind).expect("kind"),
        "seq": e.seq,
        "body_hex": hex::encode(&body),
    });
    Vector { name: format!("envelope_{name}"), kind: "envelope", bytes, value }
}

fn rpc(name: &str, m: RpcMessage) -> Vector {
    let bytes = m.encode();
    let value: Value = serde_json::from_slice(&bytes).expect("rpc is json");
    Vector { name: format!("rpc_{name}"), kind: "rpc", bytes, value }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn main() {
    let out: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors")));
    fs::create_dir_all(&out).expect("mkdir");

    let sdk_client = ClientInfo {
        name: "ci-runner".into(),
        kind: DeviceKind::Sdk,
        platform: "linux-x86_64".into(),
        version: "0.1.0".into(),
        origin: None,
    };
    let web_client = ClientInfo {
        name: "Chrome · macOS · hub.example.com".into(),
        kind: DeviceKind::Web,
        platform: "browser".into(),
        version: "0.1.0".into(),
        origin: Some("https://hub.example.com".into()),
    };
    let quota = Quota {
        devices: 2,
        devices_max: Some(10),
        daemons: 1,
        daemons_max: None,
        connections: 300,
        connections_max: Some(70000),
    };

    let auth_body = rmp_serde::to_vec_named(&Control::Auth {
        device_token: None,
        protocol_version: PROTOCOL_VERSION,
        client: sdk_client.clone(),
    })
    .expect("auth body");

    let mut v: Vec<Vector> = vec![
        // ----- 信封：覆盖 seq 的 fixint / u16 / u32 / u64 与 body 的空 / bin8 / bin16 -----
        envelope("handshake_small", DEVICE_A, DEVICE_B, Kind::Handshake, 1, vec![0, 1, 2, 3]),
        envelope("data_bin8_max", DEVICE_A, DEVICE_B, Kind::Data, 300, pattern(255)),
        envelope("data_bin16_min", DEVICE_B, DEVICE_A, Kind::Data, 70_000, pattern(256)),
        envelope("data_seq_u64", DEVICE_A, DEVICE_B, Kind::Data, 4_294_967_296, pattern(40)),
        envelope("notify_empty_body", DEVICE_A, DEVICE_B, Kind::Notify, 0, vec![]),
        envelope("control_auth", "", "", Kind::Control, 1, auth_body),
        // ----- 控制消息：全部 22 个变体，可选字段有 / 无各覆盖 -----
        control("auth_no_token", Control::Auth { device_token: None, protocol_version: PROTOCOL_VERSION, client: sdk_client }),
        control(
            "auth_with_token_web",
            Control::Auth { device_token: Some("tok_7f3a9c1e5b2d4f60".into()), protocol_version: PROTOCOL_VERSION, client: web_client },
        ),
        control(
            "auth_ok",
            Control::AuthOk {
                device_id: DEVICE_A.into(),
                server_time_ms: 1_760_000_000_000,
                peers_online: vec![DEVICE_B.into(), "00112233445566778899aabbccddeeff".into()],
                quota: None,
            },
        ),
        control(
            "auth_ok_quota_empty_peers",
            Control::AuthOk { device_id: DEVICE_A.into(), server_time_ms: 42, peers_online: vec![], quota: Some(quota.clone()) },
        ),
        control("auth_fail", Control::AuthFail { reason: AuthFailReason::Revoked }),
        control("pair_begin", Control::PairBegin { match_id: "0123456789abcdef".into() }),
        control("pair_registered", Control::PairRegistered { expires_at_ms: 1_760_000_300_000 }),
        control("pair_join", Control::PairJoin { match_id: "0123456789abcdef".into() }),
        control("pair_matched", Control::PairMatched { peer: DEVICE_B.into(), role: PairRole::Joiner }),
        control("pair_confirm", Control::PairConfirm { peer: DEVICE_B.into() }),
        control(
            "pair_done_joiner",
            Control::PairDone {
                peer: DEVICE_B.into(),
                device_id: Some(DEVICE_A.into()),
                device_token: Some("tok_0a1b2c3d4e5f60718293a4b5c6d7e8f9".into()),
            },
        ),
        control("pair_done_host", Control::PairDone { peer: DEVICE_A.into(), device_id: None, device_token: None }),
        control("pair_failed", Control::PairFailed { reason: PairFailReason::Timeout }),
        control("enroll_begin", Control::EnrollBegin { node: DEVICE_B.into() }),
        control(
            "enroll_started",
            Control::EnrollStarted {
                enroll_id: "00112233445566778899aabbccddeeff".into(),
                node: DEVICE_B.into(),
                expires_at_ms: 1_700_000_300_000,
            },
        ),
        control(
            "enroll_request",
            Control::EnrollRequest {
                enroll_id: "00112233445566778899aabbccddeeff".into(),
                peer: DEVICE_A.into(),
                expires_at_ms: 1_700_000_300_000,
            },
        ),
        control("enroll_cancel", Control::EnrollCancel { enroll_id: "00112233445566778899aabbccddeeff".into() }),
        control(
            "enroll_failed",
            Control::EnrollFailed {
                enroll_id: Some("00112233445566778899aabbccddeeff".into()),
                reason: EnrollFailReason::Offline,
            },
        ),
        control("enroll_failed_no_id", Control::EnrollFailed { enroll_id: None, reason: EnrollFailReason::Forbidden }),
        control("revoke", Control::Revoke { device: DEVICE_B.into() }),
        control("peer_online", Control::PeerOnline { device: DEVICE_B.into() }),
        control("peer_offline", Control::PeerOffline { device: DEVICE_B.into() }),
        control("undeliverable", Control::Undeliverable { to: DEVICE_B.into(), seq: 17 }),
        control("queue_overflow", Control::QueueOverflow { to: DEVICE_B.into(), dropped_seqs: vec![3, 4, 300, 70_000] }),
        control("queue_drop", Control::QueueDrop { seq: 9 }),
        control("backpressure", Control::Backpressure { to: DEVICE_B.into(), seq: 256 }),
        control("dropped_too_large", Control::Dropped { seq: 70_000, reason: DropReason::TooLarge }),
        control("dropped_not_paired", Control::Dropped { seq: 3, reason: DropReason::NotPaired }),
        control("dropped_rate_limited", Control::Dropped { seq: 300, reason: DropReason::RateLimited }),
        control("rate_limited", Control::RateLimited { retry_after_ms: 1500 }),
        control("quota_update", Control::QuotaUpdate { quota }),
        control("rotate_token", Control::RotateToken {}),
        control("token_rotated", Control::TokenRotated { device_token: "tok_new".into() }),
        // ----- JSON-RPC：serde_json 紧凑输出，键序 jsonrpc / id / method / params -----
        rpc("request_num_id", RpcMessage::request(1u64, "node.info", json!({}))),
        rpc(
            "request_str_id",
            RpcMessage::request(
                "c3b1",
                "session.send",
                json!({ "session_id": "sess_abc", "message_id": "m-1", "content": "跑一下测试 ✅" }),
            ),
        ),
        rpc("notification", RpcMessage::notification("session.watch", json!({ "session_id": "sess_abc" }))),
        rpc("response_result", RpcMessage::result(RpcId::Num(7), json!({ "turn_id": 3, "queued": false }))),
        rpc(
            "response_error_settings",
            RpcMessage::error(RpcId::Num(8), RpcError::invalid_settings("thinking", "not supported by model")),
        ),
        rpc("response_error_not_found", RpcMessage::error(RpcId::Str("x".into()), RpcError::new(ErrorCode::NotFound, "no such session"))),
    ];

    v.sort_by(|a, b| a.name.cmp(&b.name));

    // 清掉旧的 .hex，避免 Control 变体改名后留下过期文件（只动 .hex 和 manifest / frames）。
    if let Ok(rd) = fs::read_dir(&out) {
        for entry in rd.flatten() {
            if entry.path().extension().is_some_and(|e| e == "hex") {
                fs::remove_file(entry.path()).expect("remove stale hex");
            }
        }
    }

    let mut manifest = Vec::new();
    for vec in &v {
        fs::write(out.join(format!("{}.hex", vec.name)), format!("{}\n", hex::encode(&vec.bytes))).expect("write hex");
        manifest.push(json!({ "name": vec.name, "type": vec.kind, "value": vec.value }));
    }
    let text = serde_json::to_string_pretty(&manifest).expect("manifest") + "\n";
    fs::write(out.join("manifest.json"), text).expect("write manifest");
    eprintln!("wrote {} vectors to {}", v.len(), out.display());

    let frames = frame_vectors();
    let text = serde_json::to_string_pretty(&frames).expect("frames") + "\n";
    fs::write(out.join("frames.json"), text).expect("write frames");
    eprintln!("wrote {} frame vectors", frames.as_array().map_or(0, Vec::len));
}

// ---------------------------------------------------------------------------
// data 信封 body 框架（envelope::frame）
// ---------------------------------------------------------------------------

/// 手工拼 FRAGMENT body：独立于 `frame::encode`，用来构造 encode 不会产生的分片序列（小片、乱序、重复……）。
fn fragment_body(msg_id: u32, index: u16, count: u16, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0x01];
    b.extend_from_slice(&msg_id.to_be_bytes());
    b.extend_from_slice(&index.to_be_bytes());
    b.extend_from_slice(&count.to_be_bytes());
    b.extend_from_slice(data);
    b
}

fn frame_error_name(e: &frame::FrameError) -> &'static str {
    match e {
        frame::FrameError::Empty => "empty",
        frame::FrameError::UnknownFlags(_) => "unknown_flags",
        frame::FrameError::Truncated => "truncated",
        frame::FrameError::OutOfOrder { .. } => "out_of_order",
        frame::FrameError::CountMismatch => "count_mismatch",
        frame::FrameError::TooLarge => "too_large",
    }
}

/// Rust 解码一个 body 的结果，写成 TS 好比对的 JSON。
fn decode_outcome(body: &[u8]) -> Value {
    match frame::decode(body) {
        Ok(frame::DataFrame::Whole(d)) => json!({ "ok": "whole", "data_hex": hex::encode(d) }),
        Ok(frame::DataFrame::Reset) => json!({ "ok": "reset" }),
        Ok(frame::DataFrame::Fragment { msg_id, index, count, data }) => {
            json!({ "ok": "fragment", "msg_id": msg_id, "index": index, "count": count, "data_hex": hex::encode(data) })
        }
        Err(e) => json!({ "err": frame_error_name(&e) }),
    }
}

fn frame_vectors() -> Value {
    let mut out: Vec<Value> = Vec::new();

    // ----- encode：大消息只存头 + 长度，密文 = pattern(ciphertext_len) -----
    let encode_case = |name: &str, len: usize, msg_id: u32| -> Value {
        let ct = pattern(len);
        let bodies = frame::encode(&ct, msg_id);
        let mut offset = 0usize;
        let frames: Vec<Value> = bodies
            .iter()
            .map(|b| {
                let header_len = if b[0] == frame::FRAGMENT { 9 } else { 1 };
                let data_len = b.len() - header_len;
                assert_eq!(&b[header_len..], &ct[offset..offset + data_len], "body payload must be a contiguous slice of the ciphertext");
                let entry = json!({
                    "header_hex": hex::encode(&b[..header_len]),
                    "data_len": data_len,
                    "body_hex": if b.len() <= 1024 { Value::String(hex::encode(b)) } else { Value::Null },
                });
                offset += data_len;
                entry
            })
            .collect();
        assert_eq!(offset, ct.len());
        json!({ "kind": "encode", "name": name, "msg_id": msg_id, "ciphertext_len": len, "pattern": "i%251", "frames": frames })
    };
    out.push(encode_case("encode_whole_empty", 0, 1));
    out.push(encode_case("encode_whole_small", 100, 1));
    out.push(encode_case("encode_whole_exact_max_fragment", frame::MAX_FRAGMENT, 7));
    out.push(encode_case("encode_two_fragments_min", frame::MAX_FRAGMENT + 1, 0x0102_0304));
    out.push(encode_case("encode_three_fragments", frame::MAX_FRAGMENT * 2 + 10, 42));
    out.push(encode_case("encode_exact_two_fragments", frame::MAX_FRAGMENT * 2, 0xffff_ffff));
    out.push(encode_case("encode_one_mebibyte", 1024 * 1024, 9));
    assert_eq!(frame::reset_body(), vec![0x02]);
    out.push(json!({ "kind": "reset_body", "name": "reset_body", "body_hex": hex::encode(frame::reset_body()) }));

    // ----- decode：各种坏 / 好的 body -----
    let mut decode_case = |name: &str, body: Vec<u8>| {
        out.push(json!({ "kind": "decode", "name": name, "body_hex": hex::encode(&body), "outcome": decode_outcome(&body) }));
    };
    decode_case("decode_empty", vec![]);
    decode_case("decode_whole_empty_payload", vec![0x00]);
    decode_case("decode_whole", vec![0x00, 1, 2, 3]);
    decode_case("decode_reset", vec![0x02]);
    decode_case("decode_reset_with_trailing", vec![0x02, 9, 9]);
    decode_case("decode_unknown_flags_3", vec![0x03, 1]);
    decode_case("decode_unknown_flags_ff", vec![0xff]);
    decode_case("decode_fragment_truncated_0", vec![0x01]);
    decode_case("decode_fragment_truncated_7", vec![0x01, 0, 0, 0, 1, 0, 0, 0]);
    decode_case("decode_fragment_header_only", fragment_body(5, 0, 2, &[]));
    decode_case("decode_fragment_max_fields", fragment_body(0xffff_ffff, 0xffff, 0xffff, &[0xaa]));
    decode_case("decode_fragment_count_zero", fragment_body(5, 0, 0, &[1]));

    // ----- reassemble：一串 body 依次 push，记录每一步的结果（含出错后的状态清理） -----
    let mut scenario = |name: &str, bodies: Vec<Vec<u8>>| {
        let mut r = frame::Reassembler::new();
        let steps: Vec<Value> = bodies
            .iter()
            .map(|b| match frame::decode(b) {
                Err(e) => json!({ "body_hex": hex::encode(b), "decode_err": frame_error_name(&e) }),
                Ok(f) => match r.push(f) {
                    Ok(Some(buf)) => json!({ "body_hex": hex::encode(b), "out_hex": hex::encode(buf) }),
                    Ok(None) => json!({ "body_hex": hex::encode(b), "out": null }),
                    Err(e) => json!({ "body_hex": hex::encode(b), "err": frame_error_name(&e) }),
                },
            })
            .collect();
        out.push(json!({ "kind": "reassemble", "name": name, "steps": steps }));
    };
    scenario("reassemble_whole", vec![vec![0x00, 1, 2, 3]]);
    scenario("reassemble_in_order", vec![fragment_body(1, 0, 3, &[1, 2]), fragment_body(1, 1, 3, &[3]), fragment_body(1, 2, 3, &[4, 5, 6])]);
    scenario("reassemble_single_fragment", vec![fragment_body(8, 0, 1, &[7, 7])]);
    scenario("reassemble_out_of_order_first", vec![fragment_body(1, 1, 2, &[1])]);
    scenario("reassemble_out_of_order_skip", vec![fragment_body(1, 0, 3, &[1]), fragment_body(1, 2, 3, &[3])]);
    scenario("reassemble_error_clears_state", vec![fragment_body(1, 0, 3, &[1]), fragment_body(1, 2, 3, &[3]), fragment_body(1, 1, 3, &[2])]);
    scenario("reassemble_new_msg_id_restarts", vec![fragment_body(1, 0, 2, &[1]), fragment_body(2, 0, 2, &[9]), fragment_body(2, 1, 2, &[8])]);
    scenario("reassemble_new_msg_id_mid_message", vec![fragment_body(1, 0, 3, &[1]), fragment_body(2, 1, 3, &[9])]);
    scenario("reassemble_whole_drops_pending", vec![fragment_body(1, 0, 2, &[1]), vec![0x00, 5], fragment_body(1, 1, 2, &[2])]);
    scenario("reassemble_reset_drops_pending", vec![fragment_body(1, 0, 2, &[1]), vec![0x02], fragment_body(1, 1, 2, &[2])]);
    scenario("reassemble_count_zero", vec![fragment_body(1, 0, 0, &[1])]);
    scenario("reassemble_count_changes_midway", vec![fragment_body(1, 0, 3, &[1]), fragment_body(1, 1, 2, &[2])]);
    scenario("reassemble_back_to_back_messages", vec![fragment_body(1, 0, 2, &[1]), fragment_body(1, 1, 2, &[2]), fragment_body(2, 0, 2, &[3]), fragment_body(2, 1, 2, &[4])]);
    // 恶意头部：index >= count、index = u16::MAX、count 中途变化（变大 / 变小）、出错后状态被清空、
    // count = 1 的单片 FRAGMENT、完成后同一 msg_id 再来要从 0 开始。
    scenario("reassemble_index_equals_count", vec![fragment_body(1, 2, 2, &[1])]);
    scenario("reassemble_index_u16_max", vec![fragment_body(1, 0xffff, 2, &[1])]);
    scenario("reassemble_index_ge_count_mid_message", vec![fragment_body(1, 0, 3, &[1]), fragment_body(1, 3, 3, &[2]), fragment_body(1, 1, 3, &[3])]);
    scenario("reassemble_count_grows_midway", vec![fragment_body(7, 0, 3, &[1]), fragment_body(7, 1, 4, &[2])]);
    scenario("reassemble_count_change_clears_state", vec![fragment_body(7, 0, 3, &[1]), fragment_body(7, 1, 4, &[2]), fragment_body(7, 1, 3, &[3])]);
    scenario("reassemble_count_u16_max_single", vec![fragment_body(9, 0, 0xffff, &[1])]);
    scenario("reassemble_count_one_twice", vec![fragment_body(3, 0, 1, &[1]), fragment_body(3, 0, 1, &[2])]);
    scenario("reassemble_same_msg_id_after_complete", vec![fragment_body(3, 0, 1, &[1]), fragment_body(3, 1, 2, &[2])]);

    Value::Array(out)
}
