mod common;

use common::*;
use senca_grid::*;

#[test]
fn spread_and_binpack() {
    let mut g = Grid::new();
    g.register(node("a", 4000, 8000));
    g.register(node("b", 4000, 8000));
    g.put(wl("w1", 1000, 1000, Priority::Normal));
    g.put(wl("w2", 1000, 1000, Priority::Normal));
    g.step();
    assert_ne!(g.node_of("w1"), g.node_of("w2"), "spread puts them apart");

    let mut g = Grid::new();
    g.register(node("a", 4000, 8000));
    g.register(node("b", 4000, 8000));
    for id in ["w1", "w2"] {
        let mut w = wl(id, 1000, 1000, Priority::Normal);
        w.placement = Placement::Binpack;
        g.put(w);
    }
    g.step();
    assert_eq!(g.node_of("w1"), g.node_of("w2"), "binpack packs them");
}

#[test]
fn locality_labels_and_anti_affinity() {
    let mut g = Grid::new();
    let mut a = node("a", 8000, 8000);
    a.labels.insert("zone".into(), "x".into());
    let mut b = node("b", 8000, 8000);
    b.labels.insert("zone".into(), "y".into());
    g.register(a);
    g.register(b);
    g.register(node("c", 8000, 8000));
    g.streams.entry("s1".into()).or_default().insert("c".into());

    let mut w = wl("rec", 100, 100, Priority::Normal);
    w.locality = vec!["s1".into()];
    g.put(w);
    let mut w = wl("lab", 100, 100, Priority::Normal);
    w.constraints.labels = vec![LabelSelector::Eq { key: "zone".into(), value: "y".into() }];
    g.put(w);
    let mut w = wl("notin", 100, 100, Priority::Normal);
    w.constraints.labels = vec![LabelSelector::NotIn { key: "zone".into(), values: vec!["x".into(), "y".into()] }];
    g.put(w);
    for i in 0..3 {
        let mut w = wl(&format!("r{i}"), 100, 100, Priority::Normal);
        w.constraints.anti_affinity = Some("grp".into());
        g.put(w);
    }
    let mut w = wl("r3", 100, 100, Priority::Normal);
    w.constraints.anti_affinity = Some("grp".into());
    g.put(w);
    g.step();
    assert_eq!(g.node_of("rec"), Some("c"));
    assert_eq!(g.node_of("lab"), Some("b"));
    assert_eq!(g.node_of("notin"), Some("c"));
    let nodes: std::collections::BTreeSet<_> = (0..3).map(|i| g.node_of(&format!("r{i}")).unwrap().to_string()).collect();
    assert_eq!(nodes.len(), 3);
    assert!(g.at("r3").is_none(), "4th replica has no node");
    let r = &g.state.unschedulable["r3"];
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].reason, Reject::AntiAffinity);
    assert_eq!(r[0].nodes, 3);
}

#[test]
fn gpu_vendor_and_device_pin() {
    let mut g = Grid::new();
    let mut a = node("a", 8000, 8000);
    a.gpus = vec![gpu("g0", "intel", 4)];
    let mut b = node("b", 8000, 8000);
    b.gpus = vec![gpu("g0", "nvidia", 4), gpu("g1", "nvidia", 4)];
    g.register(a);
    g.register(b);
    let mut w = wl("nv", 100, 100, Priority::Normal);
    w.requirements.gpu = Some(GpuReq { encode: 2, decode: 1, memory_mb: 0, vendor: Some("NVIDIA".into()) });
    g.put(w);
    let mut w = wl("pin", 100, 100, Priority::Normal);
    w.requirements.gpu = Some(GpuReq { encode: 3, decode: 0, memory_mb: 0, vendor: None });
    w.constraints.node = Some("b".into());
    w.constraints.device = Some("g1".into());
    w.constraints.pin_policy = PinPolicy::Strict;
    g.put(w);
    g.step();
    assert_eq!(g.node_of("nv"), Some("b"));
    assert_eq!(g.at("pin").unwrap().device.as_deref(), Some("g1"));
    // pin した GPU が壊れると strict は止まる
    g.cmd(GridCommand::DeviceDown { node: "b".into(), device: "g1".into() });
    let d = g.step();
    assert!(d.iter().any(|d| matches!(d, Decision::Evict { workload, .. } if workload == "pin")));
    assert_eq!(g.state.unschedulable["pin"][0].reason, Reject::PinTargetDown);
    g.cmd(GridCommand::DeviceUp { node: "b".into(), device: "g1".into() });
    g.step();
    assert_eq!(g.at("pin").unwrap().device.as_deref(), Some("g1"));
    assert!(!g.state.unschedulable.contains_key("pin"));
}

