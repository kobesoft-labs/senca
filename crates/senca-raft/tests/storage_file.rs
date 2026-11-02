//! ファイルによる記憶域の試験（書きかけの末尾・破損・圧縮・snapshot との整合）。

use bytes::Bytes;
use senca_raft::storage_file::FileStorage;
use senca_raft::*;
use std::fs;
use std::path::PathBuf;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("senca-raft-fs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ent(index: u64, term: u64) -> Entry {
    Entry {
        term,
        index,
        kind: EntryKind::Normal,
        data: Bytes::from(format!(
            "e{index}t{term}-{}",
            "x".repeat(index as usize % 7)
        )),
    }
}

fn ents(lo: u64, hi: u64, term: u64) -> Vec<Entry> {
    (lo..=hi).map(|i| ent(i, term)).collect()
}

fn segs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir.join("log"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    v
}

#[test]
fn roundtrip_rotation_and_truncation() {
    let t = TempDir::new("rt");
    {
        let mut s = FileStorage::open_with(&t.0, 200).unwrap();
        s.save_snapshot(&Snapshot::bootstrap([1, 2, 3])).unwrap();
        s.append(&ents(1, 30, 1)).unwrap();
        s.sync().unwrap();
        s.set_hard_state(&HardState {
            term: 2,
            vote: 3,
            commit: 10,
        })
        .unwrap();
        // 衝突: 21 以降を term 2 で置き換える
        s.append(&ents(21, 25, 2)).unwrap();
        s.sync().unwrap();
    }
    assert!(segs(&t.0).len() > 3, "rotated");
    let mut s = FileStorage::open_with(&t.0, 200).unwrap();
    let st = s.load().unwrap();
    assert_eq!(
        st.hard_state,
        HardState {
            term: 2,
            vote: 3,
            commit: 10
        }
    );
    assert_eq!(
        st.snapshot.unwrap().meta.conf_state,
        ConfState::new([1, 2, 3])
    );
    let mut want = ents(1, 20, 1);
    want.extend(ents(21, 25, 2));
    assert_eq!(st.entries, want);
    // 範囲外の追記は誤り
    assert!(s.append(&ents(30, 30, 2)).is_err());
    // セグメントの境目ちょうどでの切り詰め
    let first_of_last = segs(&t.0)
        .last()
        .unwrap()
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    s.append(&ents(first_of_last, first_of_last + 1, 3))
        .unwrap();
    s.sync().unwrap();
    drop(s);
    let st = FileStorage::open_with(&t.0, 200).unwrap().load().unwrap();
    assert_eq!(st.entries.last().unwrap().index, first_of_last + 1);
    assert_eq!(st.entries.last().unwrap().term, 3);
    assert_eq!(st.entries.len() as u64, first_of_last + 1);
}

#[test]
fn torn_tail_is_truncated_at_every_offset() {
    let t = TempDir::new("torn");
    {
        let mut s = FileStorage::open(&t.0).unwrap();
        s.append(&ents(1, 5, 1)).unwrap();
        s.sync().unwrap();
    }
    let seg = segs(&t.0).pop().unwrap();
    let full = fs::read(&seg).unwrap();
    let rec_len = full.len() - {
        let tmp = TempDir::new("torn-ref");
        let mut s = FileStorage::open(&tmp.0).unwrap();
        s.append(&ents(1, 4, 1)).unwrap();
        s.sync().unwrap();
        fs::read(segs(&tmp.0).pop().unwrap()).unwrap().len()
    };
    let base = full.len() - rec_len;
    for cut in base..full.len() {
        fs::write(&seg, &full[..cut]).unwrap();
        let mut s = FileStorage::open(&t.0).unwrap();
        let st = s.load().unwrap();
        assert_eq!(st.entries, ents(1, 4, 1), "cut at {cut}");
        assert_eq!(
            fs::metadata(&seg).unwrap().len() as usize,
            base,
            "torn bytes removed"
        );
        // 続けて書ける
        s.append(&ents(5, 6, 2)).unwrap();
        s.sync().unwrap();
        drop(s);
        let st = FileStorage::open(&t.0).unwrap().load().unwrap();
        assert_eq!(st.entries.len(), 6);
    }
    // 末尾のゴミ（CRC 不一致）も書きかけとして扱う
    let mut g = full.clone();
    g.extend_from_slice(&[0, 0, 0, 9, 1, 2, 3, 4, 5]);
    fs::write(&seg, &g).unwrap();
    assert_eq!(
        FileStorage::open(&t.0)
            .unwrap()
            .load()
            .unwrap()
            .entries
            .len(),
        5
    );
}

#[test]
fn corruption_in_the_middle_is_an_error() {
    let t = TempDir::new("mid");
    {
        let mut s = FileStorage::open_with(&t.0, 150).unwrap();
        s.append(&ents(1, 20, 1)).unwrap();
        s.sync().unwrap();
    }
    let all = segs(&t.0);
    assert!(all.len() >= 3);
    // 最後でないセグメントの途中を壊す
    let victim = &all[0];
    let mut b = fs::read(victim).unwrap();
    b[12] ^= 0xFF;
    fs::write(victim, &b).unwrap();
    assert!(FileStorage::open_with(&t.0, 150).is_err());
    b[12] ^= 0xFF;
    fs::write(victim, &b).unwrap();
    // セグメントが抜けている
    fs::rename(&all[1], t.0.join("moved")).unwrap();
    assert!(FileStorage::open_with(&t.0, 150).is_err());
    fs::rename(t.0.join("moved"), &all[1]).unwrap();
    assert_eq!(
        FileStorage::open_with(&t.0, 150)
            .unwrap()
            .load()
            .unwrap()
            .entries
            .len(),
        20
    );
    // hard state の破損
    {
        let mut s = FileStorage::open(&t.0).unwrap();
        s.set_hard_state(&HardState {
            term: 1,
            vote: 1,
            commit: 1,
        })
        .unwrap();
    }
    let mut h = fs::read(t.0.join("hardstate")).unwrap();
    let n = h.len();
    h[n - 1] ^= 1;
    fs::write(t.0.join("hardstate"), &h).unwrap();
    assert!(FileStorage::open(&t.0).is_err());
}

