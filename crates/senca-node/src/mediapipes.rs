//! 部屋のメディアパイプライン（ADR-0016、AWS Chime SDK Media Pipelines 相当）の係。
//!
//! 部屋を置いたノードで動く（部屋が移れば、移った先のノードが引き継ぐ）。毎秒、部屋の参加者を見て:
//! - 参加者ごとに SENCA の stream を用意し、送ってきた映像（いちばん良い層）・音声をそのまま流す（再符号化しない）。
//!   stream の録画・字幕は普通の stream と同じ仕組み（自動録画・captions）で動く。
//! - 合成があれば、映像のある参加者を並べるパイプライン（senca-pipeline の合成）を作り、参加者が変われば図を直す。
//! - 送り出しは合成の stream の出力（RTMP・SRT・PaaS）。
//!
//! 子の資源（stream・パイプライン・出力）の ID はメディアパイプラインの ID から決まる（何度作っても同じ）。

use crate::app::App;
use crate::rooms::{BridgeItem, Location, RoomCmd, locate};
use senca_control::model::{CodecProfile, MediaPipeline, Output, Pipeline, PlacementSpec, RecordingPolicy, Stream};
use senca_control::state::Command;
use senca_core::{MediaKind, TrackId};
use senca_rooms::Kind;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const TICK: Duration = Duration::from_secs(1);

fn derive_id(parts: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"senca-media-pipeline/1\0");
    h.update(parts.as_bytes());
    h.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn participant_stream_id(mp: &str, key: &str) -> String {
    derive_id(&format!("{mp}/participant/{key}"))
}
pub fn composite_stream_id(mp: &str) -> String {
    derive_id(&format!("{mp}/composite"))
}
pub fn composite_pipeline_id(mp: &str) -> String {
    derive_id(&format!("{mp}/compose"))
}
pub fn egress_output_id(mp: &str, i: usize) -> String {
    derive_id(&format!("{mp}/egress/{i}"))
}

/// stream の名前（英数字と -_. で 64 文字まで）。
fn safe_name(base: &str, suffix: &str, id: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect::<String>();
    let mut n = format!("{}-{}", clean(base), clean(suffix));
    n = n.trim_start_matches('.').to_string();
    if n.len() > 56 {
        n.truncate(56);
    }
    // 名前はテナントの中で一意。ID の頭を付けて衝突を避ける
    format!("{n}-{}", &id[..6])
}

struct Part {
    stream: String,
    /// (track_id, rid) と書き出しの番号
    tracks: Vec<(String, Option<String>, u64)>,
    task: tokio::task::JoinHandle<()>,
    has_video: bool,
}

#[derive(Default)]
struct Run {
    room: String,
    /// 動かしているメディアパイプラインの世代（変わったら合成・送り出しを作り直す）。
    generation: u64,
    parts: BTreeMap<String, Part>,
    graph: Option<Value>,
}

#[derive(Default)]
pub struct MediaPipelines {
    runs: Mutex<HashMap<String, Run>>,
    next_bridge: AtomicU64,
}

impl MediaPipelines {
    /// 動いている様子（API の `status`）。
    pub fn status(&self, id: &str) -> Option<Value> {
        let g = self.runs.lock().unwrap();
        let r = g.get(id)?;
        Some(json!({
            "participants": r.parts.iter().map(|(k, p)| json!({ "participant": k, "stream": p.stream, "video": p.has_video })).collect::<Vec<_>>(),
            "composite_members": r.graph.as_ref().map(|g| g["nodes"].as_array().map(|a| a.iter().filter(|n| n["type"] == "source").count()).unwrap_or(0)),
        }))
    }
}

pub fn start(app: &Arc<App>) {
    let app = app.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(TICK);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            tick(&app).await;
        }
    });
}

async fn propose(app: &Arc<App>, cmd: Command) -> Result<(), senca_control::Problem> {
    let a = app.clone();
    tokio::task::spawn_blocking(move || a.propose(cmd)).await.unwrap_or_else(|_| Err(senca_control::Problem::internal()))
}

/// 部屋の参加者（このノードの部屋の核から）。
async fn participants(app: &Arc<App>, room: &str) -> Option<Vec<senca_rooms::signaling::ParticipantInfo>> {
    let rtc = app.rtc.get()?.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let rid = room.to_string();
    rtc.room_cmd(RoomCmd::Call(Box::new(move |svc, _| {
        let r = svc.api_list_participants(&rid, 1000, None).map(|p| p.items.into_iter().map(|v| v.info).collect::<Vec<_>>());
        let _ = tx.send(r.ok());
    })));
    tokio::time::timeout(Duration::from_secs(3), rx).await.ok()?.ok()?
}