#[test]
fn node_down_moves_with_new_token_and_fences_old() {
    let mut g = Grid::new();
    g.register(node("a", 4000, 4000));
    g.register(node("b", 4000, 4000));
    g.put(wl("w", 1000, 1000, Priority::Normal));
    g.step();
    let old = g.at("w").unwrap().clone();
    let other = if old.node == "a" { "b" } else { "a" };
    g.cmd(GridCommand::NodeDown { node: old.node.clone(), now_ms: g.now, last_heartbeat_ms: g.now - 6000 });
    let d = g.step();
    assert!(matches!(&d[0], Decision::Move { from, node, reason: Reason::NodeDown { .. }, .. } if *from == old.node && node == other));
    let new = g.at("w").unwrap();
    assert!(new.token > old.token);
    assert!(!g.state.is_current("w", &old.node, old.token), "old node must stop");
    assert!(g.state.is_current("w", other, new.token));
}

#[test]
fn pin_policies_on_node_failure_and_return() {
    let mut g = Grid::new();
    g.cfg.restore_cooldown_ms = 10_000;
    g.register(node("a", 4000, 4000));
    g.register(node("b", 4000, 4000));
    let mut s = wl("strict", 500, 500, Priority::High);
    s.constraints.node = Some("a".into());
    s.constraints.pin_policy = PinPolicy::Strict;
    g.put(s);
    let mut p = wl("prefer", 500, 500, Priority::High);
    p.constraints.node = Some("a".into());
    g.put(p);
    g.step();
    assert_eq!(g.node_of("strict"), Some("a"));
    assert_eq!(g.node_of("prefer"), Some("a"));

    g.advance(1000);
    g.cmd(GridCommand::NodeDown { node: "a".into(), now_ms: g.now, last_heartbeat_ms: g.now - 6000 });
    g.step();
    assert!(g.at("strict").is_none());
    let why: Vec<_> = g.state.unschedulable["strict"].iter().map(|r| r.reason).collect();
    assert_eq!(why, vec![Reject::NodeDown, Reject::PinTargetDown]);
    assert_eq!(g.node_of("prefer"), Some("b"));

    g.advance(1000);
    g.cmd(GridCommand::NodeUp { node: "a".into(), now_ms: g.now });
    let d = g.step();
    assert_eq!(g.node_of("strict"), Some("a"));
    assert_eq!(g.node_of("prefer"), Some("b"), "cooldown keeps prefer where it is: {d:?}");
    g.advance(10_000);
    let d = g.step();
    assert!(matches!(&d[0], Decision::Move { reason: Reason::PinReturned, .. }));
    assert_eq!(g.node_of("prefer"), Some("a"));
}

