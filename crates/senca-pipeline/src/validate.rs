//! 検証と変換計画（仕様 5.1・5.2・7 章、REQ-005 AC-P11）。
//!
//! # 検証すること
//!
//! 1. **形と上限**: id の形（英数字・`_`・`-`、1〜64 文字）と重複、部品の数、解像度・fps・ビットレートなどの範囲、
//!    シーン・レイヤー・画像の数と中身（画像は実際に復号して確かめる）。
//! 2. **参照**: 入力の参照先の部品・ポートがあること。
//! 3. **非巡回**: 循環があれば、循環に関わる部品を挙げて拒否する。
//! 4. **ポートの型**: 符号化済み（映像／音声、コーデック付き）と生（映像／音声）を区別し、つなげない組み合わせを
//!    拒否する（例: 符号化済みの映像を合成に入れる、映像を Opus で符号化する、sink に生のフレームを入れる）。
//! 5. **使い残し**: 終端（`sink`・`snapshot_on_change`）以外の部品の出力は、どこかで使われていること
//!    （`source` は宣言したトラックごと、`compose` は `program`）。
//! 6. **コーデックの方針**（ADR-0004・REQ-007）: AAC の復号・符号化は `UNSUPPORTED_CODEC`、
//!    ロイヤリティフリー限定では H.264/H.265/AAC（素通しも含む）を `POLICY_REJECTED`。
//! 7. **能力**: 復号・符号化ごとに、与えられた能力（worker の Hello）に合う提供者があること。H.264/H.265 は
//!    ハードウェアだけを数え、無ければ `CAPACITY_UNAVAILABLE`。ハードウェアの同時セッション数の上限も数える。
//! 8. **見積もり**: 部品ごとの画素/秒、CPU・GPU のコーデックのセッション数、追加の遅延。出力ごとの遅延は経路の最大。

use crate::convert;
use crate::error::{Issue, IssueCode, PipelineError};
use crate::model::*;
use senca_codec::caps::{Capability, Direction, ProviderId};
use senca_codec::registry::Policy;
use senca_core::{Codec, MediaKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// 上限。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphLimits {
    pub max_nodes: usize,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: f64,
    pub max_compose_inputs: usize,
    pub max_scenes: usize,
    pub max_images: usize,
    /// 画像 1 枚の base64 の長さ。
    pub max_image_b64: usize,
    pub max_mix_inputs: usize,
}

impl Default for GraphLimits {
    fn default() -> Self {
        GraphLimits {
            max_nodes: 256,
            max_width: 7680,
            max_height: 4320,
            max_fps: 240.0,
            max_compose_inputs: 32,
            max_scenes: 64,
            max_images: 32,
            max_image_b64: 8 * 1024 * 1024,
            max_mix_inputs: 32,
        }
    }
}

/// 検証の前提（方針と、実行する所の能力）。
#[derive(Debug, Clone)]
pub struct ValidationContext {
    pub policy: Policy,
    /// codec worker の能力（`WorkerPool::capabilities()`、または worker の中の `ProviderRegistry::capabilities`）。
    pub capabilities: Vec<Capability>,
    pub limits: GraphLimits,
}

impl ValidationContext {
    pub fn new(policy: Policy, capabilities: Vec<Capability>) -> Self {
        ValidationContext {
            policy,
            capabilities,
            limits: GraphLimits::default(),
        }
    }
}

/// ポート。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Port {
    Video,
    Audio,
    Out,
    Program,
    Preview,
}

impl Port {
    pub fn as_str(self) -> &'static str {
        match self {
            Port::Video => "video",
            Port::Audio => "audio",
            Port::Out => "out",
            Port::Program => "program",
            Port::Preview => "preview",
        }
    }
}

/// ポートの型。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PortType {
    Encoded {
        kind: MediaKind,
        codec: Codec,
        width: Option<u32>,
        height: Option<u32>,
        fps: Option<f64>,
    },
    RawVideo {
        width: Option<u32>,
        height: Option<u32>,
        fps: Option<f64>,
    },
    RawAudio {
        sample_rate: u32,
        channels: Option<u8>,
    },
}

impl PortType {
    pub fn describe(&self) -> String {
        match self {
            PortType::Encoded { kind, codec, .. } => format!(
                "encoded {} ({codec})",
                if *kind == MediaKind::Video {
                    "video"
                } else {
                    "audio"
                }
            ),
            PortType::RawVideo { width, height, .. } => match (width, height) {
                (Some(w), Some(h)) => format!("raw video {w}x{h}"),
                _ => "raw video".into(),
            },
            PortType::RawAudio { sample_rate, .. } => format!("raw audio {sample_rate} Hz"),
        }
    }

    fn is_raw_video(&self) -> bool {
        matches!(self, PortType::RawVideo { .. })
    }
}

/// 部品の処理の形（仕様 5.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeMode {
    /// 符号化済みのまま渡す。
    Passthrough,
    /// 外部コーデックで復号・符号化する。
    Transcode,
    /// 生のフレームの処理（自前）。
    Process,
    /// 終端（出力・通知）。
    Terminal,
}

/// 部品ごとの計画。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodePlan {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub mode: NodeMode,
    /// 出力の型の説明。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub outputs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub codec: Option<String>,
    /// コーデックをハードウェアで動かす見込み。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub hardware: Option<bool>,
    /// 見込みの提供者。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provider: Option<String>,
    pub pixels_per_sec: u64,
    pub cpu_codec_sessions: u32,
    pub gpu_codec_sessions: u32,
    /// この部品が足す遅延（ms）。
    pub added_latency_ms: f64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notes: Vec<String>,
}

/// 出力のトラックの計画。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SinkTrackPlan {
    pub track: u32,
    pub kind: String,
    pub codec: String,
    pub mode: NodeMode,
    /// 入力から出力までの追加の遅延（経路の最大、ms）。
    pub latency_ms: f64,
    /// 元の部品（`encode` か `source`）。
    pub from: String,
}

/// 出力の計画。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SinkPlan {
    pub id: String,
    pub stream: String,
    pub tracks: Vec<SinkTrackPlan>,
}

/// 合計。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Totals {
    pub pixels_per_sec: u64,
    pub cpu_codec_sessions: u32,
    pub gpu_codec_sessions: u32,
    /// 使うスレッドの目安（復号・合成・ミキサー・符号化ごとに 1 つ＋合成の帯）。
    pub threads: u32,
    pub max_latency_ms: f64,
}

/// 変換計画（仕様 7 章: 実行前に処理の形・必要資源・追加の遅延を返す）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// 実行の順（トポロジカル順）。
    pub order: Vec<String>,
    pub nodes: Vec<NodePlan>,
    pub sinks: Vec<SinkPlan>,
    pub totals: Totals,
}

/// 検証済みのグラフ（実行に使う）。
#[derive(Debug, Clone)]
pub struct Resolved {
    pub graph: Graph,
    /// トポロジカル順の部品の番号。
    pub order: Vec<usize>,
    /// 部品ごとの入力（上流の部品の番号とポート）。名前付きの入力は名前の順（`BTreeMap` の順）。
    pub inputs: Vec<Vec<(usize, Port)>>,
    /// 出力ポートの型。
    pub port_types: HashMap<(usize, Port), PortType>,
    /// 出力ポートの使い手（部品の番号、その部品の何番目の入力か）。
    pub consumers: HashMap<(usize, Port), Vec<(usize, usize)>>,
    pub plan: Plan,
}

