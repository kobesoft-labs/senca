//! グリッドの型（ノード・仕事・割り当て・決定）。すべて serde で直列化できる。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub type NodeId = String;
pub type WorkloadId = String;

/// ノードの役割。`control` は Raft の投票者、`media` は仕事を動かす。仕事は `media` にだけ置く。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Control,
    Media,
}

/// 数えられる資源（容量・要求・使用量に共通）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default)]
    pub cpu_millis: u64,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub net_in_mbps: u64,
    #[serde(default)]
    pub net_out_mbps: u64,
}

impl Resources {
    pub fn plus(&self, o: &Resources) -> Resources {
        Resources {
            cpu_millis: self.cpu_millis.saturating_add(o.cpu_millis),
            memory_mb: self.memory_mb.saturating_add(o.memory_mb),
            net_in_mbps: self.net_in_mbps.saturating_add(o.net_in_mbps),
            net_out_mbps: self.net_out_mbps.saturating_add(o.net_out_mbps),
        }
    }

    pub fn minus(&self, o: &Resources) -> Resources {
        Resources {
            cpu_millis: self.cpu_millis.saturating_sub(o.cpu_millis),
            memory_mb: self.memory_mb.saturating_sub(o.memory_mb),
            net_in_mbps: self.net_in_mbps.saturating_sub(o.net_in_mbps),
            net_out_mbps: self.net_out_mbps.saturating_sub(o.net_out_mbps),
        }
    }

    /// すべての次元で `cap` 以下か。
    pub fn within(&self, cap: &Resources) -> bool {
        self.cpu_millis <= cap.cpu_millis
            && self.memory_mb <= cap.memory_mb
            && self.net_in_mbps <= cap.net_in_mbps
            && self.net_out_mbps <= cap.net_out_mbps
    }
}

fn yes() -> bool {
    true
}

/// GPU などの符号化・復号の装置。セッション数とメモリで数える。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub id: String,
    /// `nvidia`・`intel`・`amd`・`apple` など（比べるときは大文字・小文字を区別しない）。
    pub vendor: String,
    pub encode_sessions: u32,
    pub decode_sessions: u32,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default = "yes")]
    pub healthy: bool,
}

/// キャプチャ機器（SDI・HDMI の入力など）。1 つの仕事が専有する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureDevice {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default = "yes")]
    pub healthy: bool,
}

/// ノードが登録で申告する内容（`RegisterNode`）。容量は仕事に貸せる分（OS などの取り分を引いた値）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: NodeId,
    #[serde(default)]
    pub roles: BTreeSet<Role>,
    #[serde(default)]
    pub capacity: Resources,
    #[serde(default)]
    pub gpus: Vec<Gpu>,
    #[serde(default)]
    pub capture: Vec<CaptureDevice>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// 新しい仕事を受けない（今の仕事は移せるものから移す）。
    #[serde(default)]
    pub draining: bool,
}

impl NodeSpec {
    pub fn gpu_index(&self, id: &str) -> Option<usize> {
        self.gpus.iter().position(|g| g.id == id)
    }

    pub fn capture_index(&self, id: &str) -> Option<usize> {
        self.capture.iter().position(|c| c.id == id)
    }
}

/// 合意ログで決まったノードの健康（心拍の途絶・復帰で変わる）。観測した使用量は心拍の側
/// （[`crate::Heartbeat`]）にあり、配置は予約（割り当ての要求の和）で数える。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub up: bool,
    /// `up` が最後に変わった時刻。
    #[serde(default)]
    pub changed_ms: i64,
    /// 最後に状態が変わったときにリーダーが見ていた最後の心拍の時刻。
    #[serde(default)]
    pub last_heartbeat_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub spec: NodeSpec,
    pub status: NodeStatus,
}

impl Node {
    /// 新しい仕事を置ける（生きている・media・排出中でない）。
    pub fn accepts_new(&self) -> bool {
        self.status.up && !self.spec.draining && self.spec.roles.contains(&Role::Media)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    Ingest,
    Room,
    Pipeline,
    Recording,
    Transcode,
    Capture,
    Relay,
    MediaPipeline,
}

/// 1 つの GPU 装置の上に要るもの（セッションはすべて同じ装置に置く）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GpuReq {
    #[serde(default)]
    pub encode: u32,
    #[serde(default)]
    pub decode: u32,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub vendor: Option<String>,
}

