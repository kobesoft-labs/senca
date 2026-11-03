//! 録画の索引（ADR-0005）。I/O を持たない。
//!
//! - セグメントは stream ごとに 1 回だけ作り（ライブと同じバイト列）、`segments/{stream}/{run}/` に不変の
//!   ファイルとして置く。複数の録画が同じセグメントを参照できる（REQ-002 の「重なる予約で共有」）。
//! - 録画ごとの索引は追記のみの JSON Lines（`index.jsonl`）。1 行 = 1 つの [`Entry`]。
//!   書きかけの末尾行は回収時に捨てる。途中の行が壊れていたら索引には触らない（消さない）。
//! - 確定（`Entry::End`）の後は追記しない。

use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Entry {
    /// 先頭行。
    Begin {
        schema: u32,
        recording: String,
        stream: String,
        /// "ts" または "fmp4"。
        container: String,
        started_at_ms: i64,
        /// 表示の開始（この時刻より前のフレームは書き出しで切り捨てる。UNIX ms）。
        #[serde(default)]
        not_before_ms: Option<i64>,
    },
    /// fMP4 の初期化セグメント（設定が変わるたびに新しいものを指す）。
    Init {
        path: String,
        sha256: String,
        /// 保存先（S3 など）のオブジェクトのキー。上げ終わった索引にだけ付く。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    Segment(SegmentRef),
    /// 欠落（入力の切断、キューのあふれ、ノードの障害など）。
    Gap { at_ms: i64, reason: String, #[serde(default)] dropped: u64 },
    /// 確定。
    End {
        ended_at_ms: i64,
        /// "finalized"（正常）または "interrupted"（回収）。
        state: String,
        /// 表示の終わり（UNIX ms）。
        #[serde(default)]
        not_after_ms: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRef {
    /// データ置き場からの相対パス（`segments/{stream}/{run}/{name}`）。
    pub path: String,
    pub epoch: u64,
    pub sequence: u64,
    pub duration_ms: u32,
    pub bytes: u64,
    pub sha256: String,
    /// セグメントの先頭の壁時計（UNIX ms、受信時刻から推定）。
    pub wall_ms: i64,
    /// 直前のセグメントとの間に不連続がある。
    #[serde(default)]
    pub discontinuity: bool,
    /// 保存先（S3 など）のオブジェクトのキー。上げ終わった索引にだけ付く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// 索引を読んだ結果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Index {
    pub recording: String,
    pub stream: String,
    pub container: String,
    pub started_at_ms: i64,
    pub not_before_ms: Option<i64>,
    pub init: Option<(String, String)>,
    /// 初期化セグメントの保存先のキー。
    pub init_key: Option<String>,
    pub segments: Vec<SegmentRef>,
    pub gaps: Vec<(i64, String, u64)>,
    pub end: Option<(i64, String, Option<i64>)>,
    /// 末尾の書きかけを捨てた。
    pub truncated_tail: bool,
    /// 読めた部分のバイト数（書きかけを切り捨てるのに使う）。
    pub valid_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// 途中の行が壊れている（何行目か）。
    Corrupt(usize),
    MissingBegin,
    UnsupportedSchema(u32),
    /// End の後に行がある。
    AfterEnd(usize),
    /// 確定していない（End が無い）。
    NotFinal,
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Corrupt(n) => write!(f, "index line {n} is corrupt"),
            IndexError::MissingBegin => f.write_str("index has no begin line"),
            IndexError::UnsupportedSchema(s) => write!(f, "unsupported index schema {s}"),
            IndexError::AfterEnd(n) => write!(f, "index line {n} after end"),
            IndexError::NotFinal => f.write_str("index is not finalized"),
        }
    }
}

impl std::error::Error for IndexError {}

/// 1 行にする（末尾の改行つき）。
pub fn encode(e: &Entry) -> Vec<u8> {
    let mut v = serde_json::to_vec(e).expect("entry serializes");
    v.push(b'\n');
    v
}

/// 索引を読む。最後の行が改行で終わっていない・壊れているなら書きかけとして捨てる。
pub fn parse(data: &[u8]) -> Result<Index, IndexError> {
    let mut idx = Index::default();
    let mut pos = 0usize;
    let mut line_no = 0usize;
    let mut began = false;
    while pos < data.len() {
        line_no += 1;
        let nl = data[pos..].iter().position(|&c| c == b'\n');
        let (line, next, complete) = match nl {
            Some(n) => (&data[pos..pos + n], pos + n + 1, true),
            None => (&data[pos..], data.len(), false),
        };
        let parsed: Option<Entry> = serde_json::from_slice(line).ok();
        let e = match (parsed, complete) {
            (Some(e), true) => e,
            (_, false) | (None, true) if next == data.len() => {
                idx.truncated_tail = true;
                break;
            }
            _ => return Err(IndexError::Corrupt(line_no)),
        };
        if idx.end.is_some() {
            return Err(IndexError::AfterEnd(line_no));
        }
        match e {
            Entry::Begin { schema, recording, stream, container, started_at_ms, not_before_ms } => {
                if began || line_no != 1 {
                    return Err(IndexError::Corrupt(line_no));
                }
                if schema != SCHEMA {
                    return Err(IndexError::UnsupportedSchema(schema));
                }
                began = true;
                idx.recording = recording;
                idx.stream = stream;
                idx.container = container;
                idx.started_at_ms = started_at_ms;
                idx.not_before_ms = not_before_ms;
            }
            _ if !began => return Err(IndexError::MissingBegin),
            Entry::Init { path, sha256, key } => {
                idx.init = Some((path, sha256));
                idx.init_key = key;
            }
            Entry::Segment(s) => idx.segments.push(s),
            Entry::Gap { at_ms, reason, dropped } => idx.gaps.push((at_ms, reason, dropped)),
            Entry::End { ended_at_ms, state, not_after_ms } => idx.end = Some((ended_at_ms, state, not_after_ms)),
        }
        pos = next;
        idx.valid_len = pos;
    }
    if !began {
        return Err(IndexError::MissingBegin);
    }
    Ok(idx)
}

/// 確定した索引の各行に、保存先のキーを付けて書き直す（`key` はデータ置き場からの相対パス → キー）。
/// 行の順番と中身は変えない。書きかけの末尾は捨てる。確定していない索引は書き直さない。
pub fn with_keys(data: &[u8], key: impl Fn(&str) -> String) -> Result<Vec<u8>, IndexError> {
    let idx = parse(data)?;
    if idx.end.is_none() {
        return Err(IndexError::NotFinal);
    }
    let mut out = Vec::with_capacity(data.len() + 64 * (idx.segments.len() + 1));
    for line in data[..idx.valid_len].split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
        let mut e: Entry = serde_json::from_slice(line).map_err(|_| IndexError::Corrupt(0))?;
        match &mut e {
            Entry::Segment(s) => s.key = Some(key(&s.path)),
            Entry::Init { path, key: k, .. } => *k = Some(key(path)),
            _ => {}
        }
        out.extend(encode(&e));
    }
    Ok(out)
}

impl Index {
    /// 索引が参照するオブジェクト（初期化セグメントとセグメント）の（相対パス, 保存先のキー）。
    pub fn objects(&self) -> Vec<(&str, Option<&str>)> {
        let mut v: Vec<(&str, Option<&str>)> = Vec::with_capacity(self.segments.len() + 1);
        if let Some((p, _)) = &self.init {
            v.push((p, self.init_key.as_deref()));
        }
        v.extend(self.segments.iter().map(|s| (s.path.as_str(), s.key.as_deref())));
        v
    }

    pub fn duration_ms(&self) -> u64 {
        self.segments.iter().map(|s| s.duration_ms as u64).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }

    /// VOD のメディアプレイリスト（RFC 8216）。`uri` はセグメントの参照から URL を作る。
    pub fn vod_playlist(&self, uri: impl Fn(&SegmentRef) -> String, init_uri: Option<String>) -> String {
        let target = self.segments.iter().map(|s| s.duration_ms.div_ceil(1000)).max().unwrap_or(1).max(1);
        let mut p = String::new();
        p.push_str("#EXTM3U\n");
        p.push_str(if self.container == "fmp4" { "#EXT-X-VERSION:7\n" } else { "#EXT-X-VERSION:3\n" });
        p.push_str(&format!("#EXT-X-TARGETDURATION:{target}\n"));
        p.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n");
        if let Some(u) = init_uri {
            p.push_str(&format!("#EXT-X-MAP:URI=\"{u}\"\n"));
        }
        for (i, s) in self.segments.iter().enumerate() {
            if i > 0 && s.discontinuity {
                p.push_str("#EXT-X-DISCONTINUITY\n");
            }
            p.push_str(&format!("#EXTINF:{:.3},\n{}\n", s.duration_ms as f64 / 1000.0, uri(s)));
        }
        p.push_str("#EXT-X-ENDLIST\n");
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(n: u64) -> Entry {
        Entry::Segment(SegmentRef {
            path: format!("segments/s/r/e0-{n}.ts"),
            epoch: 0,
            sequence: n,
            duration_ms: 2000,
            bytes: 100,
            sha256: "00".into(),
            wall_ms: 0,
            discontinuity: n == 2,
            key: None,
        })
    }

    fn begin() -> Entry {
        Entry::Begin {
            schema: SCHEMA,
            recording: "r".into(),
            stream: "s".into(),
            container: "ts".into(),
            started_at_ms: 1,
            not_before_ms: None,
        }
    }

    #[test]
    fn roundtrip_and_torn_tail() {
        let mut d = Vec::new();
        for e in [begin(), seg(0), seg(1), seg(2)] {
            d.extend(encode(&e));
        }
        let full = d.len();
        d.extend_from_slice(b"{\"t\":\"segm");
        let idx = parse(&d).unwrap();
        assert!(idx.truncated_tail);
        assert_eq!(idx.valid_len, full);
        assert_eq!(idx.segments.len(), 3);
        assert_eq!(idx.duration_ms(), 6000);
        let pl = idx.vod_playlist(|s| format!("{}.ts", s.sequence), None);
        assert!(pl.contains("#EXT-X-DISCONTINUITY\n#EXTINF:2.000,\n2.ts"));
        assert!(pl.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn corrupt_middle_is_error() {
        let mut d = Vec::new();
        d.extend(encode(&begin()));
        d.extend_from_slice(b"garbage\n");
        d.extend(encode(&seg(0)));
        assert_eq!(parse(&d).unwrap_err(), IndexError::Corrupt(2));
    }

    #[test]
    fn after_end_is_error() {
        let mut d = Vec::new();
        d.extend(encode(&begin()));
        d.extend(encode(&Entry::End { ended_at_ms: 5, state: "finalized".into(), not_after_ms: None }));
        d.extend(encode(&seg(0)));
        assert_eq!(parse(&d).unwrap_err(), IndexError::AfterEnd(3));
        assert_eq!(parse(b"").unwrap_err(), IndexError::MissingBegin);
    }

    #[test]
    fn keys_rewrite() {
        let mut d = Vec::new();
        let init = Entry::Init { path: "segments/s/r/init-0.mp4".into(), sha256: "11".into(), key: None };
        for e in [begin(), init, seg(0), seg(1)] {
            d.extend(encode(&e));
        }
        // 確定していなければ書き直さない
        assert_eq!(with_keys(&d, |p| p.into()).unwrap_err(), IndexError::NotFinal);
        d.extend(encode(&Entry::End { ended_at_ms: 9, state: "finalized".into(), not_after_ms: Some(8) }));
        // 古い索引（key なし）は key を出さない
        assert!(!String::from_utf8_lossy(&d).contains("\"key\""));
        let k = with_keys(&d, |p| format!("pre/{p}")).unwrap();
        let idx = parse(&k).unwrap();
        assert_eq!(idx.init_key.as_deref(), Some("pre/segments/s/r/init-0.mp4"));
        assert_eq!(idx.segments[1].key.as_deref(), Some("pre/segments/s/r/e0-1.ts"));
        assert_eq!(idx.end, Some((9, "finalized".into(), Some(8))));
        assert_eq!(idx.objects().len(), 3);
        // 2 度目も同じ
        assert_eq!(with_keys(&k, |p| format!("pre/{p}")).unwrap(), k);
    }
}
