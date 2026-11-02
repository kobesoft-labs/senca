//! 配置係（ADR-0013 決定 3・4）。今の状態から決定の列を作る純粋な関数 [`plan()`]。
//!
//! 手順（同じ入力からはいつも同じ出力）:
//! 1. **今の割り当ての検査**（優先度の高い順・ID 順）: ノードの停止・装置の故障・条件の変更・反親和性・
//!    キャプチャ機器の重複で合わなくなったものを「置き直し」の列へ。合うものはそのまま数える（動かさない）。
//! 2. **容量の超過の修理**: ノードの容量が減って予約が超えたら、優先度の低い順に段を下げ、それでも
//!    足りなければ追い出す（追い出したものは置き直しの列へ。他に置ければ Move になる）。
//! 3. **優先度ごと**（critical → high → normal → low）に:
//!    a. 置き直しの列を ID 順に置く。各段（full から）で、空きに置く → 低い優先度の仕事の段を下げる
//!    → 低い優先度の仕事を追い出す、の順に試し、どれも駄目なら次の段（自分の縮退）へ。
//!    b. 縮退している仕事を、冷却（`restore_cooldown_ms`）の後に元の段へ戻す: 同じノードの空き →
//!    他のノードの空きへ移す → 同じノードで低い優先度の仕事を縮退・追い出し、の順。
//!    移して元のノードが空いたら、その優先度を終えたところで critical からやり直す
//!    （空いた所を低い優先度が先に取らないように）。
//! 4. **任意の移動**: 排出中のノードの仕事（pin の所にいないもの）と、prefer の pin の対象が戻った仕事を、
//!    空きのあるノードへ同じか良い段で移す（1 回の計画で `max_voluntary_moves` まで）。
//! 5. 3・4 で何か変わったら繰り返す（変わらなくなるまで、最大 `MAX_PASSES` 回）。結果は不動点:
//!    適用した直後に同じ時刻で計画し直すと決定は空になる（シミュレーションで毎回確かめている）。
//! 6. 置けなかった仕事は理由ごとのノードの数を報告する（前回と同じなら出さない）。
//! 7. 元の割り当てとの差から決定を作り、Evict → Degrade → Move → Assign → Restore → Unschedulable の
//!    順（同じ種類は ID 順）に並べ、Assign・Move に `token_base + 1` から token を振る。
//!
//! 行ったり来たりの抑え: 段を良くするのは、割り当ての `level_since_ms` から冷却が過ぎた後だけ
//! （戻すときも、置き直し・移動で良い段になるときも同じ）。Move は `level_since_ms` を始め直す。
//! 段を下げる・追い出すのは高い優先度の仕事のためか、ノード・装置の故障・容量の減少のときだけ。
//!
//! 手動の割り当て（pin）: 対象（ノード・GPU）が生きている間は、strict も prefer もそこにしか置かない
//! （排出中なら新しくは置かない）。pin の所で動いているものは移さない（追い出し・縮退はありうる）。
//! 対象が止まると strict は置けないまま、prefer は他へ移り、対象が戻って冷却が過ぎたら戻る。
//!
//! 点数（空きに置くとき、高いほど良い。同点は ID の小さいノード）:
//! - データのある所（`locality` の stream を持つノード）に `locality_bonus`。
//! - spread は置いた後の空きの割合（千分率、資源と GPU の小さい方）、binpack は 1000 から引いたもの。
//!
//! 計算量: 置く仕事ごとに候補のノード（静的な条件は計画の中で覚える）を 1 回なめる（O(W×N)）。
//! 縮退・追い出しは空きに置けない仕事だけで、優先度ごとの資源の和による O(1) の見積もりで
//! 見込みの無いノード・仕事を飛ばす。置けなかった仕事の再試行は、候補のノードが空いたときだけ。

use crate::fit::{PinState, Usage, dev_pin, gpu_ok, pin_state, static_check};
use crate::model::*;
use crate::state::GridCommand;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanConfig {
    /// 段を変えてから元へ戻すまでの最短の時間（行ったり来たりを抑える）。prefer の pin の
    /// 戻りにも使う（置いてからこの時間は戻さない）。
    pub restore_cooldown_ms: i64,
    /// 戻した後もこの千分率の余裕（CPU・メモリ・帯域）が残るときだけ戻す。
    pub restore_headroom_permille: u32,
    /// 1 回の計画での任意の移動（排出・pin の戻り）の上限。
    pub max_voluntary_moves: usize,
    /// データのある所の点数。
    pub locality_bonus: i64,
}

impl Default for PlanConfig {
    fn default() -> Self {
        PlanConfig { restore_cooldown_ms: 30_000, restore_headroom_permille: 0, max_voluntary_moves: 64, locality_bonus: 10_000 }
    }
}

