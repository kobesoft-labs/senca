//! 個々の機能の試験（移譲・learner・構成変更・断りと手がかり・上限・流量制御・読み取り）。

mod common;
use bytes::Bytes;
use common::*;
use senca_raft::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn calm_sim(seed: u64, voters: usize, pool: usize) -> Sim {
    let mut p = Params::calm(voters);
    p.pool = pool;
    let mut sim = Sim::new(seed, p);
    sim.heal_and_check_liveness(100);
    sim
}

#[test]
fn leadership_transfer() {
    for seed in 0..10 {
        let mut sim = calm_sim(seed, 3, 3);
        let l = sim.leader().unwrap();
        let to = sim.ids().into_iter().find(|&x| x != l).unwrap();
        sim.raft_mut(l).transfer_leader(to).unwrap();
        assert_eq!(
            sim.propose_kv(l, "a"),
            Err(ProposeError::TransferInProgress)
        );
        sim.process(l);
        assert!(
            sim.run_until(20, |s| s.leader() == Some(to)),
            "transfer did not finish"
        );
        assert!(sim.raft(to).term() == sim.raft(l).term());
        sim.heal_and_check_liveness(100);
    }
}

#[test]
fn transfer_to_non_voter_is_rejected() {
    let mut sim = calm_sim(1, 3, 4);
    let l = sim.leader().unwrap();
    assert!(matches!(
        sim.raft_mut(l).transfer_leader(4),
        Err(ProposeError::Invalid(_))
    ));
}

#[test]
fn learner_catches_up_by_chunked_snapshot_and_is_promoted() {
    for seed in 0..5 {
        let mut sim = calm_sim(seed, 3, 4);
        let snaps = Arc::new(AtomicUsize::new(0));
        let c = snaps.clone();
        sim.intercept = Some(Box::new(move |m| {
            if matches!(m.body, MsgBody::Snapshot { .. }) {
                c.fetch_add(1, Ordering::Relaxed);
            }
            Some(m)
        }));
        let l = sim.leader().unwrap();
        for i in 0..60 {
            sim.propose_kv(l, &format!("key{}", i % 10)).unwrap();
            sim.step_tick(false);
        }
        sim.run(10, false);
        // 全ノードで圧縮
        for id in sim.ids() {
            let n = sim.nodes.get_mut(&id).unwrap();
            let r = n.raft.as_mut().unwrap();
            if let Ok(s) = r.compact(n.kv.applied, n.kv.encode()) {
                n.store_save_snapshot(&s);
            }
        }
        let p = sim
            .raft_mut(l)
            .propose_conf_change(&[ConfChangeOp::AddLearner(4)])
            .unwrap();
        sim.process(l);
        assert!(
            sim.run_until(100, |s| s.nodes[&4].kv.applied >= p.index),
            "learner did not catch up"
        );
        assert!(
            snaps.load(Ordering::Relaxed) > 3,
            "snapshot should be sent in chunks"
        );
        assert_eq!(sim.nodes[&4].kv.map, sim.nodes[&l].kv.map);
        assert!(sim.raft(4).conf_state().learners.contains(&4));
        // 昇格（joint を通って自動で抜ける）
        let l = sim.leader().unwrap();
        sim.raft_mut(l)
            .propose_conf_change(&[ConfChangeOp::AddVoter(4)])
            .unwrap();
        sim.process(l);
        assert!(sim.run_until(50, |s| {
            let c = s.raft(s.leader().unwrap()).conf_state();
            !c.is_joint() && c.voters.len() == 4
        }));
        sim.heal_and_check_liveness(100);
        sim.check_conf_history();
    }
}

#[test]
fn removing_the_leader() {
    for seed in 0..10 {
        let mut sim = calm_sim(seed, 3, 3);
        let l = sim.leader().unwrap();
        let p = sim
            .raft_mut(l)
            .propose_conf_change(&[ConfChangeOp::RemoveVoter(l)])
            .unwrap();
        sim.process(l);
        // 次の変更は joint を抜けるまで受け付けない
        assert_eq!(
            sim.raft_mut(l)
                .propose_conf_change(&[ConfChangeOp::AddLearner(9)]),
            Err(ProposeError::ConfChangePending)
        );
        assert!(
            sim.run_until(100, |s| s.leader().is_some_and(|x| x != l)),
            "no new leader"
        );
        assert!(!sim.raft(l).is_leader());
        let nl = sim.leader().unwrap();
        assert!(!sim.raft(nl).conf_state().voters.contains(&l));
        assert!(sim.committed_entry(p.index).is_some());
        // 外れたノードは選挙を起こさない（term が上がらない）
        let term = sim.raft(nl).term();
        sim.run(100, false);
        assert_eq!(sim.raft(nl).term(), term);
        assert_eq!(sim.leader(), Some(nl));
        sim.check_conf_history();
    }
}

#[test]
fn follower_rejects_with_leader_hint() {
    let mut sim = calm_sim(3, 3, 3);
    let l = sim.leader().unwrap();
    let f = sim.ids().into_iter().find(|&x| x != l).unwrap();
    assert_eq!(
        sim.propose_kv(f, "a"),
        Err(ProposeError::NotLeader { leader: Some(l) })
    );
    assert_eq!(
        sim.raft_mut(f).read_index(1),
        Err(ProposeError::NotLeader { leader: Some(l) })
    );
    assert!(matches!(
        sim.raft_mut(f)
            .propose_conf_change(&[ConfChangeOp::AddLearner(5)]),
        Err(ProposeError::NotLeader { .. })
    ));
}

