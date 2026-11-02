//! 故障の検出（ADR-0013 決定 2）。リーダーの手元だけで動き、心拍は合意ログに載せない。
//!
//! - 各ノードは 1 秒ごとに [`Heartbeat`] をリーダーへ送る。リーダーは [`FailureDetector::heartbeat`] に渡す。
//! - リーダーは周期的に [`FailureDetector::tick`] を呼び、返ったコマンドを合意ログに提案する。
//!   コマンドは合意済みの状態との差だけ（同じ状態なら何も出さない）。
//! - リーダーが替わったら作り直す。作った時刻から `timeout_ms` の間は、心拍をまだ受けていないノードも
//!   生きているとみなす（就任直後にすべてを止めない）。

use crate::model::*;
use crate::state::{GridCommand, GridState, Running};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHealth {
    pub id: String,
    pub healthy: bool,
}

/// ノードからの心拍。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub node: NodeId,
    /// 観測した使用量（表示・将来の判断用。配置は予約で数える）。
    #[serde(default)]
    pub usage: Resources,
    /// 装置（GPU・キャプチャ機器）の健康。載っていない装置は変えない。
    #[serde(default)]
    pub devices: Vec<DeviceHealth>,
    /// このノードが持つ stream（データのある所の判断に使う）。
    #[serde(default)]
    pub streams: Vec<String>,
    /// 動いている仕事。
    #[serde(default)]
    pub running: Vec<Running>,
}

#[derive(Debug, Clone)]
struct Seen {
    at_ms: i64,
    hb: Heartbeat,
}

#[derive(Debug, Clone)]
pub struct FailureDetector {
    pub timeout_ms: i64,
    started_ms: i64,
    seen: BTreeMap<NodeId, Seen>,
}

impl FailureDetector {
    pub const DEFAULT_TIMEOUT_MS: i64 = 5_000;

    pub fn new(timeout_ms: i64, now_ms: i64) -> Self {
        FailureDetector { timeout_ms, started_ms: now_ms, seen: BTreeMap::new() }
    }

    pub fn heartbeat(&mut self, hb: Heartbeat, now_ms: i64) {
        self.seen.insert(hb.node.clone(), Seen { at_ms: now_ms, hb });
    }

    /// 最後の心拍とその時刻。
    pub fn last(&self, node: &str) -> Option<(i64, &Heartbeat)> {
        self.seen.get(node).map(|s| (s.at_ms, &s.hb))
    }

    /// 合意済みの状態と心拍を比べ、提案するコマンドを返す（ノードの ID 順、決定的）。
    pub fn tick(&self, state: &GridState, now_ms: i64) -> Vec<GridCommand> {
        let mut out = Vec::new();
        for (id, node) in &state.nodes {
            let seen = self.seen.get(id);
            let last = seen.map_or(self.started_ms, |s| s.at_ms);
            let alive = now_ms - last <= self.timeout_ms;
            if node.status.up && !alive {
                out.push(GridCommand::NodeDown { node: id.clone(), now_ms, last_heartbeat_ms: last });
                continue;
            }
            let Some(s) = seen.filter(|_| alive) else { continue };
            if !node.status.up {
                out.push(GridCommand::NodeUp { node: id.clone(), now_ms });
            }
            for d in &s.hb.devices {
                let cur = node
                    .spec
                    .gpus
                    .iter()
                    .find(|g| g.id == d.id)
                    .map(|g| g.healthy)
                    .or_else(|| node.spec.capture.iter().find(|c| c.id == d.id).map(|c| c.healthy));
                match cur {
                    Some(true) if !d.healthy => out.push(GridCommand::DeviceDown { node: id.clone(), device: d.id.clone() }),
                    Some(false) if d.healthy => out.push(GridCommand::DeviceUp { node: id.clone(), device: d.id.clone() }),
                    _ => {}
                }
            }
        }
        out
    }

    /// stream → それを持つノード（心拍が新しいノードだけ）。[`crate::PlanInput::stream_nodes`] に渡す。
    pub fn stream_nodes_at(&self, now_ms: i64) -> BTreeMap<String, BTreeSet<NodeId>> {
        let mut m: BTreeMap<String, BTreeSet<NodeId>> = BTreeMap::new();
        for (id, s) in &self.seen {
            if now_ms - s.at_ms > self.timeout_ms {
                continue;
            }
            for st in &s.hb.streams {
                m.entry(st.clone()).or_default().insert(id.clone());
            }
        }
        m
    }
}
