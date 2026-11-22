//! 周期项（docs/spec/daemon.md § 26.9）：带 `repeat` 的清单项是模板，daemon 每分钟看一次，到点复制出一项新的。
//!
//! 规则按机器的本地时区算，当地时间用平台的 `localtime_r`（Windows 是 `_localtime64_s`）读，不改进程的 `TZ`。
//! 夏令时切换日按当地时钟：被跳过的时刻算在跳过之后的第一分钟，重复的时刻只算第一次出现的那一个。
//!
//! 「已经处理过的规则时间」（`repeat_last_ms`）存在库里：同一次只复制一次，重启也一样。停机错过的次数不补积压：
//! 只看最近一个该到的时间，离现在不到 24 小时才补，更早的只推进 `repeat_last_ms`。
//!
//! 复制出来的项只带标题、正文、项目和处理人；规则本身和 `due_ms` 不带。标题和正文是用户内容：不进日志、不进审计。

use std::sync::Arc;
use std::time::Duration;

use anycon_protocol::{
    RpcError, Todo, TodoActor, TodoEventData, TodoHandler, TodoRepeat, TodoRepeatKind, TodoSource, TodoSourceKind,
    TodoState, TwinTrigger,
};
use serde_json::json;
use sqlx::{Row, SqliteConnection};

use crate::clients::Caller;
use crate::daemon::Ctx;
use crate::store::{Store, StoreError};

const MINUTE_MS: u64 = 60_000;
const DAY_MS: u64 = 24 * 60 * MINUTE_MS;
/// 停机之后只补最近一次、并且离现在不到这么久的。
pub const CATCH_UP_MS: u64 = DAY_MS;

// ---------------- 当地时间 ----------------

/// 当地日历时间，精确到分。
#[derive(Clone, Copy)]
struct Civil {
    year: i64,
    mon: i64,
    day: i64,
    hour: i64,
    min: i64,
}

#[cfg(unix)]
fn local_civil(ms: u64) -> Option<Civil> {
    let seconds: libc::time_t = (ms / 1000).try_into().ok()?;
    // 指针指向有效的栈内存；localtime_r 不共享可变的 tm 缓冲区。
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&seconds, &mut tm).is_null() {
            return None;
        }
        Some(Civil {
            year: i64::from(tm.tm_year) + 1900,
            mon: i64::from(tm.tm_mon) + 1,
            day: i64::from(tm.tm_mday),
            hour: i64::from(tm.tm_hour),
            min: i64::from(tm.tm_min),
        })
    }
}

#[cfg(windows)]
fn local_civil(ms: u64) -> Option<Civil> {
    // Windows CRT 的 tm 是九个 int；使用 64 位 time_t，避免 2038 年溢出。
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
    }
    unsafe extern "C" {
        fn _localtime64_s(result: *mut Tm, time: *const i64) -> i32;
    }
    let seconds = i64::try_from(ms / 1000).ok()?;
    // CRT 写入已分配的结构。
    unsafe {
        let mut tm: Tm = std::mem::zeroed();
        if _localtime64_s(&mut tm, &seconds) != 0 {
            return None;
        }
        Some(Civil {
            year: i64::from(tm.year) + 1900,
            mon: i64::from(tm.mon) + 1,
            day: i64::from(tm.mday),
            hour: i64::from(tm.hour),
            min: i64::from(tm.min),
        })
    }
}

/// 公历日期 → 从 1970-01-01 起的天数（Howard Hinnant 的 days_from_civil）。
fn days_from_civil(year: i64, mon: i64, day: i64) -> i64 {
    let y = if mon <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mon + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// ISO 星期：周一 = 1 … 周日 = 7。1970-01-01 是周四。
fn iso_weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7) + 1
}

/// 当地时钟读数换成「从 1970-01-01 起的当地分钟数」：单调（夏令时回拨的那一小时除外），用来比较当地先后。
fn local_minutes(minute: i64) -> Option<i64> {
    let c = local_civil(u64::try_from(minute.checked_mul(60)?).ok()?.checked_mul(1000)?)?;
    Some(days_from_civil(c.year, c.mon, c.day) * 1440 + c.hour * 60 + c.min)
}

