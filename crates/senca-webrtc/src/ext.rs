//! RTP ヘッダー拡張（RFC 8285 の 1 byte・2 byte の形）の値。
//!
//! BUNDLE では同じ URI は全 m 行で同じ id を使う（RFC 8843 9.1）。[`ExtIds`] はその対応表。

use bytes::Bytes;
use senca_rtp::{HeaderExtension, RtpPacket};
use std::time::Duration;

pub use senca_sdp::negotiate::ext::{
    ABS_SEND_TIME, AUDIO_LEVEL, MID, REPAIRED_RTP_STREAM_ID, RTP_STREAM_ID, TOFFSET, TRANSPORT_CC,
};

/// playout-delay（libwebrtc の拡張）。
pub const PLAYOUT_DELAY: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";

/// mid・rid の長さの上限（RFC 8852 は 255 だが、実際は短い）。
pub const MAX_RID_LEN: usize = 64;

/// 交渉した拡張の id。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtIds {
    pub mid: Option<u8>,
    pub rid: Option<u8>,
    pub repaired_rid: Option<u8>,
    pub abs_send_time: Option<u8>,
    pub transport_cc: Option<u8>,
    pub audio_level: Option<u8>,
    pub playout_delay: Option<u8>,
}

impl ExtIds {
    /// (id, URI) の並びから作る。
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = &'a (u8, String)>) -> ExtIds {
        let mut e = ExtIds::default();
        for (id, uri) in pairs {
            let slot = match uri.as_str() {
                MID => &mut e.mid,
                RTP_STREAM_ID => &mut e.rid,
                REPAIRED_RTP_STREAM_ID => &mut e.repaired_rid,
                ABS_SEND_TIME => &mut e.abs_send_time,
                TRANSPORT_CC => &mut e.transport_cc,
                AUDIO_LEVEL => &mut e.audio_level,
                PLAYOUT_DELAY => &mut e.playout_delay,
                _ => continue,
            };
            if slot.is_none() && (1..=255).contains(id) {
                *slot = Some(*id);
            }
        }
        e
    }

    /// 1 byte の形で書けるか（id 1〜14）。
    fn one_byte_ok(&self) -> bool {
        [
            self.mid,
            self.rid,
            self.repaired_rid,
            self.abs_send_time,
            self.transport_cc,
            self.playout_delay,
        ]
        .iter()
        .flatten()
        .all(|id| (1..=14).contains(id))
    }
}

/// 受けた packet の拡張の値。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedExt {
    pub mid: Option<String>,
    pub rid: Option<String>,
    pub repaired_rid: Option<String>,
    pub transport_seq: Option<u16>,
    pub abs_send_time: Option<u32>,
    /// (voice activity, level -dBov)。
    pub audio_level: Option<(bool, u8)>,
}

fn sdes_text(b: &[u8]) -> Option<String> {
    // RFC 8852: 英数字・'-'・'_' だけ。末尾の 0 埋めを許す。
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let b = &b[..end];
    if b.is_empty()
        || b.len() > MAX_RID_LEN
        || !b
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
    {
        return None;
    }
    Some(String::from_utf8_lossy(b).into_owned())
}

/// packet の拡張を読む。壊れた拡張・知らない id は無視する。
pub fn parse(pkt: &RtpPacket, ids: &ExtIds) -> ParsedExt {
    let mut out = ParsedExt::default();
    let Some(ext) = &pkt.header.extension else {
        return out;
    };
    let Ok(elems) = ext.elements() else {
        return out;
    };
    for e in elems {
        let id = Some(e.id);
        let d = &e.data[..];
        if id == ids.mid {
            out.mid = sdes_text(d);
        } else if id == ids.rid {
            out.rid = sdes_text(d);
        } else if id == ids.repaired_rid {
            out.repaired_rid = sdes_text(d);
        } else if id == ids.transport_cc && d.len() >= 2 {
            out.transport_seq = Some(u16::from_be_bytes([d[0], d[1]]));
        } else if id == ids.abs_send_time && d.len() >= 3 {
            out.abs_send_time = Some(u32::from_be_bytes([0, d[0], d[1], d[2]]));
        } else if id == ids.audio_level && !d.is_empty() {
            out.audio_level = Some((d[0] & 0x80 != 0, d[0] & 0x7f));
        }
    }
    out
}

