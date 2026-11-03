//! グリッドの統合（ADR-0013、senca-grid）。
//!
//! - 各ノード: 起動時に自分の資源（CPU・メモリ・帯域・GPU・キャプチャ機器・ラベル）を `RegisterNode` で申告し、
//!   毎秒の心拍をリーダーへ送る（心拍は合意ログに載せない。クラスタの接続の枠 K_HEARTBEAT）。
//! - リーダー: 心拍から停止を見つけ（`NodeDown`・`DeviceDown`）、毎秒 `plan()` で配置を決めて提案する。
//!   リーダーが替わったら検出器を作り直す（最初の 1 周期は猶予）。
//! - 各ノード: 適用された割り当てのうち自分の分だけを動かす（入力・出力・パイプライン。`io.rs`・`pipelines.rs`）。
//!   割り当てが別のノードへ移ったら止める（token が変わる）。
//!
//! 単一ノードでも同じ仕組みで動く（自分がリーダー）。

use crate::app::App;
use senca_control::state::Command;
use senca_grid::{
    CaptureDevice, DeviceHealth, FailureDetector, GridCommand, Gpu, Heartbeat, NodeSpec, PlanConfig, Resources, Role, Running,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 心拍の間隔と、途絶えたと見なすまでの時間（ADR-0013）。
const HEARTBEAT: Duration = Duration::from_secs(1);
const DOWN_AFTER_MS: i64 = 5000;
/// 申告の見直しの間隔（装置の増減・ラベルの変更）。
const RESPEC_EVERY: u32 = 30;

#[derive(Default)]
pub struct GridRuntime {
    /// リーダーのときだけ持つ。
    detector: Mutex<Option<FailureDetector>>,
    /// 計画の提案が進行中（重ねて出さない）。
    planning: AtomicBool,
    /// 自分の申告（最後に提案したもの）。
    spec: Mutex<Option<NodeSpec>>,
    /// 仕事が増えた・消えた: 次の周期を待たずに計画する（リーダーのとき）。
    kick: tokio::sync::Notify,
}

/// 外向きの帯域（Mbit/s）の既定。`--net-mbps` で変える。
pub const DEFAULT_NET_MBPS: u64 = 10_000;

/// GPU の供給元から作り手の名前。
fn vendor_of(p: senca_codec::ProviderId) -> &'static str {
    use senca_codec::ProviderId::*;
    match p {
        Nvidia => "nvidia",
        IntelVpl => "intel",
        Amf => "amd",
        Vaapi => "vaapi",
        _ => "software",
    }
}

fn meminfo_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(4096)
}

/// このノードの申告を作る（ハードウェアを調べる。ブロックしてよいスレッドから呼ぶ）。
pub fn local_spec(app: &App) -> NodeSpec {
    let cpus = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1);
    // OS・制御面の取り分を残す
    let capacity = Resources {
        cpu_millis: app.cfg.capacity_cpu_millis.unwrap_or(cpus * 1000 * 9 / 10),
        memory_mb: app.cfg.capacity_memory_mb.unwrap_or(meminfo_mb() * 85 / 100),
        net_in_mbps: app.cfg.net_mbps,
        net_out_mbps: app.cfg.net_mbps,
    };
    // GPU: ハードウェアの符号化・復号の能力を装置ごとにまとめる
    let mut gpus: BTreeMap<String, Gpu> = BTreeMap::new();
    if let Some(c) = &app.codec {
        for cap in c.capability_list().into_iter().filter(|c| c.hardware) {
            let id = format!("{}:{}", cap.provider.as_str(), cap.device_index);
            let g = gpus.entry(id.clone()).or_insert_with(|| Gpu {
                id,
                vendor: vendor_of(cap.provider).into(),
                encode_sessions: 0,
                decode_sessions: 0,
                memory_mb: 0,
                healthy: true,
                disabled: false,
            });
            // 上限が分からなければ控えめな値（消費者向けの NVENC は 5 本など、分かれば能力に載る）
            let n = cap.max_sessions.unwrap_or(8);
            match cap.direction {
                senca_codec::Direction::Encode => g.encode_sessions = g.encode_sessions.max(n),
                senca_codec::Direction::Decode => g.decode_sessions = g.decode_sessions.max(n.max(16)),
            }
        }
    }
    // キャプチャ機器（worker が無ければ無し）
    let capture: Vec<CaptureDevice> = crate::capture::listing(app)
        .get("devices")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let id = d.get("id").and_then(|v| v.as_str())?;
                    let kind = d.get("kind").or_else(|| d.get("backend")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    Some(CaptureDevice { id: id.to_string(), kind, healthy: true, disabled: false })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut labels: BTreeMap<String, String> = app.cfg.labels.clone();
    labels.insert("senca.io/name".into(), app.cfg.node_name.clone());
    // 部屋（WebRTC の参加者）を受けられるノード
    labels.insert("senca.io/webrtc".into(), app.cfg.webrtc_udp.is_some().to_string());
    let mut roles: BTreeSet<Role> = [Role::Media].into_iter().collect();
    let voter = app.cfg.cluster.as_ref().is_none_or(|c| c.peers.iter().any(|p| p.id == c.node_id));
    if voter {
        roles.insert(Role::Control);
    }
    let draining = app.control.lock().unwrap().state.nodes.get(&app.node_id).is_some_and(|n| n.draining);
    NodeSpec { id: app.node_id.clone(), roles, capacity, gpus: gpus.into_values().collect(), capture, labels, draining }
}

