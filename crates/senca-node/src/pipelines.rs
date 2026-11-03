//! パイプラインの実行（M5、REQ-005・REQ-008）。グラフは codec worker のセッションで動く（生のフレームは worker の中だけ）。
//! ノードの役目: 入力の stream の AU を渡し、出力の AU を出力先の stream に送り手として入れ、
//! 状態・メーター・板書の変化の静止画を受け取る。worker が落ちたら作り直す。

use crate::app::App;
use crate::hub::HubEvent;
use senca_codec_worker::session_pool::{SessionEventItem, SessionHandle, SessionOutputItem};
use senca_control::model::Pipeline;
use senca_core::TrackId;
use senca_pipeline::{Control, Graph};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default, Clone)]
pub struct PipelineRuntime {
    pub state: String,
    pub last_error: Option<String>,
    pub status: Option<Value>,
    pub meters: HashMap<String, Value>,
    pub plan: Option<Value>,
    pub restarts: u64,
}

struct Running {
    generation: u64,
    /// 割り当ての token と縮退の段（どちらかが変われば起動し直す）。
    token: u64,
    level: usize,
    task: tokio::task::JoinHandle<()>,
    rt: Arc<Mutex<PipelineRuntime>>,
    control_tx: tokio::sync::mpsc::Sender<(Control, tokio::sync::oneshot::Sender<Result<Value, String>>)>,
}

#[derive(Default)]
pub struct PipelineManager {
    running: Mutex<HashMap<String, Running>>,
}

impl PipelineManager {
    pub fn runtime(&self, id: &str) -> Option<PipelineRuntime> {
        self.running.lock().unwrap().get(id).map(|r| r.rt.lock().unwrap().clone())
    }

    /// 操作（シーンの切り替えなど）を送る。
    pub async fn control(&self, id: &str, c: Control) -> Result<Value, String> {
        let tx = self.running.lock().unwrap().get(id).map(|r| r.control_tx.clone()).ok_or("パイプラインが動いていません")?;
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send((c, reply)).await.map_err(|_| "パイプラインが止まりました".to_string())?;
        tokio::time::timeout(Duration::from_secs(15), rx).await.map_err(|_| "時間切れ".to_string())?.map_err(|_| "パイプラインが止まりました".to_string())?
    }

    /// 動いている仕事（グリッドの心拍に載せる）。
    pub fn running_workloads(&self) -> Vec<senca_grid::Running> {
        self.running
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| !r.task.is_finished())
            .map(|(id, r)| senca_grid::Running { workload: format!("{}{id}", senca_control::workloads::PIPELINE), token: r.token, level: r.level })
            .collect()
    }

    /// 制御状態と突き合わせる: このノードに割り当てられたパイプラインだけを、割り当ての段で動かす（ADR-0013）。
    /// 縮退の段では、段で止める節点を図から外して動かす。
    pub fn reconcile(&self, app: &Arc<App>) {
        // 過半数と連絡が取れないノードは何も動かさない（自分での fencing。ADR-0013）
        let want: Vec<(Pipeline, u64, usize)> = if app.fenced() {
            Vec::new()
        } else {
            let core = app.control.lock().unwrap();
            core.state
                .pipelines
                .values()
                .filter(|p| p.desired_state == "running")
                .filter_map(|p| {
                    let a = core.state.grid.assignments.get(&format!("{}{}", senca_control::workloads::PIPELINE, p.id))?;
                    (a.node == app.node_id).then(|| (p.clone(), a.token, a.level))
                })
                .collect()
        };
        let mut run = self.running.lock().unwrap();
        run.retain(|id, r| {
            let keep = want.iter().any(|(p, t, l)| &p.id == id && p.generation == r.generation && *t == r.token && *l == r.level) && !r.task.is_finished();
            if !keep {
                r.task.abort();
            }
            keep
        });
        for (p, token, level) in want {
            if run.contains_key(&p.id) {
                continue;
            }
            let mut pl = p.clone();
            if level > 0 {
                let off = senca_control::workloads::pipeline_disabled_at(&p, level);
                pl.graph = senca_control::workloads::prune_graph(&p.graph, &off);
                app.log(&format!("pipeline {}: running degraded (level {level}, without {:?})", p.id, off));
            }
            let rt = Arc::new(Mutex::new(PipelineRuntime { state: "starting".into(), ..Default::default() }));
            let (control_tx, control_rx) = tokio::sync::mpsc::channel(32);
            let wid = format!("{}{}", senca_control::workloads::PIPELINE, p.id);
            let task = tokio::spawn(crate::grid::guarded(app.clone(), wid, token, supervise(app.clone(), pl, rt.clone(), control_rx)));
            run.insert(p.id.clone(), Running { generation: p.generation, token, level, task, rt, control_tx });
        }
    }
}

