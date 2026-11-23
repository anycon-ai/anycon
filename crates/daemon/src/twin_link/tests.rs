//! 节点表、远程项 id 和错误分类的单元测试；端到端（真 hub 和两个 daemon）在 `tests/twin_links.rs`。

use super::*;
use anycon_protocol::TwinPeerAgent;

fn status(name: &str, projects: &[&str]) -> TwinPeerStatus {
    TwinPeerStatus {
        name: name.into(),
        group: None,
        projects: projects.iter().map(|p| p.to_string()).collect(),
        agents: vec![TwinPeerAgent { id: "claude-code".into(), label: "Claude Code".into(), ready: true }],
        remaining_sessions: 2,
        remaining_dispatches: 5,
        paused: false,
        accept_links: true,
    }
}

fn row(id: &str, name: &str, status: Option<TwinPeerStatus>) -> PeerRow {
    PeerRow { id: id.into(), name: name.into(), status }
}

#[test]
fn the_node_table_is_sorted_by_name_marks_unavailable_nodes_and_stays_within_its_budget() {
    assert!(render(vec![]).is_none());
    let t = render(vec![
        row("n2", "zeta", Some(status("zeta", &["p"]))),
        row("n1", "alpha", None),
        row("n3", "mid", Some(status("mid", &["q", "r"]))),
    ])
    .unwrap();
    let lines: Vec<Value> = t.rows.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.iter().map(|l| l["name"].as_str().unwrap()).collect::<Vec<_>>(), ["alpha", "mid", "zeta"]);
    assert_eq!(lines[0]["available"], false);
    assert!(lines[0].get("projects").is_none());
    assert_eq!(lines[1]["projects"], json!(["q", "r"]));
    assert_eq!(lines[1]["remaining_dispatches"], 5);

    // 超出 4 KiB：按节点名排序，后面的丢掉
    let many: Vec<PeerRow> = (0..200)
        .map(|i| {
            let name = format!("node-{i:03}");
            row(&format!("id-{i}"), &name, Some(status(&name, &["p1", "p2", "p3"])))
        })
        .collect();
    let t = render(many).unwrap();
    assert!(t.rows.len() <= TABLE_BYTES, "{}", t.rows.len());
    let first: Value = serde_json::from_str(t.rows.lines().next().unwrap()).unwrap();
    assert_eq!(first["name"], "node-000");
    assert!(t.rows.lines().count() < 200);
}

#[test]
fn names_and_projects_from_another_node_are_cleaned_and_capped() {
    let hostile = format!("evil\u{1b}[31m\u{202e}name{}", "x".repeat(500));
    let projects: Vec<String> = (0..100).map(|i| format!("p{i}\n")).collect();
    let mut s = status(&hostile, &[]);
    s.projects = projects;
    let t = render(vec![row("n1", &hostile, Some(s))]).unwrap();
    assert_eq!(t.rows.lines().count(), 1, "a name cannot add lines");
    assert!(!t.rows.contains('\u{1b}') && !t.rows.contains('\u{202e}'));
    let v: Value = serde_json::from_str(&t.rows).unwrap();
    assert!(v["name"].as_str().unwrap().chars().count() <= LABEL_CHARS);
    assert_eq!(v["projects"].as_array().unwrap().len(), TABLE_PROJECTS);
}

#[test]
fn the_remote_id_is_stable_per_attempt_and_a_canonical_uuid() {
    let a = remote_id("todo-1", 0);
    assert_eq!(a, remote_id("todo-1", 0), "a retry after a failed dispatch reuses the item on the node");
    assert_ne!(a, remote_id("todo-1", 1), "a dispatch that went through moves on to a new item");
    assert_ne!(a, remote_id("todo-2", 0));
    assert!(crate::todos::check_id(&a).is_ok());
}

#[test]
fn a_node_error_is_a_refusal_with_a_cleaned_reason_and_anything_else_is_unavailable() {
    let rpc = err_reason(ErrorCode::Forbidden, "no", "out_of_scope\n; DROP");
    assert_eq!(failure_of(ClientError::from_rpc(rpc)), Failure::Refused("out_of_scope".into()));
    assert_eq!(failure_of(ClientError::offline("gone")), Failure::Unavailable("offline".into()));
    let timeout = ClientError::new(ErrorClass::Offline, "timeout", "slow");
    assert_eq!(failure_of(timeout), Failure::Unavailable("timeout".into()));
}
