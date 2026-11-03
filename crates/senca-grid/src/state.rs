//! 合意ログで複製するグリッドの状態とコマンド。
//!
//! 規則（senca-control の `ControlState` と同じ）:
//! - 状態を変えるのは [`GridState::apply`] だけ。時刻・token はコマンドに入れて渡す（決定的）。
//! - `ApplyDecisions` は適用の時点の状態で決定をすべて検査する（提案の後にノードが止まった等）。
//!   一つでも合わなければ全体を拒み、状態を変えない（リーダーは次の周期で計画し直す）。
//! - 失敗したコマンドは状態を変えない。

use crate::fit::{Usage, dev_pin, gpu_ok, pin_state, static_check};
use crate::model::*;
use crate::plan::{Plan, PlanConfig, PlanInput, plan};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridState {
    #[serde(default)]
    pub nodes: BTreeMap<NodeId, Node>,
    #[serde(default)]
    pub workloads: BTreeMap<WorkloadId, Workload>,
    #[serde(default)]
    pub assignments: BTreeMap<WorkloadId, Assignment>,
    /// 置けない仕事と理由（API・イベントで見せる）。
    #[serde(default)]
    pub unschedulable: BTreeMap<WorkloadId, Vec<RejectCount>>,
    /// fencing token の通し番号（[`GridState::apply`] で使う。制御状態の `fencing_counter` を共有するなら
    /// [`GridState::apply_with_counter`] を使い、これは使わない）。
    #[serde(default)]
    pub fencing_counter: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GridCommand {
    /// ノードの登録・申告の更新（起動時）。生きているものとして扱う。
    RegisterNode { spec: NodeSpec, now_ms: i64 },
    /// ノードを消す（そのノードの仕事は次の計画で移す）。
    RemoveNode { node: NodeId },
    SetDraining { node: NodeId, draining: bool },
    /// 心拍の途絶（[`crate::FailureDetector`] が作る）。
    NodeDown { node: NodeId, now_ms: i64, last_heartbeat_ms: i64 },
    NodeUp { node: NodeId, now_ms: i64 },
    /// 装置（GPU・キャプチャ機器）の故障の報告。
    DeviceDown { node: NodeId, device: String },
    DeviceUp { node: NodeId, device: String },
    /// 運用者が装置を手で止める・戻す（測った健康とは別。心拍で戻らない）。
    DisableDevice { node: NodeId, device: String },
    EnableDevice { node: NodeId, device: String },
    PutWorkload { workload: Workload },
    DeleteWorkload { id: WorkloadId },
    /// 配置係の決定。`token_base` は計画した時点の通し番号（違えば古い計画として拒む）。
    ApplyDecisions { decisions: Vec<Decision>, token_base: u64, now_ms: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum GridError {
    UnknownNode { node: NodeId },
    UnknownDevice { node: NodeId, device: String },
    UnknownWorkload { workload: WorkloadId },
    Invalid { message: String },
    /// 計画の後に別の token が払い出された（計画し直す）。
    StalePlan { expected: u64, got: u64 },
    /// 適用の時点の状態に合わない決定（計画し直す）。
    InvalidDecision { index: usize, message: String },
}

impl fmt::Display for GridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GridError::UnknownNode { node } => write!(f, "ノード {node} がありません"),
            GridError::UnknownDevice { node, device } => write!(f, "ノード {node} に装置 {device} がありません"),
            GridError::UnknownWorkload { workload } => write!(f, "仕事 {workload} がありません"),
            GridError::Invalid { message } => write!(f, "不正な値: {message}"),
            GridError::StalePlan { expected, got } => write!(f, "古い計画です（token の通し番号 {got}、今は {expected}）"),
            GridError::InvalidDecision { index, message } => write!(f, "決定 {index} が今の状態に合いません: {message}"),
        }
    }
}

impl std::error::Error for GridError {}

/// ノードで動いている仕事（突き合わせの入力）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    pub workload: WorkloadId,
    pub token: u64,
    pub level: usize,
}

/// 突き合わせの結果（ノードがすること）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconcile {
    /// 始める（割り当てがあり、動いていない・token が違う）。
    pub start: Vec<Assignment>,
    /// 止める（割り当てが無い・別のノード・token が古い）。
    pub stop: Vec<Running>,
    /// 段（・装置）を変える（token は同じ）。
    pub change_level: Vec<Assignment>,
}

/// `assignment` が `node` の `token` の仕事として今も有効か（古いノードは false を見たら止める。
/// 録画の確定・出力の公開など外部への副作用の前にも確かめる）。
pub fn is_current(assignment: Option<&Assignment>, node: &str, token: u64) -> bool {
    assignment.is_some_and(|a| a.node == node && a.token == token)
}