impl Resolved {
    pub fn index(&self, id: &str) -> Option<usize> {
        self.graph.nodes.iter().position(|n| n.id() == id)
    }
}

/// 検証して計画を返す。
pub fn validate(graph: &Graph, ctx: &ValidationContext) -> Result<Plan, PipelineError> {
    resolve(graph, ctx).map(|r| r.plan)
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

struct V<'a> {
    ctx: &'a ValidationContext,
    issues: Vec<Issue>,
}

impl V<'_> {
    fn push(&mut self, code: IssueCode, node: &str, msg: impl Into<String>) {
        self.issues.push(Issue::new(code, Some(node), msg));
    }
}

/// 検証し、実行に必要な形にする。
pub fn resolve(graph: &Graph, ctx: &ValidationContext) -> Result<Resolved, PipelineError> {
    let mut v = V {
        ctx,
        issues: Vec::new(),
    };
    let lim = &ctx.limits;
    if graph.version != GRAPH_VERSION {
        v.issues.push(Issue::new(
            IssueCode::Malformed,
            None,
            format!("unsupported graph version {}", graph.version),
        ));
    }
    if graph.nodes.is_empty() {
        v.issues.push(Issue::new(
            IssueCode::InvalidParam,
            None,
            "graph has no nodes",
        ));
    }
    if graph.nodes.len() > lim.max_nodes {
        return Err(PipelineError::Invalid(vec![Issue::new(
            IssueCode::LimitExceeded,
            None,
            format!("too many nodes ({} > {})", graph.nodes.len(), lim.max_nodes),
        )]));
    }
    check_options(&mut v, &graph.options);
    // id
    let mut ids: HashMap<&str, usize> = HashMap::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        if !valid_name(n.id()) {
            v.push(
                IssueCode::InvalidParam,
                n.id(),
                "node id must be 1-64 characters of [A-Za-z0-9_-]",
            );
        }
        if ids.insert(n.id(), i).is_some() {
            v.push(IssueCode::DuplicateId, n.id(), "duplicate node id");
        }
    }
    for n in &graph.nodes {
        check_params(&mut v, n);
    }
    // 参照
    let mut inputs: Vec<Vec<(usize, Port)>> = vec![Vec::new(); graph.nodes.len()];
    let mut refs_ok = true;
    for (i, n) in graph.nodes.iter().enumerate() {
        for r in n.inputs() {
            let (nid, port) = split_ref(r);
            let Some(&j) = ids.get(nid) else {
                v.push(
                    IssueCode::UnknownReference,
                    n.id(),
                    format!("input '{r}' refers to an unknown node"),
                );
                refs_ok = false;
                continue;
            };
            match resolve_port(&graph.nodes[j], port) {
                Ok(p) => inputs[i].push((j, p)),
                Err(m) => {
                    v.push(
                        IssueCode::UnknownReference,
                        n.id(),
                        format!("input '{r}': {m}"),
                    );
                    refs_ok = false;
                }
            }
        }
    }
    if !refs_ok {
        return Err(PipelineError::Invalid(v.issues));
    }
    // 非巡回（Kahn）
    let n = graph.nodes.len();
    let mut indeg = vec![0usize; n];
    let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ins) in inputs.iter().enumerate() {
        for &(j, _) in ins {
            indeg[i] += 1;
            out_edges[j].push(i);
        }
    }
    let mut q: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = q.pop_front() {
        order.push(i);
        for &k in &out_edges[i] {
            indeg[k] -= 1;
            if indeg[k] == 0 {
                q.push_back(k);
            }
        }
    }
    if order.len() != n {
        let mut cyc: Vec<&str> = (0..n)
            .filter(|i| indeg[*i] > 0)
            .map(|i| graph.nodes[i].id())
            .collect();
        cyc.sort();
        v.issues.push(Issue::new(
            IssueCode::Cycle,
            cyc.first().copied(),
            format!("graph has a cycle through: {}", cyc.join(", ")),
        ));
        return Err(PipelineError::Invalid(v.issues));
    }
    // 型
    let mut types: HashMap<(usize, Port), PortType> = HashMap::new();
    for &i in &order {
        infer(&mut v, graph, i, &inputs[i], &mut types);
    }
    // 使い手
    let mut consumers: HashMap<(usize, Port), Vec<(usize, usize)>> = HashMap::new();
    for (i, ins) in inputs.iter().enumerate() {
        for (k, &(j, p)) in ins.iter().enumerate() {
            consumers.entry((j, p)).or_default().push((i, k));
        }
    }
    for (i, node) in graph.nodes.iter().enumerate() {
        let required: Vec<Port> = match node {
            Node::Sink(_) | Node::SnapshotOnChange(_) => vec![],
            Node::Source(s) => {
                let mut p = Vec::new();
                if s.video.is_some() {
                    p.push(Port::Video);
                }
                if s.audio.is_some() {
                    p.push(Port::Audio);
                }
                p
            }
            Node::Compose(_) => vec![Port::Program],
            _ => vec![Port::Out],
        };
        for p in required {
            if !consumers.contains_key(&(i, p)) {
                v.push(
                    IssueCode::UnconsumedOutput,
                    node.id(),
                    format!("output '{}' is not consumed by any node", p.as_str()),
                );
            }
        }
    }
    if !v.issues.is_empty() {
        return Err(PipelineError::Invalid(v.issues));
    }
    let plan = make_plan(&mut v, graph, &order, &inputs, &types, &consumers);
    if !v.issues.is_empty() {
        return Err(PipelineError::Invalid(v.issues));
    }
    Ok(Resolved {
        graph: graph.clone(),
        order,
        inputs,
        port_types: types,
        consumers,
        plan,
    })
}

fn check_options(v: &mut V<'_>, o: &GraphOptions) {
    let mut bad = |m: &str| {
        v.issues.push(Issue::new(
            IssueCode::InvalidParam,
            None,
            format!("options: {m}"),
        ))
    };
    if o.latency_ms > 10_000 {
        bad("latency_ms must be <= 10000");
    }
    if !(100..=60_000).contains(&o.missing_timeout_ms) {
        bad("missing_timeout_ms must be 100..=60000");
    }
    if !(10..=60_000).contains(&o.block_timeout_ms) {
        bad("block_timeout_ms must be 10..=60000");
    }
    if !(8..=100_000).contains(&o.input_queue_aus) {
        bad("input_queue_aus must be 8..=100000");
    }
    if !(1..=256).contains(&o.frame_queue) {
        bad("frame_queue must be 1..=256");
    }
    if o.status_interval_ms != 0 && !(20..=60_000).contains(&o.status_interval_ms) {
        bad("status_interval_ms must be 0 or 20..=60000");
    }
    if !(100..=600_000).contains(&o.commit_timeout_ms) {
        bad("commit_timeout_ms must be 100..=600000");
    }
}

fn resolve_port(n: &Node, port: Option<&str>) -> Result<Port, String> {
    let p = match (n, port) {
        (Node::Source(s), Some("video")) if s.video.is_some() => Port::Video,
        (Node::Source(s), Some("audio")) if s.audio.is_some() => Port::Audio,
        (Node::Source(_), Some(p)) => return Err(format!("source has no '{p}' track")),
        (Node::Source(_), None) => return Err("specify the track ('.video' or '.audio')".into()),
        (Node::Compose(_), None | Some("program")) => Port::Program,
        (Node::Compose(_), Some("preview")) => Port::Preview,
        (Node::Sink(_) | Node::SnapshotOnChange(_), _) => {
            return Err(format!("{} has no outputs", n.type_name()));
        }
        (_, None | Some("out")) => Port::Out,
        (_, Some(p)) => return Err(format!("{} has no port '{p}'", n.type_name())),
    };
    Ok(p)
}