/// 当地某天（`days`）的 `hour:min` 对应的时刻（毫秒）。落在夏令时跳过的那段里，取跳过之后的第一分钟；
/// 落在回拨重复的那段里，取第一次出现的。
fn instant_of(days: i64, hour: i64, min: i64) -> Option<u64> {
    let want = days * 1440 + hour * 60 + min;
    // 用当地偏移估一个时刻；偏移在估计点和真值之间变过的话，下面逐分钟找
    let guess = {
        let first = want - (local_minutes(want)? - want);
        want - (local_minutes(first)? - first)
    };
    let to_ms = |m: i64| u64::try_from(m).ok()?.checked_mul(MINUTE_MS);
    let (before, after) = (local_minutes(guess - 180)?, local_minutes(guess + 180)?);
    if local_minutes(guess)? == want && before == want - 180 && after == want + 180 {
        return to_ms(guess);
    }
    // 前后三小时内偏移变过：从前往后找第一个当地读数不早于目标的分钟
    for t in guess - 180..=guess + 180 {
        if local_minutes(t)? >= want {
            return to_ms(t);
        }
    }
    to_ms(guess)
}

/// `HH:MM`（`00:00`–`23:59`，必须是两位数字）。
pub fn parse_at(at: &str) -> Option<(i64, i64)> {
    let b = at.as_bytes();
    if b.len() != 5 || b[2] != b':' || ![0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit()) {
        return None;
    }
    let (h, m) =
        (i64::from(b[0] - b'0') * 10 + i64::from(b[1] - b'0'), i64::from(b[3] - b'0') * 10 + i64::from(b[4] - b'0'));
    (h < 24 && m < 60).then_some((h, m))
}

fn on_day(r: &TodoRepeat, days: i64) -> bool {
    let iso = iso_weekday(days);
    match r.kind {
        TodoRepeatKind::Daily => true,
        TodoRepeatKind::Weekdays => iso <= 5,
        TodoRepeatKind::Weekly => r.weekday.is_some_and(|w| i64::from(w) == iso),
        TodoRepeatKind::Unknown => false,
    }
}

fn local_day(ms: u64) -> Option<i64> {
    let c = local_civil(ms)?;
    Some(days_from_civil(c.year, c.mon, c.day))
}

/// 规则上不晚于 `now_ms` 的最近一个时间（毫秒）。
pub fn slot_at_or_before(r: &TodoRepeat, now_ms: u64) -> Option<u64> {
    let (h, m) = parse_at(&r.at)?;
    let today = local_day(now_ms)?;
    (0..=8).map(|k| today - k).filter(|&d| on_day(r, d)).find_map(|d| instant_of(d, h, m).filter(|&t| t <= now_ms))
}

/// 规则上晚于 `after_ms` 的第一个时间（毫秒）。
pub fn slot_after(r: &TodoRepeat, after_ms: u64) -> Option<u64> {
    let (h, m) = parse_at(&r.at)?;
    let today = local_day(after_ms)?;
    (0..=8).map(|k| today + k).filter(|&d| on_day(r, d)).find_map(|d| instant_of(d, h, m).filter(|&t| t > after_ms))
}

/// 给要发出去的项填上模板的下一次时间；已关闭的模板没有。
pub(crate) fn fill_next(c: &Ctx, t: &mut Todo) {
    let closed = t.state.is_closed();
    if let Some(r) = t.repeat.as_mut() {
        r.next_ms = if closed { None } else { slot_after(r, c.judge.clock().now()) };
    }
}

/// 请求里的规则只留规则本身（`next_ms` 是算出来的，请求带了也不算）。
pub(crate) fn normalized(r: &TodoRepeat) -> TodoRepeat {
    TodoRepeat { next_ms: None, ..r.clone() }
}

// ---------------- 存储 ----------------

