//! 決定的なシミュレーション（ADR-0013 の検証）。種（seed）ごとに、GPU を持つノード・乱択の仕事・
//! ノードと GPU の停止と復帰・手動の割り当て・排出・容量の申告し直しを回し、毎回の後に不変条件を確かめる。
//! 不変条件の検査は配置係のコードを使わずに書き直している（同じ誤りを見逃さないため）。

use senca_grid::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len() as u64) as usize]
    }
}

const COOLDOWN: i64 = 20_000;
const TIMEOUT: i64 = 5_000;

#[derive(Clone, Copy)]
struct Profile {
    /// 仕事の重さ（ノードの容量に対する 1 つの仕事の大きさの上限、千分率）。
    heavy: u64,
    /// 乱れ（停止・故障・排出）を起こすか。
    chaos: bool,
    /// 制約（strict の pin・ラベル）を使うか。
    constraints: bool,
}

struct World {
    rng: Rng,
    state: GridState,
    det: FailureDetector,
    now: i64,
    cfg: PlanConfig,
    specs: BTreeMap<NodeId, NodeSpec>,
    phys_up: BTreeMap<NodeId, bool>,
    phys_dev: BTreeMap<(NodeId, String), bool>,
    max_token: HashMap<WorkloadId, u64>,
    next_id: u64,
    profile: Profile,
    stats: Stats,
    seed: u64,
}

#[derive(Default, Debug)]
struct Stats {
    rounds: u64,
    assign: u64,
    moves: u64,
    degrade: u64,
    restore: u64,
    evict: u64,
    unsched: u64,
    plan_us_max: u128,
}

impl World {
    fn new(seed: u64, profile: Profile) -> Self {
        let mut rng = Rng(seed);
        let now = 1_000_000;
        let mut w = World {
            state: GridState::default(),
            det: FailureDetector::new(TIMEOUT, now),
            now,
            cfg: PlanConfig { restore_cooldown_ms: COOLDOWN, ..PlanConfig::default() },
            specs: BTreeMap::new(),
            phys_up: BTreeMap::new(),
            phys_dev: BTreeMap::new(),
            max_token: HashMap::new(),
            next_id: 0,
            profile,
            stats: Stats::default(),
            rng: Rng(0),
            seed,
        };
        let n = rng.range(3, 10);
        w.rng = rng;
        for i in 0..n {
            let spec = w.gen_node(i);
            w.specs.insert(spec.id.clone(), spec.clone());
            w.phys_up.insert(spec.id.clone(), true);
            for g in &spec.gpus {
                w.phys_dev.insert((spec.id.clone(), g.id.clone()), true);
            }
            for c in &spec.capture {
                w.phys_dev.insert((spec.id.clone(), c.id.clone()), true);
            }
            w.state.apply(GridCommand::RegisterNode { spec, now_ms: now }).unwrap();
        }
        let k = w.rng.range(5, 40);
        for _ in 0..k {
            w.add_workload();
        }
        w
    }

    fn gen_node(&mut self, i: u64) -> NodeSpec {
        let r = &mut self.rng;
        let mut roles: BTreeSet<Role> = [Role::Media].into();
        if i < 3 {
            roles.insert(Role::Control);
        }
        if i > 0 && r.chance(8) {
            roles = [Role::Control].into();
        }
        let gpus = (0..r.below(3))
            .map(|g| Gpu {
                id: format!("gpu{g}"),
                vendor: r.pick(&["nvidia", "intel"]).to_string(),
                encode_sessions: r.range(3, 10) as u32,
                decode_sessions: r.range(3, 10) as u32,
                memory_mb: r.range(4, 16) * 1000,
                healthy: true,
                disabled: false,
            })
            .collect();
        let capture = if r.chance(25) {
            vec![CaptureDevice { id: format!("cap-n{i:02}"), kind: "sdi".into(), healthy: true, disabled: false }]
        } else {
            vec![]
        };
        NodeSpec {
            id: format!("n{i:02}"),
            roles,
            capacity: Resources {
                cpu_millis: r.range(8, 32) * 1000,
                memory_mb: r.range(16, 64) * 1000,
                net_in_mbps: r.range(1, 10) * 1000,
                net_out_mbps: r.range(1, 10) * 1000,
            },
            gpus,
            capture,
            labels: [("zone".to_string(), r.pick(&["a", "b", "c"]).to_string())].into(),
            draining: false,
        }
    }

