//! 制御状態の資源（入力・出力・パイプライン）から、グリッドの仕事（ADR-0013）を決定的に作る。
//!
//! 仕事は資源から導くだけで、利用者が直接は作らない。資源を変えるコマンドを適用するたびに
//! [`derive`] の結果と `grid.workloads` の差を同じコマンドの中で反映する（ずれない・原子的）。
//!
//! 見積もり（予約）は控えめな固定値と、パイプラインの図から数えた GPU のセッション数。
//! 観測した使用量は心拍で別に届き、表示に使う（配置は予約で数える）。

use crate::model::{Input, MediaPipeline, Output, Pipeline, PlacementSpec, Room};
use crate::state::ControlState;
use senca_grid::{Constraints, GpuReq, LabelSelector, Level, Requirements, Resources, Workload, WorkloadKind};
use std::collections::{BTreeMap, BTreeSet};

/// 仕事の ID の頭（資源の種類）。
pub const INPUT: &str = "input:";
pub const OUTPUT: &str = "output:";
pub const PIPELINE: &str = "pipeline:";
pub const ROOM: &str = "room:";

/// 資源から導いた仕事の ID か。
pub fn is_derived(id: &str) -> bool {
    id.starts_with(INPUT) || id.starts_with(OUTPUT) || id.starts_with(PIPELINE) || id.starts_with(ROOM)
}

/// 符号化・復号にハードウェア（GPU の SDK）が要るコーデック（ADR-0004: H.264/H.265 はソフトウェアで持たない）。
fn needs_gpu(codec: &str) -> bool {
    matches!(codec.to_ascii_lowercase().as_str(), "h264" | "h265" | "avc" | "hevc")
}

fn constraints(p: Option<&PlacementSpec>) -> Constraints {
    let Some(p) = p else { return Constraints::default() };
    Constraints {
        node: p.node.clone(),
        device: p.device.clone(),
        labels: p.labels.clone(),
        anti_affinity: p.anti_affinity.clone(),
        pin_policy: p.pin_policy,
    }
}

fn base(id: String, tenant: &str, kind: WorkloadKind, req: Requirements, p: Option<&PlacementSpec>) -> Workload {
    Workload {
        id,
        tenant: tenant.to_string(),
        kind,
        requirements: req,
        degrade: vec![],
        constraints: constraints(p),
        priority: p.map(|p| p.priority).unwrap_or_default(),
        locality: vec![],
        placement: p.map(|p| p.strategy).unwrap_or_default(),
    }
}

pub fn input_workload(i: &Input) -> Option<Workload> {
    if !i.enabled {
        return None;
    }
    let mut req = Requirements {
        resources: Resources { cpu_millis: 150, memory_mb: 64, net_in_mbps: 10, net_out_mbps: 0 },
        ..Default::default()
    };
    if i.protocol == "device" {
        // キャプチャ: 機器を専有し、符号化する（H.264/H.265 なら GPU の符号化のセッション）
        req.capture = i.url.strip_prefix("device:").map(str::to_string);
        req.resources.cpu_millis = 500;
        req.resources.net_in_mbps = 0;
        if i.options.get("codec").is_some_and(|c| needs_gpu(c)) {
            req.gpu = Some(GpuReq { encode: 1, ..Default::default() });
        }
    }
    Some(base(format!("{INPUT}{}", i.id), &i.tenant, if i.protocol == "device" { WorkloadKind::Capture } else { WorkloadKind::Ingest }, req, i.placement.as_ref()))
}

pub fn output_workload(o: &Output) -> Option<Workload> {
    if !o.enabled {
        return None;
    }
    let req = Requirements {
        resources: Resources { cpu_millis: 100, memory_mb: 64, net_in_mbps: 0, net_out_mbps: 10 },
        ..Default::default()
    };
    let mut w = base(format!("{OUTPUT}{}", o.id), &o.tenant, WorkloadKind::Egress, req, o.placement.as_ref());
    // 送り出しは配信を受けたノードの近くで（中継の 1 段を省く）
    w.locality = vec![o.stream.clone()];
    Some(w)
}