/// このノードが今動かしている仕事（心拍に載せる）。
fn running(app: &App) -> Vec<Running> {
    let mut v = app.io.running_workloads();
    v.extend(app.pipelines.running_workloads());
    v
}

fn heartbeat(app: &App, spec: &NodeSpec) -> Heartbeat {
    let streams: Vec<String> = {
        let relay = app.relay.get();
        app.hubs
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, h)| h.is_live() && !relay.is_some_and(|r| r.is_mirror(id)))
            .map(|(id, _)| id.clone())
            .collect()
    };
    let devices = spec
        .gpus
        .iter()
        .map(|g| DeviceHealth { id: g.id.clone(), healthy: g.healthy })
        .chain(spec.capture.iter().map(|c| DeviceHealth { id: c.id.clone(), healthy: c.healthy }))
        .collect();
    Heartbeat { node: app.node_id.clone(), usage: Resources::default(), devices, streams, running: running(app) }
}

/// 提案する（ブロックするので専用のスレッドで）。結果は捨てる（次の周期でやり直す）。
fn propose_bg(app: &Arc<App>, cmd: GridCommand, what: &'static str) {
    let me = app.clone();
    tokio::task::spawn_blocking(move || {
        // 古い計画・状態に合わない決定は普通に起きる（次の周期で計画し直す）
        if let Err(e) = me.propose(Command::Grid { cmd })
            && e.status != 409
        {
            me.log(&format!("grid: {what} failed: {}", e.title));
        }
    });
}

/// 心拍を受ける（リーダーのとき。他のノードから・自分から）。
pub fn on_heartbeat(app: &App, hb: Heartbeat) {
    let now = crate::util::now_ms();
    if let Some(d) = app.grid.detector.lock().unwrap().as_mut() {
        d.heartbeat(hb, now);
    }
}

fn is_leader(app: &App) -> bool {
    match (app.cluster.get(), app.cfg.cluster.as_ref()) {
        (Some(c), Some(cc)) => c.leader() == Some(cc.node_id),
        _ => true,
    }
}

/// 適用されたコマンドを見て、このノードの仕事を突き合わせ直す（どのノードで受けた変更でも、割り当ての変更でも）。
fn watch_applied(app: &Arc<App>) {
    let mut rx = app.control.lock().unwrap().applied_tx.subscribe();
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            // (突き合わせるか, 計画し直すか)
            let (relevant, replan) = match rx.recv().await {
                Ok(a) => match a.cmd.as_ref() {
                    // 配置係の決定そのもの: 突き合わせるだけ（計画は不動点なので、し直しても何も出ない）
                    Command::Grid { cmd: GridCommand::ApplyDecisions { .. } } => (true, false),
                    Command::Grid { .. }
                    | Command::PutInput { .. }
                    | Command::DeleteInput { .. }
                    | Command::PutOutput { .. }
                    | Command::DeleteOutput { .. }
                    | Command::PutPipeline { .. }
                    | Command::DeletePipeline { .. }
                    | Command::DeleteStream { .. } => (true, true),
                    // 部屋: 自分に置かれた部屋を作る・移す（グリッドの決定の後にも）
                    Command::PutRoom { .. } | Command::DeleteRoom { .. } => (true, true),
                    _ => (false, false),
                },
                // 取りこぼした: 念のため突き合わせる
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => (true, true),
                Err(_) => return,
            };
            if relevant {
                app.io.reconcile(&app);
                app.pipelines.reconcile(&app);
                if let Some(r) = app.rtc.get() {
                    r.room_cmd(crate::rooms::RoomCmd::Reconcile);
                }
            }
            if replan {
                app.grid.kick.notify_one();
            }
        }
    });
}