    fn gen_workload(&mut self) -> Workload {
        self.next_id += 1;
        let id = format!("w{:04}", self.next_id);
        let p = self.profile;
        let specs: Vec<NodeSpec> = self.specs.values().cloned().collect();
        let r = &mut self.rng;
        let h = p.heavy;
        let priority = *r.pick(&[Priority::Low, Priority::Low, Priority::Normal, Priority::Normal, Priority::High, Priority::Critical]);
        let full = Resources {
            cpu_millis: r.range(100, 8 * h),
            memory_mb: r.range(100, 16 * h),
            net_in_mbps: r.range(5, h),
            net_out_mbps: r.range(5, h),
        };
        let gpu = r.chance(40).then(|| GpuReq {
            encode: r.range(1, 3) as u32,
            decode: r.range(0, 2) as u32,
            memory_mb: r.range(0, 3) * 1000,
            vendor: r.chance(30).then(|| r.pick(&["nvidia", "intel"]).to_string()),
        });
        let mut constraints = Constraints::default();
        let mut capture = None;
        if r.chance(6) {
            let with_cap: Vec<&NodeSpec> = specs.iter().filter(|s| !s.capture.is_empty()).collect();
            if !with_cap.is_empty() {
                let s = *r.pick(&with_cap);
                capture = Some(s.capture[0].id.clone());
            }
        }
        if r.chance(12) {
            let s = r.pick(&specs);
            constraints.node = Some(s.id.clone());
            constraints.pin_policy = if p.constraints && r.chance(50) { PinPolicy::Strict } else { PinPolicy::Prefer };
            if gpu.is_some() && !s.gpus.is_empty() && r.chance(50) {
                constraints.device = Some(r.pick(&s.gpus).id.clone());
            }
        }
        if p.constraints && r.chance(15) {
            constraints.labels = vec![LabelSelector::In { key: "zone".into(), values: vec![r.pick(&["a", "b", "c"]).to_string()] }];
        }
        if r.chance(20) {
            constraints.anti_affinity = Some(format!("g{}", r.below(4)));
        }
        let req = Requirements { resources: full, gpu: gpu.clone(), capture: capture.clone() };
        let mut degrade = Vec::new();
        let steps = r.below(4);
        for s in 0..steps {
            let f = [700, 400, 150][s as usize];
            let scale = |x: u64| (x * f / 1000).max(1);
            let last = s + 1 == steps;
            let g = if last && r.chance(50) {
                None
            } else {
                gpu.as_ref().map(|g| GpuReq { encode: g.encode, decode: g.decode, memory_mb: g.memory_mb * f / 1000, vendor: g.vendor.clone() })
            };
            degrade.push(Level {
                label: ["no_translation", "720p", "audio_only"][s as usize].into(),
                requirements: Requirements {
                    resources: Resources {
                        cpu_millis: scale(full.cpu_millis),
                        memory_mb: scale(full.memory_mb),
                        net_in_mbps: scale(full.net_in_mbps),
                        net_out_mbps: scale(full.net_out_mbps),
                    },
                    gpu: g,
                    capture: capture.clone(),
                },
            });
        }
        Workload {
            id,
            tenant: "t".into(),
            kind: *r.pick(&[WorkloadKind::Ingest, WorkloadKind::Transcode, WorkloadKind::Recording, WorkloadKind::Room]),
            requirements: req,
            degrade,
            constraints,
            priority,
            locality: if r.chance(30) { vec![format!("s{}", r.below(5))] } else { vec![] },
            placement: if r.chance(20) { Placement::Binpack } else { Placement::Spread },
        }
    }

    fn add_workload(&mut self) {
        let w = self.gen_workload();
        self.state.apply(GridCommand::PutWorkload { workload: w }).unwrap();
    }