type ControlRx = tokio::sync::mpsc::Receiver<(Control, tokio::sync::oneshot::Sender<Result<Value, String>>)>;

async fn supervise(app: Arc<App>, p: Pipeline, rt: Arc<Mutex<PipelineRuntime>>, mut control_rx: ControlRx) {
    let mut attempt = 0u32;
    loop {
        let r = run_once(&app, &p, &rt, &mut control_rx).await;
        let stop = {
            let mut g = rt.lock().unwrap();
            g.restarts += 1;
            match r {
                Ok(()) => {
                    g.state = "stopped".into();
                    true
                }
                Err(e) => {
                    app.log(&format!("pipeline {}: {e}", p.id));
                    g.state = "error".into();
                    g.last_error = Some(e);
                    false
                }
            }
        };
        if stop {
            return;
        }
        let wait = 1000u64 << attempt.min(5);
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(wait)).await;
    }
}

async fn run_once(app: &Arc<App>, p: &Pipeline, rt: &Mutex<PipelineRuntime>, control_rx: &mut ControlRx) -> Result<(), String> {
    let codec = app.codec.as_ref().ok_or("codec worker が動いていません")?;
    let graph = Graph::from_json(&p.graph.to_string()).map_err(|e| format!("{e:?}"))?;
    let royalty_free = graph_sources(&graph).iter().any(|s| {
        app.control.lock().unwrap().state.streams.get(s).is_some_and(|x| x.codec_profile == senca_control::model::CodecProfile::RoyaltyFree)
    });
    let policy = senca_codec::Policy { royalty_free_only: royalty_free, ..Default::default() };
    let mut h: SessionHandle = codec.sessions.open_session(&p.tenant, policy, &graph).await.map_err(|e| format!("{}: {}", e.code, e.message))?;
    {
        let mut g = rt.lock().unwrap();
        g.state = "running".into();
        g.plan = serde_json::to_value(h.plan()).ok();
        g.last_error = None;
    }
    let mut outputs = h.outputs().ok_or("outputs")?;
    let mut events = h.events().ok_or("events")?;
    let h = Arc::new(h);
    // 入力: stream の AU を送る
    let mut feeders = Vec::new();
    for (node_id, stream) in source_nodes(&graph) {
        let h = h.clone();
        let app = app.clone();
        feeders.push(tokio::spawn(async move { feed_source(app, h, node_id, stream).await }));
    }
    // 出力の送り手（出力先の stream ごと）
    let mut publishers: HashMap<String, crate::hub::Publisher> = HashMap::new();
    let result = loop {
        tokio::select! {
            o = outputs.recv() => match o {
                Some(SessionOutputItem::Media { stream, track, config, aus, .. }) => {
                    if !publishers.contains_key(&stream) {
                        match crate::io::attach(app, &stream, "pipeline", &format!("pipeline:{}", p.id)).await {
                            Ok(pb) => { publishers.insert(stream.clone(), pb); }
                            Err(e) => break Err(format!("出力先 {stream}: {e}")),
                        }
                    }
                    let pb = publishers.get(&stream).unwrap();
                    if let Some(c) = config {
                        let _ = pb.config(TrackId(track), c).await;
                    }
                    for mut a in aus {
                        a.track = TrackId(track);
                        if pb.access_unit(a).await.is_err() { break; }
                    }
                }
                Some(SessionOutputItem::Snapshot { node, width, height, changed_fraction, jpeg, .. }) => {
                    // パイプラインの静止画として保存（板書の変化）。部品ごとに params.keep 枚まで残す
                    let keep = snapshot_keep(&graph, &node);
                    let meta = json!({ "taken_at": crate::util::rfc3339(crate::util::now_ms()), "reason": "board_change", "node": node, "width": width, "height": height, "bytes": jpeg.len(), "changed_fraction": changed_fraction });
                    let d = crate::snapshot::pipeline_dir(app, &p.id);
                    let r = tokio::task::spawn_blocking(move || crate::snapshot::store_jpeg(&d, &jpeg, meta, Some(&node), keep)).await;
                    if let Ok(Err(e)) = r {
                        app.log(&format!("pipeline {}: snapshot store: {e}", p.id));
                    }
                }
                None => break Err("出力が止まりました".into()),
            },
            e = events.recv() => match e {
                Some(SessionEventItem::Pipeline(ev)) => {
                    let v = serde_json::to_value(&ev).unwrap_or(Value::Null);
                    let mut g = rt.lock().unwrap();
                    match v.get("event").or_else(|| v.get("kind")).or_else(|| v.get("type")).and_then(Value::as_str) {
                        Some(k) if k.contains("meter") => {
                            let node = v.get("node").and_then(Value::as_str).unwrap_or("").to_string();
                            g.meters.insert(node, v);
                        }
                        _ => g.status = Some(v),
                    }
                }
                Some(SessionEventItem::InputDropped { source, count }) => app.log(&format!("pipeline {}: input {source} dropped {count}", p.id)),
                Some(SessionEventItem::Crashed { reason }) => break Err(format!("worker が落ちました: {reason}")),
                Some(SessionEventItem::Closed) | None => break Ok(()),
            },
            c = control_rx.recv() => {
                let Some((c, reply)) = c else { break Ok(()) };
                let r = h.control(&c).await.map(|r| serde_json::to_value(r).unwrap_or(Value::Null)).map_err(|e| format!("{}: {}", e.code, e.message));
                let _ = reply.send(r);
            }
        }
    };
    for f in feeders {
        f.abort();
    }
    for (_, pb) in publishers {
        pb.detach("pipeline stopped").await;
    }
    if let Ok(h) = Arc::try_unwrap(h) {
        let _ = h.close().await;
    }
    result
}

