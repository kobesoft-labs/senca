//! 決定的シミュレーションによる安全性・活性の試験（多数の種）。
//!
//! 失敗したら標準エラーに種が出る。`SENCA_RAFT_SEED=<n>` で 1 つだけ再現できる。
//! 長い試験: `cargo test -p senca-raft --release -- --ignored`（`SENCA_RAFT_SEEDS` で種の数）。

mod common;
use common::*;

struct SeedGuard(String);

impl Drop for SeedGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("FAILED: {}", self.0);
        }
    }
}

fn only_seed() -> Option<u64> {
    std::env::var("SENCA_RAFT_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
}

fn sweep(
    name: &str,
    seeds: std::ops::Range<u64>,
    rounds: u32,
    ticks: u64,
    mk: impl Fn(u64) -> Params,
) -> Stats {
    let mut total = Stats::default();
    let seeds: Vec<u64> = match only_seed() {
        Some(s) => vec![s],
        None => seeds.collect(),
    };
    for seed in seeds {
        let _g = SeedGuard(format!("{name} seed {seed}"));
        let s = run_seed(seed, mk(seed), rounds, ticks);
        total.proposed += s.proposed;
        total.committed += s.committed;
        total.reads_ok += s.reads_ok;
        total.elections += s.elections;
        total.crashes += s.crashes;
        total.persist_crashes += s.persist_crashes;
        total.snapshots_installed += s.snapshots_installed;
        total.conf_changes += s.conf_changes;
        total.transfers += s.transfers;
        total.messages += s.messages;
    }
    eprintln!("{name}: {total:?}");
    total
}

fn mixed(voters: usize) -> impl Fn(u64) -> Params {
    move |seed| {
        let mut p = Params::chaos(voters);
        p.lease_read = seed % 4 == 3;
        // CheckQuorum・PreVote なし（古いリーダーが居座る・term が上がりやすい）でも安全であること。
        match seed % 6 {
            1 => p.check_quorum = false,
            2 => p.pre_vote = false,
            4 => {
                p.check_quorum = false;
                p.pre_vote = false;
            }
            _ => {}
        }
        p.lease_read &= p.check_quorum;
        if seed % 5 == 0 {
            p.max_inflight = 256;
            p.max_msg_bytes = 1 << 20;
        }
        p
    }
}

#[test]
fn sim_three_nodes() {
    let s = sweep("3 nodes", 0..40, 2, 500, mixed(3));
    assert!(
        s.committed > 1000 && s.reads_ok > 500 && s.conf_changes > 10 && s.snapshots_installed > 0,
        "{s:?}"
    );
}

#[test]
fn sim_five_nodes() {
    let s = sweep("5 nodes", 1000..1030, 2, 500, mixed(5));
    assert!(
        s.committed > 1000 && s.reads_ok > 500 && s.conf_changes > 10,
        "{s:?}"
    );
}

#[test]
fn sim_file_storage() {
    let s = sweep("file storage", 2000..2006, 2, 400, |seed| {
        let mut p = mixed(if seed % 2 == 0 { 3 } else { 5 })(seed);
        p.file_storage = true;
        p.crash_in_persist = 10;
        p
    });
    assert!(s.persist_crashes > 0, "{s:?}");
}

#[test]
fn sim_calm_liveness() {
    // 故障なし: 素早く決まり、止まらない。
    for seed in 0..10 {
        let _g = SeedGuard(format!("calm seed {seed}"));
        let mut sim = Sim::new(seed, Params::calm(3));
        let t = sim.heal_and_check_liveness(100);
        assert!(t < 60, "took {t} ticks");
    }
}

#[test]
#[ignore]
fn sim_long() {
    let n: u64 = std::env::var("SENCA_RAFT_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    sweep("long 3", 10_000..10_000 + n, 4, 1500, mixed(3));
    sweep("long 5", 20_000..20_000 + n, 4, 1500, mixed(5));
    sweep("long file", 30_000..30_000 + n / 20, 3, 800, |seed| {
        let mut p = mixed(3 + 2 * (seed % 2) as usize)(seed);
        p.file_storage = true;
        p.crash_in_persist = 10;
        p
    });
}