fn dim_ok(lim: &GraphLimits, w: u32, h: u32) -> bool {
    (2..=lim.max_width).contains(&w) && (2..=lim.max_height).contains(&h)
}

fn check_params(v: &mut V<'_>, node: &Node) {
    let lim = v.ctx.limits.clone();
    let id = node.id().to_string();
    let id = id.as_str();
    match node {
        Node::Source(s) => {
            if s.stream.is_empty() || s.stream.len() > 256 {
                v.push(IssueCode::InvalidParam, id, "stream id must be 1-256 bytes");
            }
            if s.video.is_none() && s.audio.is_none() {
                v.push(IssueCode::InvalidParam, id, "source selects no track");
            }
            for (t, kind) in [(&s.video, MediaKind::Video), (&s.audio, MediaKind::Audio)] {
                let Some(t) = t else { continue };
                match Codec::parse(&t.codec) {
                    Some(c) if c.kind() == kind => {}
                    Some(c) => v.push(
                        IssueCode::TypeMismatch,
                        id,
                        format!("{c} is not a {} codec", kind_name(kind)),
                    ),
                    None => v.push(
                        IssueCode::UnsupportedCodec,
                        id,
                        format!("unknown codec '{}'", t.codec),
                    ),
                }
                if let (Some(w), Some(h)) = (t.width, t.height)
                    && !dim_ok(&lim, w, h)
                {
                    v.push(IssueCode::LimitExceeded, id, format!("resolution {w}x{h}"));
                }
                if t.fps.is_some_and(|f| !(f > 0.0 && f <= lim.max_fps)) {
                    v.push(IssueCode::InvalidParam, id, "fps out of range");
                }
                if t.sample_rate
                    .is_some_and(|r| !(8000..=192_000).contains(&r))
                {
                    v.push(IssueCode::InvalidParam, id, "sample_rate out of range");
                }
                if t.channels.is_some_and(|c| !(1..=8).contains(&c)) {
                    v.push(IssueCode::InvalidParam, id, "channels out of range");
                }
            }
        }
        Node::Decode(d) => {
            if d.max_width
                .is_some_and(|w| !(2..=lim.max_width).contains(&w))
                || d.max_height
                    .is_some_and(|h| !(2..=lim.max_height).contains(&h))
            {
                v.push(
                    IssueCode::LimitExceeded,
                    id,
                    "max_width/max_height out of range",
                );
            }
            if d.threads > 64 {
                v.push(IssueCode::InvalidParam, id, "threads must be <= 64");
            }
        }
        Node::Board(b) => {
            let p = &b.params;
            if !dim_ok(&lim, p.output_width, p.output_height)
                || p.output_width < 16
                || p.output_height < 16
            {
                v.push(
                    IssueCode::InvalidParam,
                    id,
                    "board output size out of range",
                );
            }
            if let Some(c) = &p.corners {
                if c.iter().flatten().any(|x| !x.is_finite() || x.abs() > 1e5) {
                    v.push(IssueCode::InvalidParam, id, "corners must be finite pixels");
                } else if let Err(e) =
                    senca_board::BoardCorrector::new(convert::board_params(p, None))
                {
                    v.push(IssueCode::InvalidParam, id, format!("corners: {e}"));
                }
            }
            for g in [p.stroke_gain, p.chroma_gain].into_iter().flatten() {
                if !(g.is_finite() && (0.0..=10.0).contains(&g)) {
                    v.push(IssueCode::InvalidParam, id, "gain must be 0..=10");
                }
            }
            if p.redetect_frames > 1_000_000 {
                v.push(IssueCode::InvalidParam, id, "redetect_frames too large");
            }
        }
        Node::Scale(s) => {
            if !dim_ok(&lim, s.width, s.height) {
                v.push(IssueCode::InvalidParam, id, "scale size out of range");
            }
        }
        Node::Crop(c) => {
            if !dim_ok(&lim, c.width, c.height) || c.x > lim.max_width || c.y > lim.max_height {
                v.push(IssueCode::InvalidParam, id, "crop rectangle out of range");
            }
        }
        Node::Compose(c) => check_compose(v, id, c),
        Node::AudioMix(m) => {
            if m.inputs.is_empty() || m.inputs.len() > lim.max_mix_inputs {
                v.push(
                    IssueCode::LimitExceeded,
                    id,
                    format!("audio_mix needs 1..={} inputs", lim.max_mix_inputs),
                );
            }
            if !(m.master_gain_db.is_finite() && (-120.0..=24.0).contains(&m.master_gain_db)) {
                v.push(
                    IssueCode::InvalidParam,
                    id,
                    "master_gain_db must be -120..=24",
                );
            }
            for (name, i) in &m.inputs {
                if !valid_name(name) {
                    v.push(
                        IssueCode::InvalidParam,
                        id,
                        format!("bad input name '{name}'"),
                    );
                }
                if let Err(e) = convert::check_mix_input(i) {
                    v.push(IssueCode::InvalidParam, id, format!("input '{name}': {e}"));
                }
            }
        }
        Node::Encode(e) => {
            if e.bitrate_kbps == 0 || e.bitrate_kbps > 800_000 {
                v.push(
                    IssueCode::InvalidParam,
                    id,
                    "bitrate_kbps must be 1..=800000",
                );
            }
            if e.gop.is_some_and(|g| g > 100_000) {
                v.push(IssueCode::InvalidParam, id, "gop too large");
            }
            match (e.width, e.height) {
                (None, None) => {}
                (Some(w), Some(h)) if dim_ok(&lim, w, h) => {}
                _ => v.push(
                    IssueCode::InvalidParam,
                    id,
                    "width and height must be given together and within limits",
                ),
            }
            if let Some(f) = e.frame_ms
                && ![2.5f32, 5.0, 10.0, 20.0, 40.0, 60.0].contains(&f)
            {
                v.push(
                    IssueCode::InvalidParam,
                    id,
                    "frame_ms must be 2.5/5/10/20/40/60",
                );
            }
            if e.threads > 64 {
                v.push(IssueCode::InvalidParam, id, "threads must be <= 64");
            }
            if Codec::parse(&e.codec).is_none() {
                v.push(
                    IssueCode::UnsupportedCodec,
                    id,
                    format!("unknown codec '{}'", e.codec),
                );
            }
        }
        Node::SnapshotOnChange(s) => {
            let p = &s.params;
            if !(p.analysis_fps.is_finite() && (0.0..=120.0).contains(&p.analysis_fps)) {
                v.push(IssueCode::InvalidParam, id, "analysis_fps must be 0..=120");
            }
            if !(1..=10_000).contains(&p.stable_frames) {
                v.push(
                    IssueCode::InvalidParam,
                    id,
                    "stable_frames must be 1..=10000",
                );
            }
            if !(p.change_fraction > 0.0 && p.change_fraction < 1.0) {
                v.push(
                    IssueCode::InvalidParam,
                    id,
                    "change_fraction must be in (0, 1)",
                );
            }
            if !(1..=100).contains(&p.jpeg_quality) {
                v.push(IssueCode::InvalidParam, id, "jpeg_quality must be 1..=100");
            }
            if !(1..=10_000).contains(&p.keep) {
                v.push(IssueCode::InvalidParam, id, "keep must be 1..=10000");
            }
        }
        Node::Sink(s) => {
            if s.stream.is_empty() || s.stream.len() > 256 {
                v.push(IssueCode::InvalidParam, id, "stream id must be 1-256 bytes");
            }
            if s.inputs.is_empty() || s.inputs.len() > 2 {
                v.push(
                    IssueCode::InvalidParam,
                    id,
                    "sink takes 1 or 2 inputs (video, audio)",
                );
            }
        }
    }
}