#[test]
fn read_index_is_served_on_every_kind_of_cluster() {
    for (voters, lease) in [(1, false), (3, false), (3, true), (5, false)] {
        let mut p = Params::calm(voters);
        p.lease_read = lease;
        let mut sim = Sim::new(7, p);
        sim.heal_and_check_liveness(100);
        let l = sim.leader().unwrap();
        sim.propose_kv(l, "k").unwrap();
        let ctx = sim.read_at(l, "k").unwrap();
        assert!(
            sim.run_until(10, |s| s.read_served(ctx)),
            "{voters} voters lease={lease}"
        );
    }
}

/// 3 投票ノードの構成で、ノード 1 を手で当選させる（他のノードは使わない）。
fn manual_leader(cfg: Config) -> RawNode {
    let mut n = RawNode::new(cfg, MemStorage::bootstrap([1, 2, 3]).load().unwrap()).unwrap();
    n.campaign();
    let _ = n.ready();
    let t = n.term();
    n.step(Message {
        from: 2,
        to: 1,
        term: t + 1,
        body: MsgBody::PreVoteResp { reject: false },
    })
    .unwrap();
    let _ = n.ready();
    let t = n.term();
    n.step(Message {
        from: 2,
        to: 1,
        term: t,
        body: MsgBody::VoteResp { reject: false },
    })
    .unwrap();
    assert!(n.is_leader());
    n
}

#[test]
fn proposal_limits() {
    let mut cfg = Config::new(1);
    cfg.max_entry_bytes = 10;
    cfg.max_uncommitted_bytes = 100;
    let mut n = manual_leader(cfg);
    assert_eq!(
        n.propose(Bytes::from(vec![0u8; 11])),
        Err(ProposeError::TooLarge)
    );
    // 誰も応答しないので commit されず、上限で断られる
    let mut ok = 0;
    loop {
        match n.propose(Bytes::from(vec![1u8; 10])) {
            Ok(_) => ok += 1,
            Err(ProposeError::Dropped) => break,
            Err(e) => panic!("{e:?}"),
        }
        assert!(ok < 10);
    }
    assert!(ok >= 1);
}

#[test]
fn inflight_limit_and_message_size() {
    let mut cfg = Config::new(1);
    cfg.max_inflight_msgs = 3;
    cfg.max_msg_bytes = 1;
    let mut n = manual_leader(cfg);
    let rd = n.ready().unwrap();
    // 就任時の Probe（noop）にノード 2 が応答 → Replicate
    let t = n.term();
    let probe = rd
        .messages
        .iter()
        .find(|m| m.to == 2 && matches!(m.body, MsgBody::Append { .. }))
        .unwrap();
    let MsgBody::Append { entries, .. } = &probe.body else {
        unreachable!()
    };
    let last = entries.last().unwrap().index;
    n.step(Message {
        from: 2,
        to: 1,
        term: t,
        body: MsgBody::AppendResp {
            reject: false,
            index: last,
            reject_hint: 0,
            hint_term: 0,
        },
    })
    .unwrap();
    let _ = n.ready();
    for i in 0..10 {
        n.propose(Bytes::from(format!("p{i}"))).unwrap();
    }
    let rd = n.ready().unwrap();
    let to2: Vec<&Message> = rd
        .messages
        .iter()
        .filter(|m| {
            m.to == 2 && matches!(&m.body, MsgBody::Append { entries, .. } if !entries.is_empty())
        })
        .collect();
    assert_eq!(to2.len(), 3, "replicate is limited by max_inflight_msgs");
    for m in &to2 {
        let MsgBody::Append { entries, .. } = &m.body else {
            unreachable!()
        };
        assert_eq!(
            entries.len(),
            1,
            "max_msg_bytes keeps one entry per message"
        );
    }
    // ノード 3 は Probe のまま: 応答があるまで 1 通だけ
    let to3 = rd
        .messages
        .iter()
        .filter(|m| m.to == 3 && matches!(m.body, MsgBody::Append { .. }))
        .count();
    assert!(to3 <= 1);
}

#[test]
fn fencing_token_order() {
    let a = FencingToken {
        term: 3,
        index: 100,
    };
    let b = FencingToken { term: 4, index: 2 };
    assert!(b > a);
    assert_eq!(FencingToken::from_u128(b.as_u128()), b);
    assert!(b.as_u128() > a.as_u128());
}

#[test]
fn malformed_messages_are_rejected() {
    let mut n = manual_leader(Config::new(1));
    let t = n.term();
    let bad = Message {
        from: 2,
        to: 1,
        term: t + 5,
        body: MsgBody::Append {
            prev_index: 1,
            prev_term: 1,
            entries: vec![Entry {
                term: 1,
                index: 5,
                kind: EntryKind::Normal,
                data: Bytes::new(),
            }],
            commit: 0,
        },
    };
    assert!(matches!(n.step(bad), Err(StepError::Malformed(_))));
    assert!(n.is_leader(), "malformed message must not change state");
    assert_eq!(
        n.step(Message {
            from: 2,
            to: 9,
            term: t,
            body: MsgBody::TimeoutNow
        }),
        Err(StepError::WrongTarget)
    );
}
