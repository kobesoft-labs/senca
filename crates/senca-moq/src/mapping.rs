//! メディアの対応づけ（SENCA の MoQ メディアプロファイル v1、実験扱い、`DRAFT.md`）。
//!
//! moq-dev の hang（draft-lcurley-moq-hang、2026-11 時点の main）の catalog と CMAF の容器に合わせる。
//!
//! - broadcast の namespace の下に `catalog.json` track を置く。一回の更新が一つの group で、
//!   group の中の object は一つ（UTF-8 の JSON）。
//! - catalog の `video`・`audio` の `renditions` の各項目が一つの track（名前は項目の ID）。
//!   `container` は `{"kind": "cmaf", "init": base64(ftyp+moov)}`。
//! - media track の object 一つが CMAF fragment 一つ（`moof`+`mdat`）。group は keyframe の
//!   fragment から始まり（GOP ごと）、subgroup は 0、Object ID は group の中で 0 から。
//!
//! transport の相互接続はこの profile と独立で、relay は payload を読まない（プレーヤーの互換は
//! この profile の範囲でだけ確かめる、仕様 6.5）。

use base64::Engine;
use bytes::Bytes;
use serde_json::{Map, Value, json};

use crate::session::{Object, ObjectStatus};

/// catalog の track 名。
pub const CATALOG_TRACK: &str = "catalog.json";

/// rendition の種類。
#[derive(Clone, Debug, PartialEq)]
pub enum MediaKind {
    Video {
        codec: String,
        width: u32,
        height: u32,
        framerate: Option<f64>,
    },
    Audio {
        codec: String,
        sample_rate: u32,
        channels: u32,
    },
}

/// 一つの rendition（track）。
#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    pub track: String,
    pub kind: MediaKind,
    /// 初期化セグメント（ftyp+moov）。
    pub init: Bytes,
    pub bitrate: Option<u64>,
    /// デコーダーの初期化の情報（hang の `description`、16 進）。H.264 の avcC・H.265 の hvcC・
    /// AAC の AudioSpecificConfig など。無いと受け手は帯域内（Annex B）の形とみなすことがある。
    pub description: Option<Bytes>,
}

/// catalog の JSON を作る。
pub fn catalog_json(renditions: &[Rendition]) -> String {
    let mut video = Map::new();
    let mut audio = Map::new();
    for r in renditions {
        let init = base64::engine::general_purpose::STANDARD.encode(&r.init);
        let container = json!({ "kind": "cmaf", "init": init });
        let description: Option<String> = r.description.as_ref().filter(|d| !d.is_empty()).map(|d| d.iter().map(|b| format!("{b:02x}")).collect());
        match &r.kind {
            MediaKind::Video {
                codec,
                width,
                height,
                framerate,
            } => {
                let mut v = json!({
                    "codec": codec,
                    "codedWidth": width,
                    "codedHeight": height,
                    "container": container,
                });
                if let Some(f) = framerate {
                    v["framerate"] = json!(f);
                }
                if let Some(d) = &description {
                    v["description"] = json!(d);
                }
                if let Some(b) = r.bitrate {
                    v["bitrate"] = json!(b);
                }
                video.insert(r.track.clone(), v);
            }
            MediaKind::Audio {
                codec,
                sample_rate,
                channels,
            } => {
                let mut v = json!({
                    "codec": codec,
                    "sampleRate": sample_rate,
                    "numberOfChannels": channels,
                    "container": container,
                });
                if let Some(d) = &description {
                    v["description"] = json!(d);
                }
                if let Some(b) = r.bitrate {
                    v["bitrate"] = json!(b);
                }
                audio.insert(r.track.clone(), v);
            }
        }
    }
    let mut root = Map::new();
    if !video.is_empty() {
        root.insert("video".into(), json!({ "renditions": video }));
    }
    if !audio.is_empty() {
        root.insert("audio".into(), json!({ "renditions": audio }));
    }
    Value::Object(root).to_string()
}

/// catalog から読んだ rendition（容器の種類つき）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub track: String,
    pub video: bool,
    pub codec: String,
    /// `legacy`・`cmaf`・`loc` など。
    pub container: String,
    pub init: Option<Bytes>,
}

