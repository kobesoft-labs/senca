//! 監査ログ（仕様 9.5）の書き込み・回転・読み出し。
//!
//! - 書き込み: `data_dir/audit.log` に 1 行 1 JSON を追記する。行と改行は 1 回の write で書く（O_APPEND。途中で落ちても
//!   それまでの行は壊れない。書きかけの行は改行が無いので、読むときに飛ばす）。
//! - 回転: 書いた後に大きさが `max_bytes` 以上なら、`audit.log` を `audit.log.{N}` へ名前を変える。N は 1 から増えるだけの
//!   番号で、大きいほど新しい。回転したファイルは新しいものから `keep` 個を残し、古いものを消す。`max_bytes` が 0 なら回転しない。
//! - 読み出し: 新しい順。ファイルの後ろから 64 KiB ずつ読む（ファイル全体をメモリに載せない）。1 回に読む量には上限があり、
//!   上限に達したら、そこまでの位置（ファイルの番号, 行の先頭のオフセット）を返す。今書いている `audit.log` の番号は
//!   「回転したファイルの最大の番号 + 1」で、回転しても同じ番号のまま `audit.log.{N}` になるので、位置は回転をまたいで使える。
//! - 今書いているファイルは書き込みのロックの中で開く（開いた後に回転しても、開いたファイルはそのまま読める）。回転したファイルは
//!   読むときに 1 つずつ開く。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 今書いているファイルの名前。
pub const FILE: &str = "audit.log";
/// 後ろから読むときの 1 回の大きさ。
const CHUNK: usize = 64 << 10;
/// 1 回の読み出しで調べる量の上限（バイト）。
pub const SCAN_BUDGET: u64 = 16 << 20;
/// これより長い行は壊れたものとして飛ばす。
const MAX_LINE: usize = 1 << 20;
/// 時刻の前後のずれの許容（ミリ秒）。`ts` は要求を受けた時刻で、書くのは処理の後なので、ファイルの中の順と `ts` の順は
/// 少しずれることがある。範囲の外と判断して読むのを止める・飛ばすときは、この分だけ余裕を見る。
pub const SKEW_MS: i64 = 5 * 60_000;

/// 回転の既定（`--audit-log-max-bytes`・`--audit-log-keep`）。
pub const DEFAULT_MAX_BYTES: u64 = 64 << 20;
pub const DEFAULT_KEEP: usize = 8;

/// 読み出しの位置: ファイル `file` の、オフセット `off` より前に始まる行から続ける（その後は番号の小さいファイル）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pos {
    pub file: u64,
    pub off: u64,
}

/// 絞り込みの条件。`tenant` が None ならすべてのテナント。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Filter {
    #[serde(default)]
    pub tenant: Option<String>,
    /// この時刻以降（含む、UNIX ミリ秒）。
    #[serde(default)]
    pub since_ms: Option<i64>,
    /// この時刻より前（含まない）。
    #[serde(default)]
    pub until_ms: Option<i64>,
    /// 完全一致。`.` か `*` で終われば前方一致（`*` は除いて比べる）。
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    /// action・subject・resource・request_id のどれかに含まれる（大文字・小文字を区別しない）。
    #[serde(default)]
    pub q: Option<String>,
}

impl Filter {
    /// 1 行（JSON のオブジェクト）が条件に合うか。`ts` はその行の時刻。
    pub fn matches(&self, e: &Map<String, Value>, ts: i64) -> bool {
        let s = |k: &str| e.get(k).and_then(Value::as_str);
        if self.tenant.as_deref().is_some_and(|t| s("tenant") != Some(t)) {
            return false;
        }
        if self.since_ms.is_some_and(|x| ts < x) || self.until_ms.is_some_and(|x| ts >= x) {
            return false;
        }
        if let Some(a) = &self.action {
            let act = s("action").unwrap_or("");
            let ok = match a.strip_suffix('*') {
                Some(p) => act.starts_with(p),
                None if a.ends_with('.') => act.starts_with(a.as_str()),
                None => act == a,
            };
            if !ok {
                return false;
            }
        }
        if self.subject.as_deref().is_some_and(|x| s("subject") != Some(x)) || self.resource.as_deref().is_some_and(|x| s("resource") != Some(x)) {
            return false;
        }
        if let Some(q) = &self.q {
            let q = q.to_lowercase();
            if !["action", "subject", "resource", "request_id"].iter().any(|k| s(k).is_some_and(|v| v.to_lowercase().contains(&q))) {
                return false;
            }
        }
        true
    }