/// 低い優先度は段を下げられ、それでも足りなければ追い出される。critical は追い出されない。
#[test]
fn degrade_then_evict_lower_priority() {
    let mut g = Grid::new();
    g.register(node("a", 4000, 100_000));
    let mut low = wl("low", 3000, 100, Priority::Low);
    low.degrade = vec![level("720p", 2000, 100), level("audio_only", 500, 100)];
    g.put(low);
    g.step();
    assert_eq!(g.at("low").unwrap().level, 0);

    g.put(wl("hi1", 1500, 100, Priority::High));
    let d = g.step();
    assert!(d.iter().any(|d| matches!(d, Decision::Degrade { workload, level: 1, reason: Reason::Preempted { by }, .. } if workload == "low" && by == "hi1")));
    assert_eq!(g.at("hi1").unwrap().level, 0);

    g.put(wl("hi2", 1500, 100, Priority::High));
    g.step();
    assert_eq!(g.at("low").unwrap().level, 2, "degraded further, step by step");

    g.put(wl("hi3", 800, 100, Priority::High));
    let d = g.step();
    assert!(d.iter().any(|d| matches!(d, Decision::Evict { workload, .. } if workload == "low")));
    assert!(g.state.unschedulable.contains_key("low"));

    // critical は high に追い出されない。high は自分の段を下げて置かれるか、置けない。
    let mut g = Grid::new();
    g.register(node("a", 4000, 100_000));
    let mut c = wl("crit", 3500, 100, Priority::Critical);
    c.degrade = vec![level("min", 100, 100)];
    g.put(c);
    g.step();
    let mut h = wl("hi", 1000, 100, Priority::High);
    h.degrade = vec![level("min", 400, 100)];
    g.put(h);
    g.step();
    assert_eq!(g.at("crit").unwrap().level, 0);
    assert_eq!(g.at("hi").unwrap().level, 1);
    assert_eq!(g.at("hi").unwrap().reason, Reason::NoCapacity);
}

#[test]
fn restore_after_cooldown_only() {
    let mut g = Grid::new();
    g.cfg.restore_cooldown_ms = 30_000;
    g.register(node("a", 4000, 100_000));
    let mut low = wl("low", 3000, 100, Priority::Low);
    low.degrade = vec![level("720p", 1500, 100)];
    g.put(low);
    g.step();
    g.put(wl("hi", 2000, 100, Priority::High));
    g.step();
    assert_eq!(g.at("low").unwrap().level, 1);
    g.cmd(GridCommand::DeleteWorkload { id: "hi".into() });
    g.advance(10_000);
    assert!(g.step().is_empty(), "still cooling down");
    g.advance(20_000);
    let d = g.step();
    assert!(matches!(&d[0], Decision::Restore { level: 0, reason: Reason::CapacityReturned, .. }));
    assert_eq!(g.at("low").unwrap().level, 0);
}

#[test]
fn restore_moves_when_own_node_is_full() {
    let mut g = Grid::new();
    g.cfg.restore_cooldown_ms = 0;
    g.register(node("a", 4000, 100_000));
    let mut w = wl("w", 3000, 100, Priority::Normal);
    w.degrade = vec![level("sd", 1000, 100)];
    g.put(w);
    let mut f = wl("fill", 3000, 100, Priority::Normal);
    f.constraints.node = Some("a".into());
    f.constraints.pin_policy = PinPolicy::Strict;
    g.put(f);
    g.step();
    assert_eq!(g.at("w").unwrap().level, 1);
    g.register(node("b", 4000, 100_000));
    g.step();
    let a = g.at("w").unwrap();
    assert_eq!((a.node.as_str(), a.level, a.reason.clone()), ("b", 0, Reason::CapacityReturned));
}

#[test]
fn draining_moves_unpinned_and_keeps_pinned() {
    let mut g = Grid::new();
    g.register(node("a", 4000, 4000));
    let mut p = wl("pinned", 500, 500, Priority::Normal);
    p.constraints.node = Some("a".into());
    g.put(p);
    g.put(wl("free", 500, 500, Priority::Normal));
    g.step();
    g.register(node("b", 4000, 4000));
    g.cmd(GridCommand::SetDraining { node: "a".into(), draining: true });
    let d = g.step();
    assert_eq!(d.len(), 1);
    assert!(matches!(&d[0], Decision::Move { reason: Reason::Draining, .. }));
    assert_eq!(g.node_of("free"), Some("b"));
    assert_eq!(g.node_of("pinned"), Some("a"));
    // 排出中のノードには新しく置かない
    g.put(wl("new", 100, 100, Priority::Normal));
    g.step();
    assert_eq!(g.node_of("new"), Some("b"));
}

#[test]
fn shrunk_capacity_is_repaired() {
    let mut g = Grid::new();
    g.register(node("a", 8000, 8000));
    g.put(wl("c", 3000, 100, Priority::Critical));
    let mut n = wl("n", 3000, 100, Priority::Normal);
    n.degrade = vec![level("sd", 1000, 100)];
    g.put(n);
    g.put(wl("l", 1000, 100, Priority::Low));
    g.step();
    g.register(node("a", 4500, 8000)); // 申告し直して容量が減った
    let d = g.step();
    assert!(d.iter().any(|d| matches!(d, Decision::Evict { workload, reason: Reason::Overcommitted } if workload == "l")));
    assert!(d.iter().any(|d| matches!(d, Decision::Degrade { workload, level: 1, .. } if workload == "n")));
    assert_eq!(g.at("c").unwrap().level, 0);
}