/// 1 つの段で要る資源。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Requirements {
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub gpu: Option<GpuReq>,
    /// 専有するキャプチャ機器の ID（そのノードにある機器でなければならない）。
    #[serde(default)]
    pub capture: Option<String>,
}

/// 縮退の段（例: `no_translation`・`720p`・`audio_only`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Level {
    pub label: String,
    pub requirements: Requirements,
}

/// ラベルの条件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LabelSelector {
    Eq { key: String, value: String },
    In { key: String, values: Vec<String> },
    /// キーが無いノードも合う。
    NotIn { key: String, values: Vec<String> },
}

impl LabelSelector {
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        match self {
            LabelSelector::Eq { key, value } => labels.get(key) == Some(value),
            LabelSelector::In { key, values } => labels.get(key).is_some_and(|v| values.contains(v)),
            LabelSelector::NotIn { key, values } => labels.get(key).is_none_or(|v| !values.contains(v)),
        }
    }
}

/// 手動の割り当て（pin）の対象が止まったときの扱い。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinPolicy {
    /// 止めたまま（他へ移さない）。
    Strict,
    /// 他へ移す。対象が戻ったら（冷却の後に）戻す。
    #[default]
    Prefer,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    /// 手動でノードを指定する。
    #[serde(default)]
    pub node: Option<NodeId>,
    /// 手動で GPU を指定する（`node` が無ければ、その ID の GPU を持つノードのどれか）。
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub labels: Vec<LabelSelector>,
    /// 同じ組の仕事は同じノードに置かない。
    #[serde(default)]
    pub anti_affinity: Option<String>,
    #[serde(default)]
    pub pin_policy: PinPolicy,
}

/// 優先度（順序は low < normal < high < critical）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

impl Priority {
    pub const ALL_DESC: [Priority; 4] = [Priority::Critical, Priority::High, Priority::Normal, Priority::Low];

    pub(crate) fn rank(self) -> usize {
        self as usize
    }
}

/// 置き方。spread は余裕の多いノードへ、binpack は詰める。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Spread,
    Binpack,
}

/// グリッドで配置する仕事。段 0 は `requirements`（full）、段 i（1 以上）は `degrade[i-1]`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workload {
    pub id: WorkloadId,
    pub tenant: String,
    pub kind: WorkloadKind,
    pub requirements: Requirements,
    /// 縮退の段（よい順）。
    #[serde(default)]
    pub degrade: Vec<Level>,
    #[serde(default)]
    pub constraints: Constraints,
    #[serde(default)]
    pub priority: Priority,
    /// データのある所を優先する（この stream を持つノードを好む。R10-9）。
    #[serde(default)]
    pub locality: Vec<String>,
    #[serde(default)]
    pub placement: Placement,
}

impl Workload {
    /// 段の数（full を含む）。
    pub fn levels(&self) -> usize {
        1 + self.degrade.len()
    }

    pub fn req(&self, level: usize) -> &Requirements {
        if level == 0 { &self.requirements } else { &self.degrade[level - 1].requirements }
    }

    pub fn level_label(&self, level: usize) -> &str {
        if level == 0 { "full" } else { self.degrade.get(level - 1).map_or("?", |l| l.label.as_str()) }
    }

    pub fn is_pinned(&self) -> bool {
        self.constraints.node.is_some() || self.constraints.device.is_some()
    }
}

