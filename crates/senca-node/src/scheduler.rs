//! 予約の実行（REQ-002、ADR-0006）。250 ms ごと、または予約の変更で起きる。
//!
//! 決まり:
//! - 状態の遷移は保存してから副作用を起こす（Running を保存 → 録画の開始）。
//! - 開始時刻（pre-roll を引いた時刻）を過ぎても `grace_ms` 以内なら途中から実行、超えたら missed。
//! - ノードが止まっている間に終了時刻を過ぎた Running は、起動時に completed（録画は回収で interrupted）にする。
//! - 繰り返しは次の回を run として記録し、全回が終わったら completed。

use crate::app::App;
use senca_control::model::*;
use senca_control::state::Command;
use std::sync::Arc;
use std::time::Duration;

const TICK_MS: u64 = 250;

pub async fn run(app: Arc<App>) {
    let notify = app.scheduler_wake.clone();
    loop {
        let a2 = app.clone();
        let _ = tokio::task::spawn_blocking(move || tick(&a2, crate::util::now_ms())).await;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(TICK_MS)) => {}
            _ = notify.notified() => {}
        }
        if app.shutting_down() {
            return;
        }
    }
}

/// 予約 1 件の「今の回」（繰り返しなら展開して、終わっていない最初の回）。
fn current_occurrence(app: &App, s: &Schedule, now: i64) -> Option<(u32, i64, i64)> {
    match &s.recurrence {
        None => Some((0, s.start_at_ms, s.end_at_ms)),
        Some(r) => {
            let tz = app.timezone(&r.timezone)?;
            let done: Vec<u32> = s
                .runs
                .iter()
                .filter(|x| matches!(x.state, ScheduleState::Completed | ScheduleState::Missed | ScheduleState::Failed))
                .map(|x| x.occurrence)
                .collect();
            let len = s.end_at_ms - s.start_at_ms;
            senca_control::schedule::expand(s.start_at_ms, len, r, &tz, now - len - 1, 64)
                .into_iter()
                .find(|(n, _, _)| !done.contains(n))
        }
    }
}

pub fn tick(app: &App, now: i64) {
    let schedules: Vec<Schedule> = {
        let core = app.control.lock().unwrap_or_else(|e| e.into_inner());
        core.state
            .schedules
            .values()
            .filter(|s| matches!(s.state, ScheduleState::Pending | ScheduleState::Running))
            .cloned()
            .collect()
    };
    for s in schedules {
        let Some((occ, start, end)) = current_occurrence(app, &s, now) else {
            // 繰り返しの回が尽きた
            if s.recurrence.is_some() && s.state != ScheduleState::Running {
                let run = s.runs.last().cloned();
                if let Some(run) = run {
                    record(app, &s, ScheduleState::Completed, run);
                }
            }
            continue;
        };
        let begin = start - s.pre_roll_ms as i64;
        let finish = end + s.post_roll_ms as i64;
        let run_existing = s.runs.iter().find(|r| r.occurrence == occ).cloned();
        let running = run_existing.as_ref().is_some_and(|r| r.state == ScheduleState::Running);
        if running {
            if now >= finish {
                let mut run = run_existing.unwrap();
                if let Some(rid) = &run.recording {
                    app.stop_recording(rid, Some(end));
                }
                if s.action == ScheduleAction::Snapshot {
                    app.set_schedule_snapshots(&s.stream, &s.id, None);
                }
                run.state = ScheduleState::Completed;
                let overall = if s.recurrence.is_some() { ScheduleState::Pending } else { ScheduleState::Completed };
                record(app, &s, overall, run);
            }
            continue;
        }
        if now < begin {
            continue;
        }
        let mut run = ScheduleRun {
            occurrence: occ,
            start_at_ms: start,
            end_at_ms: end,
            state: ScheduleState::Running,
            recording: None,
            reason: None,
            fencing_token: None,
        };
        if now >= finish || now > begin + s.grace_ms as i64 {
            run.state = ScheduleState::Missed;
            run.reason = Some("開始時刻を猶予より過ぎていました".into());
            let overall = if s.recurrence.is_some() { ScheduleState::Pending } else { ScheduleState::Missed };
            record(app, &s, overall, run);
            continue;
        }
        // fencing token を取ってから実行する（分散で二重実行しないため。単一ノードでも同じ手順）
        let token = match app.propose(Command::NextFencingToken) {
            Ok(()) => app.control.lock().unwrap_or_else(|e| e.into_inner()).state.fencing_counter,
            Err(e) => {
                app.log(&format!("scheduler: fencing token: {e}"));
                continue;
            }
        };
        run.fencing_token = Some(token);
        // 先に Running を保存する
        if !record(app, &s, ScheduleState::Running, run.clone()) {
            continue;
        }
        let stream = {
            let core = app.control.lock().unwrap_or_else(|e| e.into_inner());
            core.state.tenant_stream(&s.tenant, &s.stream).ok().cloned()
        };
        let Some(stream) = stream else {
            run.state = ScheduleState::Failed;
            run.reason = Some("stream がありません".into());
            record(app, &s, ScheduleState::Failed, run);
            continue;
        };
        match s.action {
            ScheduleAction::Record => {
                match app.start_recording(&stream, s.name.clone(), s.retention_days, Some(s.id.clone()), Some(start)) {
                    Ok(rec) => {
                        run.recording = Some(rec.id);
                        record(app, &s, ScheduleState::Running, run);
                    }
                    Err(e) => {
                        run.state = ScheduleState::Failed;
                        run.reason = Some(e.title.clone());
                        record(app, &s, if s.recurrence.is_some() { ScheduleState::Pending } else { ScheduleState::Failed }, run);
                    }
                }
            }
            ScheduleAction::Live => {
                // 窓の間だけ publish を許す: stream の可否は app.publish_allowed が予約を見て決める
            }
            ScheduleAction::Snapshot => {
                app.set_schedule_snapshots(&s.stream, &s.id, s.snapshot_interval_secs);
            }
        }
    }
}

fn record(app: &App, s: &Schedule, state: ScheduleState, run: ScheduleRun) -> bool {
    match app.propose(Command::RecordScheduleRun { tenant: s.tenant.clone(), id: s.id.clone(), state, run }) {
        Ok(()) => true,
        Err(e) => {
            app.log(&format!("scheduler: record run for {}: {e}", s.id));
            false
        }
    }
}