async fn tick(app: &Arc<App>) {
    let Some(rtc) = app.rtc.get().cloned() else { return };
    // このノードに置かれた部屋の、動かすメディアパイプライン
    let (mps, rooms): (Vec<MediaPipeline>, BTreeMap<String, senca_control::model::Room>) = {
        let core = app.control.lock().unwrap();
        (
            core.state.media_pipelines.values().filter(|m| m.desired_state == "running").cloned().collect(),
            core.state.rooms.clone(),
        )
    };
    let mps: Vec<MediaPipeline> = mps.into_iter().filter(|m| matches!(locate(app, &m.room), Location::Here)).collect();
    // もう動かさないもの: 書き出しを止める（子の資源は API の側で片付ける）
    let stale: Vec<(String, Run)> = {
        let mut g = app.mediapipes.runs.lock().unwrap();
        let keep: Vec<String> = mps.iter().map(|m| m.id.clone()).collect();
        let ids: Vec<String> = g.keys().filter(|k| !keep.contains(k)).cloned().collect();
        ids.into_iter().filter_map(|k| g.remove(&k).map(|r| (k, r))).collect()
    };
    for (id, run) in stale {
        for (_, p) in run.parts {
            stop_part(&rtc, &run.room, p);
        }
        // 合成をここで動かしていたら止める（部屋が移った先で、そのノードの係が作り直す）
        let pid = composite_pipeline_id(&id);
        let cur = app.control.lock().unwrap().state.pipelines.get(&pid).cloned();
        if let Some(mut p) = cur.filter(|p| p.desired_state == "running" && p.placement.as_ref().and_then(|x| x.node.as_deref()) == Some(app.node_id.as_str())) {
            p.desired_state = "stopped".into();
            p.updated_at_ms = crate::util::now_ms();
            let _ = propose(app, Command::PutPipeline { pipeline: p, expect_generation: None }).await;
        }
        app.log(&format!("media pipeline {id}: stopped on this node"));
    }
    for mp in mps {
        let Some(room) = rooms.get(&mp.room) else { continue };
        let Some(people) = participants(app, &mp.room).await else { continue };
        if let Err(e) = step(app, &rtc, &mp, room, people).await {
            app.log(&format!("media pipeline {}: {e}", mp.id));
        }
    }
}

fn stop_part(rtc: &crate::webrtc::Rtc, room: &str, p: Part) {
    for (t, _, id) in &p.tracks {
        rtc.room_cmd(RoomCmd::Unbridge { room: room.to_string(), track_id: t.clone(), id: *id });
    }
    // 送り手の task は受け口が閉じれば detach して終わる
    p.task.abort();
}

fn room_codec(room: &senca_control::model::Room) -> String {
    room.settings.get("video_codec").and_then(|v| v.as_str()).unwrap_or("vp8").to_string()
}

