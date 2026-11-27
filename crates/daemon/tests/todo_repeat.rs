//! 周期项（docs/spec/daemon.md § 26.9）：规则校验、模板的限制、下一次时间（含周末、跨月、夏令时）和 daemon 的复制。
//!
//! 整个测试进程用 `America/New_York`（有夏令时）：时区在第一次调用时间 API 之前由 [`tz`] 设置。时间点写成 UTC，
//! 注释里标出当地时间。
//!
//! 只在 unix 上跑：Windows 的 C 运行库不认 IANA 时区名，而且读的是它启动时复制下来的环境，`std::env::set_var` 改不到它，
//! 测试没法把时区固定下来。Windows 上算当地时间的那条路径由 `todo_repeat` 模块自己的单元测试覆盖（不依赖时区）。
#![cfg(unix)]

use std::sync::Once;

use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_daemon::todo_repeat::{parse_at, slot_after, slot_at_or_before, tick};
use anycon_daemon::util::parse_utc;
use anycon_protocol::*;
use serde_json::{json, Value};

fn tz() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| std::env::set_var("TZ", "America/New_York"));
}

fn utc(s: &str) -> u64 {
    parse_utc(s).unwrap_or_else(|| panic!("bad time {s}"))
}

fn rule(kind: TodoRepeatKind, at: &str, weekday: Option<u8>) -> TodoRepeat {
    TodoRepeat {
        kind,
        at: at.into(),
        weekday,
        days: None,
        every_min: None,
        until: None,
        auto_close: false,
        next_ms: None,
    }
}

fn interval(days: TodoRepeatDays, weekday: Option<u8>, at: &str, every_min: u32, until: &str) -> TodoRepeat {
    TodoRepeat {
        days: Some(days),
        every_min: Some(every_min),
        until: Some(until.into()),
        ..rule(TodoRepeatKind::Interval, at, weekday)
    }
}

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[test]
fn at_must_be_hh_mm() {
    tz();
    for ok in ["00:00", "09:05", "23:59"] {
        assert!(parse_at(ok).is_some(), "{ok}");
    }
    for bad in ["", "9:00", "09:0", "24:00", "09:60", "0900", "09-00", "09:00:00", "ab:cd", " 09:00", "+9:00"] {
        assert!(parse_at(bad).is_none(), "{bad:?}");
    }
}

#[test]
fn daily_and_weekday_slots_skip_the_weekend() {
    tz();
    let daily = rule(TodoRepeatKind::Daily, "09:00", None);
    // 2024-01-10 10:00 EST：今天 09:00 已过
    assert_eq!(slot_at_or_before(&daily, utc("2024-01-10T15:00Z")), Some(utc("2024-01-10T14:00Z")));
    assert_eq!(slot_after(&daily, utc("2024-01-10T15:00Z")), Some(utc("2024-01-11T14:00Z")));
    // 正好在点上：不晚于它的是它自己，晚于它的是明天
    assert_eq!(slot_at_or_before(&daily, utc("2024-01-10T14:00Z")), Some(utc("2024-01-10T14:00Z")));
    assert_eq!(slot_after(&daily, utc("2024-01-10T14:00Z")), Some(utc("2024-01-11T14:00Z")));
    // 当地还是前一天：UTC 的 2024-01-11 03:00 是当地 1 月 10 日 22:00
    assert_eq!(slot_at_or_before(&daily, utc("2024-01-11T03:00Z")), Some(utc("2024-01-10T14:00Z")));

    let weekdays = rule(TodoRepeatKind::Weekdays, "09:00", None);
    // 周五 2024-01-12 10:00 EST 之后的下一次是周一
    assert_eq!(slot_after(&weekdays, utc("2024-01-12T15:00Z")), Some(utc("2024-01-15T14:00Z")));
    // 周日 15:00 EST 之前最近的一次是周五
    assert_eq!(slot_at_or_before(&weekdays, utc("2024-01-14T20:00Z")), Some(utc("2024-01-12T14:00Z")));
    // 周一 09:00 之前（周一 08:00 EST）最近的一次还是周五
    assert_eq!(slot_at_or_before(&weekdays, utc("2024-01-15T13:00Z")), Some(utc("2024-01-12T14:00Z")));
}

#[test]
fn weekly_slots_cross_months_and_use_iso_weekdays() {
    tz();
    // 2024-01-31 是周三
    let wed = rule(TodoRepeatKind::Weekly, "09:00", Some(3));
    assert_eq!(slot_at_or_before(&wed, utc("2024-01-31T15:00Z")), Some(utc("2024-01-31T14:00Z")));
    assert_eq!(slot_after(&wed, utc("2024-01-31T15:00Z")), Some(utc("2024-02-07T14:00Z")));
    // 周一 = 1：2024-01-30（周二）之后的下一个周一是 2 月 5 日；2024-03-01（周五）之前最近的是 2 月 26 日（闰年的 2 月有 29 天）
    let mon = rule(TodoRepeatKind::Weekly, "09:00", Some(1));
    assert_eq!(slot_after(&mon, utc("2024-01-30T20:00Z")), Some(utc("2024-02-05T14:00Z")));
    assert_eq!(slot_at_or_before(&mon, utc("2024-03-01T20:00Z")), Some(utc("2024-02-26T14:00Z")));
    // 周日 = 7：2024-12-31（周二）之后是 2025-01-05，跨年
    let sun = rule(TodoRepeatKind::Weekly, "23:30", Some(7));
    assert_eq!(slot_after(&sun, utc("2024-12-31T20:00Z")), Some(utc("2025-01-06T04:30Z")));
    // 不认识的种类没有时间
    assert_eq!(slot_after(&rule(TodoRepeatKind::Unknown, "09:00", None), utc("2024-01-10T15:00Z")), None);
}