/// 決定・割り当ての理由（API とイベントで見せる）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Reason {
    /// 普通に置いた。
    Placed,
    /// 資源が足りず、縮退した段で置いた・縮退した。
    NoCapacity,
    /// ノードが止まった・消えた。
    NodeDown { node: NodeId },
    /// 装置が故障した。
    DeviceDown { device: String },
    /// ノードの排出で移した。
    Draining,
    /// 仕事の条件・ノードのラベルが変わって合わなくなった。
    ConstraintChanged,
    /// 高い優先度の仕事のために縮退・追い出しされた。
    Preempted { by: WorkloadId },
    /// ノードの容量が減って収まらなくなった。
    Overcommitted,
    /// 資源が戻ったので元の段へ戻した。
    CapacityReturned,
    /// 手動の割り当ての対象が戻ったので戻した。
    PinReturned,
    /// 手動の割り当て（strict）の対象が止まっている。
    PinTargetDown,
}

/// 割り当て（合意ログに載る）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub workload: WorkloadId,
    pub node: NodeId,
    /// 使う GPU の ID。
    #[serde(default)]
    pub device: Option<String>,
    /// 今の縮退の段（0 = full）。
    pub level: usize,
    /// fencing token。Assign・Move のたびに新しくなる。
    pub token: u64,
    /// このノードに置いた時刻。
    pub since_ms: i64,
    /// 段が最後に変わった時刻（戻すときの冷却に使う）。
    pub level_since_ms: i64,
    pub reason: Reason,
}

impl Assignment {
    pub fn degraded(&self) -> bool {
        self.level > 0
    }
}

/// ノードが仕事を受けられない理由（置けない仕事の報告）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reject {
    NodeDown,
    NotMedia,
    Draining,
    PinnedElsewhere,
    PinTargetDown,
    Labels,
    AntiAffinity,
    NoDevice,
    Capture,
    Cpu,
    Memory,
    NetIn,
    NetOut,
    GpuEncode,
    GpuDecode,
    GpuMemory,
    /// 単独なら収まるが、優先度の同じか高い仕事で埋まっている。
    Contended,
}

/// 理由ごとのノードの数と例（最大 [`RejectCount::MAX_EXAMPLES`] 件）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectCount {
    pub reason: Reject,
    pub nodes: usize,
    pub examples: Vec<NodeId>,
}

impl RejectCount {
    pub const MAX_EXAMPLES: usize = 8;
}

/// 配置係の決定。`ApplyDecisions` で合意ログに載せて適用する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decision {
    /// 割り当ての無い仕事を置く（新しい token）。
    Assign {
        workload: WorkloadId,
        node: NodeId,
        device: Option<String>,
        level: usize,
        token: u64,
        reason: Reason,
    },
    /// 別のノード・装置へ移す（新しい token。古いノードは止める）。
    Move {
        workload: WorkloadId,
        from: NodeId,
        node: NodeId,
        device: Option<String>,
        level: usize,
        token: u64,
        reason: Reason,
    },
    /// 同じノードで段を下げる（token はそのまま）。
    Degrade { workload: WorkloadId, level: usize, device: Option<String>, reason: Reason },
    /// 同じノードで段を上げる（token はそのまま）。
    Restore { workload: WorkloadId, level: usize, device: Option<String>, reason: Reason },
    /// 割り当てを外す。
    Evict { workload: WorkloadId, reason: Reason },
    /// 置けない（理由ごとのノード）。状態の `unschedulable` に記録する。
    Unschedulable { workload: WorkloadId, reasons: Vec<RejectCount> },
}

impl Decision {
    pub fn workload(&self) -> &str {
        match self {
            Decision::Assign { workload, .. }
            | Decision::Move { workload, .. }
            | Decision::Degrade { workload, .. }
            | Decision::Restore { workload, .. }
            | Decision::Evict { workload, .. }
            | Decision::Unschedulable { workload, .. } => workload,
        }
    }

    /// 新しい token を持つか（Assign・Move）。
    pub fn token(&self) -> Option<u64> {
        match self {
            Decision::Assign { token, .. } | Decision::Move { token, .. } => Some(*token),
            _ => None,
        }
    }

    pub(crate) fn rank(&self) -> u8 {
        match self {
            Decision::Evict { .. } => 0,
            Decision::Degrade { .. } => 1,
            Decision::Move { .. } => 2,
            Decision::Assign { .. } => 3,
            Decision::Restore { .. } => 4,
            Decision::Unschedulable { .. } => 5,
        }
    }
}
