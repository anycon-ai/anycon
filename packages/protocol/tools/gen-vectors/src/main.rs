//! 生成 `packages/protocol/tests/vectors/`：
//! - 每个向量一个 `<name>.hex`（Rust 编码器的真实输出，小写 hex，一行）；
//! - 一个 `manifest.json`：向量名、类型（envelope / control / rpc）、逻辑值（供 TS 解码后比对）；
//! - 一个 `frames.json`：`data` 信封 body 框架（`envelope::frame`，ADR-30）的编码 / 解码 / 重组向量。
//!   大消息的密文不落盘：约定 `ciphertext = (i % 251) as u8` 的确定性图样，只存各片的头和长度，TS 侧用同样的图样还原。
//! - 一个 `preview.json`：预览隧道（`preview::tunnel`，ADR-39）的消息、帧载荷、头部、握手 body、prologue、label 向量。
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
    // 分身设备（另一个节点的分身，docs/spec/daemon.md § 27）：种类 `twin`
    let twin_client = ClientInfo {
        name: "mac-mini 的分身".into(),
        kind: DeviceKind::Twin,
        platform: "macos-aarch64".into(),
        version: "0.1.0".into(),
        origin: None,
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
        preview_token: None,
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
        control(
            "auth_no_token",
            Control::Auth {
                device_token: None,
                protocol_version: PROTOCOL_VERSION,
                client: sdk_client,
                preview_token: None,
            },
        ),
        control(
            "auth_with_token_twin",
            Control::Auth {
                device_token: Some("tok_5c2e8b1a7d9f4e30".into()),
                protocol_version: PROTOCOL_VERSION,
                client: twin_client,
                preview_token: None,
            },
        ),
        control(
            "auth_with_token_web",
            Control::Auth {
                device_token: Some("tok_7f3a9c1e5b2d4f60".into()),
                protocol_version: PROTOCOL_VERSION,
                client: web_client,
                preview_token: None,
            },
        ),
        control(
            "auth_preview",
            Control::Auth {
                device_token: None,
                protocol_version: PROTOCOL_VERSION,
                client: ClientInfo {
                    name: "preview".into(),
                    kind: DeviceKind::Web,
                    platform: "browser".into(),
                    version: "0.1.1".into(),
                    origin: Some("https://abcdefghijklmnopqrstuvwxyz.aiplace.app".into()),
                },
                preview_token: Some("pvt_0f1e2d3c4b5a6978".into()),
            },
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
            Control::AuthOk {
                device_id: DEVICE_A.into(),
                server_time_ms: 42,
                peers_online: vec![],
                quota: Some(quota.clone()),
            },
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
        control(
            "queue_overflow",
            Control::QueueOverflow { to: DEVICE_B.into(), dropped_seqs: vec![3, 4, 300, 70_000] },
        ),
        control("queue_drop", Control::QueueDrop { seq: 9 }),
        control("backpressure", Control::Backpressure { to: DEVICE_B.into(), seq: 256 }),
        control("dropped_too_large", Control::Dropped { seq: 70_000, reason: DropReason::TooLarge }),
        control("dropped_not_paired", Control::Dropped { seq: 3, reason: DropReason::NotPaired }),
        control("dropped_rate_limited", Control::Dropped { seq: 300, reason: DropReason::RateLimited }),
        control("rate_limited", Control::RateLimited { retry_after_ms: 1500, to: None }),
        control("rate_limited_to", Control::RateLimited { retry_after_ms: 200, to: Some(DEVICE_B.into()) }),
        control(
            "preview_peer",
            Control::PreviewPeer { peer: DEVICE_B.into(), label: "abcdefghijklmnopqrstuvwxyz".into() },
        ),
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
        rpc(
            "response_error_not_found",
            RpcMessage::error(RpcId::Str("x".into()), RpcError::new(ErrorCode::NotFound, "no such session")),
        ),
    ];

    let run = ExtractRun {
        id: "run-1".into(),
        session_id: "sess_abc".into(),
        trigger: ExtractTrigger::Manual,
        device_id: Some(DEVICE_A.into()),
        status: ExtractRunStatus::Failed,
        started_ms: 1700000000000,
        finished_ms: Some(1700000000100),
        points: 2,
        from_seq: 3,
        to_seq: 8,
        added: vec![],
        dropped: ExtractDropped { invalid: 1, duplicate: 0, full: 0 },
        backend: "local".into(),
        error: Some(JudgeError { code: JudgeErrorCode::Http, status: Some(503) }),
        usage: Some(JudgeUsage { input_tokens: Some(10), output_tokens: Some(3), cost_usd: Some(0.001) }),
    };
    v.extend([
        rpc(
            "rule_add_group",
            RpcMessage::request(901u64, methods::RULE_ADD, json!({
                "id": "11111111-2222-4333-8444-555555555555", "text": "Ask before adding a dependency.", "status": "disabled", "sync": "group"
            })),
        ),
        rpc(
            "rule_update_group",
            RpcMessage::request(902u64, methods::RULE_UPDATE, json!({
                "id": "11111111-2222-4333-8444-555555555555", "status": "active", "sync": "group"
            })),
        ),
        rpc(
            "rule_update_restore",
            RpcMessage::request(903u64, methods::RULE_UPDATE, json!({
                "id": "11111111-2222-4333-8444-555555555555", "stale": false, "if_updated_ms": 1700000000000u64
            })),
        ),
        rpc(
            "rule_extract",
            RpcMessage::request(
                20u64,
                methods::RULE_EXTRACT,
                serde_json::to_value(RuleExtractParams { session_id: "sess_abc".into() }).unwrap(),
            ),
        ),
        rpc(
            "rule_extract_result",
            RpcMessage::result(
                RpcId::Num(20),
                serde_json::to_value(RuleExtractResult { run: Some(run.clone()), points: 2 }).unwrap(),
            ),
        ),
        rpc(
            "rule_extract_empty",
            RpcMessage::result(
                RpcId::Num(21),
                serde_json::to_value(RuleExtractResult { run: None, points: 0 }).unwrap(),
            ),
        ),
        rpc(
            "rule_extract_runs",
            RpcMessage::request(
                22u64,
                methods::RULE_EXTRACT_RUNS,
                serde_json::to_value(RuleExtractRunsParams { session_id: Some("sess_abc".into()), limit: Some(10) })
                    .unwrap(),
            ),
        ),
        rpc(
            "rule_extract_runs_result",
            RpcMessage::result(
                RpcId::Num(22),
                serde_json::to_value(RuleExtractRunsResult { runs: vec![run.clone()], used_today: 1, daily_calls: 20 })
                    .unwrap(),
            ),
        ),
        rpc(
            "rule_extract_done",
            RpcMessage::notification(
                methods::RULE_EXTRACT_DONE,
                serde_json::to_value(RuleExtractDone { run }).unwrap(),
            ),
        ),
    ]);
    // 分身协议的请求、响应和通知由 Rust 类型序列化，供 TS 编解码逐字节核对。
    let scope = TwinScope {
        paused: false,
        projects: vec!["app".into()],
        agent: Some("mock".into()),
        model: None,
        mode: None,
        max_sessions: 2,
        daily_dispatches: 10,
    };
    let twin = TwinGetResult {
        scope,
        state: TwinState::Idle,
        enabled: true,
        configured: true,
        today: TwinToday { dispatches: 1, judge_calls: 2 },
        working: vec!["t".into()],
        waiting: vec![],
        last_run: Some(TwinRunBrief {
            id: "r".into(),
            started_ms: 1,
            finished_ms: Some(2),
            triggers: vec![TwinTrigger::Tick],
            result: TwinRunResult::Done,
            error: None,
        }),
        next_wake_ms: Some(60_000),
        queued: vec!["u".into()],
        deferred: vec!["v".into()],
    };
    let question = TwinQuestion {
        id: "q".into(),
        todo_id: "t".into(),
        question: "Which project?".into(),
        options: vec!["app".into()],
        created_ms: 1,
        answered_ms: Some(2),
        answer: Some("app".into()),
        answered_by: Some(TodoActor { kind: TodoSourceKind::Device, id: None, device_id: Some(DEVICE_A.into()) }),
        status: TwinQuestionStatus::Answered,
    };
    let run = TwinRun {
        id: "r".into(),
        requester: "local".into(),
        started_ms: 1,
        finished_ms: Some(2),
        triggers: vec![TwinTrigger::Todo, TwinTrigger::Manual, TwinTrigger::Pr],
        dry_run: true,
        result: TwinRunResult::Done,
        items: vec!["t".into()],
        judge_calls: vec!["j".into()],
        decisions: vec![TwinDecision {
            note: Some("Keep changes small".into()),
            question: None,
            options: vec![],
            todo_id: "t".into(),
            action: TwinAction::Dispatch,
            reason: "Ready".into(),
            rules: vec!["rule".into()],
            outcome: TwinDecisionOutcome::Skipped,
            skip_reason: Some(TwinSkipReason::NoCapacity),
            session_id: None,
            question_id: None,
            project: Some("app".into()),
            node: None,
        }],
        error: None,
        usage: Some(JudgeUsage { input_tokens: Some(10), output_tokens: Some(3), cost_usd: Some(0.001) }),
    };
    let stats = TwinStatsResult {
        days: vec![TwinDayStats {
            date: "2026-01-02".into(),
            start_ms: 1_767_283_200_000,
            runs: TwinRunCounts { done: 2, idle: 1, budget: 0, failed: 1, cancelled: 0 },
            dry_runs: 1,
            judge_calls: 5,
            usage: Some(JudgeUsage { input_tokens: Some(100), output_tokens: Some(30), cost_usd: Some(0.01) }),
            decisions: TwinDecisionCounts { dispatch: 2, ask_user: 1, ask_advisor: 0, wait: 1, hand_back: 0, done: 1 },
            skipped: 1,
            handed_back: 1,
            taken_over: 0,
            accepted: 1,
            dropped: 0,
            turnaround_ms: 3_600_000,
        }],
    };
    let peer_status = TwinPeerStatus {
        name: "mac-mini".into(),
        group: Some("home".into()),
        projects: vec!["app".into()],
        agents: vec![TwinPeerAgent { id: "mock".into(), label: "Mock".into(), ready: true }],
        remaining_sessions: 1,
        remaining_dispatches: 9,
        paused: false,
        accept_links: true,
    };
    let link = TwinLink {
        node_id: DEVICE_B.into(),
        name: "mac-mini".into(),
        group: Some("home".into()),
        fingerprint: "ab:cd:ef".into(),
        paired_ms: 1,
        online: true,
        status: Some(peer_status.clone()),
    };
    for (name, method, params, result) in [
        ("get", methods::TWIN_GET, json!({}), serde_json::to_value(&twin).unwrap()),
        (
            "get_requester",
            methods::TWIN_GET,
            serde_json::to_value(TwinGetParams { requester: Some(DEVICE_A.into()) }).unwrap(),
            serde_json::to_value(&twin).unwrap(),
        ),
        (
            "set",
            methods::TWIN_SET,
            serde_json::to_value(TwinSetParams {
                requester: Some(DEVICE_A.into()),
                paused: Some(true),
                clear: vec![ClearableTwinField::Agent, ClearableTwinField::Model, ClearableTwinField::Mode],
                ..Default::default()
            })
            .unwrap(),
            serde_json::to_value(&twin).unwrap(),
        ),
        ("wake", methods::TWIN_WAKE, json!({}), json!({})),
        (
            "dry_run",
            methods::TWIN_DRY_RUN,
            json!({}),
            serde_json::to_value(TwinDryRunResult { run: run.clone() }).unwrap(),
        ),
        (
            "runs",
            methods::TWIN_RUNS,
            serde_json::to_value(TwinRunsParams { limit: Some(10), before_ms: Some(9) }).unwrap(),
            serde_json::to_value(TwinRunsResult { runs: vec![run.clone()] }).unwrap(),
        ),
        (
            "stats",
            methods::TWIN_STATS,
            serde_json::to_value(TwinStatsParams { days: Some(7) }).unwrap(),
            serde_json::to_value(&stats).unwrap(),
        ),
        (
            "questions",
            methods::TWIN_QUESTIONS,
            serde_json::to_value(TwinQuestionsParams { open_only: Some(true) }).unwrap(),
            serde_json::to_value(TwinQuestionsResult { questions: vec![question.clone()] }).unwrap(),
        ),
        (
            "answer",
            methods::TWIN_ANSWER,
            serde_json::to_value(TwinAnswerParams { id: "q".into(), answer: "app".into() }).unwrap(),
            serde_json::to_value(&question).unwrap(),
        ),
        ("peer_status", methods::TWIN_PEER_STATUS, json!({}), serde_json::to_value(&peer_status).unwrap()),
        (
            "link",
            methods::TWIN_LINK,
            serde_json::to_value(TwinLinkParams { code: "ABCD-2345-EFGH-6789".into(), node_id: DEVICE_B.into() })
                .unwrap(),
            serde_json::to_value(&link).unwrap(),
        ),
        (
            "links",
            methods::TWIN_LINKS,
            json!({}),
            serde_json::to_value(TwinLinksResult { links: vec![link.clone()] }).unwrap(),
        ),
        (
            "unlink",
            methods::TWIN_UNLINK,
            serde_json::to_value(TwinUnlinkParams { node_id: DEVICE_B.into() }).unwrap(),
            json!({}),
        ),
        (
            "link_code",
            methods::NODE_TWIN_LINK_CODE,
            json!({}),
            serde_json::to_value(NodeTwinLinkCodeResult { code: "ABCD-2345-EFGH-6789".into(), expires_ms: 600_000 })
                .unwrap(),
        ),
    ] {
        v.push(rpc(&format!("twin_{name}"), RpcMessage::request(30u64, method, params)));
        v.push(rpc(&format!("twin_{name}_result"), RpcMessage::result(RpcId::Num(30), result)));
    }
    v.push(rpc(
        "twin_changed",
        RpcMessage::notification(
            methods::TWIN_CHANGED,
            serde_json::to_value(TwinChanged { state: TwinState::Idle, run: Some(run) }).unwrap(),
        ),
    ));
    v.push(rpc(
        "twin_question",
        RpcMessage::notification(
            methods::TWIN_QUESTION,
            serde_json::to_value(TwinQuestionNotice { question }).unwrap(),
        ),
    ));
    v.push(rpc(
        "node_security_event_twin_linked",
        RpcMessage::notification(
            methods::NODE_SECURITY_EVENT,
            serde_json::to_value(NodeSecurityEvent {
                kind: SecurityEventKind::TwinLinked,
                ts_ms: 5,
                by_device: Some("local".into()),
                device_id: Some(DEVICE_A.into()),
                device_name: Some("mac-mini 的分身".into()),
                method: None,
                failures: None,
                password_set: None,
            })
            .unwrap(),
        ),
    ));
    v.push(rpc(
        "todo_handler",
        RpcMessage::notification(
            methods::TODO_EVENT,
            serde_json::to_value(TodoEventNotice {
                todo_id: "t".into(),
                event: TodoEvent {
                    seq: 1,
                    ts_ms: 2,
                    data: TodoEventData::Handler {
                        from: TodoHandler::Me,
                        to: TodoHandler::Twin,
                        by: TodoActor { kind: TodoSourceKind::Twin, id: Some("local".into()), device_id: None },
                    },
                },
            })
            .unwrap(),
        ),
    ));
    for (name, data) in [
        ("todo_repeat_created", TodoEventData::RepeatCreated { todo_id: "n".into(), at_ms: 3, manual: false }),
        ("todo_repeat_created_manual", TodoEventData::RepeatCreated { todo_id: "n".into(), at_ms: 3, manual: true }),
        ("todo_repeat_skipped", TodoEventData::RepeatSkipped { at_ms: 3 }),
        (
            "todo_remote_dispatched",
            TodoEventData::RemoteDispatched { node_id: DEVICE_B.into(), todo_id: "r".into(), session_id: "s".into() },
        ),
        (
            "todo_taken_over_on_node",
            TodoEventData::TakenOver {
                by: TodoActor { kind: TodoSourceKind::Device, id: None, device_id: Some(DEVICE_B.into()) },
                node_id: Some(DEVICE_B.into()),
            },
        ),
        (
            "todo_remote_result",
            TodoEventData::RemoteResult { node_id: DEVICE_B.into(), session_id: "s".into(), text: "done".into() },
        ),
    ] {
        v.push(rpc(
            name,
            RpcMessage::notification(
                methods::TODO_EVENT,
                serde_json::to_value(TodoEventNotice {
                    todo_id: "t".into(),
                    event: TodoEvent { seq: 2, ts_ms: 4, data },
                })
                .unwrap(),
            ),
        ));
    }
    v.push(rpc(
        "todo_pr_event",
        RpcMessage::notification(
            methods::TODO_EVENT,
            serde_json::to_value(TodoEventNotice {
                todo_id: "t".into(),
                event: TodoEvent {
                    seq: 2,
                    ts_ms: 3,
                    data: TodoEventData::PrEvent {
                        session_id: "s".into(),
                        event: PrEventKind::Reviewed,
                        url: Some("https://github.com/o/r/pull/1".into()),
                    },
                },
            })
            .unwrap(),
        ),
    ));
    let body =
        rmp_serde::to_vec_named(&NotifyBody { category: NotifyCategory::TwinQuestion, session_id: None, detail: None })
            .unwrap();
    v.push(envelope("notify_twin_question", DEVICE_A, DEVICE_B, Kind::Notify, 1, body));
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

    let preview = preview_vectors();
    let text = serde_json::to_string_pretty(&preview).expect("preview") + "\n";
    fs::write(out.join("preview.json"), text).expect("write preview");
    eprintln!("wrote {} preview vectors", preview.as_array().map_or(0, Vec::len));
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
                assert_eq!(
                    &b[header_len..],
                    &ct[offset..offset + data_len],
                    "body payload must be a contiguous slice of the ciphertext"
                );
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
        out.push(
            json!({ "kind": "decode", "name": name, "body_hex": hex::encode(&body), "outcome": decode_outcome(&body) }),
        );
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
    scenario(
        "reassemble_in_order",
        vec![fragment_body(1, 0, 3, &[1, 2]), fragment_body(1, 1, 3, &[3]), fragment_body(1, 2, 3, &[4, 5, 6])],
    );
    scenario("reassemble_single_fragment", vec![fragment_body(8, 0, 1, &[7, 7])]);
    scenario("reassemble_out_of_order_first", vec![fragment_body(1, 1, 2, &[1])]);
    scenario("reassemble_out_of_order_skip", vec![fragment_body(1, 0, 3, &[1]), fragment_body(1, 2, 3, &[3])]);
    scenario(
        "reassemble_error_clears_state",
        vec![fragment_body(1, 0, 3, &[1]), fragment_body(1, 2, 3, &[3]), fragment_body(1, 1, 3, &[2])],
    );
    scenario(
        "reassemble_new_msg_id_restarts",
        vec![fragment_body(1, 0, 2, &[1]), fragment_body(2, 0, 2, &[9]), fragment_body(2, 1, 2, &[8])],
    );
    scenario("reassemble_new_msg_id_mid_message", vec![fragment_body(1, 0, 3, &[1]), fragment_body(2, 1, 3, &[9])]);
    scenario(
        "reassemble_whole_drops_pending",
        vec![fragment_body(1, 0, 2, &[1]), vec![0x00, 5], fragment_body(1, 1, 2, &[2])],
    );
    scenario(
        "reassemble_reset_drops_pending",
        vec![fragment_body(1, 0, 2, &[1]), vec![0x02], fragment_body(1, 1, 2, &[2])],
    );
    scenario("reassemble_count_zero", vec![fragment_body(1, 0, 0, &[1])]);
    scenario("reassemble_count_changes_midway", vec![fragment_body(1, 0, 3, &[1]), fragment_body(1, 1, 2, &[2])]);
    scenario(
        "reassemble_back_to_back_messages",
        vec![
            fragment_body(1, 0, 2, &[1]),
            fragment_body(1, 1, 2, &[2]),
            fragment_body(2, 0, 2, &[3]),
            fragment_body(2, 1, 2, &[4]),
        ],
    );
    // 恶意头部：index >= count、index = u16::MAX、count 中途变化（变大 / 变小）、出错后状态被清空、
    // count = 1 的单片 FRAGMENT、完成后同一 msg_id 再来要从 0 开始。
    scenario("reassemble_index_equals_count", vec![fragment_body(1, 2, 2, &[1])]);
    scenario("reassemble_index_u16_max", vec![fragment_body(1, 0xffff, 2, &[1])]);
    scenario(
        "reassemble_index_ge_count_mid_message",
        vec![fragment_body(1, 0, 3, &[1]), fragment_body(1, 3, 3, &[2]), fragment_body(1, 1, 3, &[3])],
    );
    scenario("reassemble_count_grows_midway", vec![fragment_body(7, 0, 3, &[1]), fragment_body(7, 1, 4, &[2])]);
    scenario(
        "reassemble_count_change_clears_state",
        vec![fragment_body(7, 0, 3, &[1]), fragment_body(7, 1, 4, &[2]), fragment_body(7, 1, 3, &[3])],
    );
    scenario("reassemble_count_u16_max_single", vec![fragment_body(9, 0, 0xffff, &[1])]);
    scenario("reassemble_count_one_twice", vec![fragment_body(3, 0, 1, &[1]), fragment_body(3, 0, 1, &[2])]);
    scenario("reassemble_same_msg_id_after_complete", vec![fragment_body(3, 0, 1, &[1]), fragment_body(3, 1, 2, &[2])]);

    Value::Array(out)
}