fn invalid(m: impl Into<String>) -> GridError {
    GridError::Invalid { message: m.into() }
}

impl GridState {
    /// 自前の `fencing_counter` を使って適用する。
    pub fn apply(&mut self, cmd: GridCommand) -> Result<(), GridError> {
        let mut c = self.fencing_counter;
        let r = self.apply_with_counter(cmd, &mut c);
        self.fencing_counter = c;
        r
    }

    /// 外の通し番号（制御状態の `fencing_counter` など）を使って適用する。
    pub fn apply_with_counter(&mut self, cmd: GridCommand, counter: &mut u64) -> Result<(), GridError> {
        match cmd {
            GridCommand::RegisterNode { spec, now_ms } => {
                if spec.id.is_empty() {
                    return Err(invalid("ノードの ID が空です"));
                }
                let mut ids = HashSet::new();
                if !spec.gpus.iter().map(|g| &g.id).chain(spec.capture.iter().map(|c| &c.id)).all(|d| ids.insert(d)) {
                    return Err(invalid("装置の ID が重複しています"));
                }
                // 手で止めた装置は、申告し直しても止めたまま
                let mut spec = spec;
                if let Some(old) = self.nodes.get(&spec.id) {
                    for g in &mut spec.gpus {
                        g.disabled |= old.spec.gpus.iter().any(|o| o.id == g.id && o.disabled);
                    }
                    for c in &mut spec.capture {
                        c.disabled |= old.spec.capture.iter().any(|o| o.id == c.id && o.disabled);
                    }
                }
                let status = NodeStatus { up: true, changed_ms: now_ms, last_heartbeat_ms: now_ms };
                self.nodes.insert(spec.id.clone(), Node { spec, status });
            }
            GridCommand::RemoveNode { node } => {
                self.nodes.remove(&node).ok_or(GridError::UnknownNode { node })?;
            }
            GridCommand::SetDraining { node, draining } => {
                self.node_mut(&node)?.spec.draining = draining;
            }
            GridCommand::NodeDown { node, now_ms, last_heartbeat_ms } => {
                let n = self.node_mut(&node)?;
                if n.status.up {
                    n.status = NodeStatus { up: false, changed_ms: now_ms, last_heartbeat_ms };
                }
            }
            GridCommand::NodeUp { node, now_ms } => {
                let n = self.node_mut(&node)?;
                if !n.status.up {
                    n.status = NodeStatus { up: true, changed_ms: now_ms, last_heartbeat_ms: now_ms };
                }
            }
            GridCommand::DeviceDown { node, device } => self.set_device(&node, &device, false)?,
            GridCommand::DeviceUp { node, device } => self.set_device(&node, &device, true)?,
            GridCommand::DisableDevice { node, device } => self.set_disabled(&node, &device, true)?,
            GridCommand::EnableDevice { node, device } => self.set_disabled(&node, &device, false)?,
            GridCommand::PutWorkload { workload } => {
                validate_workload(&workload)?;
                self.workloads.insert(workload.id.clone(), workload);
            }
            GridCommand::DeleteWorkload { id } => {
                self.workloads.remove(&id).ok_or_else(|| GridError::UnknownWorkload { workload: id.clone() })?;
                self.assignments.remove(&id);
                self.unschedulable.remove(&id);
            }
            GridCommand::ApplyDecisions { decisions, token_base, now_ms } => {
                self.apply_decisions(decisions, token_base, now_ms, counter)?;
            }
        }
        Ok(())
    }

    fn node_mut(&mut self, id: &str) -> Result<&mut Node, GridError> {
        self.nodes.get_mut(id).ok_or_else(|| GridError::UnknownNode { node: id.to_string() })
    }

    fn set_disabled(&mut self, node: &str, device: &str, disabled: bool) -> Result<(), GridError> {
        let n = self.node_mut(node)?;
        if let Some(g) = n.spec.gpus.iter_mut().find(|g| g.id == device) {
            g.disabled = disabled;
        } else if let Some(c) = n.spec.capture.iter_mut().find(|c| c.id == device) {
            c.disabled = disabled;
        } else {
            return Err(GridError::UnknownDevice { node: node.to_string(), device: device.to_string() });
        }
        Ok(())
    }

    fn set_device(&mut self, node: &str, device: &str, healthy: bool) -> Result<(), GridError> {
        let n = self.node_mut(node)?;
        if let Some(g) = n.spec.gpus.iter_mut().find(|g| g.id == device) {
            g.healthy = healthy;
        } else if let Some(c) = n.spec.capture.iter_mut().find(|c| c.id == device) {
            c.healthy = healthy;
        } else {
            return Err(GridError::UnknownDevice { node: node.to_string(), device: device.to_string() });
        }
        Ok(())
    }