fn id_of(n: &serde_json::Value) -> String {
    n.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// 参照（`"d"`・`"t.video"`・`{"from": "at"}`）から節点の ID を取り出す。
fn ref_id(e: &serde_json::Value) -> Option<String> {
    let s = e.as_str().or_else(|| e.get("from").and_then(|x| x.as_str())).or_else(|| e.get("input").and_then(|x| x.as_str()))?;
    Some(s.split('.').next().unwrap_or(s).to_string())
}

/// 節点が入力として参照する節点の ID（`input`・`inputs` の並び／名前つきの表・`layers[].input`）。
fn inputs_of(n: &serde_json::Value) -> Vec<String> {
    let mut v = vec![];
    if let Some(x) = n.get("input")
        && let Some(id) = ref_id(x)
    {
        v.push(id);
    }
    for key in ["inputs", "layers"] {
        match n.get(key) {
            Some(serde_json::Value::Array(a)) => v.extend(a.iter().filter_map(ref_id)),
            Some(serde_json::Value::Object(m)) => v.extend(m.values().filter_map(ref_id)),
            _ => {}
        }
    }
    v
}

/// 縮退の段で止める節点を図から外す。下流は、入力がすべて止まれば止まる。入力の一部だけが止まる節点
/// （複数入力の sink・合成・音の混ぜ合わせ）は、止まった入力を外して残す（合成の場面の、その入力の層も外す）。
pub fn prune_graph(graph: &serde_json::Value, disabled: &BTreeSet<String>) -> serde_json::Value {
    if disabled.is_empty() {
        return graph.clone();
    }
    let nodes = graph.get("nodes").and_then(|n| n.as_array()).cloned().unwrap_or_default();
    // 図は非巡回なので、節点の数だけ回せば収まる
    let mut off: BTreeSet<String> = disabled.clone();
    for _ in 0..nodes.len() {
        let mut changed = false;
        for n in &nodes {
            let id = id_of(n);
            let ins = inputs_of(n);
            if !off.contains(&id) && !ins.is_empty() && ins.iter().all(|i| off.contains(i)) {
                off.insert(id);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let is_off = |e: &serde_json::Value| ref_id(e).is_some_and(|r| off.contains(&r));
    let kept: Vec<serde_json::Value> = nodes
        .into_iter()
        .filter(|n| !off.contains(&id_of(n)))
        .map(|mut n| {
            if let Some(a) = n.get_mut("layers").and_then(|x| x.as_array_mut()) {
                a.retain(|e| !is_off(e));
            }
            let mut removed_names: Vec<String> = vec![];
            match n.get_mut("inputs") {
                Some(serde_json::Value::Array(a)) => a.retain(|e| !is_off(e)),
                Some(serde_json::Value::Object(m)) => {
                    removed_names = m.iter().filter(|(_, e)| is_off(e)).map(|(k, _)| k.clone()).collect();
                    m.retain(|_, e| !is_off(e));
                }
                _ => {}
            }
            // 合成: 外した入力を使う場面の層を外す
            if !removed_names.is_empty()
                && let Some(scenes) = n.get_mut("scenes").and_then(|x| x.as_object_mut())
            {
                for sc in scenes.values_mut() {
                    if let Some(layers) = sc.get_mut("layers").and_then(|x| x.as_array_mut()) {
                        layers.retain(|l| l.get("source").and_then(|x| x.as_str()).is_none_or(|s| !removed_names.iter().any(|r| r == s)));
                    }
                }
            }
            n
        })
        .collect();
    let mut g = graph.clone();
    g["nodes"] = serde_json::Value::Array(kept);
    g
}

/// パイプラインの図から要るものを数える（`disabled` の節点と、それにしか繋がらない下流を除く）。
fn pipeline_requirements(graph: &serde_json::Value, disabled: &BTreeSet<String>) -> Requirements {
    let pruned = prune_graph(graph, disabled);
    let nodes = pruned.get("nodes").and_then(|n| n.as_array()).cloned().unwrap_or_default();
    let by_id: BTreeMap<String, &serde_json::Value> = nodes.iter().map(|n| (id_of(n), n)).collect();
    let (mut enc, mut dec, mut cpu, mut mem) = (0u32, 0u32, 200u64, 128u64);
    for n in &nodes {
        match n.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "encode" => {
                let codec = n.get("codec").and_then(|c| c.as_str()).unwrap_or("");
                if needs_gpu(codec) {
                    enc += 1;
                    cpu += 100;
                } else if codec == "opus" {
                    cpu += 50;
                } else {
                    // ソフトウェアの映像の符号化（AV1・VP8・VP9）は重い
                    cpu += 1500;
                }
                mem += 128;
            }
            "decode" => {
                // 入力が source なら、参照する口（`t.video`・`t.audio`。省略は映像）のコーデックで決まる
                let raw = n.get("input").and_then(|i| i.as_str()).unwrap_or("");
                let port = raw.split_once('.').map(|(_, p)| p).unwrap_or("video");
                let src = ref_id(n.get("input").unwrap_or(&serde_json::Value::Null)).and_then(|i| by_id.get(&i).copied());
                let codec = src
                    .and_then(|s| s.get(port))
                    .and_then(|v| v.get("codec"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                if needs_gpu(codec) {
                    dec += 1;
                    cpu += 100;
                } else {
                    cpu += 400;
                }
                mem += 128;
            }
            "compose" | "board" | "scale" | "crop" => {
                cpu += 300;
                mem += 64;
            }
            "audio_mix" => cpu += 50,
            _ => {}
        }
    }
    Requirements {
        resources: Resources { cpu_millis: cpu, memory_mb: mem, net_in_mbps: 10, net_out_mbps: 10 },
        gpu: (enc + dec > 0).then_some(GpuReq { encode: enc, decode: dec, ..Default::default() }),
        capture: None,
    }
}

pub fn pipeline_workload(p: &Pipeline) -> Option<Workload> {
    if p.desired_state != "running" {
        return None;
    }
    let none = BTreeSet::new();
    let mut w = base(format!("{PIPELINE}{}", p.id), &p.tenant, WorkloadKind::Pipeline, pipeline_requirements(&p.graph, &none), p.placement.as_ref());
    if let Some(pl) = &p.placement {
        // 段ごとに止める節点は積み重ねる（段 2 は段 1 の分も止める）
        let mut acc = BTreeSet::new();
        for l in &pl.degrade {
            acc.extend(l.disable.iter().cloned());
            w.degrade.push(Level { label: l.label.clone(), requirements: pipeline_requirements(&p.graph, &acc) });
        }
    }
    // 入力の stream を持つノードを好む
    if let Some(nodes) = p.graph.get("nodes").and_then(|n| n.as_array()) {
        for n in nodes {
            if n.get("type").and_then(|t| t.as_str()) == Some("source")
                && let Some(s) = n.get("stream").and_then(|s| s.as_str())
            {
                w.locality.push(s.to_string());
            }
        }
    }
    Some(w)
}

/// パイプラインの段 `level` で止める節点（段 0 は無し）。
pub fn pipeline_disabled_at(p: &Pipeline, level: usize) -> BTreeSet<String> {
    let mut acc = BTreeSet::new();
    if let Some(pl) = &p.placement {
        for l in pl.degrade.iter().take(level) {
            acc.extend(l.disable.iter().cloned());
        }
    }
    acc
}

/// 部屋: 参加者の上限から見積もる（SFU は受けた物を複写せずに配るので CPU は軽く、下りの帯域が効く）。
/// 1 人あたり上り 2 Mbit/s、下りは見る相手を最大 8 人として 2 × 8 Mbit/s。
/// 部屋のメディアパイプラインの書き出し（参加者の AU を stream へ流す。複写しないので軽い）は部屋と同じノードで
/// 動くので、部屋の見積もりに足す。合成は別の仕事（パイプライン）として数える（二重に数えない）。
pub fn room_workload(r: &Room, mps: &[&MediaPipeline]) -> Workload {
    let max = r.settings.get("max_participants").and_then(|v| v.as_u64()).unwrap_or(50).clamp(1, 10_000);
    let mut req = Requirements {
        resources: Resources {
            cpu_millis: 200 + 20 * max,
            memory_mb: 64 + 2 * max,
            net_in_mbps: 2 * max,
            net_out_mbps: 2 * max * max.min(8),
        },
        ..Default::default()
    };
    for _ in mps.iter().filter(|m| m.desired_state == "running") {
        req.resources.cpu_millis += 20 * max;
        req.resources.memory_mb += 8 * max;
    }
    let mut w = base(format!("{ROOM}{}", r.id), &r.tenant, WorkloadKind::Room, req, r.placement.as_ref());
    // 参加者は WebRTC でつなぐので、WebRTC の UDP を開いているノードにだけ置く
    w.constraints.labels.push(LabelSelector::Eq { key: "senca.io/webrtc".into(), value: "true".into() });
    w
}

/// 今の資源から導く仕事のすべて。
pub fn derive(st: &ControlState) -> BTreeMap<String, Workload> {
    let mut out = BTreeMap::new();
    for w in st
        .inputs
        .values()
        .filter_map(input_workload)
        .chain(st.outputs.values().filter_map(output_workload))
        .chain(st.pipelines.values().filter_map(pipeline_workload))
        .chain(st.rooms.values().map(|r| {
            let mps: Vec<&MediaPipeline> = st.media_pipelines.values().filter(|m| m.room == r.id).collect();
            room_workload(r, &mps)
        }))
    {
        out.insert(w.id.clone(), w);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pipe(graph: serde_json::Value, placement: Option<PlacementSpec>) -> Pipeline {
        Pipeline {
            id: "p1".into(),
            tenant: "t".into(),
            name: "n".into(),
            generation: 1,
            graph,
            desired_state: "running".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            placement,
        }
    }

    #[test]
    fn pipeline_counts_gpu_sessions_and_degrades() {
        let g = json!({"version":1,"nodes":[
            {"type":"source","id":"s","stream":"st1","video":{"codec":"h264"}},
            {"type":"decode","id":"d","input":"s"},
            {"type":"scale","id":"sc","input":"d","width":640,"height":360},
            {"type":"encode","id":"e1","input":"d","codec":"h264","bitrate_kbps":4000},
            {"type":"encode","id":"e2","input":"sc","codec":"h265","bitrate_kbps":1000},
            {"type":"encode","id":"e3","input":"sc","codec":"av1","bitrate_kbps":800},
            {"type":"sink","id":"k1","inputs":["e1"],"stream":"o1"},
            {"type":"sink","id":"k2","inputs":["e2","e3"],"stream":"o2"}
        ]});
        let pl = PlacementSpec {
            degrade: vec![
                crate::model::DegradeLevel { label: "no_av1".into(), disable: vec!["e3".into()] },
                crate::model::DegradeLevel { label: "single".into(), disable: vec!["sc".into()] },
            ],
            ..Default::default()
        };
        let p = pipe(g, Some(pl));
        let w = pipeline_workload(&p).unwrap();
        let gpu = w.requirements.gpu.clone().unwrap();
        assert_eq!((gpu.encode, gpu.decode), (2, 1));
        assert_eq!(w.locality, vec!["st1".to_string()]);
        assert_eq!(w.levels(), 3);
        // 段 1: AV1 のソフトウェア符号化を止める → CPU が大きく減る
        assert!(w.req(1).resources.cpu_millis + 1000 < w.req(0).resources.cpu_millis);
        // 段 2: scale を止めると、その下流の e2・e3 も止まり、k2 も止まる。GPU の符号化は 1 つ
        assert_eq!(w.req(2).gpu.as_ref().unwrap().encode, 1);
        assert!(pipeline_disabled_at(&p, 0).is_empty());
        assert_eq!(pipeline_disabled_at(&p, 2).into_iter().collect::<Vec<_>>(), vec!["e3".to_string(), "sc".to_string()]);
        // 段 1 の図: e3 を外し、k2 の入力からも外す（k2 は e2 で続く）
        let g1 = prune_graph(&p.graph, &pipeline_disabled_at(&p, 1));
        let ids: Vec<&str> = g1["nodes"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["s", "d", "sc", "e1", "e2", "k1", "k2"]);
        let k2 = g1["nodes"].as_array().unwrap().iter().find(|n| n["id"] == "k2").unwrap();
        assert_eq!(k2["inputs"], json!(["e2"]));
        // 段 2 の図: sc・e2・e3・k2 が消える
        let g2 = prune_graph(&p.graph, &pipeline_disabled_at(&p, 2));
        let ids: Vec<&str> = g2["nodes"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["s", "d", "e1", "k1"]);
    }

    #[test]
    fn prune_handles_named_inputs_ports_and_scenes() {
        // 管理画面の制作の図（合成の名前つきの入力・t.video の形の参照・音の混ぜ合わせ）
        let g = json!({"version":1,"nodes":[
            {"type":"source","id":"t","stream":"a","video":{"codec":"h264"},"audio":{"codec":"opus"}},
            {"type":"source","id":"b","stream":"b","video":{"codec":"vp9"}},
            {"type":"decode","id":"dt","input":"t.video"},{"type":"decode","id":"db","input":"b.video"},{"type":"decode","id":"at","input":"t.audio"},
            {"type":"compose","id":"prod","inputs":{"board":"db","teacher":"dt"},
             "scenes":{"pinp":{"layers":[{"source":"board"},{"source":"teacher","z":1}]},"teacher":{"layers":[{"source":"teacher"}]}}},
            {"type":"audio_mix","id":"mix","inputs":{"mic":{"from":"at","gain_db":0}}},
            {"type":"encode","id":"venc","input":"prod","codec":"av1","bitrate_kbps":800},
            {"type":"encode","id":"aenc","input":"mix","codec":"opus","bitrate_kbps":96},
            {"type":"sink","id":"out","inputs":["venc","aenc"],"stream":"o"}
        ]});
        // 板書のカメラ（b）を止める: db が止まり、合成は先生だけで続く
        let p = prune_graph(&g, &BTreeSet::from(["b".to_string()]));
        let ids: Vec<&str> = p["nodes"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["t", "dt", "at", "prod", "mix", "venc", "aenc", "out"]);
        let prod = p["nodes"].as_array().unwrap().iter().find(|n| n["id"] == "prod").unwrap();
        assert_eq!(prod["inputs"], json!({"teacher":"dt"}));
        assert_eq!(prod["scenes"]["pinp"]["layers"], json!([{"source":"teacher","z":1}]));
        // 音を止める: mix・aenc が止まり、sink は映像だけ
        let p = prune_graph(&g, &BTreeSet::from(["at".to_string()]));
        let out = p["nodes"].as_array().unwrap().iter().find(|n| n["id"] == "out").unwrap();
        assert_eq!(out["inputs"], json!(["venc"]));
        // H.264 の復号は t.video の形の参照からも数える
        let w = pipeline_requirements(&g, &BTreeSet::new());
        assert_eq!(w.gpu.unwrap().decode, 1);
    }

    #[test]
    fn stopped_pipeline_and_disabled_io_have_no_workload() {
        let mut p = pipe(json!({"nodes":[]}), None);
        p.desired_state = "stopped".into();
        assert!(pipeline_workload(&p).is_none());
    }
}