    /// 行のバイト列だけで「合わない」と分かるか（JSON を読む前の安い検査）。テナントの値に JSON で書き換わる文字が無いときだけ。
    fn surely_not(&self, line: &[u8]) -> bool {
        match &self.tenant {
            Some(t) if !t.chars().any(|c| c == '"' || c == '\\' || c.is_control()) => {
                let needle = format!("\"tenant\":\"{t}\"");
                !contains(line, needle.as_bytes())
            }
            _ => false,
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// 合った 1 件。`after` はこの行の次（より古い行）から続ける位置。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    pub entry: Map<String, Value>,
    pub ts_ms: i64,
    pub after: Pos,
}

/// 1 回の読み出しの結果。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Page {
    /// 新しい順。
    pub hits: Vec<Hit>,
    /// 続きの位置（None なら最後まで読んだ）。
    pub next: Option<Pos>,
    /// 読む量の上限で止まったとき、最後に調べた行の時刻（続きの行はおおむねこれより古い）。読めた行が無ければ None。
    #[serde(default)]
    pub frontier_ms: Option<i64>,
    /// 読む量の上限で止まった（`limit` 件に達する前に）。
    #[serde(default)]
    pub budget_hit: bool,
}

pub struct AuditLog {
    dir: PathBuf,
    max_bytes: u64,
    keep: usize,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(dir: &Path, max_bytes: u64, keep: usize) -> AuditLog {
        AuditLog { dir: dir.to_path_buf(), max_bytes, keep: keep.max(1), lock: Mutex::new(()) }
    }

