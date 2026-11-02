//! 制御状態の永続化（単一ノード）。
//!
//! - `control/log`: 適用したコマンドの追記ログ。1 レコード = `len:u32be | crc32:u32be | json`。
//!   1 件ずつ書いて fsync してから応答する（仕様 8.1「多数派での永続化」の単一ノード版）。
//! - `control/snapshot.json`: 状態の写し（一時名 → fsync → rename → ディレクトリ fsync）。
//!   ログが `SNAPSHOT_EVERY` 件を超えたら写しを作り、ログを空にする。
//! - 起動時: 写しを読み、ログを頭から適用する。末尾の壊れたレコード（書きかけ）は切り捨てる。
//!   途中のレコードが壊れていたら起動しない（黙って捨てない）。

use senca_control::state::{Command, ControlState};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const SNAPSHOT_EVERY: u64 = 1000;
const MAX_RECORD: usize = 64 << 20;

pub struct Store {
    dir: PathBuf,
    log: File,
    log_records: u64,
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// 一時名 → fsync → rename → ディレクトリ fsync。
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.tmp", path.file_name().and_then(|s| s.to_str()).unwrap_or("x")));
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fsync_dir(dir)
}

impl Store {
    /// 開いて状態を復元する。
    pub fn open(dir: &Path) -> io::Result<(Store, ControlState)> {
        fs::create_dir_all(dir)?;
        let snap = dir.join("snapshot.json");
        let mut state = match fs::read(&snap) {
            Ok(b) => serde_json::from_slice::<ControlState>(&b)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("snapshot: {e}")))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => ControlState::default(),
            Err(e) => return Err(e),
        };
        let log_path = dir.join("log");
        let mut buf = Vec::new();
        if let Ok(mut f) = File::open(&log_path) {
            f.read_to_end(&mut buf)?;
        }
        let mut pos = 0usize;
        let mut n = 0u64;
        let mut good_end = 0usize;
        while pos + 8 <= buf.len() {
            let len = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            let crc = u32::from_be_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
            if len > MAX_RECORD || pos + 8 + len > buf.len() {
                break;
            }
            let body = &buf[pos + 8..pos + 8 + len];
            if crc32(body) != crc {
                if pos + 8 + len == buf.len() {
                    break; // 末尾の書きかけ
                }
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("control log corrupt at {pos}")));
            }
            let cmd: Command = serde_json::from_slice(body)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("control log record {n}: {e}")))?;
            // 失敗したコマンドはログに書かない約束なので、ここで失敗するのは異常。
            state
                .apply(cmd)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("control log record {n} does not apply: {e}")))?;
            pos += 8 + len;
            good_end = pos;
            n += 1;
        }
        if good_end < buf.len() {
            // 書きかけを切り捨てる
            let f = OpenOptions::new().write(true).open(&log_path)?;
            f.set_len(good_end as u64)?;
            f.sync_all()?;
        }
        let log = OpenOptions::new().create(true).append(true).open(&log_path)?;
        fsync_dir(dir)?;
        Ok((Store { dir: dir.to_path_buf(), log, log_records: n }, state))
    }

    /// 適用に成功したコマンドを書く。`state` は適用後の状態（写しを作るときに使う）。
    pub fn append(&mut self, cmd: &Command, state: &ControlState) -> io::Result<()> {
        let body = serde_json::to_vec(cmd).map_err(io::Error::other)?;
        let mut rec = Vec::with_capacity(body.len() + 8);
        rec.extend_from_slice(&(body.len() as u32).to_be_bytes());
        rec.extend_from_slice(&crc32(&body).to_be_bytes());
        rec.extend_from_slice(&body);
        self.log.write_all(&rec)?;
        self.log.sync_data()?;
        self.log_records += 1;
        if self.log_records >= SNAPSHOT_EVERY {
            self.snapshot(state)?;
        }
        Ok(())
    }

    pub fn snapshot(&mut self, state: &ControlState) -> io::Result<()> {
        let j = serde_json::to_vec(state).map_err(io::Error::other)?;
        write_atomic(&self.dir.join("snapshot.json"), &j)?;
        let f = OpenOptions::new().write(true).open(self.dir.join("log"))?;
        f.set_len(0)?;
        f.sync_all()?;
        self.log = OpenOptions::new().append(true).open(self.dir.join("log"))?;
        self.log_records = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_control::model::NodeInfo;

    fn node(id: &str) -> Command {
        Command::PutNode {
            node: NodeInfo {
                id: id.into(),
                name: id.into(),
                draining: false,
                registered_at_ms: 0,
                media_addr: None,
                labels: Default::default(),
            },
        }
    }

    #[test]
    fn recover_and_truncate_tail() {
        let d = std::env::temp_dir().join(format!("senca-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        {
            let (mut s, mut st) = Store::open(&d).unwrap();
            for i in 0..3 {
                let c = node(&format!("n{i}"));
                st.apply(c.clone()).unwrap();
                s.append(&c, &st).unwrap();
            }
        }
        // 書きかけを足す
        let mut f = OpenOptions::new().append(true).open(d.join("log")).unwrap();
        f.write_all(&[0, 0, 0, 50, 1, 2]).unwrap();
        drop(f);
        let (mut s, mut st) = Store::open(&d).unwrap();
        assert_eq!(st.nodes.len(), 3);
        st.apply(node("n3")).unwrap();
        s.append(&node("n3"), &st).unwrap();
        s.snapshot(&st).unwrap();
        let (_s, st2) = Store::open(&d).unwrap();
        assert_eq!(st2.nodes.len(), 4);
        assert_eq!(st2.applied_index, 4);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn crc_known() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
