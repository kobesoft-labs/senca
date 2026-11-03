//! グリッドの統合（ADR-0013、senca-grid）。
//!
//! - 各ノード: 起動時に自分の資源（CPU・メモリ・帯域・GPU・キャプチャ機器・ラベル）を `RegisterNode` で申告し、
//!   毎秒の心拍をリーダーへ送る（心拍は合意ログに載せない。クラスタの接続の枠 K_HEARTBEAT）。
//! - リーダー: 心拍から停止を見つけ（`NodeDown`・`DeviceDown`）、毎秒 `plan()` で配置を決めて提案する。
//!   リーダーが替わったら検出器を作り直す（最初の 1 周期は猶予）。
//! - 各ノード: 適用された割り当てのうち自分の分だけを動かす（入力・出力・パイプライン。`io.rs`・`pipelines.rs`）。
//!   割り当てが別のノードへ移ったら止める（token が変わる）。
//! - fencing: 仕事は [`guarded`] の中で動き、外への副作用（pull の送り手としてつなぐ・push の接続・パイプラインの公開）の
//!   前と、動いている間 0.5 秒ごとに [`App::work_current`] で「今も自分の割り当てか・過半数と連絡が取れているか」を確かめる。
//!   過半数との連絡が [`FENCE_AFTER_MS`] を超えて途絶えたノードは、自分のグリッドの仕事をすべて止める（自分で fencing する）。
//!   多数側が心拍の途絶（[`DOWN_AFTER_MS`]）で仕事を移すより先に止まるので、同じ仕事が 2 か所で動かない。
//! - 心拍の使用量は /proc から測る（CPU は /proc/stat の差、メモリは /proc/meminfo、帯域は /proc/net/dev の差）。
//!   装置（GPU・キャプチャ機器）は [`PROBE_EVERY`] ごとに調べ直し、見えなくなった装置を不健康として心拍に載せる
//!   （リーダーが `DeviceDown`・戻れば `DeviceUp`）。手で止めた印（`disabled`）とは別。
//!
//! 単一ノードでも同じ仕組みで動く（自分がリーダー）。

use crate::app::App;
use senca_control::state::Command;
use senca_grid::{
    CaptureDevice, DeviceHealth, FailureDetector, GridCommand, Gpu, Heartbeat, NodeSpec, PlanConfig, Resources, Role, Running,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 心拍の間隔と、途絶えたと見なすまでの時間（ADR-0013）。
const HEARTBEAT: Duration = Duration::from_secs(1);
pub const DOWN_AFTER_MS: i64 = 5000;
/// 過半数との連絡がこれを超えて途絶えたら、自分のグリッドの仕事を止める（自分での fencing）。
/// 多数側が仕事を移す（心拍の途絶 `DOWN_AFTER_MS` の後）より前に止まるよう、それより短くする。
/// リーダーの選挙（1〜2 秒）では止まらないよう、それより長くする。
pub const FENCE_AFTER_MS: u64 = 4000;
/// 申告の見直しの間隔（装置の増減・ラベルの変更）。
const RESPEC_EVERY: u32 = 30;
/// 装置を調べ直す間隔（codec worker の Hello・キャプチャ機器の一覧）。
const PROBE_EVERY: Duration = Duration::from_secs(10);
/// 仕事が自分のものかを確かめる間隔（動いている間）。
const GUARD_EVERY: Duration = Duration::from_millis(500);

tokio::task_local! {
    /// 今動いているグリッドの仕事（仕事の ID と token）。外への副作用の前に確かめる（`io::attach` など）。
    pub static WORK: (String, u64);
}

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
    /// 過半数との連絡が途絶えて、グリッドの仕事を止めている。
    fenced: AtomicBool,
    /// 所有者の記録が無い stream の所有権を取り直している（重ねて出さない）。
    claiming: Mutex<HashSet<String>>,
    /// 最後に調べた装置。
    probe: Mutex<Option<Probe>>,
    /// 使用量を測るための前回の値。
    usage: Mutex<UsageSampler>,
}