/// catalog の JSON を読む。大きすぎる・形が違うものは誤り。
pub fn parse_catalog(json: &[u8]) -> Result<Vec<CatalogEntry>, String> {
    if json.len() > 1 << 20 {
        return Err("catalog が 1 MiB を超える".into());
    }
    let v: Value = serde_json::from_slice(json).map_err(|e| format!("catalog の JSON が不正: {e}"))?;
    if let Some(ver) = v.get("version")
        && ver.as_u64().is_none_or(|n| n > 4)
    {
        return Err("catalog の version に対応しない".into());
    }
    let mut out = Vec::new();
    for (section, video) in [("video", true), ("audio", false)] {
        let Some(r) = v.get(section).and_then(|s| s.get("renditions")).and_then(|r| r.as_object()) else { continue };
        for (id, cfg) in r {
            let container = cfg.get("container");
            let kind = container
                .and_then(|c| c.get("kind"))
                .and_then(|k| k.as_str())
                .unwrap_or("legacy")
                .to_string();
            let init = container
                .and_then(|c| c.get("init"))
                .and_then(|i| i.as_str())
                .map(|s| base64::engine::general_purpose::STANDARD.decode(s).map(Bytes::from))
                .transpose()
                .map_err(|_| "init の base64 が不正".to_string())?;
            if kind == "cmaf" && init.is_none() {
                return Err(format!("{id}: cmaf に init がない"));
            }
            out.push(CatalogEntry {
                track: cfg.get("track").and_then(|t| t.as_str()).unwrap_or(id).to_string(),
                video,
                codec: cfg.get("codec").and_then(|c| c.as_str()).unwrap_or("").to_string(),
                container: kind,
                init,
            });
        }
    }
    Ok(out)
}

/// fMP4 の box を順に読む（大きさの検査つき）。
fn boxes(b: &[u8]) -> Result<Vec<([u8; 4], usize, usize)>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < b.len() {
        if b.len() - pos < 8 {
            return Err("box が切れている".into());
        }
        let mut size = u32::from_be_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]) as usize;
        let ty = [b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]];
        let mut hdr = 8;
        if size == 1 {
            if b.len() - pos < 16 {
                return Err("box が切れている".into());
            }
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[pos + 8..pos + 16]);
            size = usize::try_from(u64::from_be_bytes(a)).map_err(|_| "box が大きすぎる")?;
            hdr = 16;
        } else if size == 0 {
            size = b.len() - pos;
        }
        if size < hdr || size > b.len() - pos {
            return Err("box の大きさが不正".into());
        }
        out.push((ty, pos, size));
        pos += size;
        if out.len() > 100_000 {
            return Err("box が多すぎる".into());
        }
    }
    Ok(out)
}

/// fMP4 の全体を、初期化セグメントと fragment（moof+mdat、keyframe か）に分ける。
pub fn split_fmp4(data: &[u8]) -> Result<(Bytes, Vec<(Bytes, bool)>), String> {
    let bs = boxes(data)?;
    let mut init = Vec::new();
    let mut frags = Vec::new();
    let mut cur: Option<(usize, bool)> = None;
    for (ty, pos, size) in bs {
        match &ty {
            b"ftyp" | b"moov" => init.extend_from_slice(&data[pos..pos + size]),
            b"styp" | b"sidx" | b"prft" | b"emsg" => {}
            b"moof" => {
                let key = moof_is_keyframe(&data[pos..pos + size]).unwrap_or(false);
                cur = Some((pos, key));
            }
            b"mdat" => {
                if let Some((start, key)) = cur.take() {
                    frags.push((Bytes::copy_from_slice(&data[start..pos + size]), key));
                }
            }
            _ => {}
        }
    }
    if init.is_empty() {
        return Err("ftyp・moov がない".into());
    }
    Ok((Bytes::from(init), frags))
}

/// moof の最初の sample が sync sample か（trun・tfhd の sample flags）。
fn moof_is_keyframe(moof: &[u8]) -> Result<bool, String> {
    let inner = &moof[8..];
    for (ty, pos, size) in boxes(inner)? {
        if &ty != b"traf" {
            continue;
        }
        let traf = &inner[pos + 8..pos + size];
        let mut default_flags = None;
        for (t2, p2, s2) in boxes(traf)? {
            let bx = &traf[p2 + 8..p2 + s2];
            if bx.len() < 4 {
                continue;
            }
            let flags = u32::from_be_bytes([0, bx[1], bx[2], bx[3]]);
            match &t2 {
                b"tfhd" => {
                    let mut o = 8; // version/flags + track_ID
                    if flags & 0x01 != 0 {
                        o += 8;
                    }
                    if flags & 0x02 != 0 {
                        o += 4;
                    }
                    if flags & 0x08 != 0 {
                        o += 4;
                    }
                    if flags & 0x10 != 0 {
                        o += 4;
                    }
                    if flags & 0x20 != 0 && bx.len() >= o + 4 {
                        default_flags = Some(u32::from_be_bytes([bx[o], bx[o + 1], bx[o + 2], bx[o + 3]]));
                    }
                }
                b"trun" => {
                    let mut o = 8; // version/flags + sample_count
                    if flags & 0x01 != 0 {
                        o += 4;
                    }
                    let sf = if flags & 0x04 != 0 && bx.len() >= o + 4 {
                        Some(u32::from_be_bytes([bx[o], bx[o + 1], bx[o + 2], bx[o + 3]]))
                    } else if flags & 0x400 != 0 {
                        // 最初の sample の flags（duration・size の後ろ）
                        let mut q = o;
                        if flags & 0x100 != 0 {
                            q += 4;
                        }
                        if flags & 0x200 != 0 {
                            q += 4;
                        }
                        (bx.len() >= q + 4).then(|| u32::from_be_bytes([bx[q], bx[q + 1], bx[q + 2], bx[q + 3]]))
                    } else {
                        default_flags
                    };
                    // sample_is_non_sync_sample（0x00010000）が立っていなければ keyframe
                    return Ok(sf.is_none_or(|f| f & 0x0001_0000 == 0));
                }
                _ => {}
            }
        }
    }
    Ok(false)
}