    fn events(&mut self) {
        let ids: Vec<NodeId> = self.specs.keys().cloned().collect();
        let p = self.profile;
        let r = &mut self.rng;
        let mut cmds = Vec::new();
        if p.chaos {
            if r.chance(10) {
                let n = r.pick(&ids).clone();
                self.phys_up.insert(n, false);
            }
            if r.chance(12) {
                let n = r.pick(&ids).clone();
                if !self.phys_up[&n] {
                    self.phys_up.insert(n.clone(), true);
                    // 再起動したノードは登録し直す（心拍だけで戻ることもある）
                    if r.chance(50) {
                        let mut spec = self.specs[&n].clone();
                        spec.draining = self.state.nodes[&n].spec.draining;
                        for g in &mut spec.gpus {
                            g.healthy = self.phys_dev[&(n.clone(), g.id.clone())];
                        }
                        for c in &mut spec.capture {
                            c.healthy = self.phys_dev[&(n.clone(), c.id.clone())];
                        }
                        cmds.push(GridCommand::RegisterNode { spec, now_ms: self.now });
                    }
                }
            }
            if r.chance(10) {
                let devs: Vec<(NodeId, String)> = self.phys_dev.keys().cloned().collect();
                if !devs.is_empty() {
                    let d = r.pick(&devs).clone();
                    let v = !self.phys_dev[&d];
                    self.phys_dev.insert(d, v);
                }
            }
            if r.chance(4) {
                let n = r.pick(&ids).clone();
                let d = !self.state.nodes[&n].spec.draining;
                cmds.push(GridCommand::SetDraining { node: n, draining: d });
            }
            if r.chance(3) {
                // 容量を減らして申告し直す（超過の修理）、または元に戻す
                let n = r.pick(&ids).clone();
                if self.phys_up[&n] {
                    let mut spec = self.state.nodes[&n].spec.clone();
                    let orig = &self.specs[&n];
                    spec.capacity = if spec.capacity == orig.capacity {
                        Resources { cpu_millis: orig.capacity.cpu_millis / 2, ..orig.capacity }
                    } else {
                        orig.capacity
                    };
                    cmds.push(GridCommand::RegisterNode { spec, now_ms: self.now });
                }
            }
        }
        let n_w = self.state.workloads.len();
        if !p.chaos {
            for c in cmds {
                self.state.apply(c).unwrap();
            }
            return;
        }
        if r.chance(15) && n_w < 80 {
            for c in cmds.drain(..) {
                self.state.apply(c).unwrap();
            }
            self.add_workload();
        }
        let r = &mut self.rng;
        if r.chance(8) && n_w > 0 {
            let ids: Vec<WorkloadId> = self.state.workloads.keys().cloned().collect();
            let id = r.pick(&ids).clone();
            cmds.push(GridCommand::DeleteWorkload { id });
        }
        if r.chance(4) && n_w > 0 {
            // 優先度・pin を変える
            let ids: Vec<WorkloadId> = self.state.workloads.keys().cloned().collect();
            let id = r.pick(&ids).clone();
            let mut w = self.state.workloads[&id].clone();
            w.priority = *r.pick(&[Priority::Low, Priority::Normal, Priority::High, Priority::Critical]);
            if r.chance(50) {
                w.constraints.node = if w.constraints.node.is_some() { None } else { Some(r.pick(&ids_nodes(&self.specs)).clone()) };
                w.constraints.device = None;
            }
            cmds.push(GridCommand::PutWorkload { workload: w });
        }
        for c in cmds {
            self.state.apply(c).unwrap();
        }
    }

    fn heartbeats(&mut self) {
        for (id, up) in &self.phys_up {
            if !*up {
                continue;
            }
            let spec = &self.specs[id];
            let devices = spec
                .gpus
                .iter()
                .map(|g| &g.id)
                .chain(spec.capture.iter().map(|c| &c.id))
                .map(|d| DeviceHealth { id: d.clone(), healthy: self.phys_dev[&(id.clone(), d.clone())] })
                .collect();
            let streams = (0..5).filter(|s| (s + id.len()) % 3 == 0).map(|s| format!("s{s}")).collect();
            self.det.heartbeat(Heartbeat { node: id.clone(), devices, streams, ..Default::default() }, self.now);
        }
    }

    fn round(&mut self) {
        self.stats.rounds += 1;
        self.now += self.rng.range(500, 3000) as i64;
        self.events();
        self.heartbeats();
        for c in self.det.tick(&self.state, self.now) {
            self.state.apply(c).expect("detector command applies");
        }
        let streams = self.det.stream_nodes_at(self.now);
        let t = Instant::now();
        let plan = self.state.plan(self.now, self.state.fencing_counter, &streams, &self.cfg);
        self.stats.plan_us_max = self.stats.plan_us_max.max(t.elapsed().as_micros());
        let prev = self.state.clone();
        if let Err(e) = self.state.apply(plan.clone().into_command()) {
            if let GridError::InvalidDecision { index, .. } = &e {
                let d = &plan.decisions[*index];
                eprintln!("workload: {:?}\nprev asg: {:?}", self.state.workloads.get(d.workload()), self.state.assignments.get(d.workload()));
                if let Decision::Move { node, .. } | Decision::Assign { node, .. } = d {
                    eprintln!("target: {:?}", self.state.nodes.get(node));
                }
            }
            panic!("seed {} plan must apply: {e}\n{:#?}", self.seed, plan.decisions);
        }
        for d in &plan.decisions {
            match d {
                Decision::Assign { .. } => self.stats.assign += 1,
                Decision::Move { .. } => self.stats.moves += 1,
                Decision::Degrade { .. } => self.stats.degrade += 1,
                Decision::Restore { .. } => self.stats.restore += 1,
                Decision::Evict { .. } => self.stats.evict += 1,
                Decision::Unschedulable { .. } => self.stats.unsched += 1,
            }
        }
        self.check(&prev, &plan);
        // 不動点: 同じ時刻にもう一度計画しても何も変えない
        let again = self.state.plan(self.now, self.state.fencing_counter, &streams, &self.cfg);
        if !again.is_empty() {
            let ids: BTreeSet<&str> = again.decisions.iter().map(|d| d.workload()).collect();
            for id in &ids {
                eprintln!("prev {id}: {:?}\n  asg {:?}\n  uns {:?}", prev.workloads.get(*id), prev.assignments.get(*id), prev.unschedulable.get(*id));
            }
            eprintln!("plan: {:#?}", plan.decisions);
            eprintln!("nodes: {:#?}", self.state.nodes.values().map(|n| (&n.spec.id, n.status.up, n.spec.draining, n.spec.capacity, &n.spec.gpus)).collect::<Vec<_>>());
            eprintln!("assignments: {:#?}", self.state.assignments.values().map(|a| (&a.workload, &a.node, a.level, &a.device, self.state.workloads[&a.workload].priority, self.state.workloads[&a.workload].req(a.level).resources)).collect::<Vec<_>>());
            panic!("seed {} not a fixpoint at {}: {:#?}", self.seed, self.now, again.decisions);
        }
    }