/// 记下这个模板最近处理过的规则时间；`None` 清掉（不再是模板）。
pub(crate) async fn set_last_on(conn: &mut SqliteConnection, id: &str, last: Option<u64>) -> Result<(), StoreError> {
    sqlx::query("UPDATE todos SET repeat_last_ms = ? WHERE id = ?")
        .bind(last.map(|v| v as i64))
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

impl Store {
    /// 没关闭的模板和它们各自最近处理过的规则时间。
    async fn repeat_templates(&self) -> Result<Vec<(Todo, Option<u64>)>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM todos WHERE repeat_kind IS NOT NULL AND state NOT IN ('done','dropped') \
             ORDER BY created_ms, id",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .iter()
            .map(|r| (crate::todos::row_to_todo(r), r.get::<Option<i64>, _>("repeat_last_ms").map(|v| v as u64)))
            .collect())
    }

    /// 这个模板最近复制出来的一项有没有关闭；还没复制过时为 `None`。
    async fn repeat_latest_closed(&self, template: &str) -> Result<Option<bool>, StoreError> {
        let state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM todos WHERE source_kind = 'repeat' AND source_todo = ? \
             ORDER BY created_ms DESC, id DESC LIMIT 1",
        )
        .bind(template)
        .fetch_optional(self.pool())
        .await?;
        Ok(state.map(|s| matches!(s.as_str(), "done" | "dropped")))
    }
}

// ---------------- 调度 ----------------

fn internal(e: StoreError) -> RpcError {
    tracing::warn!(error = %e, "todo repeat storage error");
    RpcError::new(anycon_protocol::ErrorCode::Internal, "todo storage error")
}

/// 看一遍所有模板，到点的复制出新的一项。返回这一遍复制了几项。时间取判断后端那只可替换的时钟。
pub async fn tick(c: &Ctx) -> Result<usize, RpcError> {
    let _w = c.store.todo_lock().lock().await;
    let now = c.judge.clock().now();
    let mut created = 0;
    for (template, last) in c.store.repeat_templates().await.map_err(internal)? {
        let Some(rule) = template.repeat.as_ref().filter(|r| r.kind != TodoRepeatKind::Unknown) else { continue };
        let Some(slot) = slot_at_or_before(rule, now) else { continue };
        let Some(last) = last else {
            // 没有记录（不该出现）：从现在起算
            let mut conn = c.store.pool().acquire().await.map_err(StoreError::from).map_err(internal)?;
            set_last_on(&mut conn, &template.id, Some(now)).await.map_err(internal)?;
            continue;
        };
        if slot <= last {
            continue;
        }
        if fire(c, &template, slot, now).await? {
            created += 1;
        }
    }
    Ok(created)
}