/// 調べた装置（見えている装置だけ）。
#[derive(Debug, Clone, Default)]
struct Probe {
    gpus: Vec<Gpu>,
    capture: Vec<CaptureDevice>,
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

/// /proc の小さなファイルを読む（大きさに上限）。
fn read_proc(path: &str) -> Option<String> {
    use std::io::Read;
    let mut s = String::new();
    std::fs::File::open(path).ok()?.take(1 << 20).read_to_string(&mut s).ok()?;
    Some(s)
}

/// `/proc/meminfo` の (MemTotal, MemAvailable)（kB）。MemAvailable が無ければ MemFree + Buffers + Cached。
pub fn parse_meminfo(s: &str) -> Option<(u64, u64)> {
    let get = |k: &str| s.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok());
    let total = get("MemTotal:")?;
    let avail = get("MemAvailable:").or_else(|| Some(get("MemFree:")? + get("Buffers:").unwrap_or(0) + get("Cached:").unwrap_or(0)))?;
    Some((total, avail.min(total)))
}

/// `/proc/stat` の全 CPU の (忙しい時間, 全体の時間)（単位は jiffies）。idle と iowait を除いた分を忙しいとする。
pub fn parse_proc_stat(s: &str) -> Option<(u64, u64)> {
    let l = s.lines().find(|l| l.starts_with("cpu "))?;
    // user nice system idle iowait irq softirq steal（guest は user に含まれるので足さない）
    let v: Vec<u64> = l.split_whitespace().skip(1).take(8).map(|x| x.parse().ok()).collect::<Option<_>>()?;
    if v.len() < 4 {
        return None;
    }
    let total: u64 = v.iter().fold(0u64, |a, x| a.saturating_add(*x));
    let idle = v[3].saturating_add(v.get(4).copied().unwrap_or(0));
    Some((total.saturating_sub(idle), total))
}

/// `/proc/net/dev` の受信・送信のバイト数の和（ループバックを除く）。
pub fn parse_net_dev(s: &str) -> Option<(u64, u64)> {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let mut any = false;
    for l in s.lines().skip(2) {
        let Some((name, rest)) = l.split_once(':') else { continue };
        if name.trim() == "lo" {
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 9 {
            continue;
        }
        rx = rx.saturating_add(f[0].parse().unwrap_or(0));
        tx = tx.saturating_add(f[8].parse().unwrap_or(0));
        any = true;
    }
    any.then_some((rx, tx))
}

/// 使用量を測る（前回との差）。最初の 1 回は CPU・帯域が 0。
#[derive(Debug, Default)]
pub struct UsageSampler {
    cpu: Option<(u64, u64)>,
    net: Option<(u64, u64, u64)>,
}

impl UsageSampler {
    pub fn sample(&mut self, cpus: u64, stat: Option<&str>, meminfo: Option<&str>, net_dev: Option<&str>, now_ms: u64) -> Resources {
        let mut r = Resources::default();
        if let Some((busy, total)) = stat.and_then(parse_proc_stat) {
            if let Some((pb, pt)) = self.cpu
                && total > pt
            {
                r.cpu_millis = busy.saturating_sub(pb).saturating_mul(cpus.saturating_mul(1000)) / (total - pt);
            }
            self.cpu = Some((busy, total));
        }
        if let Some((total, avail)) = meminfo.and_then(parse_meminfo) {
            r.memory_mb = (total - avail) / 1024;
        }
        if let Some((rx, tx)) = net_dev.and_then(parse_net_dev) {
            if let Some((prx, ptx, pt)) = self.net
                && now_ms > pt
            {
                // バイト → ビット、ミリ秒 → 秒、ビット → Mbit
                let ms = now_ms - pt;
                r.net_in_mbps = rx.saturating_sub(prx).saturating_mul(8) / ms / 1000;
                r.net_out_mbps = tx.saturating_sub(ptx).saturating_mul(8) / ms / 1000;
            }
            self.net = Some((rx, tx, now_ms));
        }
        r
    }
}

fn meminfo_mb() -> u64 {
    read_proc("/proc/meminfo").and_then(|s| parse_meminfo(&s)).map(|(t, _)| t / 1024).unwrap_or(4096)
}

fn cpu_count() -> u64 {
    std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1)
}

