//! 受入れ基準 AC-15（リーダー停止・少数派の隔離・古いリーダーの復帰）と AC-16（fsync の前後での停止・
//! 書込み失敗）の筋書き試験。どの試験もシミュレーションの不変条件の検査を毎段で通る。

mod common;
use bytes::Bytes;
use common::*;
use senca_raft::*;

fn calm(voters: usize) -> Params {
    let mut p = Params::calm(voters);
    p.propose = 300;
    p.read = 100;
    p
}

fn others(sim: &Sim, id: NodeId) -> Vec<NodeId> {
    sim.ids().into_iter().filter(|&x| x != id).collect()
}

/// 新しいリーダーが決まり、新しい提案が commit されるまでの tick 数。
fn recover(sim: &mut Sim, old: NodeId, bound: u64) -> u64 {
    let start = sim.now;
    assert!(
        sim.run_until(bound, |s| s.leader().is_some_and(|l| l != old)),
        "no new leader within {bound} ticks"
    );
    let l = sim.leader().unwrap();
    let p = sim.propose_kv(l, "after").unwrap();
    assert!(
        sim.run_until(bound, |s| s.committed_entry(p.index).is_some()),
        "no commit within {bound} ticks after election"
    );
    (sim.now - start) / TICK
}

#[test]
fn ac15_leader_stop() {
    for voters in [3, 5] {
        let mut worst = 0;
        for seed in 0..20 {
            let mut sim = Sim::new(seed, calm(voters));
            sim.heal_and_check_liveness(100);
            sim.run(50, true);
            let old = sim.leader().unwrap();
            sim.kill(old);
            let t = recover(&mut sim, old, 200);
            worst = worst.max(t);
            sim.run(30, true);
            // 戻ってきた旧リーダーは追従者として追いつく
            sim.start(old);
            sim.heal_and_check_liveness(200);
            sim.check_acked();
        }
        // election_tick = 10 なので、乱択タイムアウト（10〜19）＋投票の往復で収まる。
        assert!(worst <= 60, "{voters} nodes: recovery took {worst} ticks");
        eprintln!("{voters} nodes: worst recovery {worst} ticks");
    }
}

#[test]
fn ac15_minority_isolation() {
    for seed in 0..20 {
        let mut sim = Sim::new(seed, calm(5));
        sim.heal_and_check_liveness(100);
        let old = sim.leader().unwrap();
        let f = others(&sim, old)[0];
        let majority: Vec<NodeId> = sim
            .ids()
            .into_iter()
            .filter(|&x| x != old && x != f)
            .collect();
        sim.partition(&[vec![old, f], majority.clone()]);
        let (c_old, c_f) = (sim.raft(old).committed(), sim.raft(f).committed());
        // 少数派のリーダーは提案を受け付ける（ログに入る）が、commit できない。
        let mut stale = Vec::new();
        for i in 0..5 {
            if let Ok(p) = sim.propose_kv(old, &format!("stale{i}")) {
                stale.push(p);
            }
        }
        assert!(!stale.is_empty());
        let ctx = sim.read_at(old, "k0").unwrap();
        for _ in 0..80 {
            sim.step_tick(false);
            // 多数派の側だけに負荷をかける
            if let Some(l) = sim.leader().filter(|l| majority.contains(l)) {
                let _ = sim.propose_kv(l, "k1");
            }
            assert_eq!(
                sim.raft(old).committed(),
                c_old,
                "minority leader committed"
            );
            assert_eq!(sim.raft(f).committed(), c_f, "minority follower committed");
        }
        // CheckQuorum: 少数派のリーダーは退いている。
        assert!(
            !sim.raft(old).is_leader(),
            "isolated leader did not step down"
        );
        assert!(!sim.read_served(ctx), "minority served a read");
        let l = sim.leader().unwrap();
        assert!(majority.contains(&l));
        sim.heal();
        sim.heal_and_check_liveness(200);
        // 隔離中の提案は適用されない（同じ index には別の term の entry が commit されている）。
        for p in &stale {
            if let Some((t, _)) = sim.committed_entry(p.index) {
                assert_ne!(t, p.term, "seed {seed}: stale proposal {p:?} was committed");
            }
        }
        // 旧リーダーへの提案はリーダーの手がかり付きで断られる。
        let l = sim.leader().unwrap();
        if l != old {
            match sim.propose_kv(old, "late") {
                Err(ProposeError::NotLeader { leader }) => assert_eq!(leader, Some(l)),
                r => panic!("unexpected {r:?}"),
            }
        }
        sim.check_acked();
    }
}