#[test]
fn daylight_saving_follows_the_local_clock() {
    tz();
    // 2024-03-10 02:00 EST 跳到 03:00 EDT：当天 02:30 不存在，算在跳过之后的第一分钟（03:00 EDT = 07:00Z）
    let skipped = rule(TodoRepeatKind::Daily, "02:30", None);
    assert_eq!(slot_after(&skipped, utc("2024-03-09T07:30Z")), Some(utc("2024-03-10T07:00Z")));
    assert_eq!(slot_at_or_before(&skipped, utc("2024-03-10T07:30Z")), Some(utc("2024-03-10T07:00Z")));
    assert_eq!(slot_at_or_before(&skipped, utc("2024-03-10T06:59Z")), Some(utc("2024-03-09T07:30Z")));
    // 第二天恢复正常（EDT：02:30 = 06:30Z）
    assert_eq!(slot_after(&skipped, utc("2024-03-10T07:00Z")), Some(utc("2024-03-11T06:30Z")));
    // 切换日的 09:00 是 EDT：和前一天（EST，14:00Z）差 23 小时
    let nine = rule(TodoRepeatKind::Daily, "09:00", None);
    assert_eq!(slot_after(&nine, utc("2024-03-09T14:00Z")), Some(utc("2024-03-10T13:00Z")));

    // 2024-11-03 02:00 EDT 回拨到 01:00 EST：01:30 出现两次，只算第一次（EDT，05:30Z）
    let repeated = rule(TodoRepeatKind::Daily, "01:30", None);
    assert_eq!(slot_after(&repeated, utc("2024-11-02T05:30Z")), Some(utc("2024-11-03T05:30Z")));
    // 第二次出现（EST，06:30Z）不再算：下一次是第二天
    assert_eq!(slot_after(&repeated, utc("2024-11-03T05:30Z")), Some(utc("2024-11-04T06:30Z")));
    assert_eq!(slot_at_or_before(&repeated, utc("2024-11-03T06:45Z")), Some(utc("2024-11-03T05:30Z")));
    // 切换日的 09:00 是 EST：25 小时之后
    assert_eq!(slot_after(&nine, utc("2024-11-02T13:00Z")), Some(utc("2024-11-03T14:00Z")));
}

// ---------------------------------------------------------------- RPC

async fn start() -> (TestDaemon, TestClient) {
    tz();
    let td = TestDaemon::start().await;
    let c = td.local_cli_client();
    (td, c)
}

async fn add(td: &TestDaemon, c: &mut TestClient, params: Value) -> Result<Todo, RpcError> {
    c.call(td, methods::TODO_ADD, params).await
}

async fn update(td: &TestDaemon, c: &mut TestClient, params: Value) -> Result<Todo, RpcError> {
    c.call(td, methods::TODO_UPDATE, params).await
}

async fn get(td: &TestDaemon, c: &mut TestClient, id: &str) -> Todo {
    c.call(td, methods::TODO_GET, json!({ "id": id })).await.unwrap()
}

async fn list_all(td: &TestDaemon, c: &mut TestClient) -> Vec<Todo> {
    c.call::<TodoListResult>(td, methods::TODO_LIST, json!({ "include_closed": true })).await.unwrap().todos
}

async fn history(td: &TestDaemon, c: &mut TestClient, id: &str) -> Vec<TodoEvent> {
    let r: TodoHistoryResult = c.call(td, methods::TODO_HISTORY, json!({ "id": id })).await.unwrap();
    let mut events = r.events;
    events.reverse();
    events
}

fn reason(e: &RpcError) -> (ErrorCode, Option<String>, Option<String>) {
    let d = e.data.clone().expect("error data");
    (d.code, d.field, d.reason)
}

fn copies(todos: &[Todo], template: &str) -> Vec<Todo> {
    let mut c: Vec<Todo> = todos.iter().filter(|t| t.source.todo_id.as_deref() == Some(template)).cloned().collect();
    c.sort_by_key(|t| t.created_ms);
    c
}