#[test]
fn capture_device_is_exclusive() {
    let mut g = Grid::new();
    let mut a = node("a", 8000, 8000);
    a.capture = vec![CaptureDevice { id: "sdi0".into(), kind: "sdi".into(), healthy: true }];
    g.register(a);
    g.register(node("b", 8000, 8000));
    for (id, p) in [("cap1", Priority::Normal), ("cap2", Priority::High)] {
        let mut w = wl(id, 100, 100, p);
        w.requirements.capture = Some("sdi0".into());
        g.put(w);
    }
    g.step();
    assert_eq!(g.node_of("cap2"), Some("a"));
    assert!(g.at("cap1").is_none());
    assert_eq!(g.state.unschedulable["cap1"].iter().map(|r| r.reason).collect::<Vec<_>>(), vec![Reject::Capture]);
}

#[test]
fn apply_rejects_stale_and_invalid_plans_without_change() {
    let mut g = Grid::new();
    g.register(node("a", 4000, 4000));
    g.put(wl("w", 1000, 1000, Priority::Normal));
    let p = g.plan();
    let before = g.state.clone();
    // 計画の後に別の token が払い出された
    let mut stale = p.clone();
    stale.token_base += 1;
    assert!(matches!(g.state.apply(stale.into_command()), Err(GridError::StalePlan { .. })));
    // 計画の後にノードが止まった
    g.cmd(GridCommand::NodeDown { node: "a".into(), now_ms: g.now, last_heartbeat_ms: 0 });
    let snapshot = g.state.clone();
    assert!(matches!(g.state.apply(p.into_command()), Err(GridError::InvalidDecision { .. })));
    assert_eq!(g.state, snapshot);
    assert_ne!(before, snapshot);
    // 容量を超える手作りの決定
    g.cmd(GridCommand::NodeUp { node: "a".into(), now_ms: g.now });
    g.put(wl("big", 3500, 100, Priority::Normal));
    let bad = Plan {
        token_base: 0,
        now_ms: g.now,
        decisions: ["big", "w"]
            .iter()
            .enumerate()
            .map(|(i, w)| Decision::Assign {
                workload: w.to_string(),
                node: "a".into(),
                device: None,
                level: 0,
                token: i as u64 + 1,
                reason: Reason::Placed,
            })
            .collect(),
    };
    assert!(matches!(g.state.apply(bad.into_command()), Err(GridError::InvalidDecision { .. })));
    assert_eq!(g.state.fencing_counter, 0);
}

#[test]
fn shared_fencing_counter() {
    let mut g = Grid::new();
    g.register(node("a", 4000, 4000));
    g.put(wl("w", 1000, 1000, Priority::Normal));
    let mut counter = 41u64; // 制御状態の fencing_counter
    let p = g.state.plan(g.now, counter, &g.streams, &g.cfg);
    g.state.apply_with_counter(p.into_command(), &mut counter).unwrap();
    assert_eq!(counter, 42);
    assert_eq!(g.at("w").unwrap().token, 42);
}