fn source_nodes(g: &Graph) -> Vec<(String, String)> {
    g.nodes
        .iter()
        .filter_map(|n| match n {
            senca_pipeline::model::Node::Source(s) => Some((s.id.clone(), s.stream.clone())),
            _ => None,
        })
        .collect()
}

pub fn graph_sources(g: &Graph) -> Vec<String> {
    source_nodes(g).into_iter().map(|(_, s)| s).collect()
}

/// `snapshot_on_change` の部品の `params.keep`。
fn snapshot_keep(g: &Graph, node: &str) -> u32 {
    g.nodes
        .iter()
        .find_map(|n| match n {
            senca_pipeline::model::Node::SnapshotOnChange(s) if s.id == node => Some(s.params.keep),
            _ => None,
        })
        .unwrap_or(1000)
}

pub fn sink_streams(g: &Graph) -> Vec<String> {
    g.nodes
        .iter()
        .filter_map(|n| match n {
            senca_pipeline::model::Node::Sink(s) => Some(s.stream.clone()),
            _ => None,
        })
        .collect()
}

/// 入力の stream の AU を session へ（配信が始まるまで待つ）。
async fn feed_source(app: Arc<App>, h: Arc<SessionHandle>, node_id: String, stream: String) {
    loop {
        // 別のノードが受けている stream は中継の写しから（ADR-0014）
        let hub = app.source_hub(&stream);
        let Some(hub) = hub.filter(|h| h.is_live()) else {
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        };
        let mut sub = hub.subscribe();
        let send = |ev: HubEvent| -> bool {
            match ev {
                HubEvent::Config(_, c) => h.send_input(&node_id, Some(c), vec![]).is_ok(),
                HubEvent::Au(a) => h.send_input(&node_id, None, vec![a]).is_ok(),
                HubEvent::Discontinuity | HubEvent::Segment { .. } => true,
                HubEvent::End => false,
            }
        };
        let mut alive = true;
        for ev in sub.initial() {
            alive &= send(ev);
        }
        while alive {
            match sub.recv().await {
                Ok(ev) => alive = send(ev),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => sub.lagged(n),
                Err(_) => break,
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}