/// 配置係への入力（すべて借りるだけ）。
pub struct PlanInput<'a> {
    pub nodes: &'a BTreeMap<NodeId, Node>,
    pub workloads: &'a BTreeMap<WorkloadId, Workload>,
    pub assignments: &'a BTreeMap<WorkloadId, Assignment>,
    /// 前回報告した置けない理由（同じなら報告しない）。
    pub unschedulable: &'a BTreeMap<WorkloadId, Vec<RejectCount>>,
    /// stream → それを持つノード（心拍から。[`crate::FailureDetector::stream_nodes_at`]）。
    pub stream_nodes: &'a BTreeMap<String, BTreeSet<NodeId>>,
    pub now_ms: i64,
    /// fencing token の通し番号の今の値。新しい token は `token_base + 1` から。
    pub token_base: u64,
    pub config: &'a PlanConfig,
}

/// 計画（決定の列）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub token_base: u64,
    pub now_ms: i64,
    pub decisions: Vec<Decision>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
    }

    /// 合意ログに載せるコマンド。
    pub fn into_command(self) -> GridCommand {
        GridCommand::ApplyDecisions { decisions: self.decisions, token_base: self.token_base, now_ms: self.now_ms }
    }
}

/// 配置係の本体。
pub fn plan(input: &PlanInput<'_>) -> Plan {
    let mut p = Planner::new(input);
    p.validate_existing();
    p.repair_overcommit();
    // 移動・移した戻しで空いた所を、置けなかった仕事・縮退した仕事・pin の戻りに回す（変わらなくなるまで）
    for _ in 0..MAX_PASSES {
        let placed = p.pass();
        let moved = p.voluntary_moves();
        if !placed && !moved {
            break;
        }
    }
    p.finish()
}

/// 置く・戻すを繰り返す回数の上限（1 回ごとに必ず何かが良くなるので、普通は数回で止まる。
/// 上限に達したら残りは次の計画で続く）。
const MAX_PASSES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Place {
    node: usize,
    gpu: Option<usize>,
    level: usize,
}

#[derive(Debug, Clone, Default)]
struct Load {
    u: Usage,
    groups: HashMap<String, u32>,
    members: Vec<usize>,
    /// 優先度ごとの資源の和（横取りできるかの見積もり）。
    by_prio: [Resources; 4],
}

/// 縮退・追い出しの案。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Room {
    degrade: Vec<(usize, usize)>,
    evict: Vec<usize>,
    /// (追い出す数, 追い出す優先度の和, 下げる段の数)。小さいほど良い。
    cost: (usize, usize, usize),
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
    free: Resources,
    /// [r]: 優先度 r より低い仕事をすべて除いたときの最大の空き。
    keep_free: [Resources; 4],
}

fn max_res(a: Resources, b: Resources) -> Resources {
    Resources {
        cpu_millis: a.cpu_millis.max(b.cpu_millis),
        memory_mb: a.memory_mb.max(b.memory_mb),
        net_in_mbps: a.net_in_mbps.max(b.net_in_mbps),
        net_out_mbps: a.net_out_mbps.max(b.net_out_mbps),
    }
}

struct Need<'r> {
    req: &'r Requirements,
    gpu: Option<usize>,
    group: Option<&'r str>,
}

struct Planner<'a> {
    inp: &'a PlanInput<'a>,
    nodes: Vec<&'a Node>,
    nidx: HashMap<&'a str, usize>,
    wls: Vec<&'a Workload>,
    load: Vec<Load>,
    cur: Vec<Option<Place>>,
    reason: Vec<Option<Reason>>,
    /// この計画で高い優先度の仕事に段を下げられた。
    victim: Vec<bool>,
    pending: [Vec<usize>; 4],
    pins: Vec<PinState>,
    cands: Vec<Option<std::rc::Rc<Vec<usize>>>>,
    /// 容量が空いた回数（take のたびに増える）と、ノードごとの最後に空いた時点。
    epoch: u64,
    node_freed: Vec<u64>,
    /// 置けなかった仕事を最後に試した時点（その後に候補のノードが空いていなければ試し直さない）。
    tried: Vec<Option<u64>>,
    /// クラスタ全体の上限の見積もり（次元ごとの最大の空き、優先度ごとの「低い優先度を除いた」最大の空き）。
    /// 置くと空きは減るだけなので古い値も上限として正しい。空いた（take）ときだけ作り直す。
    bound: std::cell::RefCell<Option<Bounds>>,
    moves: usize,
}

/// 空のノードに収まるか（キャプチャ機器は使われていないとみなす）。
fn fits_empty_node(spec: &NodeSpec, req: &Requirements, gpu: Option<usize>) -> bool {
    req.resources.within(&spec.capacity)
        && match (&req.gpu, gpu) {
            (Some(gr), Some(g)) => {
                let d = &spec.gpus[g];
                gr.encode <= d.encode_sessions && gr.decode <= d.decode_sessions && gr.memory_mb <= d.memory_mb
            }
            (Some(_), None) => false,
            (None, _) => true,
        }
}

fn reject_reason(r: Reject, node: &str) -> Reason {
    match r {
        Reject::NodeDown | Reject::NotMedia => Reason::NodeDown { node: node.to_string() },
        Reject::PinTargetDown => Reason::PinTargetDown,
        _ => Reason::ConstraintChanged,
    }
}