fn check_compose(v: &mut V<'_>, id: &str, c: &ComposeNode) {
    let lim = v.ctx.limits.clone();
    let cv = &c.canvas;
    if !dim_ok(&lim, cv.width, cv.height)
        || !cv.width.is_multiple_of(2)
        || !cv.height.is_multiple_of(2)
    {
        v.push(
            IssueCode::InvalidParam,
            id,
            "canvas size must be even and within limits",
        );
    }
    if cv.fps_num == 0
        || cv.fps_den == 0
        || cv.fps_num as f64 / cv.fps_den as f64 > lim.max_fps.min(120.0)
    {
        v.push(IssueCode::InvalidParam, id, "canvas fps out of range");
    }
    if parse_color(&cv.background).is_none() {
        v.push(IssueCode::InvalidParam, id, "background must be #RRGGBB");
    }
    if c.inputs.len() > lim.max_compose_inputs {
        v.push(
            IssueCode::LimitExceeded,
            id,
            format!("too many compose inputs (> {})", lim.max_compose_inputs),
        );
    }
    for k in c.inputs.keys() {
        if !valid_name(k) {
            v.push(IssueCode::InvalidParam, id, format!("bad input name '{k}'"));
        }
    }
    if c.images.len() > lim.max_images {
        v.push(IssueCode::LimitExceeded, id, "too many images");
    }
    for (name, img) in &c.images {
        if !valid_name(name) {
            v.push(
                IssueCode::InvalidParam,
                id,
                format!("bad image name '{name}'"),
            );
        }
        let len = img
            .png_base64
            .as_ref()
            .or(img.jpeg_base64.as_ref())
            .map_or(0, String::len);
        if len > lim.max_image_b64 {
            v.push(
                IssueCode::LimitExceeded,
                id,
                format!("image '{name}' is too large"),
            );
            continue;
        }
        if let Err(e) = convert::decode_image(img, senca_image::YuvSpec::BT709_LIMITED) {
            v.push(IssueCode::InvalidParam, id, format!("image '{name}': {e}"));
        }
    }
    if c.scenes.is_empty() || c.scenes.len() > lim.max_scenes {
        v.push(
            IssueCode::LimitExceeded,
            id,
            format!("compose needs 1..={} scenes", lim.max_scenes),
        );
    }
    let names = convert::ComposeNames::new(c);
    for (sname, s) in &c.scenes {
        if !valid_name(sname) {
            v.push(
                IssueCode::InvalidParam,
                id,
                format!("bad scene name '{sname}'"),
            );
        }
        if let Err(e) = names.scene(s) {
            v.push(IssueCode::InvalidParam, id, format!("scene '{sname}': {e}"));
        }
    }
    if !c.scenes.contains_key(&c.program) {
        v.push(
            IssueCode::UnknownReference,
            id,
            format!("program scene '{}' does not exist", c.program),
        );
    }
    if let Some(p) = &c.preview
        && !c.scenes.contains_key(p)
    {
        v.push(
            IssueCode::UnknownReference,
            id,
            format!("preview scene '{p}' does not exist"),
        );
    }
    if c.transition.duration_ms > 60_000 {
        v.push(IssueCode::InvalidParam, id, "transition longer than 60 s");
    }
    if c.missing.hold_ms > 600_000 {
        v.push(IssueCode::InvalidParam, id, "hold_ms too large");
    }
    if parse_color(&c.missing.slate_color).is_none() {
        v.push(IssueCode::InvalidParam, id, "slate_color must be #RRGGBB");
    }
    if let Some(s) = &c.missing.slate_image
        && !c.images.contains_key(s)
    {
        v.push(
            IssueCode::UnknownReference,
            id,
            format!("slate image '{s}' does not exist"),
        );
    }
    if c.threads > 64 {
        v.push(IssueCode::InvalidParam, id, "threads must be <= 64");
    }
}

fn kind_name(k: MediaKind) -> &'static str {
    match k {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Subtitle => "subtitle",
        MediaKind::Data => "data",
    }
}

