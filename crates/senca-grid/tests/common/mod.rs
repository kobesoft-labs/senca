#![allow(dead_code)]

use senca_grid::*;
use std::collections::{BTreeMap, BTreeSet};

pub fn res(cpu: u64, mem: u64) -> Resources {
    Resources { cpu_millis: cpu, memory_mb: mem, net_in_mbps: 0, net_out_mbps: 0 }
}

pub fn node(id: &str, cpu: u64, mem: u64) -> NodeSpec {
    NodeSpec {
        id: id.into(),
        roles: [Role::Media].into(),
        capacity: Resources { cpu_millis: cpu, memory_mb: mem, net_in_mbps: 10_000, net_out_mbps: 10_000 },
        gpus: vec![],
        capture: vec![],
        labels: BTreeMap::new(),
        draining: false,
    }
}

pub fn gpu(id: &str, vendor: &str, enc: u32) -> Gpu {
    Gpu { id: id.into(), vendor: vendor.into(), encode_sessions: enc, decode_sessions: enc, memory_mb: 16_000, healthy: true, disabled: false }
}

pub fn wl(id: &str, cpu: u64, mem: u64, prio: Priority) -> Workload {
    Workload {
        id: id.into(),
        tenant: "t".into(),
        kind: WorkloadKind::Transcode,
        requirements: Requirements { resources: res(cpu, mem), gpu: None, capture: None },
        degrade: vec![],
        constraints: Constraints::default(),
        priority: prio,
        locality: vec![],
        placement: Placement::Spread,
    }
}

pub fn level(label: &str, cpu: u64, mem: u64) -> Level {
    Level { label: label.into(), requirements: Requirements { resources: res(cpu, mem), gpu: None, capture: None } }
}

/// 状態と計画・適用の小さな枠。
pub struct Grid {
    pub state: GridState,
    pub now: i64,
    pub cfg: PlanConfig,
    pub streams: BTreeMap<String, BTreeSet<NodeId>>,
}

impl Grid {
    pub fn new() -> Self {
        Grid { state: GridState::default(), now: 1_000_000, cfg: PlanConfig::default(), streams: BTreeMap::new() }
    }

    pub fn cmd(&mut self, c: GridCommand) {
        self.state.apply(c).expect("command applies");
    }

    pub fn register(&mut self, spec: NodeSpec) {
        let now = self.now;
        self.cmd(GridCommand::RegisterNode { spec, now_ms: now });
    }

    pub fn put(&mut self, w: Workload) {
        self.cmd(GridCommand::PutWorkload { workload: w });
    }

    pub fn plan(&self) -> Plan {
        self.state.plan(self.now, self.state.fencing_counter, &self.streams, &self.cfg)
    }

    /// 計画して適用し、決定を返す。適用の後にもう一度計画すると空であること（不動点）も確かめる。
    pub fn step(&mut self) -> Vec<Decision> {
        let p = self.plan();
        let d = p.decisions.clone();
        self.state.apply(p.into_command()).expect("plan applies");
        let again = self.plan();
        assert!(again.is_empty(), "plan is not a fixpoint: {:#?}", again.decisions);
        d
    }

    pub fn at(&self, w: &str) -> Option<&Assignment> {
        self.state.assignments.get(w)
    }

    pub fn node_of(&self, w: &str) -> Option<&str> {
        self.at(w).map(|a| a.node.as_str())
    }

    pub fn advance(&mut self, ms: i64) {
        self.now += ms;
    }
}