#[test]
fn ac15_stale_leader_returns() {
    for seed in 0..20 {
        // CheckQuorum なし: 隔離された旧リーダーは自分がリーダーだと思い続ける（最悪の場合）。
        let mut p = calm(3);
        p.check_quorum = false;
        let mut sim = Sim::new(seed, p);
        sim.heal_and_check_liveness(100);
        let old = sim.leader().unwrap();
        let old_token = sim.raft(old).leader_token().unwrap();
        sim.isolate(old);
        let mut stale = Vec::new();
        for i in 0..3 {
            stale.push(sim.propose_kv(old, &format!("stale{i}")).unwrap());
        }
        let ctx = sim.read_at(old, "k0").unwrap();
        assert!(
            sim.run_until(200, |s| s.leader().is_some_and(|l| l != old)),
            "no new leader"
        );
        let new = sim.leader().unwrap();
        assert!(
            sim.raft(old).is_leader(),
            "old leader still believes it leads"
        );
        assert!(sim.run_until(50, |s| s.raft(new).leader_token().is_some()));
        let new_token = sim.raft(new).leader_token().unwrap();
        assert!(new_token > old_token, "fencing token must increase");
        // fencing: 受け手は最大の token より古い書込みを拒否する。
        let mut fence = FencingToken::default();
        let mut accept = |t: FencingToken| {
            if t >= fence {
                fence = t;
                true
            } else {
                false
            }
        };
        assert!(accept(new_token));
        assert!(!accept(old_token), "stale owner write must be rejected");
        let p = sim.propose_kv(new, "fresh").unwrap();
        assert!(sim.run_until(100, |s| s.committed_entry(p.index).is_some()));
        // 旧リーダーは過半数の確認を得られないので読み取りを返さない。
        assert!(!sim.read_served(ctx));
        sim.heal();
        assert!(
            sim.run_until(50, |s| !s.raft(old).is_leader()),
            "stale leader did not step down"
        );
        sim.heal_and_check_liveness(200);
        for p in &stale {
            if let Some((t, _)) = sim.committed_entry(p.index) {
                assert_ne!(t, p.term, "stale proposal committed");
            }
        }
        assert!(!sim.read_served(ctx));
        sim.check_acked();
    }
}

#[test]
fn ac16_crash_before_and_after_fsync() {
    // Ready の永続化の途中・後での停止を高い頻度で起こす。ACK 済みの書込みは失われない。
    for (seed, file) in (0..16).map(|s| (s, s % 4 == 0)) {
        let mut p = calm(3 + 2 * (seed as usize % 2));
        p.crash_in_persist = 40;
        p.restart = 200;
        p.file_storage = file;
        let mut sim = Sim::new(5000 + seed, p);
        for _ in 0..3 {
            sim.run(300, true);
            sim.heal_and_check_liveness(400);
        }
        sim.check_acked();
        assert!(sim.stats.persist_crashes > 0);
        assert!(!sim.acked.is_empty());
    }
}

/// 永続化に失敗する記憶域（ディスクフル・fsync 失敗）。
struct Failing {
    inner: MemStorage,
    fail: bool,
}

impl Storage for Failing {
    fn load(&mut self) -> std::io::Result<InitialState> {
        self.inner.load()
    }
    fn append(&mut self, e: &[Entry]) -> std::io::Result<()> {
        if self.fail {
            return Err(std::io::Error::other("disk full"));
        }
        self.inner.append(e)
    }
    fn set_hard_state(&mut self, hs: &HardState) -> std::io::Result<()> {
        self.inner.set_hard_state(hs)
    }
    fn save_snapshot(&mut self, s: &Snapshot) -> std::io::Result<()> {
        self.inner.save_snapshot(s)
    }
    fn sync(&mut self) -> std::io::Result<()> {
        if self.fail {
            return Err(std::io::Error::other("fsync failed"));
        }
        Ok(())
    }
}