#[allow(clippy::too_many_arguments)]
async fn ensure_stream(
    app: &Arc<App>,
    mp: &MediaPipeline,
    id: &str,
    name: &str,
    participant: Option<&str>,
    recording: bool,
    captions: bool,
    royalty_free: bool,
) -> Result<(), String> {
    let want_rec = RecordingPolicy { enabled: recording, retention_days: mp.recording.as_ref().and_then(|r| r.retention_days) };
    let want_cap: Option<senca_control::model::CaptionSettings> = if captions { mp.captions.clone().and_then(|c| serde_json::from_value(c).ok()) } else { None };
    let existing = app.control.lock().unwrap().state.streams.get(id).cloned();
    if let Some(mut cur) = existing {
        // 設定を変えたメディアパイプライン: 録画・字幕の方針を書き出し先の stream に写す
        if cur.recording != want_rec || cur.captions != want_cap {
            let g = cur.generation;
            cur.recording = want_rec;
            cur.captions = want_cap;
            cur.updated_at_ms = crate::util::now_ms();
            propose(app, Command::UpdateStream { stream: cur, expect_generation: g }).await.map_err(|e| format!("stream {name}: {}", e.title))?;
        }
        return Ok(());
    }
    let mut key = [0u8; 24];
    getrandom::fill(&mut key).map_err(|e| e.to_string())?;
    let now = crate::util::now_ms();
    let s = Stream {
        id: id.to_string(),
        tenant: mp.tenant.clone(),
        name: name.to_string(),
        generation: 1,
        created_at_ms: now,
        updated_at_ms: now,
        // publish 鍵では受けない（部屋の書き出しだけが送り手）。使えない鍵のハッシュを入れておく
        key_hash: senca_control::secret::hash(&key.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        latency_profile: Default::default(),
        codec_profile: if royalty_free { CodecProfile::RoyaltyFree } else { CodecProfile::Any },
        recording: want_rec,
        snapshots: None,
        hls: senca_control::model::HlsSettings { container: "fmp4".into(), ..Default::default() },
        limits: Default::default(),
        metadata: [("media_pipeline".to_string(), mp.id.clone()), ("room".to_string(), mp.room.clone())]
            .into_iter()
            .chain(participant.map(|p| ("participant".to_string(), p.to_string())))
            .collect(),
        desired_state: "running".into(),
        captions: want_cap,
    };
    match propose(app, Command::CreateStream { stream: s }).await {
        Ok(()) => Ok(()),
        Err(e) if e.status == 409 => Ok(()),
        Err(e) => Err(format!("stream {name}: {}", e.title)),
    }
}

/// 参加者の映像・音声を stream の送り手として流す。
async fn bridge_task(app: Arc<App>, stream: String, mp: String, mut rx: mpsc::Receiver<BridgeItem>) {
    let Some(s) = app.control.lock().unwrap().state.streams.get(&stream).cloned() else { return };
    let p = match app.attach_stream(&s, "room", &format!("media-pipeline:{mp}")).await {
        Ok(p) => p,
        Err(e) => {
            app.log(&format!("media pipeline {mp}: stream {stream} refused the publisher: {e:?}"));
            return;
        }
    };
    let mut norm = crate::webrtc::ToHub::default();
    while let Some(it) = rx.recv().await {
        let mut au = it.au;
        au.track = if au.codec.kind() == MediaKind::Video { TrackId(0) } else { TrackId(1) };
        let track = au.track;
        let (cfg, au) = norm.process(au, it.config);
        if let Some(c) = cfg
            && p.config(track, c).await.is_err()
        {
            break;
        }
        if p.access_unit(au).await.is_err() {
            break;
        }
    }
    p.detach("participant left").await;
}

async fn step(
    app: &Arc<App>,
    rtc: &crate::webrtc::Rtc,
    mp: &MediaPipeline,
    room: &senca_control::model::Room,
    people: Vec<senca_rooms::signaling::ParticipantInfo>,
) -> Result<(), String> {
    let codec = room_codec(room);
    let rf_room = codec != "h264" && codec != "h265";
    // 書き出す参加者: client_id（無ければ connection_id）で決める。別のノードの参加者は除く
    let mut want: BTreeMap<String, Vec<(String, Option<String>, Kind)>> = BTreeMap::new();
    for p in people.into_iter().filter(|p| p.node.is_none()) {
        if !mp.participants.is_empty() && !p.client_id.as_ref().is_some_and(|c| mp.participants.contains(c)) {
            continue;
        }
        let key = p.client_id.clone().unwrap_or_else(|| p.connection_id.clone());
        let mut tracks = Vec::new();
        if let Some(v) = p.tracks.iter().find(|t| t.kind == Kind::Video) {
            tracks.push((v.track_id.clone(), v.rids.last().cloned(), Kind::Video));
        }
        if let Some(a) = p.tracks.iter().find(|t| t.kind == Kind::Audio) {
            tracks.push((a.track_id.clone(), None, Kind::Audio));
        }
        if !tracks.is_empty() {
            want.insert(key, tracks);
        }
    }
    // stream を用意する（await を挟むのでロックの外で）
    let rec_parts = mp.recording.as_ref().is_some_and(|r| r.participants) && mp.export_participants;
    for key in want.keys() {
        let sid = participant_stream_id(&mp.id, key);
        ensure_stream(app, mp, &sid, &safe_name(&mp.name, key, &sid), Some(key), rec_parts, mp.captions.is_some(), rf_room).await?;
    }
    // 書き出しを突き合わせる
    {
        let mut g = app.mediapipes.runs.lock().unwrap();
        let run = g.entry(mp.id.clone()).or_insert_with(|| Run { room: mp.room.clone(), generation: mp.generation, ..Default::default() });
        if run.generation != mp.generation {
            // 設定が変わった（API が合成・送り出しを片付けたかもしれない）: 図を作り直す
            run.generation = mp.generation;
            run.graph = None;
        }
        let gone: Vec<String> = run
            .parts
            .iter()
            .filter(|(k, p)| {
                let now: Vec<(String, Option<String>)> = want.get(*k).map(|v| v.iter().map(|(t, r, _)| (t.clone(), r.clone())).collect()).unwrap_or_default();
                let had: Vec<(String, Option<String>)> = p.tracks.iter().map(|(t, r, _)| (t.clone(), r.clone())).collect();
                now != had || p.task.is_finished()
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            if let Some(p) = run.parts.remove(&k) {
                stop_part(rtc, &mp.room, p);
            }
        }
        for (key, tracks) in &want {
            if run.parts.contains_key(key) {
                continue;
            }
            let sid = participant_stream_id(&mp.id, key);
            let (tx, rx) = mpsc::channel(2048);
            let mut ts = Vec::new();
            for (t, rid, _) in tracks {
                let id = app.mediapipes.next_bridge.fetch_add(1, Ordering::Relaxed);
                rtc.room_cmd(RoomCmd::Bridge { room: mp.room.clone(), track_id: t.clone(), rid: rid.clone(), id, tx: tx.clone() });
                ts.push((t.clone(), rid.clone(), id));
            }
            drop(tx);
            let task = tokio::spawn(bridge_task(app.clone(), sid.clone(), mp.id.clone(), rx));
            let has_video = tracks.iter().any(|(_, _, k)| *k == Kind::Video);
            app.log(&format!("media pipeline {}: exporting participant {key} to stream {sid}", mp.id));
            run.parts.insert(key.clone(), Part { stream: sid, tracks: ts, task, has_video });
        }
    }
    // 合成
    let Some(c) = &mp.composite else { return Ok(()) };
    let csid = composite_stream_id(&mp.id);
    let rf_out = !matches!(c.video_codec.as_str(), "h264" | "h265");
    let rec_comp = mp.recording.as_ref().is_some_and(|r| r.composite);
    ensure_stream(app, mp, &csid, &safe_name(&mp.name, "composite", &csid), None, rec_comp, false, rf_out && rf_room).await?;
    let members: Vec<(String, String, bool)> = {
        let g = app.mediapipes.runs.lock().unwrap();
        g.get(&mp.id).map(|r| r.parts.iter().map(|(k, p)| (k.clone(), p.stream.clone(), p.has_video)).collect()).unwrap_or_default()
    };
    // 参加者の stream が流れ始めてから図に入れる（入力を待つ間に合成が止まらないよう）
    let live: Vec<(String, String, bool)> = members.into_iter().filter(|(_, s, _)| app.hubs.lock().unwrap().get(s).is_some_and(|h| h.is_live())).collect();
    let graph = composite_graph(c, &codec, &csid, &live);
    let changed = {
        let g = app.mediapipes.runs.lock().unwrap();
        g.get(&mp.id).is_none_or(|r| r.graph.as_ref() != graph.as_ref())
    };
    if changed {
        let pid = composite_pipeline_id(&mp.id);
        let now = crate::util::now_ms();
        let (desired, g) = match &graph {
            Some(g) => ("running", g.clone()),
            None => ("stopped", json!({ "version": 1, "nodes": [] })),
        };
        if graph.is_some() {
            senca_pipeline::Graph::from_json(&g.to_string()).map_err(|e| format!("composite graph: {e:?}"))?;
        }
        let cur = app.control.lock().unwrap().state.pipelines.get(&pid).cloned();
        let p = Pipeline {
            id: pid.clone(),
            tenant: mp.tenant.clone(),
            name: safe_name(&mp.name, "compose", &pid),
            generation: cur.as_ref().map(|c| c.generation).unwrap_or(1),
            graph: if graph.is_some() { g } else { cur.as_ref().map(|c| c.graph.clone()).unwrap_or(g) },
            desired_state: desired.into(),
            created_at_ms: cur.as_ref().map(|c| c.created_at_ms).unwrap_or(now),
            updated_at_ms: now,
            // 部屋と同じノードで（参加者の stream がここにある。部屋が移れば移った先で置き直す）。
            // 優先度は部屋と同じ（自分の部屋を押しのけない）
            placement: Some(PlacementSpec {
                node: Some(app.node_id.clone()),
                priority: room.placement.as_ref().map(|p| p.priority).unwrap_or_default(),
                ..Default::default()
            }),
        };
        propose(app, Command::PutPipeline { pipeline: p, expect_generation: None }).await.map_err(|e| format!("composite pipeline: {}", e.title))?;
        app.log(&format!("media pipeline {}: composite with {} participants", mp.id, live.iter().filter(|m| m.2).count()));
        if let Some(r) = app.mediapipes.runs.lock().unwrap().get_mut(&mp.id) {
            r.graph = graph;
        }
    }
    // 送り出し（合成の stream の出力）
    for (i, e) in mp.egress.iter().enumerate() {
        let oid = egress_output_id(&mp.id, i);
        let cur = app.control.lock().unwrap().state.outputs.get(&oid).cloned();
        let same = cur.as_ref().is_some_and(|o| o.protocol == e.protocol && o.url == e.url && o.credential_ref == e.credential_ref && o.provider == e.provider && o.options == e.options && o.enabled);
        if same {
            continue;
        }
        let o = Output {
            id: oid,
            tenant: mp.tenant.clone(),
            stream: csid.clone(),
            generation: cur.as_ref().map(|c| c.generation + 1).unwrap_or(1),
            protocol: e.protocol.clone(),
            url: e.url.clone(),
            credential_ref: e.credential_ref.clone(),
            enabled: true,
            provider: e.provider.clone(),
            provider_resource: cur.as_ref().and_then(|c| c.provider_resource.clone()),
            options: e.options.clone(),
            created_at_ms: cur.as_ref().map(|c| c.created_at_ms).unwrap_or_else(crate::util::now_ms),
            placement: None,
        };
        propose(app, Command::PutOutput { output: o }).await.map_err(|e| format!("egress: {}", e.title))?;
    }
    Ok(())
}

/// 合成の図: 映像のある参加者を格子（または話者を大きく）に並べ、音声はすべて混ぜる。映像の参加者がいなければ None。
fn composite_graph(c: &senca_control::model::CompositeSpec, src_codec: &str, sink: &str, members: &[(String, String, bool)]) -> Option<Value> {
    let videos: Vec<usize> = (0..members.len()).filter(|&i| members[i].2).collect();
    if videos.is_empty() {
        return None;
    }
    let mut nodes = Vec::new();
    let mut inputs = serde_json::Map::new();
    let mut mix = serde_json::Map::new();
    for (i, (_, sid, video)) in members.iter().enumerate() {
        let mut src = json!({ "type": "source", "id": format!("p{i}"), "stream": sid, "audio": { "codec": "opus" } });
        if *video {
            src["video"] = json!({ "codec": src_codec });
            nodes.push(src);
            nodes.push(json!({ "type": "decode", "id": format!("dv{i}"), "input": format!("p{i}.video") }));
            inputs.insert(format!("p{i}"), json!(format!("dv{i}")));
        } else {
            nodes.push(src);
        }
        nodes.push(json!({ "type": "decode", "id": format!("da{i}"), "input": format!("p{i}.audio") }));
        mix.insert(format!("p{i}"), json!({ "from": format!("da{i}"), "gain_db": 0 }));
    }
    let (w, h) = (c.width.max(64) as i64, c.height.max(64) as i64);
    let n = videos.len() as i64;
    let mut layers = Vec::new();
    if c.layout == "speaker" && n > 1 {
        // 最初の人を大きく、残りを下に並べる（話者の検出が合成に届くまでは参加順）
        let strip = h / 4;
        layers.push(json!({ "source": format!("p{}", videos[0]), "x": 0, "y": 0, "width": w, "height": h - strip }));
        let k = n - 1;
        for (j, &i) in videos[1..].iter().enumerate() {
            let tw = w / k;
            layers.push(json!({ "source": format!("p{i}"), "x": tw * j as i64, "y": h - strip, "width": tw, "height": strip, "z": 1 }));
        }
    } else {
        let cols = (n as f64).sqrt().ceil() as i64;
        let rows = (n + cols - 1) / cols;
        let (cw, ch) = (w / cols, h / rows);
        for (j, &i) in videos.iter().enumerate() {
            let j = j as i64;
            layers.push(json!({ "source": format!("p{i}"), "x": (j % cols) * cw, "y": (j / cols) * ch, "width": cw, "height": ch }));
        }
    }
    nodes.push(json!({
        "type": "compose", "id": "c", "inputs": inputs,
        "canvas": { "width": w, "height": h, "fps_num": c.fps.max(1) },
        "scenes": { "main": { "layers": layers } }, "program": "main",
    }));
    nodes.push(json!({ "type": "audio_mix", "id": "mix", "inputs": mix }));
    nodes.push(json!({ "type": "encode", "id": "venc", "input": "c", "codec": c.video_codec, "bitrate_kbps": c.bitrate_kbps, "gop": c.fps.max(1) * 2 }));
    nodes.push(json!({ "type": "encode", "id": "aenc", "input": "mix", "codec": "opus", "bitrate_kbps": 96 }));
    nodes.push(json!({ "type": "sink", "id": "out", "inputs": ["venc", "aenc"], "stream": sink }));
    Some(json!({ "version": 1, "options": { "clock": "realtime" }, "nodes": nodes }))
}