fn infer(
    v: &mut V<'_>,
    graph: &Graph,
    i: usize,
    ins: &[(usize, Port)],
    types: &mut HashMap<(usize, Port), PortType>,
) {
    let node = &graph.nodes[i];
    let id = node.id();
    let in_types: Vec<Option<PortType>> = ins.iter().map(|k| types.get(k).copied()).collect();
    // 上流に誤りがあって型が無いときは、ここでは重ねて報告しない。
    if in_types.iter().any(Option::is_none) {
        return;
    }
    let in_types: Vec<PortType> = in_types.into_iter().flatten().collect();
    let raw_video_in =
        |v: &mut V<'_>, t: &PortType| -> Option<(Option<u32>, Option<u32>, Option<f64>)> {
            match *t {
                PortType::RawVideo { width, height, fps } => Some((width, height, fps)),
                other => {
                    v.push(
                        IssueCode::TypeMismatch,
                        id,
                        format!(
                            "{} needs raw video, got {} (insert a decode node)",
                            node.type_name(),
                            other.describe()
                        ),
                    );
                    None
                }
            }
        };
    match node {
        Node::Source(s) => {
            for (t, port, kind) in [
                (&s.video, Port::Video, MediaKind::Video),
                (&s.audio, Port::Audio, MediaKind::Audio),
            ] {
                if let Some(t) = t
                    && let Some(codec) = Codec::parse(&t.codec)
                    && codec.kind() == kind
                {
                    types.insert(
                        (i, port),
                        PortType::Encoded {
                            kind,
                            codec,
                            width: t.width,
                            height: t.height,
                            fps: t.fps,
                        },
                    );
                }
            }
        }
        Node::Decode(_) => match in_types[0] {
            PortType::Encoded {
                kind: MediaKind::Video,
                width,
                height,
                fps,
                ..
            } => {
                types.insert((i, Port::Out), PortType::RawVideo { width, height, fps });
            }
            PortType::Encoded {
                kind: MediaKind::Audio,
                ..
            } => {
                let ch = match &graph.nodes[ins[0].0] {
                    Node::Source(s) => s.audio.as_ref().and_then(|a| a.channels),
                    _ => None,
                };
                types.insert(
                    (i, Port::Out),
                    PortType::RawAudio {
                        sample_rate: 48000,
                        channels: ch,
                    },
                );
            }
            other => v.push(
                IssueCode::TypeMismatch,
                id,
                format!("decode needs an encoded input, got {}", other.describe()),
            ),
        },
        Node::Board(b) => {
            if let Some((_, _, fps)) = raw_video_in(v, &in_types[0]) {
                let (w, h) = if b.params.corners.is_some() || b.params.auto_detect {
                    (Some(b.params.output_width), Some(b.params.output_height))
                } else {
                    match in_types[0] {
                        PortType::RawVideo { width, height, .. } => (width, height),
                        _ => (None, None),
                    }
                };
                types.insert(
                    (i, Port::Out),
                    PortType::RawVideo {
                        width: w,
                        height: h,
                        fps,
                    },
                );
            }
        }
        Node::Scale(s) => {
            if let Some((_, _, fps)) = raw_video_in(v, &in_types[0]) {
                types.insert(
                    (i, Port::Out),
                    PortType::RawVideo {
                        width: Some(s.width),
                        height: Some(s.height),
                        fps,
                    },
                );
            }
        }
        Node::Crop(c) => {
            if let Some((w, h, fps)) = raw_video_in(v, &in_types[0]) {
                if let (Some(w), Some(h)) = (w, h)
                    && (c.x as u64 + c.width as u64 > w as u64
                        || c.y as u64 + c.height as u64 > h as u64)
                {
                    v.push(
                        IssueCode::InvalidParam,
                        id,
                        format!("crop rectangle exceeds the {w}x{h} input"),
                    );
                }
                types.insert(
                    (i, Port::Out),
                    PortType::RawVideo {
                        width: Some(c.width & !1),
                        height: Some(c.height & !1),
                        fps,
                    },
                );
            }
        }
        Node::Compose(c) => {
            let mut ok = true;
            for t in &in_types {
                ok &= raw_video_in(v, t).is_some();
            }
            if !ok {
                return;
            }
            let fps = Some(c.canvas.fps_num as f64 / c.canvas.fps_den.max(1) as f64);
            let t = PortType::RawVideo {
                width: Some(c.canvas.width),
                height: Some(c.canvas.height),
                fps,
            };
            types.insert((i, Port::Program), t);
            types.insert((i, Port::Preview), t);
        }
        Node::AudioMix(_) => {
            for t in &in_types {
                if !matches!(t, PortType::RawAudio { .. }) {
                    v.push(
                        IssueCode::TypeMismatch,
                        id,
                        format!("audio_mix needs raw audio, got {}", t.describe()),
                    );
                }
            }
            types.insert(
                (i, Port::Out),
                PortType::RawAudio {
                    sample_rate: 48000,
                    channels: Some(2),
                },
            );
        }
        Node::Encode(e) => {
            let Some(codec) = Codec::parse(&e.codec) else {
                return;
            };
            match (in_types[0], codec.kind()) {
                (PortType::RawVideo { width, height, fps }, MediaKind::Video) => {
                    let (w, h) = match (e.width, e.height) {
                        (Some(w), Some(h)) => (Some(w), Some(h)),
                        _ => (width, height),
                    };
                    types.insert(
                        (i, Port::Out),
                        PortType::Encoded {
                            kind: MediaKind::Video,
                            codec,
                            width: w,
                            height: h,
                            fps,
                        },
                    );
                }
                (PortType::RawAudio { .. }, MediaKind::Audio) => {
                    types.insert(
                        (i, Port::Out),
                        PortType::Encoded {
                            kind: MediaKind::Audio,
                            codec,
                            width: None,
                            height: None,
                            fps: None,
                        },
                    );
                }
                (t, k) => v.push(
                    IssueCode::TypeMismatch,
                    id,
                    format!(
                        "cannot encode {} with the {} codec {codec}",
                        t.describe(),
                        kind_name(k)
                    ),
                ),
            }
        }
        Node::SnapshotOnChange(_) => {
            if !in_types[0].is_raw_video() {
                raw_video_in(v, &in_types[0]);
            }
        }
        Node::Sink(_) => {
            let (mut nv, mut na) = (0, 0);
            for t in &in_types {
                match t {
                    PortType::Encoded {
                        kind: MediaKind::Video,
                        ..
                    } => nv += 1,
                    PortType::Encoded {
                        kind: MediaKind::Audio,
                        ..
                    } => na += 1,
                    other => v.push(
                        IssueCode::TypeMismatch,
                        id,
                        format!(
                            "sink needs encoded media, got {} (insert an encode node)",
                            other.describe()
                        ),
                    ),
                }
            }
            if nv > 1 || na > 1 {
                v.push(
                    IssueCode::TypeMismatch,
                    id,
                    "sink takes at most one video and one audio input",
                );
            }
        }
    }
}

/// コーデックが方針・能力で使えるか。使えるなら (ハードウェアか, 提供者)。
pub fn check_codec(
    ctx: &ValidationContext,
    codec: Codec,
    dir: Direction,
    res: Option<(u32, u32)>,
) -> Result<(bool, ProviderId), Issue> {
    let what = format!("{codec} {}", dir.as_str());
    if codec == Codec::Aac {
        return Err(Issue::new(
            IssueCode::UnsupportedCodec,
            None,
            "AAC is passed through only; AAC decode/encode is not provided (ADR-0004); use Opus",
        ));
    }
    if ctx.policy.royalty_free_only && codec.is_patent_pooled() {
        return Err(Issue::new(
            IssueCode::PolicyRejected,
            None,
            format!(
                "{codec} is rejected by the royalty-free-only profile (REQ-007); use AV1/VP9/VP8/Opus"
            ),
        ));
    }
    if !matches!(
        codec,
        Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Vp8 | Codec::Vp9 | Codec::Opus
    ) {
        return Err(Issue::new(
            IssueCode::UnsupportedCodec,
            None,
            format!("{what} is not supported"),
        ));
    }
    let allowed = |c: &&Capability| {
        c.codec == codec
            && c.direction == dir
            && (!codec.is_patent_pooled() || (c.hardware && c.provider.is_hardware()))
            && (ctx.policy.allow_software || c.provider.is_hardware())
    };
    let cands: Vec<&Capability> = ctx.capabilities.iter().filter(allowed).collect();
    if cands.is_empty() {
        return Err(Issue::new(
            IssueCode::CapacityUnavailable,
            None,
            if codec.is_patent_pooled() {
                format!(
                    "no hardware {what} is available (software H.264/H.265 codecs are not used, ADR-0004)"
                )
            } else {
                format!("no provider for {what} is available")
            },
        ));
    }
    let fit: Vec<&&Capability> = cands
        .iter()
        .filter(|c| res.is_none_or(|(w, h)| w <= c.max_width && h <= c.max_height))
        .collect();
    let Some(best) = fit.iter().min_by_key(|c| {
        (
            !c.provider.is_hardware(),
            pref(&ctx.policy, c.provider),
            c.provider,
        )
    }) else {
        let (w, h) = res.unwrap_or_default();
        return Err(Issue::new(
            IssueCode::CapacityUnavailable,
            None,
            format!("no provider for {what} supports {w}x{h}"),
        ));
    };
    Ok((best.provider.is_hardware(), best.provider))
}

fn pref(p: &Policy, id: ProviderId) -> usize {
    p.prefer.iter().position(|x| *x == id).unwrap_or(usize::MAX)
}

fn fps_or_default(f: Option<f64>) -> f64 {
    f.unwrap_or(30.0)
}