/// 起動する: 申告・心拍・（リーダーなら）停止の検出と配置。
pub fn start(app: &Arc<App>) {
    watch_applied(app);
    let app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut n: u32 = 0;
        loop {
            let kicked = tokio::select! {
                _ = tick.tick() => false,
                _ = app.grid.kick.notified() => true,
            };
            if kicked {
                // 仕事が変わった: リーダーなら、心拍を待たずに計画する
                if is_leader(&app)
                    && let Some(spec) = app.grid.spec.lock().unwrap().clone()
                {
                    let hb = heartbeat(&app, &spec);
                    leader_round(&app, hb);
                }
                continue;
            }
            // 申告（最初と、時々見直す。変わったときだけ提案する）
            if n.is_multiple_of(RESPEC_EVERY) || app.grid.spec.lock().unwrap().is_none() {
                let a = app.clone();
                let spec = tokio::task::spawn_blocking(move || local_spec(&a)).await.ok();
                if let Some(mut spec) = spec {
                    let registered = app.control.lock().unwrap().state.grid.nodes.get(&app.node_id).map(|x| x.spec.clone());
                    // 手で止めた印は制御状態のもの（申告と比べるときに写す。申告し直しでも消えない）
                    if let Some(r) = &registered {
                        for g in &mut spec.gpus {
                            g.disabled = r.gpus.iter().any(|o| o.id == g.id && o.disabled);
                        }
                        for c in &mut spec.capture {
                            c.disabled = r.capture.iter().any(|o| o.id == c.id && o.disabled);
                        }
                    }
                    if registered.as_ref() != Some(&spec) {
                        let a = app.clone();
                        let s2 = spec.clone();
                        let ok = tokio::task::spawn_blocking(move || {
                            a.propose(Command::Grid { cmd: GridCommand::RegisterNode { spec: s2, now_ms: crate::util::now_ms() } }).is_ok()
                        })
                        .await
                        .unwrap_or(false);
                        if ok {
                            app.log(&format!(
                                "grid: registered (cpu {} m, mem {} MB, gpus {}, capture {})",
                                spec.capacity.cpu_millis,
                                spec.capacity.memory_mb,
                                spec.gpus.len(),
                                spec.capture.len()
                            ));
                        } else {
                            // まだ合意が取れない（リーダーが決まっていない）: 次の周期でやり直す
                            *app.grid.spec.lock().unwrap() = None;
                            n = n.wrapping_add(1);
                            continue;
                        }
                    }
                    *app.grid.spec.lock().unwrap() = Some(spec);
                }
            }
            n = n.wrapping_add(1);
            let Some(spec) = app.grid.spec.lock().unwrap().clone() else { continue };
            let hb = heartbeat(&app, &spec);
            if !is_leader(&app) {
                *app.grid.detector.lock().unwrap() = None;
                if let Some(c) = app.cluster.get()
                    && let Some(l) = c.leader()
                    && let Ok(b) = serde_json::to_vec(&hb)
                {
                    c.send_heartbeat(l, &b);
                }
                continue;
            }
            leader_round(&app, hb);
        }
    });
}