    /// この状態で計画する（リーダーが周期的に呼ぶ）。
    pub fn plan(
        &self,
        now_ms: i64,
        token_base: u64,
        stream_nodes: &BTreeMap<String, BTreeSet<NodeId>>,
        config: &PlanConfig,
    ) -> Plan {
        plan(&PlanInput {
            nodes: &self.nodes,
            workloads: &self.workloads,
            assignments: &self.assignments,
            unschedulable: &self.unschedulable,
            stream_nodes,
            now_ms,
            token_base,
            config,
        })
    }

    /// `node` の `token` の仕事 `workload` が今も有効か。
    pub fn is_current(&self, workload: &str, node: &str, token: u64) -> bool {
        is_current(self.assignments.get(workload), node, token)
    }

    /// `node` に割り当てられた仕事（ID 順）。
    pub fn assignments_for<'s>(&'s self, node: &'s str) -> impl Iterator<Item = &'s Assignment> + 's {
        self.assignments.values().filter(move |a| a.node == node)
    }

    /// ノードの突き合わせ: 動いている仕事と割り当てを比べて、始める・止める・段を変えるものを返す。
    pub fn reconcile(&self, node: &str, running: &[Running]) -> Reconcile {
        let mut out = Reconcile::default();
        let mut seen = HashSet::new();
        for r in running {
            match self.assignments.get(&r.workload) {
                Some(a) if a.node == node && a.token == r.token => {
                    seen.insert(r.workload.as_str());
                    if a.level != r.level {
                        out.change_level.push(a.clone());
                    }
                }
                _ => out.stop.push(r.clone()),
            }
        }
        out.start = self.assignments_for(node).filter(|a| !seen.contains(a.workload.as_str())).cloned().collect();
        out
    }

    fn apply_decisions(
        &mut self,
        decisions: Vec<Decision>,
        token_base: u64,
        now_ms: i64,
        counter: &mut u64,
    ) -> Result<(), GridError> {
        if token_base != *counter {
            return Err(GridError::StalePlan { expected: *counter, got: token_base });
        }
        let mut next = token_base;
        let mut asg = self.assignments.clone();
        let mut uns: Vec<(WorkloadId, Vec<RejectCount>)> = Vec::new();
        let mut touched: BTreeSet<NodeId> = BTreeSet::new();
        let mut seen: HashSet<(&str, bool)> = HashSet::new();
        for (index, d) in decisions.iter().enumerate() {
            let bad = |m: &str| GridError::InvalidDecision { index, message: m.to_string() };
            // 置けない報告は Evict と並んでよい（それ以外は仕事ごとに 1 つ）
            if !seen.insert((d.workload(), matches!(d, Decision::Unschedulable { .. }))) {
                return Err(bad("同じ仕事への決定が重複しています"));
            }
            let w = self.workloads.get(d.workload()).ok_or_else(|| bad("仕事がありません"))?;
            if let Some(t) = d.token() {
                next += 1;
                if t != next {
                    return Err(bad("token が通し番号に合いません"));
                }
            }
            match d {
                Decision::Assign { node, device, level, token, reason, .. } => {
                    if asg.contains_key(&w.id) {
                        return Err(bad("すでに割り当てがあります"));
                    }
                    self.check_target(w, node, device.as_deref(), *level, false).map_err(|m| bad(&m))?;
                    asg.insert(
                        w.id.clone(),
                        Assignment {
                            workload: w.id.clone(),
                            node: node.clone(),
                            device: device.clone(),
                            level: *level,
                            token: *token,
                            since_ms: now_ms,
                            level_since_ms: now_ms,
                            reason: reason.clone(),
                        },
                    );
                    touched.insert(node.clone());
                }
                Decision::Move { from, node, device, level, token, reason, .. } => {
                    let a = asg.get_mut(&w.id).ok_or_else(|| bad("割り当てがありません"))?;
                    if a.node != *from {
                        return Err(bad("移す元のノードが違います"));
                    }
                    self.check_target(w, node, device.as_deref(), *level, node == from).map_err(|m| bad(&m))?;
                    // 移すと仕事は始め直しになるので、段の冷却も始め直す
                    a.level_since_ms = now_ms;
                    a.node = node.clone();
                    a.device = device.clone();
                    a.level = *level;
                    a.token = *token;
                    a.since_ms = now_ms;
                    a.reason = reason.clone();
                    touched.insert(node.clone());
                }
                Decision::Degrade { level, device, reason, .. } | Decision::Restore { level, device, reason, .. } => {
                    let a = asg.get_mut(&w.id).ok_or_else(|| bad("割り当てがありません"))?;
                    let ok = match d {
                        Decision::Degrade { .. } => *level > a.level,
                        _ => *level < a.level,
                    };
                    if !ok {
                        return Err(bad("段の向きが違います"));
                    }
                    if a.device.is_some() && a.device != *device {
                        return Err(bad("段の変更で装置は変えられません（Move を使う）"));
                    }
                    self.check_target(w, &a.node, device.as_deref(), *level, true).map_err(|m| bad(&m))?;
                    a.level = *level;
                    a.device = device.clone();
                    a.level_since_ms = now_ms;
                    a.reason = reason.clone();
                    touched.insert(a.node.clone());
                }
                Decision::Evict { .. } => {
                    asg.remove(&w.id).ok_or_else(|| bad("割り当てがありません"))?;
                }
                Decision::Unschedulable { reasons, .. } => uns.push((w.id.clone(), reasons.clone())),
            }
        }
        for (id, _) in &uns {
            if asg.contains_key(id) {
                return Err(GridError::InvalidDecision { index: 0, message: format!("割り当てのある仕事 {id} を置けないと報告しています") });
            }
        }
        self.verify_nodes(&asg, &touched)?;
        // 確定
        for id in decisions.iter().filter(|d| !matches!(d, Decision::Unschedulable { .. })).map(|d| d.workload()) {
            if asg.contains_key(id) {
                self.unschedulable.remove(id);
            }
        }
        for (id, r) in uns {
            self.unschedulable.insert(id, r);
        }
        self.assignments = asg;
        *counter = next;
        Ok(())
    }

    /// 新しい置き場所（`existing` なら同じノードでの変更）の静的な条件。
    fn check_target(&self, w: &Workload, node: &str, device: Option<&str>, level: usize, existing: bool) -> Result<(), String> {
        let n = self.nodes.get(node).ok_or("ノードがありません")?;
        if level >= w.levels() {
            return Err("段がありません".into());
        }
        let pin = pin_state(&self.nodes, w);
        static_check(n, w, pin, existing).map_err(|r| format!("ノードが条件に合いません（{r:?}）"))?;
        let req = w.req(level);
        match (&req.gpu, device) {
            (Some(gr), Some(d)) => {
                let g = n.spec.gpu_index(d).ok_or("GPU がありません")?;
                if !gpu_ok(&n.spec, g, gr, dev_pin(w, pin, existing)) {
                    return Err("GPU が使えません".into());
                }
            }
            (Some(_), None) => return Err("GPU が指定されていません".into()),
            (None, Some(d)) if n.spec.gpu_index(d).is_none() => return Err("GPU がありません".into()),
            _ => {}
        }
        if let Some(c) = &req.capture
            && !n.spec.capture.iter().any(|x| &x.id == c && x.usable())
        {
            return Err("キャプチャ機器が使えません".into());
        }
        Ok(())
    }

    /// 変更のあったノードの容量・反親和性・キャプチャ機器の専有を確かめる。
    fn verify_nodes(&self, asg: &BTreeMap<WorkloadId, Assignment>, touched: &BTreeSet<NodeId>) -> Result<(), GridError> {
        let mut usage: BTreeMap<&str, (Usage, HashSet<&str>)> = BTreeMap::new();
        for a in asg.values() {
            if !touched.contains(&a.node) {
                continue;
            }
            let (Some(n), Some(w)) = (self.nodes.get(&a.node), self.workloads.get(&a.workload)) else { continue };
            let e = usage.entry(a.node.as_str()).or_insert_with(|| (Usage::new(&n.spec), HashSet::new()));
            let g = a.device.as_deref().and_then(|d| n.spec.gpu_index(d));
            e.0.add(&n.spec, w.req(a.level), g);
            if let Some(grp) = w.constraints.anti_affinity.as_deref()
                && !e.1.insert(grp)
            {
                return Err(GridError::InvalidDecision { index: 0, message: format!("ノード {} で反親和性の組 {grp} が重なります", a.node) });
            }
        }
        for (node, (u, _)) in &usage {
            if u.deficit(&self.nodes[*node].spec, None) > 0 {
                return Err(GridError::InvalidDecision { index: 0, message: format!("ノード {node} の容量を超えます") });
            }
        }
        Ok(())
    }
}

fn validate_workload(w: &Workload) -> Result<(), GridError> {
    if w.id.is_empty() {
        return Err(invalid("仕事の ID が空です"));
    }
    if w.tenant.is_empty() {
        return Err(invalid("テナントが空です"));
    }
    if w.degrade.iter().any(|l| l.label.is_empty() || l.label == "full") {
        return Err(invalid("縮退の段の名前が空か full です"));
    }
    Ok(())
}