fn make_plan(
    v: &mut V<'_>,
    graph: &Graph,
    order: &[usize],
    inputs: &[Vec<(usize, Port)>],
    types: &HashMap<(usize, Port), PortType>,
    consumers: &HashMap<(usize, Port), Vec<(usize, usize)>>,
) -> Plan {
    let opts = &graph.options;
    let live_buffer = if opts.clock == ClockMode::Realtime {
        opts.latency_ms as f64
    } else {
        0.0
    };
    let mut plans: Vec<NodePlan> = Vec::new();
    // 部品の出力までの遅延（経路の最大）
    let mut path: Vec<f64> = vec![0.0; graph.nodes.len()];
    // ハードウェアのセッション（提供者ごと）
    let mut hw_sessions: BTreeMap<ProviderId, u32> = BTreeMap::new();
    let mut totals = Totals::default();
    for &i in order {
        let node = &graph.nodes[i];
        let id = node.id();
        let mut p = NodePlan {
            id: id.to_string(),
            node_type: node.type_name().to_string(),
            mode: NodeMode::Process,
            outputs: Vec::new(),
            codec: None,
            hardware: None,
            provider: None,
            pixels_per_sec: 0,
            cpu_codec_sessions: 0,
            gpu_codec_sessions: 0,
            added_latency_ms: 0.0,
            notes: Vec::new(),
        };
        for port in [
            Port::Video,
            Port::Audio,
            Port::Out,
            Port::Program,
            Port::Preview,
        ] {
            if let Some(t) = types.get(&(i, port))
                && (port != Port::Preview || consumers.contains_key(&(i, port)))
            {
                p.outputs
                    .push(format!("{}: {}", port.as_str(), t.describe()));
            }
        }
        let in_t = inputs[i].first().and_then(|k| types.get(k)).copied();
        let mut codec_use = |v: &mut V<'_>, p: &mut NodePlan, codec: Codec, dir: Direction, res| {
            p.codec = Some(codec.to_string());
            match check_codec(v.ctx, codec, dir, res) {
                Ok((hw, prov)) => {
                    p.hardware = Some(hw);
                    p.provider = Some(prov.as_str().to_string());
                    if hw {
                        p.gpu_codec_sessions += 1;
                        *hw_sessions.entry(prov).or_default() += 1;
                    } else {
                        p.cpu_codec_sessions += 1;
                    }
                }
                Err(mut e) => {
                    e.node = Some(p.id.clone());
                    v.issues.push(e);
                }
            }
        };
        match node {
            Node::Source(s) => {
                // 素通し（sink に直接つながる）のトラックの方針
                p.mode = NodeMode::Passthrough;
                for (port, t) in [(Port::Video, &s.video), (Port::Audio, &s.audio)] {
                    let Some(t) = t else { continue };
                    let direct = consumers.get(&(i, port)).is_some_and(|cs| {
                        cs.iter()
                            .any(|(c, _)| matches!(graph.nodes[*c], Node::Sink(_)))
                    });
                    if direct
                        && let Some(c) = Codec::parse(&t.codec)
                        && v.ctx.policy.royalty_free_only
                        && c.is_patent_pooled()
                    {
                        v.push(
                            IssueCode::PolicyRejected,
                            id,
                            format!(
                                "passing {c} through is rejected by the royalty-free-only profile (REQ-007)"
                            ),
                        );
                    }
                }
            }
            Node::Decode(d) => {
                p.mode = NodeMode::Transcode;
                if let Some(PortType::Encoded {
                    kind,
                    codec,
                    width,
                    height,
                    fps,
                }) = in_t
                {
                    let res = match (width, height) {
                        (Some(w), Some(h)) => Some((w, h)),
                        _ => None,
                    };
                    if let (Some((w, h)), Some(mw), Some(mh)) = (res, d.max_width, d.max_height)
                        && (w > mw || h > mh)
                    {
                        v.push(
                            IssueCode::LimitExceeded,
                            id,
                            format!("source {w}x{h} exceeds max {mw}x{mh}"),
                        );
                    }
                    codec_use(v, &mut p, codec, Direction::Decode, res);
                    if kind == MediaKind::Video {
                        let fps_v = fps_or_default(fps);
                        let (w, h) = res.unwrap_or((1280, 720));
                        if res.is_none() || fps.is_none() {
                            p.notes.push(
                                "source size/fps not declared; estimate assumes 1280x720@30".into(),
                            );
                        }
                        p.pixels_per_sec = (w as f64 * h as f64 * fps_v) as u64;
                        p.added_latency_ms = 1000.0 / fps_v;
                    } else {
                        p.added_latency_ms = 20.0;
                    }
                }
            }
            Node::Board(b) => {
                if let Some(PortType::RawVideo { fps, .. }) = in_t {
                    let f = fps_or_default(fps);
                    p.pixels_per_sec =
                        (b.params.output_width as f64 * b.params.output_height as f64 * f) as u64;
                    p.added_latency_ms = 1000.0 / f;
                    if b.params.auto_detect && b.params.corners.is_none() {
                        p.notes
                            .push("corners are detected automatically (best effort)".into());
                    }
                }
            }
            Node::Scale(s) => {
                if let Some(PortType::RawVideo { fps, .. }) = in_t {
                    p.pixels_per_sec =
                        (s.width as f64 * s.height as f64 * fps_or_default(fps)) as u64;
                }
            }
            Node::Crop(c) => {
                if let Some(PortType::RawVideo { fps, .. }) = in_t {
                    p.pixels_per_sec =
                        (c.width as f64 * c.height as f64 * fps_or_default(fps)) as u64;
                }
            }
            Node::Compose(c) => {
                let fps = c.canvas.fps_num as f64 / c.canvas.fps_den.max(1) as f64;
                let layers = c
                    .scenes
                    .get(&c.program)
                    .map_or(1, |s| s.layers.len().max(1));
                let renders = if consumers.contains_key(&(i, Port::Preview)) {
                    2.0
                } else {
                    1.0
                };
                p.pixels_per_sec = (c.canvas.width as f64
                    * c.canvas.height as f64
                    * fps
                    * (1.0 + layers as f64)
                    * renders) as u64;
                p.added_latency_ms = live_buffer + 1000.0 / fps;
                totals.threads += if c.threads == 0 { 4 } else { c.threads };
            }
            Node::AudioMix(_) => {
                p.added_latency_ms = live_buffer + 20.0 + 1.5;
                totals.threads += 1;
            }
            Node::Encode(e) => {
                p.mode = NodeMode::Transcode;
                if let (Some(codec), Some(t)) = (Codec::parse(&e.codec), types.get(&(i, Port::Out)))
                {
                    if let PortType::Encoded {
                        kind: MediaKind::Video,
                        width,
                        height,
                        fps,
                        ..
                    } = *t
                    {
                        let res = match (width, height) {
                            (Some(w), Some(h)) => Some((w, h)),
                            _ => None,
                        };
                        codec_use(v, &mut p, codec, Direction::Encode, res);
                        let f = fps_or_default(fps);
                        let (w, h) = res.unwrap_or((1280, 720));
                        p.pixels_per_sec = (w as f64 * h as f64 * f) as u64;
                        p.added_latency_ms = if e.low_latency {
                            1000.0 / f
                        } else {
                            p.notes
                                .push("non-low-latency encoding adds look-ahead".into());
                            1000.0 / f * 16.0
                        };
                    } else {
                        codec_use(v, &mut p, codec, Direction::Encode, None);
                        p.added_latency_ms = e.frame_ms.unwrap_or(20.0) as f64 + 6.5;
                    }
                }
                totals.threads += 1;
            }
            Node::SnapshotOnChange(_) => {
                p.mode = NodeMode::Terminal;
            }
            Node::Sink(_) => {
                p.mode = NodeMode::Terminal;
            }
        }
        if matches!(node, Node::Decode(_)) {
            totals.threads += 1;
        }
        let up = inputs[i].iter().map(|(j, _)| path[*j]).fold(0.0, f64::max);
        path[i] = up + p.added_latency_ms;
        totals.pixels_per_sec = totals.pixels_per_sec.saturating_add(p.pixels_per_sec);
        totals.cpu_codec_sessions += p.cpu_codec_sessions;
        totals.gpu_codec_sessions += p.gpu_codec_sessions;
        plans.push(p);
    }
    // ハードウェアの同時セッション数
    for (prov, n) in &hw_sessions {
        let max = v
            .ctx
            .capabilities
            .iter()
            .filter(|c| c.provider == *prov)
            .filter_map(|c| c.max_sessions)
            .min();
        if let Some(m) = max
            && *n > m
        {
            v.issues.push(Issue::new(
                IssueCode::CapacityUnavailable,
                None,
                format!(
                    "pipeline needs {n} {} sessions but the device allows {m}",
                    prov.as_str()
                ),
            ));
        }
    }
    let mut sinks = Vec::new();
    for &i in order {
        let Node::Sink(s) = &graph.nodes[i] else {
            continue;
        };
        let mut tracks = Vec::new();
        for (k, &(j, port)) in inputs[i].iter().enumerate() {
            let Some(PortType::Encoded { kind, codec, .. }) = types.get(&(j, port)) else {
                continue;
            };
            let src_is_source = matches!(graph.nodes[j], Node::Source(_));
            tracks.push(SinkTrackPlan {
                track: k as u32,
                kind: kind_name(*kind).to_string(),
                codec: codec.to_string(),
                mode: if src_is_source {
                    NodeMode::Passthrough
                } else {
                    NodeMode::Transcode
                },
                latency_ms: (path[j] * 10.0).round() / 10.0,
                from: graph.nodes[j].id().to_string(),
            });
            totals.max_latency_ms = totals.max_latency_ms.max(path[j]);
        }
        sinks.push(SinkPlan {
            id: s.id.clone(),
            stream: s.stream.clone(),
            tracks,
        });
    }
    totals.max_latency_ms = (totals.max_latency_ms * 10.0).round() / 10.0;
    for p in &mut plans {
        p.added_latency_ms = (p.added_latency_ms * 10.0).round() / 10.0;
    }
    Plan {
        order: order
            .iter()
            .map(|&i| graph.nodes[i].id().to_string())
            .collect(),
        nodes: plans,
        sinks,
        totals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_codec::PixelFormat;

    fn cap(provider: ProviderId, codec: Codec, dir: Direction, hw: bool) -> Capability {
        Capability {
            provider,
            codec,
            direction: dir,
            hardware: hw,
            max_width: 4096,
            max_height: 2304,
            max_sessions: if hw { Some(2) } else { None },
            formats: vec![PixelFormat::I420],
            device_index: 0,
            device_name: provider.as_str().into(),
            driver_version: None,
        }
    }

    /// GPU の無い開発機と同じ能力。
    fn software() -> ValidationContext {
        use Direction::*;
        ValidationContext::new(
            Policy::default(),
            vec![
                cap(ProviderId::Dav1d, Codec::Av1, Decode, false),
                cap(ProviderId::SvtAv1, Codec::Av1, Encode, false),
                cap(ProviderId::Libvpx, Codec::Vp8, Decode, false),
                cap(ProviderId::Libvpx, Codec::Vp8, Encode, false),
                cap(ProviderId::Libvpx, Codec::Vp9, Decode, false),
                cap(ProviderId::Libvpx, Codec::Vp9, Encode, false),
                cap(ProviderId::Opus, Codec::Opus, Decode, false),
                cap(ProviderId::Opus, Codec::Opus, Encode, false),
            ],
        )
    }

    fn with_nvidia() -> ValidationContext {
        let mut c = software();
        c.capabilities.push(cap(
            ProviderId::Nvidia,
            Codec::H264,
            Direction::Encode,
            true,
        ));
        c.capabilities.push(cap(
            ProviderId::Nvidia,
            Codec::H264,
            Direction::Decode,
            true,
        ));
        c
    }

    fn g(nodes: serde_json::Value) -> Graph {
        Graph::from_json(&serde_json::json!({"version": 1, "nodes": nodes}).to_string()).unwrap()
    }

    fn codes(e: PipelineError) -> Vec<(IssueCode, Option<String>)> {
        e.issues()
            .iter()
            .map(|i| (i.code, i.node.clone()))
            .collect()
    }

    fn transcode(src_codec: &str, enc_codec: &str) -> Graph {
        g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":src_codec,"width":1280,"height":720,"fps":30}},
            {"type":"decode","id":"dec","input":"cam.video"},
            {"type":"encode","id":"enc","input":"dec","codec":enc_codec,"bitrate_kbps":2000},
            {"type":"sink","id":"out","inputs":["enc"],"stream":"out"}
        ]))
    }

    #[test]
    fn valid_transcode_plan() {
        let p = validate(&transcode("vp9", "av1"), &software()).unwrap();
        assert_eq!(p.order, vec!["cam", "dec", "enc", "out"]);
        let dec = &p.nodes[1];
        assert_eq!(dec.mode, NodeMode::Transcode);
        assert_eq!(dec.provider.as_deref(), Some("libvpx"));
        assert_eq!(dec.hardware, Some(false));
        assert_eq!(dec.pixels_per_sec, 1280 * 720 * 30);
        assert_eq!(p.nodes[2].provider.as_deref(), Some("svt-av1"));
        assert_eq!(p.totals.cpu_codec_sessions, 2);
        assert_eq!(p.totals.gpu_codec_sessions, 0);
        assert_eq!(p.sinks[0].tracks[0].codec, "av1");
        assert_eq!(p.sinks[0].tracks[0].mode, NodeMode::Transcode);
        // 復号 1 フレーム + 符号化 1 フレーム
        assert!((p.sinks[0].tracks[0].latency_ms - 66.7).abs() < 0.2);
        // JSON にできる
        let js = serde_json::to_string(&p).unwrap();
        assert!(js.contains("\"mode\":\"transcode\""));
    }

    #[test]
    fn cycle_is_rejected() {
        let graph = g(serde_json::json!([
            {"type":"scale","id":"a","input":"b","width":64,"height":64},
            {"type":"scale","id":"b","input":"a","width":64,"height":64},
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9"}},
            {"type":"sink","id":"out","inputs":["cam.video"],"stream":"o"}
        ]));
        let e = validate(&graph, &software()).unwrap_err();
        let issues = e.issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, IssueCode::Cycle);
        assert!(issues[0].message.contains("a, b"), "{}", issues[0].message);
    }

    #[test]
    fn type_errors() {
        // 符号化済みを合成へ、生を sink へ、映像を Opus で
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9"},"audio":{"codec":"opus"}},
            {"type":"compose","id":"mix","inputs":{"a":"cam.video"},
             "canvas":{"width":320,"height":240,"fps_num":30},
             "scenes":{"s":{"layers":[{"source":"a","x":0,"y":0,"width":320,"height":240}]}},
             "program":"s"},
            {"type":"encode","id":"enc","input":"mix","codec":"opus","bitrate_kbps":64},
            {"type":"decode","id":"adec","input":"cam.audio"},
            {"type":"sink","id":"out","inputs":["adec"],"stream":"o"}
        ]));
        let e = validate(&graph, &software()).unwrap_err();
        let c = codes(e);
        assert!(
            c.contains(&(IssueCode::TypeMismatch, Some("mix".into()))),
            "{c:?}"
        );
        assert!(
            c.contains(&(IssueCode::TypeMismatch, Some("out".into()))),
            "{c:?}"
        );
        // mix の誤りで enc の入力の型は決まらないので、enc は重ねて報告しない
        assert!(
            !c.contains(&(IssueCode::TypeMismatch, Some("enc".into()))),
            "{c:?}"
        );

        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9"}},
            {"type":"decode","id":"dec","input":"cam.video"},
            {"type":"encode","id":"enc","input":"dec","codec":"opus","bitrate_kbps":64},
            {"type":"sink","id":"out","inputs":["enc"],"stream":"o"}
        ]));
        let c = codes(validate(&graph, &software()).unwrap_err());
        assert_eq!(c, vec![(IssueCode::TypeMismatch, Some("enc".into()))]);
    }

    #[test]
    fn references_and_unconsumed_outputs() {
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9"},"audio":{"codec":"opus"}},
            {"type":"decode","id":"dec","input":"cam.video"},
            {"type":"sink","id":"out","inputs":["cam.video"],"stream":"o"}
        ]));
        let c = codes(validate(&graph, &software()).unwrap_err());
        assert!(c.contains(&(IssueCode::UnconsumedOutput, Some("dec".into()))));
        assert!(c.contains(&(IssueCode::UnconsumedOutput, Some("cam".into())))); // audio
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9"}},
            {"type":"sink","id":"out","inputs":["cam.audio", "nope", "cam"],"stream":"o"}
        ]));
        let c = codes(validate(&graph, &software()).unwrap_err());
        assert_eq!(
            c.iter()
                .filter(|x| x.0 == IssueCode::UnknownReference)
                .count(),
            3
        );
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9"}},
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9"}},
            {"type":"sink","id":"bad id!","inputs":["cam.video"],"stream":"o"}
        ]));
        let c = codes(validate(&graph, &software()).unwrap_err());
        assert!(c.contains(&(IssueCode::DuplicateId, Some("cam".into()))));
        assert!(c.contains(&(IssueCode::InvalidParam, Some("bad id!".into()))));
    }

    #[test]
    fn policy_rejections() {
        let mut ctx = with_nvidia();
        ctx.policy = Policy::royalty_free();
        let e = validate(&transcode("h264", "vp9"), &ctx).unwrap_err();
        assert_eq!(e.issues()[0].code, IssueCode::PolicyRejected);
        assert_eq!(e.issues()[0].node.as_deref(), Some("dec"));
        assert_eq!(e.api_code(), "INVALID_CONFIG");
        let e = validate(&transcode("vp9", "h265"), &ctx).unwrap_err();
        assert_eq!(e.issues()[0].code, IssueCode::PolicyRejected);
        // AAC の素通しもロイヤリティフリー限定では拒否
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","audio":{"codec":"aac"}},
            {"type":"sink","id":"out","inputs":["cam.audio"],"stream":"o"}
        ]));
        let e = validate(&graph, &ctx).unwrap_err();
        assert_eq!(e.issues()[0].code, IssueCode::PolicyRejected);
        // 方針が無ければ AAC の素通しは良い
        let p = validate(&graph, &software()).unwrap();
        assert_eq!(p.sinks[0].tracks[0].mode, NodeMode::Passthrough);
        // AAC の復号は常に未対応
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","audio":{"codec":"aac"}},
            {"type":"decode","id":"dec","input":"cam.audio"},
            {"type":"encode","id":"enc","input":"dec","codec":"opus","bitrate_kbps":64},
            {"type":"sink","id":"out","inputs":["enc"],"stream":"o"}
        ]));
        let e = validate(&graph, &software()).unwrap_err();
        assert_eq!(e.issues()[0].code, IssueCode::UnsupportedCodec);
        assert_eq!(e.api_code(), "UNSUPPORTED_CODEC");
    }

    #[test]
    fn capability_checks() {
        // GPU の無い機械: H.264 の復号・符号化は CAPACITY_UNAVAILABLE（ソフトウェアには落とさない）
        let e = validate(&transcode("h264", "vp9"), &software()).unwrap_err();
        assert_eq!(e.issues()[0].code, IssueCode::CapacityUnavailable);
        assert!(e.issues()[0].message.contains("ADR-0004"));
        assert_eq!(e.api_code(), "CAPACITY_UNAVAILABLE");
        assert_eq!(e.http_status(), 503);
        let e = validate(&transcode("vp9", "h264"), &software()).unwrap_err();
        assert_eq!(e.issues()[0].node.as_deref(), Some("enc"));
        // NVIDIA があれば通り、GPU のセッションとして数える
        let p = validate(&transcode("h264", "h264"), &with_nvidia()).unwrap();
        assert_eq!(p.totals.gpu_codec_sessions, 2);
        assert_eq!(p.nodes[1].provider.as_deref(), Some("nvidia"));
        // ソフトウェアの能力として H.264 があっても数えない
        let mut ctx = software();
        ctx.capabilities.push(cap(
            ProviderId::Libvpx,
            Codec::H264,
            Direction::Encode,
            false,
        ));
        assert!(validate(&transcode("vp9", "h264"), &ctx).is_err());
        // セッション数の上限（max_sessions = 2）
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":"h264"}},
            {"type":"decode","id":"dec","input":"cam.video"},
            {"type":"encode","id":"e1","input":"dec","codec":"h264","bitrate_kbps":2000},
            {"type":"encode","id":"e2","input":"dec","codec":"h264","bitrate_kbps":1000},
            {"type":"sink","id":"o1","inputs":["e1"],"stream":"o1"},
            {"type":"sink","id":"o2","inputs":["e2"],"stream":"o2"}
        ]));
        let e = validate(&graph, &with_nvidia()).unwrap_err();
        assert_eq!(e.issues()[0].code, IssueCode::CapacityUnavailable);
        assert!(e.issues()[0].message.contains("3 nvidia sessions"));
        // 解像度が能力を超える
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9","width":7680,"height":4320}},
            {"type":"decode","id":"dec","input":"cam.video","max_width":7680,"max_height":4320},
            {"type":"encode","id":"enc","input":"dec","codec":"vp9","bitrate_kbps":2000},
            {"type":"sink","id":"out","inputs":["enc"],"stream":"o"}
        ]));
        let e = validate(&graph, &software()).unwrap_err();
        assert_eq!(e.issues()[0].code, IssueCode::CapacityUnavailable);
        // ソフトウェア不可の方針
        let mut ctx = software();
        ctx.policy.allow_software = false;
        assert!(validate(&transcode("vp9", "vp9"), &ctx).is_err());
    }

    #[test]
    fn param_checks() {
        let graph = g(serde_json::json!([
            {"type":"source","id":"cam","stream":"in","video":{"codec":"vp9"}},
            {"type":"decode","id":"dec","input":"cam.video"},
            {"type":"compose","id":"mix","inputs":{"a":"dec"},
             "canvas":{"width":321,"height":240,"fps_num":30,"background":"red"},
             "scenes":{"s":{"layers":[{"source":"zzz","x":0,"y":0,"width":320,"height":240,"opacity":2.0}]}},
             "program":"nope"},
            {"type":"encode","id":"enc","input":"mix","codec":"vp9","bitrate_kbps":0,"frame_ms":7},
            {"type":"sink","id":"out","inputs":["enc"],"stream":"o"}
        ]));
        let e = validate(&graph, &software()).unwrap_err();
        let msgs: Vec<String> = e.issues().iter().map(|i| i.to_string()).collect();
        let all = msgs.join("\n");
        assert!(all.contains("canvas size"), "{all}");
        assert!(all.contains("background"), "{all}");
        assert!(all.contains("zzz"), "{all}");
        assert!(all.contains("program scene"), "{all}");
        assert!(all.contains("bitrate_kbps"), "{all}");
        assert!(all.contains("frame_ms"), "{all}");
    }
}