    fn active_path(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    fn rotated_path(&self, n: u64) -> PathBuf {
        self.dir.join(format!("{FILE}.{n}"))
    }

    /// 回転したファイルの番号（大きい順）。
    fn rotated(&self) -> Vec<u64> {
        let prefix = format!("{FILE}.");
        let mut v: Vec<u64> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_prefix(&prefix)).and_then(|n| n.parse::<u64>().ok()))
                    .filter(|n| *n > 0)
                    .collect()
            })
            .unwrap_or_default();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    }

    /// 1 行を追記する（`line` は改行を含まない JSON）。大きさが上限を超えたら回転する。
    pub fn append(&self, line: &str) -> std::io::Result<()> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.active_path())?;
        f.write_all(&buf)?;
        if self.max_bytes > 0 && f.metadata()?.len() >= self.max_bytes {
            drop(f);
            self.rotate()?;
        }
        Ok(())
    }

    /// `audit.log` → `audit.log.{最大 + 1}`。古いものを消す（ロックの中で呼ぶ）。
    fn rotate(&self) -> std::io::Result<()> {
        let gens = self.rotated();
        let n = gens.first().map_or(1, |g| g + 1);
        std::fs::rename(self.active_path(), self.rotated_path(n))?;
        for g in gens.iter().skip(self.keep.saturating_sub(1)) {
            let _ = std::fs::remove_file(self.rotated_path(*g));
        }
        Ok(())
    }

    /// 読むファイル: (今書いているファイルの番号, そのファイル, 回転したファイルの番号（大きい順）)。今書いているファイルは
    /// ロックの中で開く（開いた後に回転しても、開いたものはそのまま読める）。回転したファイルは中身が変わらず、番号も
    /// 使い回さないので、読むときに開く（消えていれば飛ばす。一度に開くのは 1 つだけ）。
    fn files(&self) -> (u64, Option<File>, Vec<u64>) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let gens = self.rotated();
        let active = gens.first().map_or(1, |g| g + 1);
        (active, File::open(self.active_path()).ok(), gens)
    }

    /// 新しい順に、`start`（None なら最新）から `limit` 件まで読む。読む量は `budget` バイトまで。
    pub fn query(&self, f: &Filter, start: Option<Pos>, limit: usize, budget: u64) -> Page {
        let (active, active_file, gens) = self.files();
        let active_len = active_file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
        let start = start.unwrap_or(Pos { file: active, off: active_len });
        let mut page = Page::default();
        let mut scanned = 0u64;
        let stop_before = f.since_ms.map(|s| s.saturating_sub(SKEW_MS));
        for n in std::iter::once(active).chain(gens).filter(|n| *n <= start.file) {
            let rotated;
            let (file, len) = if n == active {
                match &active_file {
                    Some(x) => (x, active_len),
                    None => continue,
                }
            } else {
                rotated = match File::open(self.rotated_path(n)) {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                let l = rotated.metadata().map(|m| m.len()).unwrap_or(0);
                (&rotated, l)
            };
            let mut end = if n == start.file { start.off.min(len) } else { len };
            if let Some(u) = f.until_ms {
                end = seek_until(file, end, u.saturating_add(SKEW_MS));
            }
            let mut lines = RevLines::new(file, end);
            loop {
                let (s, line) = match lines.next_line() {
                    Ok(Some(x)) => x,
                    Ok(None) => break,
                    // 読めないファイルは、そこまでとして次（古い方）へ
                    Err(_) => break,
                };
                scanned += line.as_ref().map_or(MAX_LINE, |l| l.len() + 1) as u64;
                let here = Pos { file: n, off: s };
                if let Some(l) = line
                    && !f.surely_not(&l)
                    && let Some((e, ts)) = parse_line(&l)
                {
                    page.frontier_ms = Some(ts);
                    if stop_before.is_some_and(|b| ts < b) {
                        page.next = None;
                        return page;
                    }
                    if f.matches(&e, ts) {
                        page.hits.push(Hit { entry: e, ts_ms: ts, after: here });
                        if page.hits.len() >= limit {
                            page.next = Some(here);
                            return page;
                        }
                    }
                }
                if scanned >= budget {
                    page.next = Some(here);
                    page.budget_hit = true;
                    return page;
                }
            }
        }
        page.next = None;
        page
    }
}

/// 1 行を読む。JSON のオブジェクトで、`ts`（RFC 3339）と `action`（文字列）があるものだけ。
fn parse_line(l: &[u8]) -> Option<(Map<String, Value>, i64)> {
    let v: Value = serde_json::from_slice(l).ok()?;
    let Value::Object(m) = v else { return None };
    let ts = senca_core::rfc3339::parse_millis(m.get("ts")?.as_str()?)?;
    m.get("action")?.as_str()?;
    Some((m, ts))
}

/// `[0, end)` のうち、ここから後ろはすべて `after_ms` より新しい、と言える行の先頭を二分探索で探す（おおむね時刻順なので）。
/// 見つからない・読めないときは `end` のまま（余分に読むだけで、取りこぼさない）。
fn seek_until(file: &File, end: u64, after_ms: i64) -> u64 {
    let (mut lo, mut hi) = (0u64, end);
    let mut buf = vec![0u8; 8 << 10];
    while hi - lo > CHUNK as u64 {
        let mid = lo + (hi - lo) / 2;
        // mid の後の最初の行の先頭と時刻
        let found = (|| {
            let mut f = file;
            f.seek(SeekFrom::Start(mid)).ok()?;
            let n = f.read(&mut buf).ok()?;
            let b = &buf[..n.min((hi - mid) as usize)];
            let nl = b.iter().position(|c| *c == b'\n')?;
            let rest = &b[nl + 1..];
            let nl2 = rest.iter().position(|c| *c == b'\n')?;
            let (_, ts) = parse_line(&rest[..nl2])?;
            Some((mid + nl as u64 + 1, ts))
        })();
        match found {
            Some((start, ts)) if ts > after_ms => hi = start,
            _ => lo = mid,
        }
    }
    hi
}