impl<'a> Planner<'a> {
    fn new(inp: &'a PlanInput<'a>) -> Self {
        let nodes: Vec<&Node> = inp.nodes.values().collect();
        let nidx = nodes.iter().enumerate().map(|(i, n)| (n.spec.id.as_str(), i)).collect();
        let wls: Vec<&Workload> = inp.workloads.values().collect();
        let load = nodes.iter().map(|n| Load { u: Usage::new(&n.spec), ..Default::default() }).collect();
        let pins = wls.iter().map(|w| pin_state(inp.nodes, w)).collect();
        let nodes_len = nodes.len();
        let n = wls.len();
        Planner {
            inp,
            nodes,
            nidx,
            wls,
            load,
            cur: vec![None; n],
            reason: vec![None; n],
            victim: vec![false; n],
            pending: Default::default(),
            pins,
            cands: vec![None; n],
            epoch: 0,
            node_freed: vec![0; nodes_len],
            tried: vec![None; n],
            bound: std::cell::RefCell::new(None),
            moves: 0,
        }
    }

    fn spec(&self, n: usize) -> &'a NodeSpec {
        &self.nodes[n].spec
    }

    fn group(&self, i: usize) -> Option<&'a str> {
        self.wls[i].constraints.anti_affinity.as_deref()
    }

    fn put(&mut self, i: usize, p: Place) {
        let spec = self.spec(p.node);
        let l = &mut self.load[p.node];
        l.u.add(spec, self.wls[i].req(p.level), p.gpu);
        let r = self.wls[i].priority.rank();
        l.by_prio[r] = l.by_prio[r].plus(&self.wls[i].req(p.level).resources);
        if let Some(g) = self.wls[i].constraints.anti_affinity.as_deref() {
            *l.groups.entry(g.to_string()).or_default() += 1;
        }
        l.members.push(i);
        self.cur[i] = Some(p);
    }

    fn take(&mut self, i: usize) -> Option<Place> {
        let p = self.cur[i].take()?;
        self.epoch += 1;
        *self.bound.get_mut() = None;
        self.node_freed[p.node] = self.epoch;
        let spec = self.spec(p.node);
        let l = &mut self.load[p.node];
        l.u.sub(spec, self.wls[i].req(p.level), p.gpu);
        let r = self.wls[i].priority.rank();
        l.by_prio[r] = l.by_prio[r].minus(&self.wls[i].req(p.level).resources);
        if let Some(g) = self.wls[i].constraints.anti_affinity.as_deref()
            && let Some(c) = l.groups.get_mut(g)
        {
            *c = c.saturating_sub(1);
        }
        if let Some(k) = l.members.iter().position(|&m| m == i) {
            l.members.remove(k);
        }
        Some(p)
    }

    fn group_conflict(&self, n: usize, i: usize) -> bool {
        self.group(i).is_some_and(|g| self.load[n].groups.get(g).is_some_and(|&c| c > 0))
    }

    fn displace(&mut self, i: usize, r: Reason) {
        self.reason[i] = Some(r);
        self.pending[self.wls[i].priority.rank()].push(i);
    }

    // ---- 1. 今の割り当ての検査 ----

    fn validate_existing(&mut self) {
        let mut order: Vec<usize> =
            (0..self.wls.len()).filter(|&i| self.inp.assignments.contains_key(&self.wls[i].id)).collect();
        order.sort_by_key(|&i| (Reverse(self.wls[i].priority), i));
        for i in order {
            let a = &self.inp.assignments[&self.wls[i].id];
            match self.check_existing(i, a) {
                Ok(p) => self.put(i, p),
                Err(r) => self.displace(i, r),
            }
        }
        for i in 0..self.wls.len() {
            if !self.inp.assignments.contains_key(&self.wls[i].id) {
                self.pending[self.wls[i].priority.rank()].push(i);
            }
        }
    }

    fn check_existing(&self, i: usize, a: &Assignment) -> Result<Place, Reason> {
        let w = self.wls[i];
        let Some(&n) = self.nidx.get(a.node.as_str()) else {
            return Err(Reason::NodeDown { node: a.node.clone() });
        };
        let node = self.nodes[n];
        if a.level >= w.levels() {
            return Err(Reason::ConstraintChanged);
        }
        static_check(node, w, self.pins[i], true).map_err(|r| reject_reason(r, &node.spec.id))?;
        let req = w.req(a.level);
        let gpu = match (&req.gpu, &a.device) {
            (Some(gr), Some(d)) => {
                let g = node.spec.gpu_index(d).ok_or(Reason::DeviceDown { device: d.clone() })?;
                if !node.spec.gpus[g].healthy {
                    return Err(Reason::DeviceDown { device: d.clone() });
                }
                if !gpu_ok(&node.spec, g, gr, dev_pin(w, self.pins[i], true)) {
                    return Err(Reason::ConstraintChanged);
                }
                Some(g)
            }
            (Some(_), None) => return Err(Reason::ConstraintChanged),
            (None, Some(d)) => node.spec.gpu_index(d),
            (None, None) => None,
        };
        if let Some(c) = &req.capture {
            match node.spec.capture_index(c) {
                Some(k) if node.spec.capture[k].healthy => {
                    if self.load[n].u.capture[k] > 0 {
                        return Err(Reason::ConstraintChanged);
                    }
                }
                _ => return Err(Reason::DeviceDown { device: c.clone() }),
            }
        }
        if self.group_conflict(n, i) {
            return Err(Reason::ConstraintChanged);
        }
        Ok(Place { node: n, gpu, level: a.level })
    }

    // ---- 2. 容量の超過の修理 ----

    fn repair_overcommit(&mut self) {
        for n in 0..self.nodes.len() {
            if self.load[n].u.deficit(self.spec(n), None) > 0
                && let Some(room) = self.make_room(n, None, None)
            {
                self.apply_room(room, &Reason::Overcommitted);
            }
        }
    }

    // ---- 縮退・追い出しの案 ----

    /// 仕事 v を段 `from` から下げられるいちばん下の段（GPU・キャプチャの扱いが変わらない範囲）。
    fn bottom(&self, v: usize, from: usize, gpu: Option<usize>) -> usize {
        let w = self.wls[v];
        let cap = w.req(from).capture.clone();
        let mut b = from;
        for l in from + 1..w.levels() {
            let r = w.req(l);
            if (r.gpu.is_some() && gpu.is_none()) || (r.capture.is_some() && r.capture != cap) {
                break;
            }
            b = l;
        }
        b
    }

    /// ノード n で `need` を空けるための縮退・追い出しの案。`below` があればそれより低い優先度の仕事だけを
    /// 対象にする（無ければすべて。容量の超過の修理）。
    fn make_room(&self, n: usize, need: Option<Need<'_>>, below: Option<Priority>) -> Option<Room> {
        let spec = self.spec(n);
        let wls = &self.wls;
        let mut victims: Vec<usize> =
            self.load[n].members.iter().copied().filter(|&m| below.is_none_or(|b| wls[m].priority < b)).collect();
        victims.sort_by_key(|&m| (wls[m].priority, m));
        let place = |v: usize| self.cur[v].expect("member is placed");
        if let Some(nd) = &need {
            for &m in &self.load[n].members {
                if victims.contains(&m) {
                    continue;
                }
                if nd.group.is_some() && self.group(m) == nd.group {
                    return None;
                }
                if nd.req.capture.is_some() && wls[m].req(place(m).level).capture == nd.req.capture {
                    return None;
                }
            }
        }
        let np = need.as_ref().map(|nd| (nd.req, nd.gpu));
        let mut u = self.load[n].u.clone();
        let mut evict = Vec::new();
        let mut keep = Vec::new();
        // 同じ組・同じキャプチャ機器を使う仕事は追い出すしかない
        for &v in &victims {
            let p = place(v);
            let mandatory = need.as_ref().is_some_and(|nd| {
                (nd.group.is_some() && self.group(v) == nd.group)
                    || (nd.req.capture.is_some() && wls[v].req(p.level).capture == nd.req.capture)
            });
            if mandatory {
                u.sub(spec, wls[v].req(p.level), p.gpu);
                evict.push(v);
            } else {
                keep.push(v);
            }
        }
        // 残りをすべていちばん下の段にしても足りなければ、低い優先度から追い出す
        let mut lb = u.clone();
        for &v in &keep {
            let p = place(v);
            lb.sub(spec, wls[v].req(p.level), p.gpu);
            lb.add(spec, wls[v].req(self.bottom(v, p.level, p.gpu)), p.gpu);
        }
        let mut d = lb.deficit(spec, np);
        let mut kept = Vec::new();
        for &v in &keep {
            if d == 0 {
                kept.push(v);
                continue;
            }
            let p = place(v);
            let b = self.bottom(v, p.level, p.gpu);
            lb.sub(spec, wls[v].req(b), p.gpu);
            let nd = lb.deficit(spec, np);
            if nd < d {
                d = nd;
                u.sub(spec, wls[v].req(p.level), p.gpu);
                evict.push(v);
            } else {
                lb.add(spec, wls[v].req(b), p.gpu);
                kept.push(v);
            }
        }
        if d > 0 {
            return None;
        }
        // 段を下げる: 低い優先度の組から、組の中は 1 段ずつ順番に
        let mut level: BTreeMap<usize, usize> = BTreeMap::new();
        let mut d = u.deficit(spec, np);
        let mut steps = 0;
        let mut s = 0;
        while s < kept.len() && d > 0 {
            let prio = wls[kept[s]].priority;
            let e = s + kept[s..].iter().take_while(|&&v| wls[v].priority == prio).count();
            loop {
                let mut progressed = false;
                for &v in &kept[s..e] {
                    if d == 0 {
                        break;
                    }
                    let p = place(v);
                    let from = *level.get(&v).unwrap_or(&p.level);
                    for to in from + 1..=self.bottom(v, p.level, p.gpu) {
                        u.sub(spec, wls[v].req(from), p.gpu);
                        u.add(spec, wls[v].req(to), p.gpu);
                        let nd = u.deficit(spec, np);
                        if nd < d {
                            d = nd;
                            level.insert(v, to);
                            steps += 1;
                            progressed = true;
                            break;
                        }
                        u.sub(spec, wls[v].req(to), p.gpu);
                        u.add(spec, wls[v].req(from), p.gpu);
                    }
                }
                if d == 0 || !progressed {
                    break;
                }
            }
            s = e;
        }
        if d > 0 {
            // 1 段ずつでは減らない形（複数の次元が同時に足りない）: すべていちばん下へ
            for &v in &kept {
                let p = place(v);
                let from = *level.get(&v).unwrap_or(&p.level);
                let b = self.bottom(v, p.level, p.gpu);
                u.sub(spec, wls[v].req(from), p.gpu);
                u.add(spec, wls[v].req(b), p.gpu);
                if b != p.level {
                    level.insert(v, b);
                    steps += b - from;
                }
            }
            if u.deficit(spec, np) > 0 {
                return None;
            }
        }
        let degrade: Vec<(usize, usize)> = level.into_iter().filter(|&(v, l)| l != place(v).level).collect();
        let cost = (evict.len(), evict.iter().map(|&v| wls[v].priority.rank() + 1).sum(), steps);
        Some(Room { degrade, evict, cost })
    }

    fn apply_room(&mut self, room: Room, reason: &Reason) {
        for (v, l) in room.degrade {
            if let Some(mut p) = self.take(v) {
                p.level = l;
                self.put(v, p);
                self.victim[v] = true;
                self.reason[v] = Some(reason.clone());
            }
        }
        for v in room.evict {
            self.take(v);
            self.displace(v, reason.clone());
        }
    }

    // ---- 3a. 置く ----

    /// 新しく置ける候補のノード（静的な条件だけ。計画の中では変わらないので覚えておく）。
    fn candidates(&mut self, i: usize) -> std::rc::Rc<Vec<usize>> {
        if let Some(c) = &self.cands[i] {
            return c.clone();
        }
        let c = std::rc::Rc::new(self.compute_candidates(i));
        self.cands[i] = Some(c.clone());
        c
    }

    fn compute_candidates(&self, i: usize) -> Vec<usize> {
        let w = self.wls[i];
        if let Some(p) = &w.constraints.node
            && self.pins[i].healthy
        {
            return self.nidx.get(p.as_str()).copied().filter(|&n| static_check(self.nodes[n], w, self.pins[i], false).is_ok()).into_iter().collect();
        }
        (0..self.nodes.len()).filter(|&n| static_check(self.nodes[n], w, self.pins[i], false).is_ok()).collect()
    }

    /// 段 `level` で使える GPU の候補（GPU が要らなければ `None` 1 つ）。割り当てを作らない。
    fn gpu_choices(&self, i: usize, n: usize, level: usize, existing: bool) -> impl Iterator<Item = Option<usize>> + use<'a> {
        let w: &'a Workload = self.wls[i];
        let spec: &'a NodeSpec = &self.nodes[n].spec;
        let pin = dev_pin(w, self.pins[i], existing);
        let gr = w.req(level).gpu.as_ref();
        let none = gr.is_none().then_some(None);
        let some = gr.into_iter().flat_map(move |gr| (0..spec.gpus.len()).filter(move |&g| gpu_ok(spec, g, gr, pin)).map(Some));
        none.into_iter().chain(some)
    }

    fn score(&self, i: usize, n: usize, req: &Requirements, gpu: Option<usize>) -> i64 {
        let w = self.wls[i];
        let free = self.load[n].u.free_permille(self.spec(n), req, gpu);
        let mut s = match w.placement {
            Placement::Spread => free,
            Placement::Binpack => 1000 - free,
        };
        let id = &self.spec(n).id;
        if w.locality.iter().any(|st| self.inp.stream_nodes.get(st).is_some_and(|ns| ns.contains(id))) {
            s += self.inp.config.locality_bonus;
        }
        s
    }

    fn bounds(&self) -> Bounds {
        if let Some(b) = *self.bound.borrow() {
            return b;
        }
        let mut b = Bounds { free: Resources::default(), keep_free: [Resources::default(); 4] };
        for (n, l) in self.load.iter().enumerate() {
            if !self.nodes[n].accepts_new() {
                continue;
            }
            let cap = &self.spec(n).capacity;
            b.free = max_res(b.free, cap.minus(&l.u.res));
            for r in 0..4 {
                let keep = l.by_prio[r..].iter().fold(Resources::default(), |a, x| a.plus(x));
                b.keep_free[r] = max_res(b.keep_free[r], cap.minus(&keep));
            }
        }
        *self.bound.borrow_mut() = Some(b);
        b
    }

    /// 空きに置ける最良の (ノード, GPU)。
    fn best_free(&self, i: usize, level: usize, cands: &[usize], headroom: u32) -> Option<(usize, Option<usize>)> {
        let req = self.wls[i].req(level);
        if !req.resources.within(&self.bounds().free) {
            return None;
        }
        let mut best: Option<(i64, usize, Option<usize>)> = None;
        for &n in cands {
            if self.group_conflict(n, i) {
                continue;
            }
            let spec = self.spec(n);
            for g in self.gpu_choices(i, n, level, false) {
                if self.load[n].u.fits(spec, req, g).is_err() {
                    continue;
                }
                if headroom > 0 {
                    let mut u = self.load[n].u.clone();
                    u.add(spec, req, g);
                    if !u.within_headroom(spec, headroom) {
                        continue;
                    }
                }
                let s = self.score(i, n, req, g);
                if best.is_none_or(|(b, _, _)| s > b) {
                    best = Some((s, n, g));
                }
            }
        }
        best.map(|(_, n, g)| (n, g))
    }

    /// 低い優先度の仕事を縮退・追い出しして置ける最良の (ノード, GPU, 案)。
    fn best_room(&self, i: usize, level: usize, cands: &[usize]) -> Option<(usize, Option<usize>, Room)> {
        let w = self.wls[i];
        let req = w.req(level);
        if !req.resources.within(&self.bounds().keep_free[w.priority.rank()]) {
            return None;
        }
        let mut best: Option<(usize, Option<usize>, Room)> = None;
        for &n in cands {
            // 低い優先度の仕事をすべて除いても資源が足りないノードは見ない（O(1) の見積もり）
            let keep = self.load[n].by_prio[w.priority.rank()..].iter().fold(Resources::default(), |a, r| a.plus(r));
            if !keep.plus(&req.resources).within(&self.spec(n).capacity)
                || !self.load[n].members.iter().any(|&m| self.wls[m].priority < w.priority)
            {
                continue;
            }
            for g in self.gpu_choices(i, n, level, false) {
                let need = Need { req, gpu: g, group: self.group(i) };
                if let Some(room) = self.make_room(n, Some(need), Some(w.priority))
                    && best.as_ref().is_none_or(|(_, _, b)| room.cost < b.cost)
                {
                    best = Some((n, g, room));
                }
            }
        }
        best
    }

    /// 置き直すときのいちばん良い段。段を変えてから冷却の間は、元の段より良くしない
    /// （置き直しで戻すのも「戻す」なので、行ったり来たりの抑えを同じにする）。
    fn min_level(&self, i: usize) -> usize {
        let w = self.wls[i];
        match self.inp.assignments.get(&w.id) {
            Some(a) if a.level < w.levels() && self.inp.now_ms.saturating_sub(a.level_since_ms) < self.inp.config.restore_cooldown_ms => a.level,
            _ => 0,
        }
    }

    fn try_place(&mut self, i: usize) -> bool {
        let w = self.wls[i];
        let cands = self.candidates(i);
        if cands.is_empty() {
            return false;
        }
        if let Some(t) = self.tried[i]
            && !cands.iter().any(|&n| self.node_freed[n] > t)
        {
            return false;
        }
        self.tried[i] = Some(self.epoch);
        for level in self.min_level(i)..w.levels() {
            if let Some((n, g)) = self.best_free(i, level, &cands, 0) {
                self.put(i, Place { node: n, gpu: g, level });
                return true;
            }
            if w.priority > Priority::Low
                && let Some((n, g, room)) = self.best_room(i, level, &cands)
            {
                self.apply_room(room, &Reason::Preempted { by: w.id.clone() });
                self.put(i, Place { node: n, gpu: g, level });
                return true;
            }
        }
        false
    }

    /// 優先度の高い順に、置く → 戻す。何か変わったら true。戻すために移した（元のノードが空いた）ら、
    /// その優先度を終えたところで打ち切って上の優先度からやり直す（空いた所を低い優先度が先に取らないように）。
    fn pass(&mut self) -> bool {
        let mut changed = false;
        for prio in Priority::ALL_DESC {
            changed |= self.place_pending(prio);
            let (restored, freed) = self.restore(prio);
            changed |= restored;
            if freed {
                break;
            }
        }
        changed
    }

    fn place_pending(&mut self, prio: Priority) -> bool {
        let r = prio.rank();
        let mut k = 0;
        let mut changed = false;
        while k < self.pending[r].len() {
            let i = self.pending[r][k];
            k += 1;
            if self.cur[i].is_none() {
                changed |= self.try_place(i);
            }
        }
        // 置けたものは列から外す（次の繰り返しで回らない）
        let cur = &self.cur;
        self.pending[r].retain(|&i| cur[i].is_none());
        changed
    }

    // ---- 3b. 元の段へ戻す ----

    /// (戻したか, 移して元のノードを空けたか)。
    fn restore(&mut self, prio: Priority) -> (bool, bool) {
        let cfg = self.inp.config;
        let now = self.inp.now_ms;
        // 戻してよいのは: 元の割り当ての段の冷却が済み、今の段が元より良くなっておらず
        // （この計画でもう戻したものは除く）、この計画で元より下げられていないもの
        // （下げられたら Degrade が出て冷却が始まる）。高い優先度に元の段まで押し戻されたものは、
        // 同じ計画でもう一度試す（次の計画で初めて戻ると、計画が不動点にならない）。
        let list: Vec<usize> = (0..self.wls.len())
            .filter(|&i| {
                let w = self.wls[i];
                let (Some(p), Some(a)) = (self.cur[i], self.inp.assignments.get(&w.id)) else { return false };
                w.priority == prio
                    && p.level > 0
                    && now.saturating_sub(a.level_since_ms) >= cfg.restore_cooldown_ms
                    && (p.level == a.level || (p.level > a.level && !self.victim[i]))
            })
            .collect();
        let mut changed = false;
        let mut freed = false;
        for i in list {
            let Some(p) = self.take(i) else { continue };
            // 手動の割り当ての所にいるものは動かさない（同じノードで戻すだけ）
            let may_move = !(self.wls[i].is_pinned() && self.at_pin(i, p));
            let found = self.restore_same_node(i, p).or_else(|| {
                if !may_move {
                    return None;
                }
                let cands = self.candidates(i);
                (0..p.level).find_map(|l| {
                    self.best_free(i, l, &cands, cfg.restore_headroom_permille).map(|(n, g)| Place { node: n, gpu: g, level: l })
                })
            });
            match found {
                Some(q) => {
                    self.put(i, q);
                    self.reason[i] = Some(Reason::CapacityReturned);
                    changed = true;
                    freed |= q.node != p.node;
                }
                None => match self.restore_by_preempting(i, p) {
                    Some((q, room)) => {
                        self.apply_room(room, &Reason::Preempted { by: self.wls[i].id.clone() });
                        self.put(i, q);
                        self.reason[i] = Some(Reason::CapacityReturned);
                        changed = true;
                    }
                    None => self.put(i, p),
                },
            }
        }
        (changed, freed)
    }

    /// 同じノードで、低い優先度の仕事を縮退・追い出しして戻す案（置くときと同じ順: 下げる → 追い出す）。
    /// 高い優先度の仕事が縮退したまま、同じノードで低い優先度の仕事が full で動き続けないように。
    fn restore_by_preempting(&self, i: usize, p: Place) -> Option<(Place, Room)> {
        let w = self.wls[i];
        if w.priority == Priority::Low || !self.load[p.node].members.iter().any(|&m| self.wls[m].priority < w.priority) {
            return None;
        }
        for l in 0..p.level {
            let choices: Vec<Option<usize>> = match (&w.req(l).gpu, p.gpu) {
                (None, g) => vec![g],
                (Some(gr), Some(g)) if gpu_ok(self.spec(p.node), g, gr, dev_pin(w, self.pins[i], true)) => vec![Some(g)],
                (Some(_), Some(_)) => vec![],
                (Some(_), None) => self.gpu_choices(i, p.node, l, false).collect(),
            };
            for g in choices {
                let need = Need { req: w.req(l), gpu: g, group: self.group(i) };
                if let Some(room) = self.make_room(p.node, Some(need), Some(w.priority)) {
                    return Some((Place { node: p.node, gpu: g, level: l }, room));
                }
            }
        }
        None
    }

    fn restore_same_node(&self, i: usize, p: Place) -> Option<Place> {
        let w = self.wls[i];
        let spec = self.spec(p.node);
        let headroom = self.inp.config.restore_headroom_permille;
        for l in 0..p.level {
            let req = w.req(l);
            let choices: Vec<Option<usize>> = match (&req.gpu, p.gpu) {
                (None, g) => vec![g],
                (Some(_), Some(g)) => vec![Some(g)],
                // 新しく GPU を選ぶのは新しく置くのと同じ（GPU の指定が効く）
                (Some(_), None) => self.gpu_choices(i, p.node, l, false).collect(),
            };
            for g in choices {
                if let (Some(gr), Some(gi)) = (&req.gpu, g)
                    && !gpu_ok(spec, gi, gr, dev_pin(w, self.pins[i], true))
                {
                    continue;
                }
                if self.load[p.node].u.fits(spec, req, g).is_err() {
                    continue;
                }
                let mut u = self.load[p.node].u.clone();
                u.add(spec, req, g);
                if u.within_headroom(spec, headroom) {
                    return Some(Place { node: p.node, gpu: g, level: l });
                }
            }
        }
        None
    }

    // ---- 4. 任意の移動 ----

    fn at_pin(&self, i: usize, p: Place) -> bool {
        let c = &self.wls[i].constraints;
        let spec = self.spec(p.node);
        c.node.as_ref().is_none_or(|n| *n == spec.id) && c.device.as_ref().is_none_or(|d| spec.gpu_index(d).is_some())
    }

    fn voluntary_moves(&mut self) -> bool {
        let cfg = self.inp.config;
        let now = self.inp.now_ms;
        let before = self.moves;
        let mut order: Vec<usize> = (0..self.wls.len()).filter(|&i| self.cur[i].is_some()).collect();
        order.sort_by_key(|&i| (Reverse(self.wls[i].priority), i));
        for i in order {
            if self.moves >= cfg.max_voluntary_moves {
                break;
            }
            let w = self.wls[i];
            let Some(p) = self.cur[i] else { continue };
            let pinned_here = w.is_pinned() && self.at_pin(i, p);
            let draining = self.spec(p.node).draining && !pinned_here;
            let pin_back = w.is_pinned()
                && !pinned_here
                && self.pins[i].usable
                && self.inp.assignments.get(&w.id).is_some_and(|a| now.saturating_sub(a.since_ms) >= cfg.restore_cooldown_ms);
            if !draining && !pin_back {
                continue;
            }
            self.take(i);
            let cands: Vec<usize> = self.candidates(i).iter().copied().filter(|&n| n != p.node).collect();
            match (self.min_level(i).min(p.level)..=p.level).find_map(|l| self.best_free(i, l, &cands, 0).map(|(n, g)| Place { node: n, gpu: g, level: l })) {
                Some(q) => {
                    self.put(i, q);
                    self.reason[i] = Some(if pin_back { Reason::PinReturned } else { Reason::Draining });
                    self.moves += 1;
                }
                None => self.put(i, p),
            }
        }
        self.moves > before
    }

    // ---- 5・6. 報告と決定 ----

    fn reasons(&self, i: usize) -> Vec<RejectCount> {
        let w = self.wls[i];
        let level = w.levels() - 1;
        let req = w.req(level);
        let mut m: BTreeMap<Reject, RejectCount> = BTreeMap::new();
        for n in 0..self.nodes.len() {
            let r = static_check(self.nodes[n], w, self.pins[i], false).err().or_else(|| {
                if self.group_conflict(n, i) {
                    return Some(Reject::AntiAffinity);
                }
                let spec = self.spec(n);
                let mut first = None;
                let mut fits_empty = false;
                for g in self.gpu_choices(i, n, level, false) {
                    match self.load[n].u.fits(spec, req, g) {
                        Ok(()) => return Some(Reject::Contended),
                        Err(e) => {
                            first.get_or_insert(e);
                        }
                    }
                    fits_empty |= fits_empty_node(spec, req, g);
                }
                // 候補の GPU が無い。空のノードなら収まるなら、埋まっているだけ
                let first = first.unwrap_or(Reject::NoDevice);
                Some(if fits_empty && first != Reject::Capture { Reject::Contended } else { first })
            });
            let r = r.unwrap_or(Reject::Contended);
            let e = m.entry(r).or_insert(RejectCount { reason: r, nodes: 0, examples: Vec::new() });
            e.nodes += 1;
            if e.examples.len() < RejectCount::MAX_EXAMPLES {
                e.examples.push(self.spec(n).id.clone());
            }
        }
        m.into_values().collect()
    }

    fn finish(self) -> Plan {
        let inp = self.inp;
        let mut out = Vec::new();
        for (i, w) in self.wls.iter().enumerate() {
            let a = inp.assignments.get(&w.id);
            let reason = || {
                self.reason[i].clone().unwrap_or(match self.cur[i] {
                    Some(p) if p.level > 0 => Reason::NoCapacity,
                    _ => Reason::Placed,
                })
            };
            match (a, self.cur[i]) {
                (None, None) => {}
                (Some(_), None) => out.push(Decision::Evict { workload: w.id.clone(), reason: reason() }),
                (a, Some(p)) => {
                    let spec = self.spec(p.node);
                    let device = p.gpu.map(|g| spec.gpus[g].id.clone());
                    let Some(a) = a else {
                        out.push(Decision::Assign {
                            workload: w.id.clone(),
                            node: spec.id.clone(),
                            device,
                            level: p.level,
                            token: 0,
                            reason: reason(),
                        });
                        continue;
                    };
                    let moved = a.node != spec.id
                        || (a.device.is_some() && a.device != device)
                        || (a.level == p.level && a.device != device);
                    if moved {
                        out.push(Decision::Move {
                            workload: w.id.clone(),
                            from: a.node.clone(),
                            node: spec.id.clone(),
                            device,
                            level: p.level,
                            token: 0,
                            reason: reason(),
                        });
                    } else if p.level > a.level {
                        out.push(Decision::Degrade { workload: w.id.clone(), level: p.level, device, reason: reason() });
                    } else if p.level < a.level {
                        out.push(Decision::Restore { workload: w.id.clone(), level: p.level, device, reason: reason() });
                    }
                }
            }
            if self.cur[i].is_none() {
                let rs = self.reasons(i);
                if inp.unschedulable.get(&w.id) != Some(&rs) || a.is_some() {
                    out.push(Decision::Unschedulable { workload: w.id.clone(), reasons: rs });
                }
            }
        }
        out.sort_by(|x, y| x.rank().cmp(&y.rank()).then_with(|| x.workload().cmp(y.workload())));
        let mut t = inp.token_base;
        for d in &mut out {
            if let Decision::Assign { token, .. } | Decision::Move { token, .. } = d {
                t += 1;
                *token = t;
            }
        }
        Plan { token_base: inp.token_base, now_ms: inp.now_ms, decisions: out }
    }
}