/// fragment を object に割り当てる（keyframe で新しい group）。
#[derive(Debug, Default, Clone)]
pub struct GroupMapper {
    group: Option<u64>,
    next_object: u64,
}

impl GroupMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// 最初の group の番号を決める（再開時に前より大きくするため）。
    pub fn with_start(group: u64) -> Self {
        Self {
            group: group.checked_sub(1),
            next_object: 0,
        }
    }

    /// fragment を一つの object にする。keyframe でない fragment が最初に来たら `None`（group は keyframe から）。
    pub fn push(&mut self, fragment: Bytes, keyframe: bool, priority: u8) -> Option<Object> {
        if keyframe {
            self.group = Some(self.group.map_or(0, |g| g + 1));
            self.next_object = 0;
        }
        let g = self.group?;
        let id = self.next_object;
        self.next_object += 1;
        Some(Object {
            group: g,
            subgroup: Some(0),
            id,
            priority,
            status: ObjectStatus::Normal,
            properties: Bytes::new(),
            payload: fragment,
        })
    }

    pub fn current_group(&self) -> Option<u64> {
        self.group
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(body);
        v
    }

    fn fragment(key: bool) -> Vec<u8> {
        // tfhd（default_sample_flags あり）と trun（first_sample_flags なし）
        let mut tfhd = vec![0, 0, 0, 0x20];
        tfhd.extend_from_slice(&1u32.to_be_bytes());
        tfhd.extend_from_slice(&(if key { 0u32 } else { 0x0001_0000 }).to_be_bytes());
        let mut trun = vec![0, 0, 0, 0];
        trun.extend_from_slice(&1u32.to_be_bytes());
        let traf = [bx(b"tfhd", &tfhd), bx(b"trun", &trun)].concat();
        let moof = bx(b"moof", &bx(b"traf", &traf));
        [moof, bx(b"mdat", b"data")].concat()
    }

    #[test]
    fn split_and_map() {
        let mut f = bx(b"ftyp", b"isom");
        f.extend(bx(b"moov", b"m"));
        f.extend(fragment(true));
        f.extend(fragment(false));
        f.extend(fragment(true));
        let (init, frags) = split_fmp4(&f).unwrap();
        assert_eq!(init.len(), 12 + 9);
        assert_eq!(frags.iter().map(|x| x.1).collect::<Vec<_>>(), vec![true, false, true]);
        let mut m = GroupMapper::new();
        let objs: Vec<_> = frags.into_iter().filter_map(|(b, k)| m.push(b, k, 128)).collect();
        assert_eq!(objs.iter().map(|o| (o.group, o.id)).collect::<Vec<_>>(), vec![(0, 0), (0, 1), (1, 0)]);
        assert!(split_fmp4(&[0, 0, 0, 100, b'f', b't', b'y', b'p']).is_err());
    }

    #[test]
    fn catalog_roundtrip() {
        let r = vec![
            Rendition {
                track: "video".into(),
                kind: MediaKind::Video {
                    codec: "avc1.64001f".into(),
                    width: 1280,
                    height: 720,
                    framerate: Some(30.0),
                },
                init: Bytes::from_static(b"init-v"),
                bitrate: None,
                description: Some(Bytes::from_static(&[1, 0x64, 0, 0x1f])),
            },
            Rendition {
                track: "audio".into(),
                kind: MediaKind::Audio {
                    codec: "opus".into(),
                    sample_rate: 48000,
                    channels: 2,
                },
                init: Bytes::from_static(b"init-a"),
                bitrate: Some(64000),
                description: None,
            },
        ];
        let j = catalog_json(&r);
        assert!(j.contains("\"description\":\"0164001f\""));
        let e = parse_catalog(j.as_bytes()).unwrap();
        assert_eq!(e.len(), 2);
        assert!(e.iter().all(|x| x.container == "cmaf"));
        assert_eq!(e.iter().find(|x| x.video).unwrap().init.as_deref(), Some(&b"init-v"[..]));
        assert!(parse_catalog(b"{\"version\": 99}").is_err());
        assert!(parse_catalog(b"not json").is_err());
    }
}