    // ---- 不変条件（配置係のコードを使わない） ----

    fn check(&mut self, prev: &GridState, plan: &Plan) {
        let s = &self.state;
        // 容量・装置・反親和性・キャプチャ機器
        let mut per_node: BTreeMap<&str, Vec<&Assignment>> = BTreeMap::new();
        for a in s.assignments.values() {
            per_node.entry(a.node.as_str()).or_default().push(a);
        }
        for (nid, list) in &per_node {
            let node = s.nodes.get(*nid).unwrap_or_else(|| panic!("assignment to unknown node {nid}"));
            assert!(node.status.up, "assignment to dead node {nid}");
            assert!(node.spec.roles.contains(&Role::Media), "assignment to non-media node {nid}");
            let mut used = Resources::default();
            let mut gpu_use: HashMap<&str, (u32, u32, u64)> = HashMap::new();
            let mut groups = BTreeSet::new();
            let mut caps = BTreeSet::new();
            for a in list {
                let w = &s.workloads[&a.workload];
                assert!(a.level < w.levels());
                let req = w.req(a.level);
                used = used.plus(&req.resources);
                if let Some(gr) = &req.gpu {
                    let d = a.device.as_deref().expect("gpu level has device");
                    let g = node.spec.gpus.iter().find(|g| g.id == d).expect("device exists");
                    assert!(g.healthy, "workload {} on unhealthy gpu {nid}/{d}", w.id);
                    if let Some(v) = &gr.vendor {
                        assert!(v.eq_ignore_ascii_case(&g.vendor));
                    }
                    let e = gpu_use.entry(d).or_default();
                    e.0 += gr.encode;
                    e.1 += gr.decode;
                    e.2 += gr.memory_mb;
                }
                if let Some(c) = &req.capture {
                    assert!(node.spec.capture.iter().any(|x| &x.id == c && x.healthy), "capture device");
                    assert!(caps.insert(c.clone()), "capture device shared");
                }
                if let Some(g) = &w.constraints.anti_affinity {
                    assert!(groups.insert(g.clone()), "anti-affinity {g} violated on {nid}");
                }
                assert!(w.constraints.labels.iter().all(|l| l.matches(&node.spec.labels)), "labels");
                let c = &w.constraints;
                if c.pin_policy == PinPolicy::Strict {
                    if let Some(p) = &c.node {
                        assert_eq!(p, *nid, "strict node pin");
                    }
                    if let (Some(dp), Some(_)) = (&c.device, &req.gpu) {
                        assert_eq!(a.device.as_ref(), Some(dp), "strict device pin");
                    }
                }
            }
            assert!(used.within(&node.spec.capacity), "overcommit on {nid}: {used:?} > {:?}", node.spec.capacity);
            for (d, (e, dc, m)) in gpu_use {
                let g = node.spec.gpus.iter().find(|g| g.id == d).unwrap();
                assert!(e <= g.encode_sessions && dc <= g.decode_sessions && m <= g.memory_mb, "gpu overcommit {nid}/{d}");
            }
        }
        // token: Assign・Move は新しく、通し番号は単調
        assert!(s.fencing_counter >= prev.fencing_counter);
        for d in &plan.decisions {
            let wid = d.workload().to_string();
            if let Some(t) = d.token() {
                let m = self.max_token.entry(wid.clone()).or_insert(0);
                assert!(t > *m, "token not increasing for {wid}");
                *m = t;
                assert!(t > prev.fencing_counter && t <= s.fencing_counter);
            }
            let pw = &s.workloads[&wid];
            match d {
                // 行ったり来たりの抑え: 戻すのは冷却の後だけ
                Decision::Restore { .. } | Decision::Move { reason: Reason::CapacityReturned, .. } => {
                    let pa = &prev.assignments[&wid];
                    assert!(self.now - pa.level_since_ms >= COOLDOWN, "restore within cooldown for {wid}: {d:?}");
                }
                Decision::Move { reason: Reason::PinReturned, .. } => {
                    let pa = &prev.assignments[&wid];
                    assert!(self.now - pa.since_ms >= COOLDOWN, "pin return within cooldown for {wid}");
                }
                // 置き直しでも、冷却の間は元の段より良くしない
                Decision::Move { level, .. } => {
                    let pa = &prev.assignments[&wid];
                    if self.now - pa.level_since_ms < COOLDOWN && pa.level < pw.levels() {
                        assert!(*level >= pa.level, "level improved within cooldown by move for {wid}: {d:?}");
                    }
                }
                _ => {}
            }
            // 横取りは高い優先度のためだけ
            if let Decision::Degrade { reason: Reason::Preempted { by }, .. } | Decision::Evict { reason: Reason::Preempted { by }, .. } = d {
                assert!(s.workloads[by].priority > pw.priority, "preempted by equal/lower priority");
                assert_ne!(pw.priority, Priority::Critical);
            }
            // 手動の割り当ては、その対象が健康な間は動かさない
            if let Decision::Move { from, node, .. } = d
                && from != node
                && let (Some(pa), Some(ow)) = (prev.assignments.get(&wid), prev.workloads.get(&wid))
                && ow == pw
                && ow.is_pinned()
                && pin_target_healthy(s, pw)
            {
                let c = &pw.constraints;
                let at_pin = c.node.as_ref().is_none_or(|n| n == from)
                    && c.device.as_ref().is_none_or(|dv| pa.device.as_ref().is_none_or(|x| x == dv));
                let from_node = &s.nodes[from];
                let still_ok = from_node.status.up
                    && c.labels.iter().all(|l| l.matches(&from_node.spec.labels))
                    && pa.device.as_ref().is_none_or(|x| from_node.spec.gpus.iter().any(|g| &g.id == x && g.healthy));
                assert!(!(at_pin && still_ok && c.node.is_some()), "pinned workload {wid} moved off healthy pin: {d:?}");
            }
        }
        // critical は置けるなら置く（pin の無いもの。他の critical だけを残したノードで最下段が入るか）
        for w in s.workloads.values() {
            if w.priority != Priority::Critical || s.assignments.contains_key(&w.id) || w.is_pinned() {
                continue;
            }
            for node in s.nodes.values() {
                assert!(
                    !critical_fits(s, w, node),
                    "critical {} unplaced but fits on {} (reasons {:?})",
                    w.id,
                    node.spec.id,
                    s.unschedulable.get(&w.id)
                );
            }
        }
        // 置けない仕事は報告されている
        for w in s.workloads.values() {
            assert!(s.assignments.contains_key(&w.id) != s.unschedulable.contains_key(&w.id), "status of {}", w.id);
        }
    }
}