/// 处理模板的一个到点的规则时间；返回有没有复制出新的一项。
async fn fire(c: &Ctx, template: &Todo, slot: u64, now: u64) -> Result<bool, RpcError> {
    let sql = |e: sqlx::Error| internal(StoreError::from(e));
    // 太久以前的（停机期间错过的）和还没人收下的模板：只推进记录，不复制、不记「经过」
    let stale = now.saturating_sub(slot) >= CATCH_UP_MS;
    let mut tx = c.store.pool().begin().await.map_err(sql)?;
    set_last_on(&mut tx, &template.id, Some(slot)).await.map_err(internal)?;
    if stale || template.unreviewed {
        tx.commit().await.map_err(sql)?;
        return Ok(false);
    }
    let latest_closed = c.store.repeat_latest_closed(&template.id).await.map_err(internal)?;
    let full = c.store.todo_count_open().await.map_err(internal)? >= crate::todos::MAX_OPEN;
    if latest_closed == Some(false) || full {
        // 上一次的还没关闭（或清单已满）：这一次不复制，在模板的「经过」里记一条
        let ev = crate::todo_dispatch::event_append_on(
            &mut tx,
            &template.id,
            now,
            TodoEventData::RepeatSkipped { at_ms: slot },
        )
        .await
        .map_err(internal)?;
        tx.commit().await.map_err(sql)?;
        crate::todo_dispatch::broadcast_event(&c.clients, &template.id, ev);
        return Ok(false);
    }
    let copy = Todo {
        id: uuid::Uuid::new_v4().to_string(),
        title: template.title.clone(),
        body: template.body.clone(),
        project: template.project.clone(),
        state: TodoState::Open,
        handler: template.handler,
        due_ms: None,
        source: TodoSource {
            kind: TodoSourceKind::Repeat,
            device_id: None,
            session_id: None,
            todo_id: Some(template.id.clone()),
        },
        unreviewed: false,
        created_ms: now,
        updated_ms: now,
        closed_ms: None,
        sessions: Vec::new(),
        activity: None,
        repeat: None,
    };
    crate::todos::insert_todo_on(&mut *tx, &copy).await.map_err(internal)?;
    let created = crate::todo_dispatch::event_append_on(
        &mut tx,
        &template.id,
        now,
        TodoEventData::RepeatCreated { todo_id: copy.id.clone(), at_ms: slot },
    )
    .await
    .map_err(internal)?;
    let handler_event = if copy.handler != TodoHandler::Me {
        let by = TodoActor { kind: TodoSourceKind::Repeat, id: Some(template.id.clone()), device_id: None };
        let data = TodoEventData::Handler { from: TodoHandler::Me, to: copy.handler, by };
        Some(crate::todo_dispatch::event_append_on(&mut tx, &copy.id, now, data).await.map_err(internal)?)
    } else {
        None
    };
    tx.commit().await.map_err(sql)?;
    crate::todo_dispatch::broadcast_event(&c.clients, &template.id, created);
    if let Some(ev) = handler_event {
        crate::todo_dispatch::broadcast_event(&c.clients, &copy.id, ev);
    }
    // 审计只记模板 id 和规则时间，不记标题、正文
    c.audit
        .record(&Caller::daemon(), "todo.repeat", Some(&copy.id), json!({ "template": template.id, "at_ms": slot }))
        .await;
    c.sessions.todo_signal().note_sent(&copy.id, None);
    crate::todos::notify_changed(
        &c.clients,
        anycon_protocol::TodoChanged { id: copy.id.clone(), todo: Some(copy.clone()), deleted: false },
    );
    if copy.handler == TodoHandler::Twin {
        c.twin.trigger(c, TwinTrigger::Todo).await?;
    }
    Ok(true)
}

/// 后台任务：启动时先看一遍（补停机期间错过的最近一次），之后每分钟的开头看一次。
pub fn spawn(c: &Arc<Ctx>) -> tokio::task::JoinHandle<()> {
    let c = c.clone();
    tokio::spawn(async move {
        let mut shutdown = c.shutdown_tx.subscribe();
        loop {
            if *shutdown.borrow() {
                break;
            }
            if let Err(e) = tick(&c).await {
                tracing::warn!(error = %e.message, "todo repeat tick failed");
            }
            let wait = MINUTE_MS - c.judge.clock().now() % MINUTE_MS + 200;
            tokio::select! {
                _ = shutdown.changed() => {},
                _ = tokio::time::sleep(Duration::from_millis(wait)) => {},
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不设时区，在机器自己的时区里算：每个平台（包括 Windows 上用 CRT 算当地时间的那条路径）都跑。
    #[test]
    fn the_next_time_is_ahead_within_a_week_and_the_last_one_is_behind_in_any_zone() {
        let now = 1_732_000_000_000; // 2024-11-19，不在任何夏令时切换日附近
        let day = 24 * 3600 * 1000;
        for (kind, weekday) in
            [(TodoRepeatKind::Daily, None), (TodoRepeatKind::Weekdays, None), (TodoRepeatKind::Weekly, Some(3))]
        {
            let r = TodoRepeat { kind, at: "09:30".into(), weekday, next_ms: None };
            let next = slot_after(&r, now).expect("next slot");
            assert!(next > now && next <= now + 7 * day, "{kind:?}: {next}");
            let last = slot_at_or_before(&r, now).expect("last slot");
            assert!(last <= now && last > now - 8 * day, "{kind:?}: {last}");
            // 两次之间至少隔一天（同一个时刻不会算两次）
            assert!(next - last >= day - 3600 * 1000, "{kind:?}: {last} .. {next}");
        }
    }
}