/// abs-send-time（6.18 の固定小数点の秒、24 bit）。
pub fn abs_send_time(since_start: Duration) -> [u8; 3] {
    let v = ((since_start.as_nanos() << 18) / 1_000_000_000) as u64 & 0xFF_FFFF;
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// playout-delay の値（最小・最大、10 ms 単位で 12 bit ずつ）。
pub fn playout_delay(min: Duration, max: Duration) -> [u8; 3] {
    let a = (min.as_millis() / 10).min(4095) as u32;
    let b = (max.as_millis() / 10).min(4095) as u32;
    let v = (a << 12) | b;
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// 送る packet に付ける値。
#[derive(Debug, Clone, Default)]
pub struct OutExt<'a> {
    pub mid: Option<&'a str>,
    pub rid: Option<&'a str>,
    pub repaired_rid: Option<&'a str>,
    pub abs_send_time: Option<[u8; 3]>,
    pub transport_seq: Option<u16>,
    pub playout_delay: Option<[u8; 3]>,
}

/// 拡張を書き込む（既にある拡張は置き換える）。書けなければ拡張なしのまま。
pub fn apply(pkt: &mut RtpPacket, ids: &ExtIds, v: &OutExt<'_>) {
    let mut elems: Vec<(u8, Vec<u8>)> = Vec::with_capacity(4);
    if let (Some(id), Some(m)) = (ids.mid, v.mid) {
        elems.push((id, m.as_bytes().to_vec()));
    }
    if let (Some(id), Some(r)) = (ids.rid, v.rid) {
        elems.push((id, r.as_bytes().to_vec()));
    }
    if let (Some(id), Some(r)) = (ids.repaired_rid, v.repaired_rid) {
        elems.push((id, r.as_bytes().to_vec()));
    }
    if let (Some(id), Some(t)) = (ids.abs_send_time, v.abs_send_time) {
        elems.push((id, t.to_vec()));
    }
    if let (Some(id), Some(s)) = (ids.transport_cc, v.transport_seq) {
        elems.push((id, s.to_be_bytes().to_vec()));
    }
    if let (Some(id), Some(p)) = (ids.playout_delay, v.playout_delay) {
        elems.push((id, p.to_vec()));
    }
    if elems.is_empty() {
        pkt.header.extension = None;
        return;
    }
    if !ids.one_byte_ok() || elems.iter().any(|(_, d)| d.is_empty() || d.len() > 16) {
        // 2 byte の形（RFC 8285 4.3）
        let mut data = Vec::with_capacity(32);
        for (id, d) in &elems {
            if d.len() > 255 {
                continue;
            }
            data.push(*id);
            data.push(d.len() as u8);
            data.extend_from_slice(d);
        }
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }
        pkt.header.extension = Some(HeaderExtension {
            profile: senca_rtp::packet::TWO_BYTE_PROFILE,
            data: Bytes::from(data),
        });
        return;
    }
    let refs: Vec<(u8, &[u8])> = elems.iter().map(|(i, d)| (*i, &d[..])).collect();
    pkt.header.extension = HeaderExtension::build(&refs).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> ExtIds {
        ExtIds::from_pairs(&[
            (4, MID.to_string()),
            (10, RTP_STREAM_ID.to_string()),
            (11, REPAIRED_RTP_STREAM_ID.to_string()),
            (2, ABS_SEND_TIME.to_string()),
            (3, TRANSPORT_CC.to_string()),
            (1, AUDIO_LEVEL.to_string()),
        ])
    }

    #[test]
    fn roundtrip_one_byte() {
        let ids = ids();
        let mut p = RtpPacket::new(96, 1, 2, 3, false, Bytes::from_static(b"x"));
        apply(
            &mut p,
            &ids,
            &OutExt {
                mid: Some("0"),
                abs_send_time: Some(abs_send_time(Duration::from_millis(1500))),
                transport_seq: Some(0xBEEF),
                ..Default::default()
            },
        );
        let p = RtpPacket::parse(p.to_bytes().unwrap()).unwrap();
        let e = parse(&p, &ids);
        assert_eq!(e.mid.as_deref(), Some("0"));
        assert_eq!(e.transport_seq, Some(0xBEEF));
        assert_eq!(e.abs_send_time, Some(((3u32 << 18) / 2) & 0xFF_FFFF));
    }

    #[test]
    fn two_byte_when_needed() {
        let ids = ExtIds::from_pairs(&[(20, MID.to_string()), (3, TRANSPORT_CC.to_string())]);
        let mut p = RtpPacket::new(96, 1, 2, 3, false, Bytes::new());
        apply(
            &mut p,
            &ids,
            &OutExt {
                mid: Some("video"),
                transport_seq: Some(7),
                ..Default::default()
            },
        );
        assert!(p.header.extension.as_ref().unwrap().is_two_byte());
        let p = RtpPacket::parse(p.to_bytes().unwrap()).unwrap();
        let e = parse(&p, &ids);
        assert_eq!(e.mid.as_deref(), Some("video"));
        assert_eq!(e.transport_seq, Some(7));
    }

    #[test]
    fn rid_validation_and_values() {
        assert_eq!(sdes_text(b"h\0\0"), Some("h".into()));
        assert_eq!(sdes_text(b""), None);
        assert_eq!(sdes_text(b"a b"), None);
        assert_eq!(
            playout_delay(Duration::ZERO, Duration::from_millis(100)),
            [0, 0, 10]
        );
        let ids = ids();
        let ext = HeaderExtension::build(&[(10, b"q"), (1, &[0x85]), (11, b"f")]).unwrap();
        let mut p = RtpPacket::new(96, 1, 2, 3, false, Bytes::new());
        p.header.extension = Some(ext);
        let e = parse(&p, &ids);
        assert_eq!(e.rid.as_deref(), Some("q"));
        assert_eq!(e.repaired_rid.as_deref(), Some("f"));
        assert_eq!(e.audio_level, Some((true, 5)));
    }
}