// ---------------------------------------------------------------------------
// 预览隧道（preview::tunnel，ADR-39）
// ---------------------------------------------------------------------------

fn frame_type_name(t: tunnel::FrameType) -> &'static str {
    use tunnel::FrameType::*;
    match t {
        Request => "request",
        Response => "response",
        Data => "data",
        End => "end",
        Reset => "reset",
        Window => "window",
        WsMessage => "ws_message",
        WsClose => "ws_close",
        Goaway => "goaway",
    }
}

fn tunnel_error_name(e: &tunnel::TunnelError) -> &'static str {
    match e {
        tunnel::TunnelError::Truncated => "truncated",
        tunnel::TunnelError::TooLarge => "too_large",
        tunnel::TunnelError::Version(_) => "version",
        tunnel::TunnelError::Malformed(_) => "malformed",
    }
}

fn frames_json(frames: &[tunnel::Frame]) -> Value {
    Value::Array(
        frames
            .iter()
            .map(|f| json!({ "type": frame_type_name(f.ty), "type_byte": f.ty as u8, "stream": f.stream, "payload_hex": hex::encode(&f.payload) }))
            .collect(),
    )
}

/// 头部向量：msgpack 字节 + 逻辑值。写成宏是为了不给这个小工具加 serde 依赖。
macro_rules! head {
    ($kind:expr, $name:expr, $h:expr) => {{
        let h = $h;
        let bytes = tunnel::encode_head(&h).expect("head");
        json!({ "kind": $kind, "name": $name, "bytes_hex": hex::encode(bytes), "value": serde_json::to_value(&h).expect("json") })
    }};
}