/// リーダーの 1 周期: 停止の検出 → 配置。
fn leader_round(app: &Arc<App>, own: Heartbeat) {
    let now = crate::util::now_ms();
    let (cmds, plan) = {
        let mut det = app.grid.detector.lock().unwrap();
        let d = det.get_or_insert_with(|| {
            app.log("grid: became the scheduler (leader)");
            FailureDetector::new(DOWN_AFTER_MS, now)
        });
        d.heartbeat(own, now);
        let core = app.control.lock().unwrap();
        let grid = &core.state.grid;
        let cmds = d.tick(grid, now);
        // 停止の知らせを先に適用してから計画する（同じ周期で古い状態の計画を出さない）
        let plan = if cmds.is_empty() && !app.grid.planning.load(Ordering::Relaxed) {
            Some(grid.plan(now, grid.fencing_counter, &d.stream_nodes_at(now), &PlanConfig::default()))
        } else {
            None
        };
        (cmds, plan)
    };
    for c in cmds {
        let what = match &c {
            GridCommand::NodeDown { node, .. } => {
                app.log(&format!("grid: node {node} is down (no heartbeat for {} s)", DOWN_AFTER_MS / 1000));
                "node down"
            }
            GridCommand::NodeUp { node, .. } => {
                app.log(&format!("grid: node {node} is back"));
                "node up"
            }
            GridCommand::DeviceDown { node, device } => {
                app.log(&format!("grid: device {device} on node {node} is unhealthy"));
                "device down"
            }
            GridCommand::DeviceUp { node, device } => {
                app.log(&format!("grid: device {device} on node {node} is healthy again"));
                "device up"
            }
            _ => "detector",
        };
        propose_bg(app, c, what);
    }
    let Some(plan) = plan else { return };
    if plan.is_empty() {
        return;
    }
    for d in &plan.decisions {
        app.log(&format!("grid: {}", describe(d)));
    }
    app.grid.planning.store(true, Ordering::Relaxed);
    let me = app.clone();
    let cmd = plan.into_command();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = me.propose(Command::Grid { cmd })
            && e.status != 409
        {
            me.log(&format!("grid: plan failed: {}", e.title));
        }
        me.grid.planning.store(false, Ordering::Relaxed);
    });
}

fn describe(d: &senca_grid::Decision) -> String {
    serde_json::to_string(d).unwrap_or_default()
}

/// GET /api/v1/grid の中身。
pub fn overview(app: &App) -> serde_json::Value {
    use serde_json::json;
    let core = app.control.lock().unwrap();
    let g = &core.state.grid;
    let last: BTreeMap<String, i64> = app
        .grid
        .detector
        .lock()
        .unwrap()
        .as_ref()
        .map(|d| g.nodes.keys().filter_map(|n| d.last(n).map(|(t, _)| (n.clone(), t))).collect())
        .unwrap_or_default();
    let nodes: Vec<_> = g
        .nodes
        .values()
        .map(|n| {
            // 予約（割り当ての要求の和）
            let mut used = Resources::default();
            let mut gpu_used: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
            for a in g.assignments.values().filter(|a| a.node == n.spec.id) {
                if let Some(w) = g.workloads.get(&a.workload) {
                    let r = w.req(a.level);
                    used = used.plus(&r.resources);
                    if let (Some(dev), Some(gr)) = (&a.device, &r.gpu) {
                        let e = gpu_used.entry(dev.as_str()).or_default();
                        e.0 += gr.encode;
                        e.1 += gr.decode;
                    }
                }
            }
            json!({
                "id": n.spec.id,
                "name": core.state.nodes.get(&n.spec.id).map(|x| x.name.clone()),
                "up": n.status.up,
                "draining": n.spec.draining,
                "roles": n.spec.roles,
                "labels": n.spec.labels,
                "capacity": n.spec.capacity,
                "reserved": used,
                "gpus": n.spec.gpus.iter().map(|gp| {
                    let u = gpu_used.get(gp.id.as_str()).copied().unwrap_or_default();
                    json!({ "id": gp.id, "vendor": gp.vendor, "healthy": gp.healthy, "enabled": !gp.disabled,
                            "encode_sessions": gp.encode_sessions, "decode_sessions": gp.decode_sessions,
                            "encode_reserved": u.0, "decode_reserved": u.1 })
                }).collect::<Vec<_>>(),
                "capture": n.spec.capture,
                "last_heartbeat": last.get(&n.spec.id).map(|t| crate::util::rfc3339(*t)),
                "workloads": g.assignments.values().filter(|a| a.node == n.spec.id).count(),
            })
        })
        .collect();
    let workloads: Vec<_> = g
        .workloads
        .values()
        .map(|w| {
            let a = g.assignments.get(&w.id);
            json!({
                "id": w.id,
                "kind": w.kind,
                "priority": w.priority,
                "pinned": w.is_pinned(),
                "state": match a { Some(a) if a.level > 0 => "degraded", Some(_) => "running", None if g.unschedulable.contains_key(&w.id) => "unschedulable", None => "pending" },
                "node": a.map(|a| a.node.clone()),
                "device": a.and_then(|a| a.device.clone()),
                "level": a.map(|a| a.level),
                "level_label": a.map(|a| w.level_label(a.level).to_string()),
                "reason": a.map(|a| json!(a.reason)),
                "unschedulable": g.unschedulable.get(&w.id),
            })
        })
        .collect();
    json!({ "scheduler": is_leader(app).then_some(&app.node_id), "nodes": nodes, "workloads": workloads })
}