/// ファイルの `[0, end)` を後ろから 1 行ずつ読む。改行で終わっていない最後の断片（書きかけ）は飛ばす。
struct RevLines<'a> {
    file: &'a File,
    /// `buf` の先頭の、ファイルの中の位置。
    pos: u64,
    /// まだ返していない範囲 `[0, limit)` のうち `[pos, pos + buf.len())`。
    buf: Vec<u8>,
    limit: u64,
    trimmed: bool,
}

impl<'a> RevLines<'a> {
    fn new(file: &'a File, end: u64) -> Self {
        RevLines { file, pos: end, buf: Vec::new(), limit: end, trimmed: false }
    }

    /// `pos` の前を 1 回分読み足す。
    fn read_more(&mut self) -> std::io::Result<()> {
        let n = (self.pos as usize).min(CHUNK);
        let start = self.pos - n as u64;
        let mut chunk = vec![0u8; n];
        let mut f = self.file;
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&self.buf);
        self.buf = chunk;
        self.pos = start;
        Ok(())
    }

    /// `upto`（絶対位置）より前の最後の改行の次の位置（無ければ 0）。行が長すぎれば、読んだものを捨てて true を返す。
    fn line_start(&mut self, upto: u64) -> std::io::Result<(u64, bool)> {
        let mut oversized = false;
        let mut upto = upto;
        loop {
            let rel = (upto.saturating_sub(self.pos) as usize).min(self.buf.len());
            if let Some(i) = self.buf[..rel].iter().rposition(|c| *c == b'\n') {
                return Ok((self.pos + i as u64 + 1, oversized));
            }
            if self.pos == 0 {
                return Ok((0, oversized));
            }
            if self.buf.len() > MAX_LINE {
                // 長すぎる行: 中身は要らないので、探す範囲だけを残す
                oversized = true;
                upto = self.pos;
                self.buf.clear();
            }
            self.read_more()?;
        }
    }

    /// 次の行（より古い行）。(行の先頭, 中身。長すぎれば None)。
    fn next_line(&mut self) -> std::io::Result<Option<(u64, Option<Vec<u8>>)>> {
        if !self.trimmed {
            self.trimmed = true;
            if self.limit == 0 {
                return Ok(None);
            }
            if self.buf.is_empty() {
                self.read_more()?;
            }
            if self.buf.last() != Some(&b'\n') {
                // 書きかけの断片を飛ばす
                let (s, _) = self.line_start(self.limit)?;
                self.cut(s);
            }
        }
        if self.limit == 0 {
            return Ok(None);
        }
        let (s, oversized) = self.line_start(self.limit - 1)?;
        let line = if oversized || self.pos > s { None } else { Some(self.buf[(s - self.pos) as usize..(self.limit - 1 - self.pos) as usize].to_vec()) };
        self.cut(s);
        Ok(Some((s, line)))
    }

    /// `[s, limit)` を返し終えた。
    fn cut(&mut self, s: u64) {
        let keep = s.saturating_sub(self.pos) as usize;
        self.buf.truncate(keep.min(self.buf.len()));
        if self.pos > s {
            self.pos = s;
            self.buf.clear();
        }
        self.limit = s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("senca-audit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn line(i: usize, tenant: &str, action: &str, subject: &str) -> String {
        serde_json::json!({
            "action": action,
            "request_id": format!("req_{i}"),
            "resource": format!("res-{i}"),
            "subject": subject,
            "tenant": tenant,
            "ts": crate::util::rfc3339(1_700_000_000_000 + i as i64 * 1000),
        })
        .to_string()
    }

    fn all(log: &AuditLog, f: &Filter, limit: usize, budget: u64) -> (Vec<String>, usize) {
        let mut out = Vec::new();
        let mut pos = None;
        let mut pages = 0;
        loop {
            let p = log.query(f, pos, limit, budget);
            pages += 1;
            out.extend(p.hits.iter().map(|h| h.entry["request_id"].as_str().unwrap().to_string()));
            match p.next {
                Some(n) => pos = Some(n),
                None => return (out, pages),
            }
            assert!(pages < 100_000);
        }
    }

    fn ids(r: impl Iterator<Item = usize>) -> Vec<String> {
        r.map(|i| format!("req_{i}")).collect()
    }

    #[test]
    fn newest_first_across_chunks_and_pages() {
        let d = tmp("chunks");
        let log = AuditLog::new(&d, 0, 4);
        // 1 行 ~ 170 バイト × 3000 = ~500 KiB（64 KiB の区切りを何度もまたぐ）
        for i in 0..3000 {
            log.append(&line(i, "default", "stream.create", "token:a")).unwrap();
        }
        let (got, pages) = all(&log, &Filter::default(), 7, SCAN_BUDGET);
        assert_eq!(got, ids((0..3000).rev()));
        assert_eq!(pages, 3000 / 7 + 1);
        // 読む量の上限が小さくても、取りこぼし・重複が無い
        let (got, pages) = all(&log, &Filter::default(), 1000, 10_000);
        assert_eq!(got, ids((0..3000).rev()));
        assert!(pages > 10);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rotation_keeps_n_files_and_reads_across_them() {
        let d = tmp("rotate");
        let one = line(0, "default", "stream.create", "token:a").len() as u64 + 1;
        // 10 行ごとに回転、回転したものは 3 個まで
        let log = AuditLog::new(&d, one * 10, 3);
        for i in 0..55 {
            log.append(&line(i, "default", "stream.create", "token:a")).unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, vec!["audit.log", "audit.log.3", "audit.log.4", "audit.log.5"]);
        // 残っているのは 20..55（audit.log.3 = 20..29、.4 = 30..39、.5 = 40..49、audit.log = 50..54）
        let (got, _) = all(&log, &Filter::default(), 4, SCAN_BUDGET);
        assert_eq!(got, ids((20..55).rev()));

        // 読んでいる途中で回転しても続きが読める
        let p1 = log.query(&Filter::default(), None, 3, SCAN_BUDGET);
        assert_eq!(p1.hits.iter().map(|h| h.entry["request_id"].as_str().unwrap()).collect::<Vec<_>>(), ["req_54", "req_53", "req_52"]);
        for i in 55..60 {
            log.append(&line(i, "default", "stream.create", "token:a")).unwrap();
        }
        assert!(d.join("audit.log.6").exists());
        let mut rest = Vec::new();
        let mut pos = p1.next;
        while let Some(p) = pos {
            let pg = log.query(&Filter::default(), Some(p), 5, SCAN_BUDGET);
            rest.extend(pg.hits.iter().map(|h| h.entry["request_id"].as_str().unwrap().to_string()));
            pos = pg.next;
        }
        // audit.log.3 は回転で消えたので 30 から
        assert_eq!(rest, ids((30..52).rev()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn filters_and_tenant_isolation() {
        let d = tmp("filters");
        let log = AuditLog::new(&d, 0, 1);
        for i in 0..40 {
            let (t, a, s) = match i % 4 {
                0 => ("default", "stream.create", "token:a"),
                1 => ("other", "stream.delete", "token:b"),
                2 => ("default", "token.create", "user:Alice"),
                _ => ("default", "streamx.update", "token:a"),
            };
            log.append(&line(i, t, a, s)).unwrap();
        }
        let f = |x: Filter| all(&log, &x, 1000, SCAN_BUDGET).0;
        let def = || Filter { tenant: Some("default".into()), ..Default::default() };
        // テナント: 他のテナントの行は出ない
        assert_eq!(f(def()).len(), 30);
        assert!(f(Filter { tenant: Some("other".into()), ..Default::default() }) == ids((0..40).rev().filter(|i| i % 4 == 1)));
        assert!(f(Filter { tenant: Some("nosuch".into()), ..Default::default() }).is_empty());
        assert_eq!(f(Filter::default()).len(), 40);
        // action: 完全一致・前方一致（`.`・`*`）
        assert_eq!(f(Filter { action: Some("stream.create".into()), ..def() }), ids((0..40).rev().filter(|i| i % 4 == 0)));
        assert_eq!(f(Filter { action: Some("stream.".into()), ..def() }), ids((0..40).rev().filter(|i| i % 4 == 0)));
        assert_eq!(f(Filter { action: Some("stream*".into()), ..def() }), ids((0..40).rev().filter(|i| i % 4 == 0 || i % 4 == 3)));
        assert!(f(Filter { action: Some("stream".into()), ..def() }).is_empty());
        // subject・resource
        assert_eq!(f(Filter { subject: Some("user:Alice".into()), ..def() }), ids((0..40).rev().filter(|i| i % 4 == 2)));
        assert_eq!(f(Filter { resource: Some("res-8".into()), ..def() }), ids([8].into_iter()));
        assert!(f(Filter { resource: Some("res-9".into()), ..def() }).is_empty());
        // q: 大文字・小文字を区別しない部分一致
        assert_eq!(f(Filter { q: Some("ALICE".into()), ..def() }), ids((0..40).rev().filter(|i| i % 4 == 2)));
        assert_eq!(f(Filter { q: Some("req_3".into()), ..def() }), ids([39, 38, 36, 35, 34, 32, 31, 30, 3].into_iter()));
        // 時刻の範囲（since は含む、until は含まない）
        let t = |i: i64| 1_700_000_000_000 + i * 1000;
        assert_eq!(
            f(Filter { since_ms: Some(t(10)), until_ms: Some(t(20)), ..Default::default() }),
            ids((10..20).rev())
        );
        assert_eq!(f(Filter { until_ms: Some(t(3)), ..Default::default() }), ids((0..3).rev()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn since_stops_early_and_until_seeks() {
        let d = tmp("seek");
        let log = AuditLog::new(&d, 0, 1);
        for i in 0..20_000 {
            log.append(&line(i, "default", "stream.update", "token:a")).unwrap();
        }
        let t = |i: i64| 1_700_000_000_000 + i * 1000;
        // ファイル（~3 MiB）は上限（1 MiB）より大きいが、until は二分探索で後ろを飛ばし、since より十分古い行で止まるので 1 回で読める
        let p = log.query(&Filter { since_ms: Some(t(1000)), until_ms: Some(t(1010)), ..Default::default() }, None, 100, 1 << 20);
        assert_eq!(p.hits.iter().map(|h| h.entry["request_id"].as_str().unwrap().to_string()).collect::<Vec<_>>(), ids((1000..1010).rev()));
        assert!(p.next.is_none() && !p.budget_hit);
        // 範囲が無ければ、上限で止まって続きを返す
        let p = log.query(&Filter::default(), None, 100_000, 1 << 20);
        assert!(p.budget_hit && p.next.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let d = tmp("bad");
        let log = AuditLog::new(&d, 0, 1);
        log.append(&line(0, "default", "stream.create", "token:a")).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(d.join(FILE)).unwrap();
        f.write_all(b"not json\n{\"action\":\"x\"}\n[1,2]\n\n{\"ts\":\"bad\",\"action\":\"x\"}\n").unwrap();
        // 長すぎる行（改行なしの 3 MiB）
        f.write_all(&vec![b'a'; 3 << 20]).unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);
        log.append(&line(1, "default", "stream.create", "token:a")).unwrap();
        // 書きかけの最後の行（改行が無い）
        let mut f = std::fs::OpenOptions::new().append(true).open(d.join(FILE)).unwrap();
        f.write_all(line(2, "default", "stream.create", "token:a").as_bytes()).unwrap();
        drop(f);
        let (got, _) = all(&log, &Filter::default(), 10, SCAN_BUDGET);
        assert_eq!(got, ids([1, 0].into_iter()));
        // 小さい上限でもくり返せば同じ
        let (got, _) = all(&log, &Filter::default(), 10, 100);
        assert_eq!(got, ids([1, 0].into_iter()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_and_missing() {
        let d = tmp("empty");
        let log = AuditLog::new(&d, 0, 1);
        let p = log.query(&Filter::default(), None, 10, SCAN_BUDGET);
        assert!(p.hits.is_empty() && p.next.is_none());
        std::fs::write(d.join(FILE), b"").unwrap();
        let p = log.query(&Filter::default(), None, 10, SCAN_BUDGET);
        assert!(p.hits.is_empty() && p.next.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