#[tokio::test]
async fn the_rule_is_validated_and_only_a_person_sets_it() {
    let (td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z"));
    for (repeat, field_reason) in [
        (json!({ "kind": "daily", "at": "9:00" }), "at"),
        (json!({ "kind": "daily", "at": "24:00" }), "at"),
        (json!({ "kind": "daily", "at": "12:60" }), "at"),
        (json!({ "kind": "weekly", "at": "09:00" }), "weekday"),
        (json!({ "kind": "weekly", "at": "09:00", "weekday": 0 }), "weekday"),
        (json!({ "kind": "weekly", "at": "09:00", "weekday": 8 }), "weekday"),
        (json!({ "kind": "daily", "at": "09:00", "weekday": 1 }), "weekday"),
        (json!({ "kind": "weekdays", "at": "09:00", "weekday": 1 }), "weekday"),
        (json!({ "kind": "monthly", "at": "09:00" }), "kind"),
        // 间隔规则：days / every_min / until 必填且在范围内，别的种类不能带
        (json!({ "kind": "interval", "at": "09:00", "every_min": 60, "until": "18:00" }), "days"),
        (json!({ "kind": "interval", "at": "09:00", "days": "monthly", "every_min": 60, "until": "18:00" }), "kind"),
        (json!({ "kind": "interval", "at": "09:00", "days": "daily", "until": "18:00" }), "every_min"),
        (json!({ "kind": "interval", "at": "09:00", "days": "daily", "every_min": 14, "until": "18:00" }), "every_min"),
        (
            json!({ "kind": "interval", "at": "09:00", "days": "daily", "every_min": 721, "until": "18:00" }),
            "every_min",
        ),
        (json!({ "kind": "interval", "at": "09:00", "days": "daily", "every_min": 60 }), "until"),
        (json!({ "kind": "interval", "at": "09:00", "days": "daily", "every_min": 60, "until": "9:00" }), "until"),
        (json!({ "kind": "interval", "at": "09:00", "days": "daily", "every_min": 60, "until": "09:00" }), "until"),
        (json!({ "kind": "interval", "at": "18:00", "days": "daily", "every_min": 60, "until": "09:00" }), "until"),
        (json!({ "kind": "interval", "at": "09:00", "days": "weekly", "every_min": 60, "until": "18:00" }), "weekday"),
        (
            json!({ "kind": "interval", "at": "09:00", "days": "daily", "every_min": 60, "until": "18:00", "weekday": 1 }),
            "weekday",
        ),
        (json!({ "kind": "daily", "at": "09:00", "days": "daily" }), "days"),
        (json!({ "kind": "daily", "at": "09:00", "every_min": 60 }), "every_min"),
        (json!({ "kind": "weekdays", "at": "09:00", "until": "18:00" }), "until"),
    ] {
        let e = add(&td, &mut c, json!({ "id": id(), "title": "t", "repeat": repeat })).await.unwrap_err();
        assert_eq!(reason(&e), (ErrorCode::Invalid, Some("repeat".into()), Some(field_reason.into())), "{repeat}");
    }
    assert!(list_all(&td, &mut c).await.is_empty());

    // 自报 mcp 的连接、agent: true 的写入：forbidden，什么都不建
    let mut mcp = td.local_mcp_client();
    let ok = json!({ "kind": "weekly", "at": "09:00", "weekday": 2 });
    let e = add(&td, &mut mcp, json!({ "id": id(), "title": "t", "repeat": ok })).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::Forbidden);
    let e = add(&td, &mut c, json!({ "id": id(), "title": "t", "agent": true, "repeat": ok })).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::Forbidden);
    assert!(list_all(&td, &mut c).await.is_empty());

    // 人可以建；返回里带规则和下一次时间（2024-01-10 08:00 EST → 当天 09:00）
    let t = add(&td, &mut c, json!({ "id": id(), "title": "Standup", "repeat": ok })).await.unwrap();
    assert_eq!(t.repeat.as_ref().map(|r| (r.kind, r.weekday)), Some((TodoRepeatKind::Weekly, Some(2))));
    // 周二：2024-01-16 09:00 EST
    assert_eq!(t.repeat.as_ref().and_then(|r| r.next_ms), Some(utc("2024-01-16T14:00Z")));
    assert_eq!(get(&td, &mut c, &t.id).await.repeat, t.repeat);

    // 请求里带的 next_ms 不算
    let t2 = add(
        &td,
        &mut c,
        json!({ "id": id(), "title": "x", "repeat": { "kind": "daily", "at": "09:00", "next_ms": 5 } }),
    )
    .await
    .unwrap();
    assert_eq!(t2.repeat.unwrap().next_ms, Some(utc("2024-01-10T14:00Z")));

    // agent 不能改、不能清
    let e =
        update(&td, &mut mcp, json!({ "id": t.id, "repeat": { "kind": "daily", "at": "10:00" } })).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::Forbidden);
    let e = update(&td, &mut mcp, json!({ "id": t.id, "clear": ["repeat"] })).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::Forbidden);
    let e = update(&td, &mut c, json!({ "id": t.id, "agent": true, "clear": ["repeat"] })).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::Forbidden);
    assert!(get(&td, &mut c, &t.id).await.repeat.is_some());

    // 人可以改、可以清；清掉之后它是普通的项
    let t = update(&td, &mut c, json!({ "id": t.id, "repeat": { "kind": "daily", "at": "10:00" } })).await.unwrap();
    assert_eq!(t.repeat.as_ref().map(|r| (r.kind, r.at.as_str())), Some((TodoRepeatKind::Daily, "10:00")));
    let e =
        update(&td, &mut c, json!({ "id": t.id, "repeat": { "kind": "weekly", "at": "10:00" } })).await.unwrap_err();
    assert_eq!(reason(&e).1.as_deref(), Some("repeat"));
    let t = update(&td, &mut c, json!({ "id": t.id, "clear": ["repeat"] })).await.unwrap();
    assert!(t.repeat.is_none());
    assert!(get(&td, &mut c, &t.id).await.repeat.is_none());
    td.shutdown().await;
}