fn ids_nodes(specs: &BTreeMap<NodeId, NodeSpec>) -> Vec<NodeId> {
    specs.keys().cloned().collect()
}

fn pin_target_healthy(s: &GridState, w: &Workload) -> bool {
    let c = &w.constraints;
    let ok = |n: &Node| {
        n.status.up && n.spec.roles.contains(&Role::Media) && c.device.as_ref().is_none_or(|d| n.spec.gpus.iter().any(|g| &g.id == d && g.healthy))
    };
    match &c.node {
        Some(id) => s.nodes.get(id).is_some_and(ok),
        None => s.nodes.values().any(ok),
    }
}

/// 他の critical だけを残したノードに、w の最下段が入るか。
fn critical_fits(s: &GridState, w: &Workload, node: &Node) -> bool {
    if !node.accepts_new() || !w.constraints.labels.iter().all(|l| l.matches(&node.spec.labels)) {
        return false;
    }
    let others: Vec<&Assignment> = s
        .assignments
        .values()
        .filter(|a| a.node == node.spec.id && s.workloads[&a.workload].priority == Priority::Critical)
        .collect();
    if let Some(g) = &w.constraints.anti_affinity
        && others.iter().any(|a| s.workloads[&a.workload].constraints.anti_affinity.as_ref() == Some(g))
    {
        return false;
    }
    let req = w.req(w.levels() - 1);
    let mut used = req.resources;
    for a in &others {
        used = used.plus(&s.workloads[&a.workload].req(a.level).resources);
    }
    if !used.within(&node.spec.capacity) {
        return false;
    }
    if let Some(c) = &req.capture {
        if !node.spec.capture.iter().any(|x| &x.id == c && x.healthy) {
            return false;
        }
        if others.iter().any(|a| s.workloads[&a.workload].req(a.level).capture.as_ref() == Some(c)) {
            return false;
        }
    }
    let Some(gr) = &req.gpu else { return true };
    node.spec.gpus.iter().any(|g| {
        if !g.healthy || gr.vendor.as_ref().is_some_and(|v| !v.eq_ignore_ascii_case(&g.vendor)) {
            return false;
        }
        let (mut e, mut d, mut m) = (gr.encode, gr.decode, gr.memory_mb);
        for a in &others {
            if a.device.as_ref() == Some(&g.id)
                && let Some(x) = &s.workloads[&a.workload].req(a.level).gpu
            {
                e += x.encode;
                d += x.decode;
                m += x.memory_mb;
            }
        }
        e <= g.encode_sessions && d <= g.decode_sessions && m <= g.memory_mb
    })
}