fn preview_vectors() -> Value {
    use tunnel::{Frame, FrameType};
    let mut out: Vec<Value> = Vec::new();

    // ----- 消息：编码（含纯回执、多帧、ack 的 u64 大端） -----
    let mut message = |name: &str, ack: u64, frames: Vec<Frame>| {
        let bytes = tunnel::encode_message(ack, &frames).expect("message");
        assert_eq!(tunnel::decode_message(&bytes).expect("decode").frames, frames);
        out.push(json!({ "kind": "message", "name": name, "ack": ack, "frames": frames_json(&frames), "bytes_hex": hex::encode(bytes) }));
    };
    message("message_ack_only", 0, vec![]);
    message("message_ack_u64", 0x0102_0304_0506_0708, vec![]);
    message(
        "message_get_request",
        3,
        vec![
            Frame::new(
                FrameType::Request,
                1,
                tunnel::encode_head(&PreviewRequestHead {
                    method: "GET".into(),
                    path: "/@vite/client".into(),
                    headers: vec![("accept".into(), "*/*".into())],
                    websocket: false,
                })
                .expect("head"),
            ),
            Frame::new(FrameType::End, 1, vec![]),
        ],
    );
    message(
        "message_all_small_frames",
        u64::from(u32::MAX) + 1,
        vec![
            Frame::new(FrameType::Data, 0xffff_ffff, vec![0xde, 0xad]),
            Frame::new(FrameType::Window, 5, tunnel::window(tunnel::STREAM_WINDOW)),
            Frame::new(FrameType::WsMessage, 7, tunnel::ws_message(tunnel::WS_FIN, b"{\"type\":\"connected\"}")),
            Frame::new(FrameType::WsClose, 7, tunnel::ws_close(1001, "going away")),
            Frame::new(
                FrameType::Reset,
                9,
                tunnel::encode_head(&PreviewReset { code: PreviewResetCode::Refused, message: None }).expect("reset"),
            ),
            Frame::new(
                FrameType::Goaway,
                0,
                tunnel::encode_head(&PreviewGoaway {
                    code: PreviewGoawayCode::Expired,
                    message: Some("ticket expired".into()),
                })
                .expect("goaway"),
            ),
        ],
    );

    // ----- 消息：解码（未知帧跳过、各种截断） -----
    let mut decode = |name: &str, bytes: Vec<u8>| {
        let outcome = match tunnel::decode_message(&bytes) {
            Ok(m) => json!({ "ok": true, "ack": m.ack, "frames": frames_json(&m.frames) }),
            Err(e) => json!({ "err": tunnel_error_name(&e) }),
        };
        out.push(json!({ "kind": "decode", "name": name, "bytes_hex": hex::encode(&bytes), "outcome": outcome }));
    };
    let mut unknown = 9u64.to_be_bytes().to_vec();
    unknown.extend_from_slice(&[0x7f, 0, 0, 0, 1, 0, 0, 0, 2, 0xaa, 0xbb]);
    unknown.extend_from_slice(&[0x04, 0, 0, 0, 3, 0, 0, 0, 0]);
    decode("decode_unknown_frame_skipped", unknown);
    decode("decode_empty", vec![]);
    decode("decode_short_ack", vec![0, 0, 0, 0, 0, 0, 1]);
    let mut short_header = 0u64.to_be_bytes().to_vec();
    short_header.extend_from_slice(&[0x03, 0, 0, 0]);
    decode("decode_truncated_frame_header", short_header);
    let mut short_payload = 0u64.to_be_bytes().to_vec();
    short_payload.extend_from_slice(&[0x03, 0, 0, 0, 1, 0, 0, 0, 4, 1, 2, 3]);
    decode("decode_truncated_payload", short_payload);
    decode("decode_too_large", vec![0; tunnel::MAX_MESSAGE + 1]);

    // ----- 头部（msgpack，字段名为键） -----
    out.push(head!(
        "request_head",
        "request_websocket",
        &PreviewRequestHead {
            method: "GET".into(),
            path: "/?token=abc".into(),
            headers: vec![
                ("sec-websocket-protocol".into(), "vite-hmr".into()),
                ("origin".into(), "https://abcdefghijklmnopqrstuvwxyz.aiplace.app".into())
            ],
            websocket: true,
        }
    ));
    out.push(head!(
        "response_head",
        "response_identity",
        &PreviewResponseHead {
            status: 200,
            headers: vec![("content-type".into(), "text/html".into()), ("set-cookie".into(), "a=1; Path=/".into())],
            encoding: PreviewBodyEncoding::Identity,
        }
    ));
    out.push(head!(
        "response_head",
        "response_gzip_101",
        &PreviewResponseHead {
            status: 101,
            headers: vec![("sec-websocket-protocol".into(), "vite-hmr".into())],
            encoding: PreviewBodyEncoding::Gzip,
        }
    ));
    out.push(head!(
        "reset",
        "reset_flow_control",
        &PreviewReset { code: PreviewResetCode::FlowControl, message: Some("window exceeded".into()) }
    ));
    out.push(head!(
        "goaway",
        "goaway_port_not_allowed",
        &PreviewGoaway { code: PreviewGoawayCode::PortNotAllowed, message: None }
    ));

    // ----- 帧载荷 -----
    out.push(json!({ "kind": "window", "name": "window_max", "increment": u32::MAX, "bytes_hex": hex::encode(tunnel::window(u32::MAX)) }));
    out.push(json!({ "kind": "ws_message", "name": "ws_binary_partial", "flags": tunnel::WS_BINARY, "data_hex": "000102", "bytes_hex": hex::encode(tunnel::ws_message(tunnel::WS_BINARY, &[0, 1, 2])) }));
    let long_reason = "关".repeat(50); // 150 字节，截到 123 以内的字符边界
    let close = tunnel::ws_close(4000, &long_reason);
    let (_, kept) = tunnel::parse_ws_close(&close).expect("close");
    out.push(json!({ "kind": "ws_close", "name": "ws_close_truncated", "code": 4000, "reason": long_reason, "kept_reason": kept, "bytes_hex": hex::encode(&close) }));

    // ----- 握手 body / prologue / label -----
    let ticket: [u8; tunnel::TICKET_ID_LEN] = core::array::from_fn(|i| i as u8);
    out.push(json!({ "kind": "handshake_init", "name": "handshake_init", "ticket_id_hex": hex::encode(ticket), "noise_hex": "aabbcc", "bytes_hex": hex::encode(tunnel::handshake_init(&ticket, &[0xaa, 0xbb, 0xcc])) }));
    out.push(json!({ "kind": "handshake_reply", "name": "handshake_accept", "accept": true, "noise_hex": "0102", "bytes_hex": hex::encode(tunnel::handshake_accept(&[1, 2])) }));
    out.push(json!({ "kind": "handshake_reply", "name": "handshake_reject", "accept": false, "reason": "port_not_allowed", "bytes_hex": hex::encode(tunnel::handshake_reject(PreviewRejectReason::PortNotAllowed)) }));
    out.push(json!({ "kind": "prologue", "name": "prologue", "label": "abcdefghijklmnopqrstuvwxyz", "ticket_id_hex": hex::encode(ticket), "bytes_hex": hex::encode(tunnel::prologue("abcdefghijklmnopqrstuvwxyz", &ticket)) }));
    for (name, bytes) in [("label_zero", [0u8; 16]), ("label_ff", [0xff; 16]), ("label_counting", ticket)] {
        let label = tunnel::label_from_bytes(&bytes);
        assert!(tunnel::is_valid_label(&label));
        out.push(json!({ "kind": "label", "name": name, "bytes_hex": hex::encode(bytes), "label": label }));
    }

    Value::Array(out)
}