#[tokio::test]
async fn interval_rules_and_auto_close_are_stored_and_changing_only_auto_close_keeps_the_schedule() {
    let (td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z")); // 08:00 EST，周三
    let rule = json!({ "kind": "interval", "at": "09:00", "days": "weekdays", "every_min": 120, "until": "18:00" });
    let t = add(&td, &mut c, json!({ "id": id(), "title": "Patrol", "repeat": rule })).await.unwrap();
    let r = t.repeat.clone().unwrap();
    assert_eq!(
        (r.kind, r.days, r.every_min, r.until.as_deref(), r.auto_close),
        (TodoRepeatKind::Interval, Some(TodoRepeatDays::Weekdays), Some(120), Some("18:00"), false)
    );
    assert_eq!(r.next_ms, Some(utc("2024-01-10T14:00Z")));
    assert_eq!(get(&td, &mut c, &t.id).await.repeat.unwrap().days, Some(TodoRepeatDays::Weekdays));
    // 只改 auto_close 不重新算「已处理的时间」：10:00 EST 改完之后，09:00 那次（没来得及复制）还能补上
    td.ctx.judge.clock().set(utc("2024-01-10T15:00Z")); // 10:00 EST
    let mut with_close = rule.clone();
    with_close["auto_close"] = json!(true);
    let t = update(&td, &mut c, json!({ "id": t.id, "repeat": with_close })).await.unwrap();
    assert!(t.repeat.unwrap().auto_close);
    assert!(get(&td, &mut c, &t.id).await.repeat.unwrap().auto_close);
    assert_eq!(tick(&td.ctx).await.unwrap(), 1);
    // 改了时间安排（间隔）就从现在起重算：补不上之前的
    let mut moved = rule.clone();
    moved["every_min"] = json!(60);
    update(&td, &mut c, json!({ "id": t.id, "repeat": moved })).await.unwrap();
    td.ctx.judge.clock().set(utc("2024-01-10T15:30Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    let daily = add(
        &td,
        &mut c,
        json!({ "id": id(), "title": "Daily", "repeat": { "kind": "daily", "at": "09:00", "auto_close": true } }),
    )
    .await
    .unwrap();
    assert!(daily.repeat.unwrap().auto_close);
    td.shutdown().await;
}

#[test]
fn interval_slots_run_from_the_start_while_not_after_the_end() {
    tz();
    let weekdays = interval(TodoRepeatDays::Weekdays, None, "09:00", 120, "18:00");
    // 2024-01-10（周三）：09:00 11:00 13:00 15:00 17:00 EST；18:00 不是时间点（09:00 + 4×2h = 17:00，下一个 19:00 过了 until）
    assert_eq!(slot_at_or_before(&weekdays, utc("2024-01-10T15:00Z")), Some(utc("2024-01-10T14:00Z"))); // 10:00
    assert_eq!(slot_after(&weekdays, utc("2024-01-10T15:00Z")), Some(utc("2024-01-10T16:00Z"))); // 11:00
    assert_eq!(slot_at_or_before(&weekdays, utc("2024-01-10T16:00Z")), Some(utc("2024-01-10T16:00Z")));
    assert_eq!(slot_after(&weekdays, utc("2024-01-10T16:00Z")), Some(utc("2024-01-10T18:00Z")));
    assert_eq!(slot_at_or_before(&weekdays, utc("2024-01-10T23:30Z")), Some(utc("2024-01-10T22:00Z"))); // 17:00
                                                                                                        // 当天最后一个之后：下一个工作日 09:00；周五之后跳过周末
    assert_eq!(slot_after(&weekdays, utc("2024-01-10T22:00Z")), Some(utc("2024-01-11T14:00Z")));
    assert_eq!(slot_after(&weekdays, utc("2024-01-12T22:00Z")), Some(utc("2024-01-15T14:00Z")));
    // 周末之后的最近一次是周五的最后一个
    assert_eq!(slot_at_or_before(&weekdays, utc("2024-01-14T20:00Z")), Some(utc("2024-01-12T22:00Z")));
    // 当天第一个之前：前一个工作日的最后一个
    assert_eq!(slot_at_or_before(&weekdays, utc("2024-01-11T13:00Z")), Some(utc("2024-01-10T22:00Z")));

    // until 含端点：09:00–17:00 每 2 小时，17:00 是时间点
    let inclusive = interval(TodoRepeatDays::Daily, None, "09:00", 120, "17:00");
    assert_eq!(slot_at_or_before(&inclusive, utc("2024-01-10T23:00Z")), Some(utc("2024-01-10T22:00Z")));
    // 15 分钟一个、weekly：只在那个星期几（2024-01-15 是周一）
    let monday = interval(TodoRepeatDays::Weekly, Some(1), "09:00", 15, "09:45");
    assert_eq!(slot_after(&monday, utc("2024-01-10T15:00Z")), Some(utc("2024-01-15T14:00Z")));
    assert_eq!(slot_after(&monday, utc("2024-01-15T14:00Z")), Some(utc("2024-01-15T14:15Z")));
    assert_eq!(slot_after(&monday, utc("2024-01-15T14:45Z")), Some(utc("2024-01-22T14:00Z")));
    // 时间点永远晚于 after_ms（相等的不算）
    for t in [utc("2024-01-10T16:00Z"), utc("2024-01-10T14:00Z"), utc("2024-01-12T22:00Z")] {
        assert!(slot_after(&weekdays, t).unwrap() > t);
    }
    // 不完整的规则没有时间点
    assert_eq!(slot_after(&rule(TodoRepeatKind::Interval, "09:00", None), utc("2024-01-10T15:00Z")), None);
}

#[test]
fn interval_slots_follow_the_local_clock_across_daylight_saving() {
    tz();
    // 2024-03-10 02:00 EST 跳到 03:00 EDT；每小时一个、00:30–04:30：02:30 不存在，落在 03:00 EDT（07:00Z），
    // 和 03:30 EDT（07:30Z）是不同的时刻；其余按当地时钟
    let r = interval(TodoRepeatDays::Daily, None, "00:30", 60, "04:30");
    assert_eq!(slot_after(&r, utc("2024-03-10T05:30Z")), Some(utc("2024-03-10T06:30Z"))); // 01:30 EST
    assert_eq!(slot_after(&r, utc("2024-03-10T06:30Z")), Some(utc("2024-03-10T07:00Z"))); // 02:30 → 03:00 EDT
    assert_eq!(slot_after(&r, utc("2024-03-10T07:00Z")), Some(utc("2024-03-10T07:30Z"))); // 03:30 EDT
    assert_eq!(slot_after(&r, utc("2024-03-10T08:30Z")), Some(utc("2024-03-11T04:30Z"))); // 04:30 EDT 之后是第二天 00:30 EDT
                                                                                          // 回拨日 2024-11-03：01:30 出现两次，只算第一次（EDT，05:30Z）；02:30 EST 在 07:30Z
    assert_eq!(slot_after(&r, utc("2024-11-03T04:30Z")), Some(utc("2024-11-03T05:30Z")));
    assert_eq!(slot_after(&r, utc("2024-11-03T05:30Z")), Some(utc("2024-11-03T07:30Z")));
}

#[tokio::test]
async fn an_interval_template_copies_at_each_slot_and_skips_while_a_copy_is_open() {
    let (td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z")); // 08:00 EST
    let rule = json!({ "kind": "interval", "at": "09:00", "days": "daily", "every_min": 120, "until": "18:00" });
    let t = add(&td, &mut c, json!({ "id": id(), "title": "Patrol", "repeat": rule })).await.unwrap();
    td.ctx.judge.clock().set(utc("2024-01-10T14:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 1);
    // 11:00 的时间点：上一份还开着，跳过
    td.ctx.judge.clock().set(utc("2024-01-10T16:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    let first = copies(&list_all(&td, &mut c).await, &t.id).remove(0);
    update(&td, &mut c, json!({ "id": first.id, "state": "done" })).await.unwrap();
    // 13:00 的时间点：关了，照常复制
    td.ctx.judge.clock().set(utc("2024-01-10T18:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 1);
    assert_eq!(copies(&list_all(&td, &mut c).await, &t.id).len(), 2);
    td.shutdown().await;
}

#[tokio::test]
async fn repeat_now_copies_once_without_touching_the_schedule() {
    let (td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z")); // 08:00 EST
    let t = add(
        &td,
        &mut c,
        json!({ "id": id(), "title": "Secret title", "body": "Secret body", "project": "home",
                "repeat": { "kind": "daily", "at": "09:00" } }),
    )
    .await
    .unwrap();
    let before = get(&td, &mut c, &t.id).await;
    let last_before: Option<i64> = sqlx::query_scalar("SELECT repeat_last_ms FROM todos WHERE id = ?")
        .bind(&t.id)
        .fetch_one(td.ctx.store.pool())
        .await
        .unwrap();
    let mut watcher = td.local_cli_client();
    td.ctx.judge.clock().set(utc("2024-01-10T13:30Z"));
    let copy: Todo = c.call(&td, methods::TODO_REPEAT_NOW, json!({ "id": t.id })).await.unwrap();
    assert_eq!(
        (copy.title.as_str(), copy.body.as_str(), copy.project.as_deref(), copy.state, copy.handler),
        ("Secret title", "Secret body", Some("home"), TodoState::Open, TodoHandler::Me)
    );
    assert_eq!(copy.source.kind, TodoSourceKind::Repeat);
    assert_eq!(copy.source.todo_id.as_deref(), Some(t.id.as_str()));
    assert!(copy.repeat.is_none() && copy.due_ms.is_none() && !copy.unreviewed);
    assert_eq!(get(&td, &mut c, &copy.id).await.id, copy.id);
    // 模板的「经过」：手动的一条；下一次时间和记录都没动
    let events = history(&td, &mut c, &t.id).await;
    assert_eq!(
        events.iter().map(|e| e.data.clone()).collect::<Vec<_>>(),
        vec![TodoEventData::RepeatCreated { todo_id: copy.id.clone(), at_ms: utc("2024-01-10T13:30Z"), manual: true }]
    );
    let after = get(&td, &mut c, &t.id).await;
    assert_eq!(after.repeat.as_ref().and_then(|r| r.next_ms), before.repeat.as_ref().and_then(|r| r.next_ms));
    assert_eq!(after.updated_ms, before.updated_ms);
    let last_after: Option<i64> = sqlx::query_scalar("SELECT repeat_last_ms FROM todos WHERE id = ?")
        .bind(&t.id)
        .fetch_one(td.ctx.store.pool())
        .await
        .unwrap();
    assert_eq!(last_before, last_after);
    // todo.changed 和审计（不含标题、正文）
    let seen: Vec<TodoChanged> = watcher
        .drain_now()
        .into_iter()
        .filter(|n| n.method == methods::TODO_CHANGED)
        .map(|n| serde_json::from_value(n.params).unwrap())
        .collect();
    assert!(seen.iter().any(|ch| ch.id == copy.id && ch.todo.is_some()), "{seen:?}");
    let audit = td.ctx.audit.query(None, None, 100).await;
    let entry = audit.iter().find(|e| e.action == "todo.repeat").expect("audit entry");
    assert_eq!(entry.target.as_deref(), Some(copy.id.as_str()));
    assert_eq!(entry.detail["manual"], json!(true));
    assert_eq!(entry.detail["template"], json!(t.id));
    assert!(!entry.detail.to_string().contains("Secret"));
    // 手动那份还开着：再点一次被拒，到点的那次也跳过
    let e = c.call::<Todo>(&td, methods::TODO_REPEAT_NOW, json!({ "id": t.id })).await.unwrap_err();
    assert_eq!(reason(&e), (ErrorCode::Invalid, Some("id".into()), Some("previous_open".into())));
    assert_eq!(e.data.unwrap().todo_id.as_deref(), Some(copy.id.as_str()));
    td.ctx.judge.clock().set(utc("2024-01-10T14:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    assert!(history(&td, &mut c, &t.id).await.iter().any(|e| matches!(e.data, TodoEventData::RepeatSkipped { .. })));
    // 关了之后又能点
    update(&td, &mut c, json!({ "id": copy.id, "state": "dropped" })).await.unwrap();
    let again: Todo = c.call(&td, methods::TODO_REPEAT_NOW, json!({ "id": t.id })).await.unwrap();
    assert_ne!(again.id, copy.id);
    td.shutdown().await;
}

#[tokio::test]
async fn repeat_now_refusals() {
    let (td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z"));
    let call = |id: String| json!({ "id": id });
    // 不存在 / id 不规范
    let e = c.call::<Todo>(&td, methods::TODO_REPEAT_NOW, call(id())).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::NotFound);
    let e = c.call::<Todo>(&td, methods::TODO_REPEAT_NOW, call("nope".into())).await.unwrap_err();
    assert_eq!(reason(&e), (ErrorCode::Invalid, Some("id".into()), None));
    // 不是周期项
    let plain = add(&td, &mut c, json!({ "id": id(), "title": "plain" })).await.unwrap();
    let e = c.call::<Todo>(&td, methods::TODO_REPEAT_NOW, call(plain.id.clone())).await.unwrap_err();
    assert_eq!(reason(&e), (ErrorCode::Invalid, Some("id".into()), Some("not_template".into())));
    // 已关闭
    let t = add(&td, &mut c, json!({ "id": id(), "title": "t", "repeat": { "kind": "daily", "at": "09:00" } }))
        .await
        .unwrap();
    update(&td, &mut c, json!({ "id": t.id, "state": "done" })).await.unwrap();
    let e = c.call::<Todo>(&td, methods::TODO_REPEAT_NOW, call(t.id.clone())).await.unwrap_err();
    assert_eq!(reason(&e), (ErrorCode::Invalid, Some("id".into()), Some("closed".into())));
    update(&td, &mut c, json!({ "id": t.id, "state": "open" })).await.unwrap();
    // 未收下（只有直接写库才有）
    sqlx::query("UPDATE todos SET unreviewed = 1 WHERE id = ?").bind(&t.id).execute(td.ctx.store.pool()).await.unwrap();
    let e = c.call::<Todo>(&td, methods::TODO_REPEAT_NOW, call(t.id.clone())).await.unwrap_err();
    assert_eq!(reason(&e), (ErrorCode::Invalid, Some("id".into()), Some("unreviewed".into())));
    sqlx::query("UPDATE todos SET unreviewed = 0 WHERE id = ?").bind(&t.id).execute(td.ctx.store.pool()).await.unwrap();
    // 只由人：自报 mcp 的连接 → forbidden，什么都不建
    let mut mcp = td.local_mcp_client();
    let e = mcp.call::<Todo>(&td, methods::TODO_REPEAT_NOW, call(t.id.clone())).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::Forbidden);
    assert!(copies(&list_all(&td, &mut c).await, &t.id).is_empty());
    // 清单满了：和 todo.add 一样的 too_many
    sqlx::query("UPDATE todos SET state = 'done', closed_ms = 1 WHERE id <> ?")
        .bind(&t.id)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    for i in 0..anycon_daemon::todos::MAX_OPEN - 1 {
        let todo = Todo {
            id: id(),
            title: format!("x{i}"),
            body: String::new(),
            project: None,
            state: TodoState::Open,
            handler: TodoHandler::Me,
            due_ms: None,
            source: TodoSource { kind: TodoSourceKind::Local, device_id: None, session_id: None, todo_id: None },
            unreviewed: false,
            created_ms: 1,
            updated_ms: 1,
            closed_ms: None,
            sessions: vec![],
            activity: None,
            repeat: None,
            remote: vec![],
        };
        td.ctx.store.todo_insert(&todo).await.unwrap();
    }
    let e = c.call::<Todo>(&td, methods::TODO_REPEAT_NOW, call(t.id.clone())).await.unwrap_err();
    assert_eq!(reason(&e), (ErrorCode::Invalid, Some("todos".into()), Some("too_many".into())));
    td.shutdown().await;
}

#[tokio::test]
async fn a_template_can_go_to_the_twin_for_every_copy_but_cannot_be_dispatched() {
    let (td, mut c) = start().await;
    let r = json!({ "kind": "daily", "at": "09:00" });
    // 新建时就交给 Twin：每一份复制出来的都交给 Twin
    let t = add(&td, &mut c, json!({ "id": id(), "title": "t", "project": "p", "handler": "twin", "repeat": r }))
        .await
        .unwrap();
    assert_eq!(t.handler, TodoHandler::Twin);
    // 模板改成交给 Twin、已经交给 Twin 的项加上规则，都可以
    let t2 = add(&td, &mut c, json!({ "id": id(), "title": "t2", "repeat": r })).await.unwrap();
    assert_eq!(
        update(&td, &mut c, json!({ "id": t2.id, "handler": "twin" })).await.unwrap().handler,
        TodoHandler::Twin
    );
    let plain = add(&td, &mut c, json!({ "id": id(), "title": "p", "handler": "twin" })).await.unwrap();
    assert!(update(&td, &mut c, json!({ "id": plain.id, "repeat": r })).await.unwrap().repeat.is_some());
    // 其余的处理人规则不变：agent 和自报 mcp 的连接不能设成 twin，未收下的项不能交给 Twin
    let mut mcp = td.local_mcp_client();
    let e = update(&td, &mut mcp, json!({ "id": t2.id, "handler": "twin" })).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::Forbidden);
    let e = add(&td, &mut c, json!({ "id": id(), "title": "x", "agent": true, "handler": "twin" })).await.unwrap_err();
    assert_eq!(reason(&e).0, ErrorCode::Forbidden);
    // 派发仍然被拒
    let e = c
        .call::<Value>(&td, methods::TODO_DISPATCH, json!({ "id": t.id, "agent": "mock", "project": "p" }))
        .await
        .unwrap_err();
    assert_eq!(reason(&e), (ErrorCode::Invalid, Some("id".into()), Some("repeat_template".into())));
    assert!(get(&td, &mut c, &t.id).await.sessions.is_empty());
    td.shutdown().await;
}

#[tokio::test]
async fn a_due_template_is_copied_once_even_across_a_restart() {
    let (mut td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z")); // 08:00 EST
    let template = add(
        &td,
        &mut c,
        json!({ "id": id(), "title": "Water the plants", "body": "# Steps\n- all of them", "project": "home",
                "due_ms": 42, "repeat": { "kind": "daily", "at": "09:00" } }),
    )
    .await
    .unwrap();
    // 还没到点（设置之前的 09:00 也不补）
    td.ctx.judge.clock().set(utc("2024-01-10T13:59Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    assert_eq!(list_all(&td, &mut c).await.len(), 1);

    let mut watcher = td.local_cli_client();
    td.ctx.judge.clock().set(utc("2024-01-10T14:00Z")); // 09:00 EST
    assert_eq!(tick(&td.ctx).await.unwrap(), 1);
    // 同一次不再复制
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    td.ctx.judge.clock().set(utc("2024-01-10T14:05Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);

    let all = list_all(&td, &mut c).await;
    let made = copies(&all, &template.id);
    assert_eq!(made.len(), 1, "{all:?}");
    let copy = &made[0];
    assert_ne!(copy.id, template.id);
    assert_eq!((copy.title.as_str(), copy.body.as_str()), ("Water the plants", "# Steps\n- all of them"));
    assert_eq!((copy.project.as_deref(), copy.state, copy.handler), (Some("home"), TodoState::Open, TodoHandler::Me));
    assert_eq!(
        copy.source,
        TodoSource {
            kind: TodoSourceKind::Repeat,
            device_id: None,
            session_id: None,
            todo_id: Some(template.id.clone())
        }
    );
    assert!(!copy.unreviewed && copy.repeat.is_none() && copy.due_ms.is_none());
    // 模板的「经过」
    let events = history(&td, &mut c, &template.id).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0].data,
        TodoEventData::RepeatCreated { todo_id: copy.id.clone(), at_ms: utc("2024-01-10T14:00Z"), manual: false }
    );
    // 复制出来的项照常发 todo.changed
    let seen: Vec<TodoChanged> = watcher
        .drain_now()
        .into_iter()
        .filter(|n| n.method == methods::TODO_CHANGED)
        .map(|n| serde_json::from_value(n.params).unwrap())
        .collect();
    assert!(seen.iter().any(|ch| ch.id == copy.id && ch.todo.is_some()), "{seen:?}");
    // 审计只记 id 和时间
    let audit = td.ctx.audit.query(None, None, 100).await;
    let entry = audit.iter().find(|e| e.action == "todo.repeat").expect("audit entry");
    assert_eq!(entry.target.as_deref(), Some(copy.id.as_str()));
    assert!(!entry.detail.to_string().contains("Water"));

    // 重启之后同一次也不再复制（时钟换成新的 ctx 之后要重新设）
    td.restart().await;
    let mut c = td.local_cli_client();
    td.ctx.judge.clock().set(utc("2024-01-10T14:10Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    assert_eq!(copies(&list_all(&td, &mut c).await, &template.id).len(), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn an_open_copy_skips_the_next_time_and_a_closed_one_does_not() {
    let (td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z"));
    let template =
        add(&td, &mut c, json!({ "id": id(), "title": "Report", "repeat": { "kind": "daily", "at": "09:00" } }))
            .await
            .unwrap();
    td.ctx.judge.clock().set(utc("2024-01-10T14:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 1);
    // 第二天：上一次的还开着，跳过，并在模板的「经过」里记一条
    td.ctx.judge.clock().set(utc("2024-01-11T14:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    assert_eq!(tick(&td.ctx).await.unwrap(), 0, "the skip is recorded once");
    let events = history(&td, &mut c, &template.id).await;
    assert_eq!(
        events.iter().map(|e| e.data.clone()).collect::<Vec<_>>(),
        vec![
            TodoEventData::RepeatCreated {
                todo_id: copies(&list_all(&td, &mut c).await, &template.id)[0].id.clone(),
                at_ms: utc("2024-01-10T14:00Z"),
                manual: false
            },
            TodoEventData::RepeatSkipped { at_ms: utc("2024-01-11T14:00Z") },
        ]
    );
    // 在 `working` 等没关闭的状态里也算没关闭
    let first = copies(&list_all(&td, &mut c).await, &template.id).remove(0);
    update(&td, &mut c, json!({ "id": first.id, "state": "working" })).await.unwrap();
    td.ctx.judge.clock().set(utc("2024-01-12T14:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    // 关闭之后下一次照常复制；被跳过的那次不补
    update(&td, &mut c, json!({ "id": first.id, "state": "done" })).await.unwrap();
    assert_eq!(tick(&td.ctx).await.unwrap(), 0, "the 01-12 time was already consumed by the skip");
    td.ctx.judge.clock().set(utc("2024-01-13T14:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 1);
    assert_eq!(copies(&list_all(&td, &mut c).await, &template.id).len(), 2);
    td.shutdown().await;
}

#[tokio::test]
async fn after_downtime_only_the_latest_time_is_made_up_and_only_within_a_day() {
    let (td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z"));
    let daily = add(&td, &mut c, json!({ "id": id(), "title": "Daily", "repeat": { "kind": "daily", "at": "09:00" } }))
        .await
        .unwrap();
    let weekly = add(
        &td,
        &mut c,
        json!({ "id": id(), "title": "Weekly", "repeat": { "kind": "weekly", "at": "09:00", "weekday": 1 } }),
    )
    .await
    .unwrap();
    // 停了六天，2024-01-16（周二）15:00 EST 起来：日项补最近一次（今天 09:00，6 小时前），不补前几天；
    // 周项最近的一次是周一 01-15 09:00，已经超过 24 小时：不补，只推进记录
    td.ctx.judge.clock().set(utc("2024-01-16T20:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 1);
    let all = list_all(&td, &mut c).await;
    assert_eq!(copies(&all, &daily.id).len(), 1);
    assert!(copies(&all, &weekly.id).is_empty());
    assert!(history(&td, &mut c, &weekly.id).await.is_empty(), "a consumed old time leaves no trace");
    assert!(history(&td, &mut c, &daily.id)
        .await
        .iter()
        .all(|e| matches!(e.data, TodoEventData::RepeatCreated { at_ms, .. } if at_ms == utc("2024-01-16T14:00Z"))));
    // 周项推进了记录：同一次不会之后又冒出来
    td.ctx.judge.clock().set(utc("2024-01-16T21:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    // 下个周一（01-22）照常复制
    update(&td, &mut c, json!({ "id": copies(&all, &daily.id)[0].id, "state": "done" })).await.unwrap();
    td.ctx.judge.clock().set(utc("2024-01-22T14:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 2);
    td.shutdown().await;
}

#[tokio::test]
async fn closed_and_unreviewed_templates_are_not_copied_and_changing_the_rule_restarts_the_count() {
    let (td, mut c) = start().await;
    td.ctx.judge.clock().set(utc("2024-01-10T13:00Z"));
    let closed =
        add(&td, &mut c, json!({ "id": id(), "title": "Closed", "repeat": { "kind": "daily", "at": "09:00" } }))
            .await
            .unwrap();
    let changed =
        add(&td, &mut c, json!({ "id": id(), "title": "Changed", "repeat": { "kind": "daily", "at": "20:00" } }))
            .await
            .unwrap();
    update(&td, &mut c, json!({ "id": closed.id, "state": "dropped" })).await.unwrap();
    // 改规则：从现在（08:00 EST）起重算，09:00 改成 07:30（已过）不补
    td.ctx.judge.clock().set(utc("2024-01-10T13:30Z"));
    update(&td, &mut c, json!({ "id": changed.id, "repeat": { "kind": "daily", "at": "08:00" } })).await.unwrap();
    td.ctx.judge.clock().set(utc("2024-01-10T14:00Z"));
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    assert_eq!(list_all(&td, &mut c).await.len(), 2);
    // 关闭的模板重新打开，只从重开之后的时间点算：重开时 09:00 已过，当天的不补
    update(&td, &mut c, json!({ "id": closed.id, "state": "open" })).await.unwrap();
    assert_eq!(tick(&td.ctx).await.unwrap(), 0);
    td.ctx.judge.clock().set(utc("2024-01-11T14:00Z"));
    let n = tick(&td.ctx).await.unwrap();
    assert_eq!(n, 2, "both templates are due on the next day");

    // agent 记的、还没收下的模板（只有直接写库才有）：只推进记录，不复制
    let sneaky = id();
    let mut row = get(&td, &mut c, &changed.id).await;
    row.id = sneaky.clone();
    row.unreviewed = true;
    row.source = TodoSource { kind: TodoSourceKind::Agent, device_id: None, session_id: None, todo_id: None };
    td.ctx.store.todo_insert(&row).await.unwrap();
    sqlx::query("UPDATE todos SET repeat_last_ms = ? WHERE id = ?")
        .bind(utc("2024-01-10T00:00Z") as i64)
        .bind(&sneaky)
        .execute(td.ctx.store.pool())
        .await
        .unwrap();
    td.ctx.judge.clock().set(utc("2024-01-12T14:00Z"));
    let before = copies(&list_all(&td, &mut c).await, &sneaky).len();
    tick(&td.ctx).await.unwrap();
    assert_eq!((before, copies(&list_all(&td, &mut c).await, &sneaky).len()), (0, 0));
    td.shutdown().await;
}