#[test]
fn ac16_persist_failure_is_explicit() {
    // 1 ノードのクラスタ: 永続化に失敗したら適用も応答もしない（ノードを止める）。再起動後も ACK していない
    // 書込みは現れず、ACK 済みのものは残る。
    let mut st = Failing {
        inner: MemStorage::bootstrap([1]),
        fail: false,
    };
    let mut node = RawNode::new(Config::new(1), st.load().unwrap()).unwrap();
    node.campaign();
    let mut applied = Vec::new();
    let drive =
        |node: &mut RawNode, st: &mut Failing, applied: &mut Vec<Entry>| -> std::io::Result<()> {
            while let Some(rd) = node.ready() {
                st.persist(&rd)?;
                applied.extend(
                    rd.committed_entries
                        .into_iter()
                        .filter(|e| e.kind == EntryKind::Normal),
                );
            }
            Ok(())
        };
    drive(&mut node, &mut st, &mut applied).unwrap();
    assert!(node.is_leader());
    node.propose(Bytes::from_static(b"a=1")).unwrap();
    drive(&mut node, &mut st, &mut applied).unwrap();
    assert_eq!(applied.len(), 1, "acked");
    st.fail = true;
    node.propose(Bytes::from_static(b"b=2")).unwrap();
    let r = drive(&mut node, &mut st, &mut applied);
    assert!(r.is_err(), "failure must be reported");
    assert_eq!(applied.len(), 1, "nothing applied after a failed persist");
    drop(node); // 止める
    st.fail = false;
    let init = st.load().unwrap();
    assert!(init.entries.iter().all(|e| &e.data[..] != b"b=2"));
    assert!(init.entries.iter().any(|e| &e.data[..] == b"a=1"));
    let mut node = RawNode::new(Config::new(1), init).unwrap();
    let mut applied2 = Vec::new();
    node.campaign();
    drive(&mut node, &mut st, &mut applied2).unwrap();
    node.propose(Bytes::from_static(b"c=3")).unwrap();
    drive(&mut node, &mut st, &mut applied2).unwrap();
    let datas: Vec<&[u8]> = applied2.iter().map(|e| &e.data[..]).collect();
    assert_eq!(datas, vec![&b"a=1"[..], &b"c=3"[..]]);
}

/// 論文の Figure 8: 前の term の entry を過半数に複製しただけでは commit しない。
#[test]
fn figure8_previous_term_entries_not_committed_by_counting() {
    let mut exercised = 0;
    for seed in 0..10 {
        let mut p = Params::calm(5);
        p.check_quorum = false;
        p.pre_vote = false;
        p.max_msg_bytes = 1;
        p.max_inflight = 1;
        let mut sim = Sim::new(seed, p);
        sim.heal_and_check_liveness(100);
        let s1 = sim.leader().unwrap();
        let rest = others(&sim, s1);
        let (s2, s3, s4, s5) = (rest[0], rest[1], rest[2], rest[3]);
        // (a) S1 が index i を S2 にだけ複製する。
        sim.intercept = Some(Box::new(move |m| {
            (!(matches!(m.body, MsgBody::Append { .. }) && m.from == s1 && m.to != s2)).then_some(m)
        }));
        let x = sim.propose_kv(s1, "x").unwrap();
        let i = x.index;
        assert!(sim.run_until(20, |s| s.raft(s2).log_term(i) == Some(x.term)));
        // (b) S1 が落ち、S5 が S3・S4 の票で当選し、自分の term の entry を index i に書く（誰にも送らない）。
        sim.kill(s1);
        sim.isolate(s2);
        sim.intercept = Some(Box::new(move |m| {
            (!(matches!(m.body, MsgBody::Append { .. }) && m.from == s5)).then_some(m)
        }));
        let mut won = false;
        for _ in 0..20 {
            if !sim.raft(s5).is_leader() {
                sim.raft_mut(s5).campaign();
                sim.process(s5);
            }
            sim.run(3, false);
            if sim.raft(s5).is_leader() {
                won = true;
                break;
            }
        }
        if !won || sim.raft(s5).log_term(i).is_none_or(|t| t == x.term) {
            continue; // 筋書きに乗らなかった種
        }
        // (c) S5 が落ち、S1 が戻る。S1 か S2 がリーダーになり、index i（古い term）だけを S3 に送る。
        sim.kill(s5);
        sim.heal();
        sim.start(s1);
        sim.process(s1);
        // 新しいリーダーの entry（index > i）は誰にも届かない（Append から取り除く）。
        sim.intercept = Some(Box::new(move |mut m| {
            if let MsgBody::Append { entries, .. } = &mut m.body {
                entries.retain(|e| e.index <= i);
            }
            Some(m)
        }));
        assert!(sim.run_until(200, |s| s.leader().is_some()));
        let l = sim.leader().unwrap();
        assert!(l == s1 || l == s2);
        sim.run(30, false);
        let holders = [s1, s2, s3, s4]
            .iter()
            .filter(|&&n| sim.raft(n).log_term(i) == Some(x.term))
            .count();
        assert!(
            holders >= 3,
            "seed {seed}: old-term entry should be on a majority ({holders})"
        );
        assert!(
            sim.raft(l).committed() < i,
            "seed {seed}: leader committed a previous-term entry by counting replicas"
        );
        exercised += 1;
        // (d) l が落ち、S5 が戻って当選すると index i を書き換える（commit されていなかったので安全）。
        sim.kill(l);
        sim.intercept = None;
        sim.start(s5);
        for _ in 0..40 {
            if sim.leader().is_some() {
                break;
            }
            sim.raft_mut(s5).campaign();
            sim.process(s5);
            sim.run(2, false);
        }
        sim.heal_and_check_liveness(300);
        sim.check_acked();
    }
    assert!(exercised >= 5, "scenario exercised only {exercised} times");
}