#[test]
fn failure_detector() {
    let mut g = Grid::new();
    let mut a = node("a", 4000, 4000);
    a.gpus = vec![gpu("g0", "nvidia", 4)];
    g.register(a);
    g.register(node("b", 4000, 4000));
    let mut det = FailureDetector::new(5000, g.now);
    // 就任直後は猶予
    assert!(det.tick(&g.state, g.now + 4000).is_empty());
    det.heartbeat(Heartbeat { node: "a".into(), streams: vec!["s".into()], ..Default::default() }, g.now + 4000);
    let cmds = det.tick(&g.state, g.now + 6000);
    assert_eq!(cmds, vec![GridCommand::NodeDown { node: "b".into(), now_ms: g.now + 6000, last_heartbeat_ms: g.now }]);
    for c in cmds {
        g.cmd(c);
    }
    assert_eq!(det.stream_nodes_at(g.now + 6000)["s"].iter().collect::<Vec<_>>(), vec!["a"]);
    det.heartbeat(
        Heartbeat { node: "a".into(), devices: vec![DeviceHealth { id: "g0".into(), healthy: false }], ..Default::default() },
        g.now + 7000,
    );
    det.heartbeat(Heartbeat { node: "b".into(), ..Default::default() }, g.now + 7000);
    let cmds = det.tick(&g.state, g.now + 7500);
    assert_eq!(
        cmds,
        vec![
            GridCommand::DeviceDown { node: "a".into(), device: "g0".into() },
            GridCommand::NodeUp { node: "b".into(), now_ms: g.now + 7500 },
        ]
    );
    for c in cmds {
        g.cmd(c);
    }
    assert!(!g.state.nodes["a"].spec.gpus[0].healthy);
    assert!(g.state.nodes["b"].status.up);
    assert!(det.tick(&g.state, g.now + 8000).is_empty(), "no repeated commands");
}

#[test]
fn reconcile_and_is_current() {
    let mut g = Grid::new();
    g.register(node("a", 8000, 8000));
    let mut w = wl("w1", 100, 100, Priority::Normal);
    w.degrade = vec![level("sd", 50, 50)];
    g.put(w);
    g.put(wl("w2", 100, 100, Priority::Normal));
    g.step();
    let t1 = g.at("w1").unwrap().token;
    let r = g.state.reconcile(
        "a",
        &[
            Running { workload: "w1".into(), token: t1, level: 1 },
            Running { workload: "old".into(), token: 1, level: 0 },
        ],
    );
    assert_eq!(r.start.iter().map(|a| a.workload.as_str()).collect::<Vec<_>>(), vec!["w2"]);
    assert_eq!(r.stop.iter().map(|x| x.workload.as_str()).collect::<Vec<_>>(), vec!["old"]);
    assert_eq!(r.change_level.len(), 1);
    assert!(is_current(g.at("w1"), "a", t1));
    assert!(!is_current(g.at("w1"), "a", t1 + 100));
    assert!(!is_current(None, "a", t1));
}

#[test]
fn commands_serialize() {
    let c = GridCommand::DeviceDown { node: "a".into(), device: "g0".into() };
    assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"op":"device_down","node":"a","device":"g0"}"#);
    let mut g = Grid::new();
    g.register(node("a", 4000, 4000));
    let mut w = wl("w", 100, 100, Priority::High);
    w.constraints.labels = vec![LabelSelector::In { key: "zone".into(), values: vec!["a".into()] }];
    g.put(w);
    let p = g.plan();
    let j = serde_json::to_string(&p.clone().into_command()).unwrap();
    let back: GridCommand = serde_json::from_str(&j).unwrap();
    assert_eq!(back, p.into_command());
    let s = serde_json::to_string(&g.state).unwrap();
    assert_eq!(serde_json::from_str::<GridState>(&s).unwrap(), g.state);
}

#[test]
fn validation_errors() {
    let mut s = GridState::default();
    assert!(matches!(s.apply(GridCommand::NodeUp { node: "x".into(), now_ms: 0 }), Err(GridError::UnknownNode { .. })));
    assert!(matches!(s.apply(GridCommand::DeleteWorkload { id: "x".into() }), Err(GridError::UnknownWorkload { .. })));
    let mut w = wl("", 1, 1, Priority::Low);
    assert!(s.apply(GridCommand::PutWorkload { workload: w.clone() }).is_err());
    w.id = "w".into();
    w.degrade = vec![level("full", 1, 1)];
    assert!(s.apply(GridCommand::PutWorkload { workload: w }).is_err());
    let mut n = node("a", 1, 1);
    n.gpus = vec![gpu("d", "x", 1)];
    n.capture = vec![CaptureDevice { id: "d".into(), kind: String::new(), healthy: true }];
    assert!(s.apply(GridCommand::RegisterNode { spec: n, now_ms: 0 }).is_err());
    assert_eq!(s, GridState::default());
}