/// 種の数（`SENCA_GRID_SEEDS` で増やせる）。
fn seeds(default: u64) -> u64 {
    std::env::var("SENCA_GRID_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// x の full の段が、ノード n に（`preempt` なら優先度の低い仕事を除いて）収まるか。
fn fits_full(s: &GridState, x: &Workload, n: &Node, preempt: bool) -> bool {
    let others: Vec<(&Assignment, &Workload)> = s
        .assignments
        .values()
        .filter(|a| a.node == n.spec.id && a.workload != x.id)
        .map(|a| (a, &s.workloads[&a.workload]))
        .filter(|(_, o)| !preempt || x.priority == Priority::Low || o.priority >= x.priority)
        .collect();
    let req = &x.requirements;
    let mut used = req.resources;
    for (a, o) in &others {
        used = used.plus(&o.req(a.level).resources);
    }
    if !used.within(&n.spec.capacity) {
        return false;
    }
    let Some(gr) = &req.gpu else { return true };
    n.spec.gpus.iter().any(|g| {
        if !g.healthy || gr.vendor.as_ref().is_some_and(|v| !v.eq_ignore_ascii_case(&g.vendor)) {
            return false;
        }
        let (mut e, mut d, mut m) = (gr.encode, gr.decode, gr.memory_mb);
        for (a, o) in &others {
            if a.device.as_ref() == Some(&g.id)
                && let Some(y) = &o.req(a.level).gpu
            {
                e += y.encode;
                d += y.decode;
                m += y.memory_mb;
            }
        }
        e <= g.encode_sessions && d <= g.decode_sessions && m <= g.memory_mb
    })
}

fn run(seed: u64, profile: Profile, rounds: u64) -> World {
    let mut w = World::new(seed, profile);
    for _ in 0..rounds {
        w.round();
    }
    w
}

/// すべてを戻し、冷却より長く回す。
fn recover(w: &mut World) {
    let ids: Vec<NodeId> = w.specs.keys().cloned().collect();
    for id in &ids {
        w.phys_up.insert(id.clone(), true);
        let spec = w.specs[id].clone();
        w.state.apply(GridCommand::RegisterNode { spec, now_ms: w.now }).unwrap();
    }
    for v in w.phys_dev.values_mut() {
        *v = true;
    }
    w.profile.chaos = false;
    let until = w.now + 4 * COOLDOWN;
    while w.now < until {
        w.round();
    }
}

#[test]
fn sim_chaos() {
    let mut total = Stats::default();
    let seeds = seeds(300);
    let first: u64 = std::env::var("SENCA_GRID_FIRST").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    for seed in first..first + seeds {
        let w = run(seed, Profile { heavy: 1000, chaos: true, constraints: true }, 150);
        total.rounds += w.stats.rounds;
        total.assign += w.stats.assign;
        total.moves += w.stats.moves;
        total.degrade += w.stats.degrade;
        total.restore += w.stats.restore;
        total.evict += w.stats.evict;
        total.unsched += w.stats.unsched;
        total.plan_us_max = total.plan_us_max.max(w.stats.plan_us_max);
    }
    eprintln!("chaos {seeds} seeds: {total:?}");
    assert!(total.degrade > 0 && total.restore > 0 && total.evict > 0 && total.moves > 0, "scenarios exercised: {total:?}");
}

/// 乱れの後、資源が戻れば（軽い負荷なら）すべて full の段へ戻る。
#[test]
fn sim_eventual_restoration() {
    let mut degraded_seen = 0;
    for seed in 1000..1000 + seeds(150) {
        let mut w = run(seed, Profile { heavy: 300, chaos: true, constraints: false }, 120);
        degraded_seen += w.stats.degrade;
        // 軽くする: 全体の要求が容量の 40% を超えるなら仕事を減らす
        loop {
            let cap = w.state.nodes.values().filter(|n| n.spec.roles.contains(&Role::Media)).fold(Resources::default(), |a, n| a.plus(&n.spec.capacity));
            let need = w.state.workloads.values().fold(Resources::default(), |a, x| a.plus(&x.requirements.resources));
            let gpu_need: u32 = w.state.workloads.values().filter_map(|x| x.requirements.gpu.as_ref()).map(|g| g.encode.max(g.decode)).sum();
            let gpu_cap: u32 = w.specs.values().flat_map(|s| &s.gpus).map(|g| g.encode_sessions.min(g.decode_sessions)).sum();
            let light = |n: u64, c: u64| n * 10 <= c * 4;
            if light(need.cpu_millis, cap.cpu_millis)
                && light(need.memory_mb, cap.memory_mb)
                && light(need.net_in_mbps, cap.net_in_mbps)
                && light(need.net_out_mbps, cap.net_out_mbps)
                && gpu_need * 10 <= gpu_cap * 4
            {
                break;
            }
            let id = w.state.workloads.keys().next_back().unwrap().clone();
            w.state.apply(GridCommand::DeleteWorkload { id }).unwrap();
        }
        // 反親和性の組・キャプチャ機器・pin の GPU が足りないものは除く（容量では解けない）
        for id in w.state.workloads.keys().cloned().collect::<Vec<_>>() {
            let x = &w.state.workloads[&id];
            if x.requirements.capture.is_some() || x.constraints.anti_affinity.is_some() || x.constraints.device.is_some() || x.requirements.gpu.is_some() && x.constraints.node.is_some() {
                w.state.apply(GridCommand::DeleteWorkload { id }).unwrap();
            }
        }
        for id in w.state.nodes.keys().cloned().collect::<Vec<_>>() {
            w.state.apply(GridCommand::SetDraining { node: id, draining: false }).unwrap();
        }
        recover(&mut w);
        for x in w.state.workloads.values() {
            // 配置係にできること: 置くときは低い優先度を押しのけてよい。縮退して置かれているものは、
            // 同じノードでは低い優先度を押しのけ、他のノードへは空きにだけ移れる。
            let cur = w.state.assignments.get(&x.id).map(|a| a.node.clone());
            let fits_anywhere = w.state.nodes.values().any(|n| {
                if !n.accepts_new() && cur.as_ref() != Some(&n.spec.id) {
                    return false;
                }
                if x.constraints.node.as_ref().is_some_and(|p| *p != n.spec.id) {
                    return false;
                }
                let preempt = cur.is_none() || cur.as_ref() == Some(&n.spec.id);
                fits_full(&w.state, x, n, preempt)
            });
            if !fits_anywhere {
                continue;
            }
            let a = w.state.assignments.get(&x.id);
            if !a.is_some_and(|a| a.level == 0) {
                eprintln!("now {} workload {:?}", w.now, x);
                for n in w.state.nodes.values() {
                    let on: Vec<_> = w.state.assignments.values().filter(|a| a.node == n.spec.id).map(|a| (a.workload.clone(), a.level, w.state.workloads[&a.workload].req(a.level).resources)).collect();
                    eprintln!("node {} up {} drain {} cap {:?} gpus {:?}\n   {:?}", n.spec.id, n.status.up, n.spec.draining, n.spec.capacity, n.spec.gpus, on);
                }
            }
            assert!(
                a.is_some_and(|a| a.level == 0),
                "seed {seed}: {} not restored: {:?} {:?}",
                x.id,
                a,
                w.state.unschedulable.get(&x.id)
            );
        }
    }
    assert!(degraded_seen > 0);
}

/// 同じ種からは同じ結果（状態を JSON で比べる）。
#[test]
fn sim_deterministic() {
    for seed in [7, 42, 99] {
        let p = Profile { heavy: 1000, chaos: true, constraints: true };
        let a = run(seed, p, 80);
        let b = run(seed, p, 80);
        assert_eq!(serde_json::to_string(&a.state).unwrap(), serde_json::to_string(&b.state).unwrap());
    }
}

// ---- 規模の測定（`cargo test -p senca-grid --release --test sim -- --ignored --nocapture`） ----

fn big_world(nodes: usize, workloads: usize, seed: u64) -> (GridState, PlanConfig) {
    let mut rng = Rng(seed);
    let mut s = GridState::default();
    for i in 0..nodes {
        let gpus = (0..(i % 3)).map(|g| Gpu {
            id: format!("gpu{g}"),
            vendor: "nvidia".into(),
            encode_sessions: 16,
            decode_sessions: 16,
            memory_mb: 16_000,
            healthy: true,
            disabled: false,
        });
        let spec = NodeSpec {
            id: format!("n{i:05}"),
            roles: [Role::Media].into(),
            capacity: Resources { cpu_millis: 32_000, memory_mb: 64_000, net_in_mbps: 10_000, net_out_mbps: 10_000 },
            gpus: gpus.collect(),
            capture: vec![],
            labels: [("zone".to_string(), format!("z{}", i % 4))].into(),
            draining: false,
        };
        s.apply(GridCommand::RegisterNode { spec, now_ms: 0 }).unwrap();
    }
    for i in 0..workloads {
        let gpu = rng.chance(30).then_some(GpuReq { encode: 1, decode: 1, memory_mb: 500, vendor: None });
        let full = Resources {
            cpu_millis: rng.range(500, 4000),
            memory_mb: rng.range(200, 4000),
            net_in_mbps: rng.range(5, 100),
            net_out_mbps: rng.range(5, 300),
        };
        let half = Resources { cpu_millis: full.cpu_millis / 2, memory_mb: full.memory_mb / 2, net_in_mbps: full.net_in_mbps / 2, net_out_mbps: full.net_out_mbps / 2 };
        let mut c = Constraints::default();
        if rng.chance(10) {
            c.labels = vec![LabelSelector::Eq { key: "zone".into(), value: format!("z{}", rng.below(4)) }];
        }
        if rng.chance(10) {
            c.anti_affinity = Some(format!("g{}", rng.below(500)));
        }
        let w = Workload {
            id: format!("w{i:06}"),
            tenant: "t".into(),
            kind: WorkloadKind::Transcode,
            requirements: Requirements { resources: full, gpu: gpu.clone(), capture: None },
            degrade: vec![Level { label: "720p".into(), requirements: Requirements { resources: half, gpu, capture: None } }],
            constraints: c,
            priority: *rng.pick(&[Priority::Low, Priority::Normal, Priority::High, Priority::Critical]),
            locality: vec![],
            placement: Placement::Spread,
        };
        s.apply(GridCommand::PutWorkload { workload: w }).unwrap();
    }
    (s, PlanConfig::default())
}

fn timed(s: &mut GridState, cfg: &PlanConfig, now: i64, label: &str) -> usize {
    let streams = BTreeMap::new();
    let t = Instant::now();
    let p = s.plan(now, s.fencing_counter, &streams, cfg);
    let dt = t.elapsed();
    let n = p.decisions.len();
    let t = Instant::now();
    s.apply(p.into_command()).unwrap();
    eprintln!("{label}: plan {:?}, apply {:?}, {n} decisions", dt, t.elapsed());
    n
}

#[test]
#[ignore]
fn bench_scale() {
    for (nodes, wls) in [(100, 1_000), (1_000, 10_000), (1_000, 25_000)] {
        eprintln!("== {nodes} nodes x {wls} workloads");
        let (mut s, cfg) = big_world(nodes, wls, 1);
        timed(&mut s, &cfg, 1_000, "initial placement");
        assert_eq!(timed(&mut s, &cfg, 2_000, "steady state"), 0);
        // 10% のノードが止まる
        for i in (0..nodes).step_by(10) {
            s.apply(GridCommand::NodeDown { node: format!("n{i:05}"), now_ms: 3_000, last_heartbeat_ms: 0 }).unwrap();
        }
        timed(&mut s, &cfg, 3_000, "10% nodes down");
        let deg = s.assignments.values().filter(|a| a.level > 0).count();
        eprintln!("   assigned {} degraded {deg} unschedulable {}", s.assignments.len(), s.unschedulable.len());
        for i in (0..nodes).step_by(10) {
            s.apply(GridCommand::NodeUp { node: format!("n{i:05}"), now_ms: 4_000 }).unwrap();
        }
        timed(&mut s, &cfg, 100_000, "recovery (restore)");
        let deg = s.assignments.values().filter(|a| a.level > 0).count();
        eprintln!("   assigned {} degraded {deg} unschedulable {}", s.assignments.len(), s.unschedulable.len());
    }
}