/// 装置を調べる（codec worker を 1 つ起動して Hello を受け、キャプチャの worker に一覧を聞く。ブロックする）。
fn probe_devices(app: &App) -> Probe {
    // GPU: ハードウェアの符号化・復号の能力を装置ごとにまとめる
    let mut gpus: BTreeMap<String, Gpu> = BTreeMap::new();
    if let Some(c) = &app.codec {
        // 新しい worker の Hello（装置の抜け・ドライバーの異常を見つける）。起動できなければ前の能力
        let caps = c.probe().map(|i| i.capabilities).unwrap_or_else(|| c.capability_list());
        for cap in caps.into_iter().filter(|c| c.hardware) {
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
    Probe { gpus: gpus.into_values().collect(), capture }
}

/// 申告に、登録済みの申告の印を写す: 手で止めた印（`disabled`）と測った健康（`healthy`。健康の変化は心拍で伝える）。
/// 登録済みで今は見えない装置は申告に残す（心拍で不健康と伝える。消すと仕事の置き直しが遅れる）。
fn merge_registered(spec: &mut NodeSpec, registered: Option<&NodeSpec>) {
    let Some(r) = registered else { return };
    for g in &mut spec.gpus {
        if let Some(o) = r.gpus.iter().find(|o| o.id == g.id) {
            g.disabled = o.disabled;
            g.healthy = o.healthy;
        }
    }
    for o in &r.gpus {
        if !spec.gpus.iter().any(|g| g.id == o.id) {
            spec.gpus.push(o.clone());
        }
    }
    for c in &mut spec.capture {
        if let Some(o) = r.capture.iter().find(|o| o.id == c.id) {
            c.disabled = o.disabled;
            c.healthy = o.healthy;
        }
    }
    for o in &r.capture {
        if !spec.capture.iter().any(|c| c.id == o.id) {
            spec.capture.push(o.clone());
        }
    }
    spec.gpus.sort_by(|a, b| a.id.cmp(&b.id));
}

/// このノードの申告を作る（ハードウェアを調べる。ブロックしてよいスレッドから呼ぶ）。
pub fn local_spec(app: &App) -> NodeSpec {
    let cpus = cpu_count();
    // OS・制御面の取り分を残す
    let capacity = Resources {
        cpu_millis: app.cfg.capacity_cpu_millis.unwrap_or(cpus * 1000 * 9 / 10),
        memory_mb: app.cfg.capacity_memory_mb.unwrap_or(meminfo_mb() * 85 / 100),
        net_in_mbps: app.cfg.net_mbps,
        net_out_mbps: app.cfg.net_mbps,
    };
    let probe = probe_devices(app);
    *app.grid.probe.lock().unwrap() = Some(probe.clone());
    let Probe { gpus, capture } = probe;
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
    NodeSpec { id: app.node_id.clone(), roles, capacity, gpus, capture, labels, draining }
}

/// 心拍に載せる装置の健康: 最後に調べたときに見えていた装置は健康、見えなかった装置は不健康。
/// まだ調べていなければ載せない（変えない）。
fn device_health(spec: &NodeSpec, probe: Option<&Probe>) -> Vec<DeviceHealth> {
    let Some(p) = probe else { return Vec::new() };
    spec.gpus
        .iter()
        .map(|g| DeviceHealth { id: g.id.clone(), healthy: p.gpus.iter().any(|x| x.id == g.id) })
        .chain(spec.capture.iter().map(|c| DeviceHealth { id: c.id.clone(), healthy: p.capture.iter().any(|x| x.id == c.id) }))
        .collect()
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
    let devices = device_health(spec, app.grid.probe.lock().unwrap().as_ref());
    let usage = app.grid.usage.lock().unwrap().sample(
        cpu_count(),
        read_proc("/proc/stat").as_deref(),
        read_proc("/proc/meminfo").as_deref(),
        read_proc("/proc/net/dev").as_deref(),
        crate::util::mono_ms(),
    );
    Heartbeat { node: app.node_id.clone(), usage, devices, streams, running: running(app) }
}

/// グリッドの仕事を動かす: `fut` を仕事の印（[`WORK`]）の中で動かし、0.5 秒ごとに今も自分の仕事かを確かめる。
/// 違えば（別のノードへ移った・token が古い・過半数との連絡が途絶えた）`fut` を止める（drop で接続も切れる）。
pub async fn guarded<F: Future<Output = ()>>(app: Arc<App>, workload: String, token: u64, fut: F) {
    let check = async {
        loop {
            tokio::time::sleep(GUARD_EVERY).await;
            if !app.work_current(&workload, token) {
                return;
            }
        }
    };
    tokio::select! {
        _ = WORK.scope((workload.clone(), token), fut) => {}
        _ = check => {
            app.log(&format!("grid: stopping {workload} (token {token}): no longer the current assignment here{}", if app.fenced() { " (fenced: no quorum)" } else { "" }));
        }
    }
}

/// 今動いている仕事の印が今も有効か（印の外では true）。
pub fn current_work_ok(app: &App) -> Result<(), String> {
    match WORK.try_with(|w| w.clone()) {
        Ok((w, t)) if !app.work_current(&w, t) => Err(format!("{w} はこのノードの今の割り当てではありません（fencing）")),
        _ => Ok(()),
    }
}

/// 過半数との連絡を見張る: 途絶えたら自分のグリッドの仕事を止め、戻れば動かし直す。
/// 所有者の記録が無いのに手元で受けている stream は、所有権を取り直す（分断の間に停止と見なされたなど）。
fn watch_fence(app: &Arc<App>) {
    if app.cluster.get().is_none() {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut n = 0u32;
        loop {
            tick.tick().await;
            n = n.wrapping_add(1);
            let fenced = app.fenced();
            if app.grid.fenced.swap(fenced, Ordering::Relaxed) != fenced {
                let age = app.cluster.get().map(|c| c.contact_age_ms()).unwrap_or(0);
                if fenced {
                    if age != u64::MAX {
                        app.log(&format!("grid: fenced: no contact with a quorum for {age} ms; stopping grid work on this node"));
                    }
                } else {
                    app.log("grid: quorum contact restored; resuming assigned work");
                }
                app.io.reconcile(&app);
                app.pipelines.reconcile(&app);
            }
            if !fenced && n.is_multiple_of(4) {
                reclaim_streams(&app);
            }
        }
    });
}

/// 手元で受けている（写しでない）stream のうち、所有者の記録が無いものの所有権を取り直す。
fn reclaim_streams(app: &Arc<App>) {
    let lost: Vec<String> = {
        let relay = app.relay.get();
        let hubs: Vec<String> =
            app.hubs.lock().unwrap().iter().filter(|(id, h)| h.is_live() && !relay.is_some_and(|r| r.is_mirror(id))).map(|(id, _)| id.clone()).collect();
        let core = app.control.lock().unwrap();
        hubs.into_iter().filter(|id| core.state.streams.contains_key(id) && !core.state.owners.contains_key(id)).collect()
    };
    for sid in lost {
        if !app.grid.claiming.lock().unwrap().insert(sid.clone()) {
            continue;
        }
        let me = app.clone();
        tokio::task::spawn_blocking(move || {
            match me.propose(Command::ClaimStream { stream: sid.clone(), node: me.node_id.clone(), now_ms: crate::util::now_ms() }) {
                Ok(()) => me.log(&format!("cluster: re-claimed stream {sid} (no owner was recorded)")),
                Err(e) => me.log(&format!("cluster: re-claim stream {sid} failed: {}", e.title)),
            }
            me.grid.claiming.lock().unwrap().remove(&sid);
        });
    }
}

/// 装置を周期的に調べ直す（心拍の装置の健康に使う）。
fn watch_devices(app: &Arc<App>) {
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PROBE_EVERY).await;
            let a = app.clone();
            let Ok(p) = tokio::task::spawn_blocking(move || probe_devices(&a)).await else { continue };
            let prev = app.grid.probe.lock().unwrap().replace(p.clone());
            if let Some(prev) = prev {
                let ids = |p: &Probe| -> BTreeSet<String> { p.gpus.iter().map(|g| g.id.clone()).chain(p.capture.iter().map(|c| c.id.clone())).collect() };
                let (a, b) = (ids(&prev), ids(&p));
                for gone in a.difference(&b) {
                    app.log(&format!("grid: device {gone} is no longer visible on this node"));
                }
                for back in b.difference(&a) {
                    app.log(&format!("grid: device {back} is visible on this node"));
                }
            }
        }
    });
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
    watch_fence(app);
    watch_devices(app);
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
                    // 手で止めた印と測った健康は制御状態のもの（申告と比べるときに写す。申告し直しでも消えない）
                    merge_registered(&mut spec, registered.as_ref());
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
    let (last, usage): (BTreeMap<String, i64>, BTreeMap<String, Resources>) = app
        .grid
        .detector
        .lock()
        .unwrap()
        .as_ref()
        .map(|d| {
            let seen: Vec<(String, i64, Resources)> = g.nodes.keys().filter_map(|n| d.last(n).map(|(t, hb)| (n.clone(), t, hb.usage))).collect();
            (seen.iter().map(|(n, t, _)| (n.clone(), *t)).collect(), seen.into_iter().map(|(n, _, u)| (n, u)).collect())
        })
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
                "usage": usage.get(&n.spec.id),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_parsers() {
        let stat = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(parse_proc_stat(stat), Some((150, 1000)));
        let mem = "MemTotal:       16000000 kB\nMemFree:  1000 kB\nMemAvailable:   12000000 kB\n";
        assert_eq!(parse_meminfo(mem), Some((16_000_000, 12_000_000)));
        let mem_old = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 250 kB\n";
        assert_eq!(parse_meminfo(mem_old), Some((1000, 400)));
        let net = "Inter-|   Receive  |  Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets\n    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n  eth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n  eth1: 5 1 0 0 0 0 0 0 7 1 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(net), Some((1005, 2007)));
        assert_eq!(parse_proc_stat("garbage"), None);
        assert_eq!(parse_net_dev("a\nb\n"), None);
    }

    #[test]
    fn usage_from_deltas() {
        let mut s = UsageSampler::default();
        let mem = "MemTotal: 2048000 kB\nMemAvailable: 1024000 kB\n";
        let net1 = "h\nh\n eth0: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n";
        let r = s.sample(4, Some("cpu 0 0 0 1000 0 0 0 0\n"), Some(mem), Some(net1), 1000);
        assert_eq!((r.cpu_millis, r.memory_mb, r.net_in_mbps), (0, 1000, 0));
        // 1 秒で 125 MB 受信 = 1000 Mbit/s、CPU は 4 コアの半分 = 2000 ミリコア
        let net2 = "h\nh\n eth0: 125000000 0 0 0 0 0 0 0 12500000 0 0 0 0 0 0 0\n";
        let r = s.sample(4, Some("cpu 500 0 0 1500 0 0 0 0\n"), Some(mem), Some(net2), 2000);
        assert_eq!((r.cpu_millis, r.net_in_mbps, r.net_out_mbps), (2000, 1000, 100));
    }

    fn gpu(id: &str) -> Gpu {
        Gpu { id: id.into(), vendor: "nvidia".into(), encode_sessions: 1, decode_sessions: 1, memory_mb: 0, healthy: true, disabled: false }
    }

    fn node(gpus: Vec<Gpu>) -> NodeSpec {
        NodeSpec {
            id: "n".into(),
            roles: Default::default(),
            capacity: Resources::default(),
            gpus,
            capture: Vec::new(),
            labels: Default::default(),
            draining: false,
        }
    }

    #[test]
    fn devices_missing_from_the_probe_are_unhealthy_and_kept_in_the_spec() {
        let mut registered = node(Vec::new());
        let mut g0 = gpu("nvidia:0");
        g0.disabled = true;
        registered.gpus = vec![g0, gpu("nvidia:1")];
        registered.capture = vec![CaptureDevice { id: "v4l2:/dev/video0".into(), kind: "v4l2".into(), healthy: false, disabled: false }];
        // 今は nvidia:0 と新しい nvidia:2、キャプチャ機器が見える
        let mut spec = node(vec![gpu("nvidia:0"), gpu("nvidia:2")]);
        spec.capture = vec![CaptureDevice { id: "v4l2:/dev/video0".into(), kind: "v4l2".into(), healthy: true, disabled: false }];
        let probe = Probe { gpus: spec.gpus.clone(), capture: spec.capture.clone() };
        merge_registered(&mut spec, Some(&registered));
        let ids: Vec<&str> = spec.gpus.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["nvidia:0", "nvidia:1", "nvidia:2"]);
        assert!(spec.gpus[0].disabled, "手で止めた印は残る");
        assert!(!spec.capture[0].healthy, "測った健康は登録済みの値（変化は心拍で伝える）");
        let h = device_health(&spec, Some(&probe));
        let get = |id: &str| h.iter().find(|d| d.id == id).map(|d| d.healthy);
        assert_eq!(get("nvidia:0"), Some(true));
        assert_eq!(get("nvidia:1"), Some(false), "見えなくなった GPU は不健康");
        assert_eq!(get("nvidia:2"), Some(true));
        assert_eq!(get("v4l2:/dev/video0"), Some(true), "見えるようになった機器は健康（DeviceUp）");
        assert!(device_health(&spec, None).is_empty(), "調べる前は載せない");
    }
}