#[test]
fn snapshot_compaction_and_mismatch() {
    let t = TempDir::new("snap");
    let conf = ConfState::new([1, 2, 3]);
    let snap = |i: u64, term: u64| Snapshot {
        meta: SnapshotMeta {
            index: i,
            term,
            conf_state: conf.clone(),
        },
        data: Bytes::from(vec![i as u8; 100]),
    };
    {
        let mut s = FileStorage::open_with(&t.0, 200).unwrap();
        s.append(&ents(1, 40, 1)).unwrap();
        s.sync().unwrap();
        let before = segs(&t.0).len();
        // 圧縮（同じ term の entry がある）→ 前のセグメントが消え、後ろは残る
        s.save_snapshot(&snap(25, 1)).unwrap();
        assert!(segs(&t.0).len() < before);
        // 古い snapshot は無視
        s.save_snapshot(&snap(10, 1)).unwrap();
    }
    let st = FileStorage::open_with(&t.0, 200).unwrap().load().unwrap();
    assert_eq!(st.snapshot.as_ref().unwrap().meta.index, 25);
    assert_eq!(st.snapshot.as_ref().unwrap().data.len(), 100);
    assert_eq!(st.entries, ents(26, 40, 1));
    {
        // 食い違う snapshot（リーダーから受け取った）→ ログ全体を捨てる
        let mut s = FileStorage::open_with(&t.0, 200).unwrap();
        s.save_snapshot(&snap(30, 5)).unwrap();
        s.append(&ents(31, 33, 5)).unwrap();
        s.sync().unwrap();
    }
    let st = FileStorage::open_with(&t.0, 200).unwrap().load().unwrap();
    assert_eq!(st.snapshot.as_ref().unwrap().meta.term, 5);
    assert_eq!(st.entries, ents(31, 33, 5));
    let n_snaps = fs::read_dir(&t.0)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("snap-")
        })
        .count();
    assert_eq!(n_snaps, 1, "old snapshots removed");
    // 圧縮でログが空になった後の追記
    {
        let mut s = FileStorage::open_with(&t.0, 200).unwrap();
        s.save_snapshot(&snap(33, 5)).unwrap();
        s.append(&ents(34, 35, 5)).unwrap();
        s.sync().unwrap();
    }
    let st = FileStorage::open_with(&t.0, 200).unwrap().load().unwrap();
    assert_eq!(st.snapshot.as_ref().unwrap().meta.index, 33);
    assert_eq!(st.entries, ents(34, 35, 5));
}

#[test]
fn crash_between_snapshot_and_log_cleanup() {
    // snapshot を書いた直後（ログを消す前）に落ちた状態: 食い違うログは起動時に捨てる。
    let t = TempDir::new("snapcrash");
    {
        let mut s = FileStorage::open(&t.0).unwrap();
        s.append(&ents(1, 10, 1)).unwrap();
        s.sync().unwrap();
    }
    let other = TempDir::new("snapcrash-src");
    {
        let mut s = FileStorage::open(&other.0).unwrap();
        s.save_snapshot(&Snapshot {
            meta: SnapshotMeta {
                index: 5,
                term: 3,
                conf_state: ConfState::new([1]),
            },
            data: Bytes::from_static(b"state"),
        })
        .unwrap();
    }
    for e in fs::read_dir(&other.0).unwrap() {
        let e = e.unwrap();
        if e.file_name().to_string_lossy().starts_with("snap-") {
            fs::copy(e.path(), t.0.join(e.file_name())).unwrap();
        }
    }
    let st = FileStorage::open(&t.0).unwrap().load().unwrap();
    assert_eq!(st.snapshot.unwrap().meta.index, 5);
    assert!(
        st.entries.is_empty(),
        "log that disagrees with the snapshot is dropped"
    );
}

#[test]
fn raft_restarts_from_file_storage() {
    // 1 ノードで書いて、閉じて、開き直して続ける。
    let t = TempDir::new("restart");
    let mut applied = Vec::new();
    for round in 0..3 {
        let mut s = FileStorage::open_with(&t.0, 300).unwrap();
        if round == 0 {
            s.save_snapshot(&Snapshot::bootstrap([1])).unwrap();
        }
        let init = s.load().unwrap();
        let mut n = RawNode::new(Config::new(1), init).unwrap();
        n.campaign();
        for i in 0..20 {
            n.propose(Bytes::from(format!("r{round}-{i}"))).unwrap();
            while let Some(rd) = n.ready() {
                s.persist(&rd).unwrap();
                for e in rd.committed_entries {
                    if e.kind == EntryKind::Normal && round == 2 {
                        applied.push(e.data.clone());
                    }
                }
            }
        }
        if round == 1 {
            let idx = n.status().applied;
            let snap = n.compact(idx - 3, Bytes::from_static(b"sm")).unwrap();
            s.save_snapshot(&snap).unwrap();
        }
    }
    // round 2 は起動時に round 1 の最後の 3 件を適用し直してから、自分の 20 件を適用する。
    let names: Vec<String> = applied
        .iter()
        .map(|b| String::from_utf8(b.to_vec()).unwrap())
        .collect();
    assert_eq!(&names[..3], &["r1-17", "r1-18", "r1-19"]);
    assert_eq!(names.len(), 23);
}
