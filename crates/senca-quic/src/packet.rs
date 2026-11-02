//! パケットのヘッダー（RFC 9000 17 章）と header protection の解除（RFC 9001 5.4）。

use crate::VERSION_1;
use crate::cid::{ConnectionId, MAX_CID_LEN};
use crate::coding::Reader;
use crate::crypto::{HeaderKey, SAMPLE_LEN, TAG_LEN};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongType {
    Initial,
    ZeroRtt,
    Handshake,
    Retry,
}

impl LongType {
    pub(crate) fn bits(self) -> u8 {
        match self {
            Self::Initial => 0,
            Self::ZeroRtt => 1,
            Self::Handshake => 2,
            Self::Retry => 3,
        }
    }
}

/// 保護を外す前に読めるヘッダー。
#[derive(Debug, Clone)]
pub(crate) enum Header<'a> {
    /// v1 の long header（Initial・0-RTT・Handshake）。
    Long {
        ty: LongType,
        dcid: ConnectionId,
        scid: ConnectionId,
        token: &'a [u8],
    },
    /// v1 の Retry。`body` は tag の前まで（integrity tag の計算に使う）。
    Retry {
        dcid: ConnectionId,
        scid: ConnectionId,
        token: &'a [u8],
    },
    Short {
        dcid: ConnectionId,
    },
    VersionNegotiation {
        dcid: &'a [u8],
        scid: &'a [u8],
        versions: &'a [u8],
    },
    /// 未知の版の long header（Version Negotiation を返す材料）。
    Unsupported {
        dcid: &'a [u8],
        scid: &'a [u8],
    },
}

/// datagram の中の一つのパケット。
#[derive(Debug, Clone)]
pub(crate) struct Partial<'a> {
    pub header: Header<'a>,
    /// packet number の位置（パケット先頭から）。
    pub pn_offset: usize,
    /// このパケットの全長（datagram の中での長さ、coalesce の区切り）。
    pub len: usize,
}

impl Partial<'_> {
    pub(crate) fn dcid_bytes(&self) -> &[u8] {
        match &self.header {
            Header::Long { dcid, .. } | Header::Retry { dcid, .. } | Header::Short { dcid } => dcid.as_slice(),
            Header::VersionNegotiation { dcid, .. } | Header::Unsupported { dcid, .. } => dcid,
        }
    }
}

/// 読めないパケット（黙って捨てる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Malformed;

/// datagram の先頭のパケットのヘッダーを読む。`local_cid_len` は short header の DCID 長。
pub(crate) fn parse(buf: &[u8], local_cid_len: usize) -> Result<Partial<'_>, Malformed> {
    let mut r = Reader::new(buf);
    let first = r.u8().map_err(|_| Malformed)?;
    if first & 0x80 == 0 {
        if first & 0x40 == 0 {
            return Err(Malformed);
        }
        let dcid = ConnectionId::new(r.bytes(local_cid_len).map_err(|_| Malformed)?).ok_or(Malformed)?;
        return Ok(Partial {
            header: Header::Short { dcid },
            pn_offset: 1 + local_cid_len,
            len: buf.len(),
        });
    }
    let version = r.u32().map_err(|_| Malformed)?;
    let dl = r.u8().map_err(|_| Malformed)? as usize;
    let dcid = r.bytes(dl).map_err(|_| Malformed)?;
    let sl = r.u8().map_err(|_| Malformed)? as usize;
    let scid = r.bytes(sl).map_err(|_| Malformed)?;
    if version == 0 {
        let versions = r.rest();
        if versions.is_empty() || !versions.len().is_multiple_of(4) {
            return Err(Malformed);
        }
        return Ok(Partial {
            header: Header::VersionNegotiation { dcid, scid, versions },
            pn_offset: 0,
            len: buf.len(),
        });
    }
    if version != VERSION_1 {
        return Ok(Partial {
            header: Header::Unsupported { dcid, scid },
            pn_offset: 0,
            len: buf.len(),
        });
    }
    if dl > MAX_CID_LEN || sl > MAX_CID_LEN || first & 0x40 == 0 {
        return Err(Malformed);
    }
    let dcid = ConnectionId::new(dcid).ok_or(Malformed)?;
    let scid = ConnectionId::new(scid).ok_or(Malformed)?;
    let ty = match (first >> 4) & 0x3 {
        0 => LongType::Initial,
        1 => LongType::ZeroRtt,
        2 => LongType::Handshake,
        _ => LongType::Retry,
    };
    if ty == LongType::Retry {
        let rest = r.rest();
        if rest.len() < TAG_LEN {
            return Err(Malformed);
        }
        let token = &rest[..rest.len() - TAG_LEN];
        return Ok(Partial {
            header: Header::Retry { dcid, scid, token },
            pn_offset: 0,
            len: buf.len(),
        });
    }
    let token = if ty == LongType::Initial {
        r.varint_bytes().map_err(|_| Malformed)?
    } else {
        &[]
    };
    let length = r.varint().map_err(|_| Malformed)?;
    let pn_offset = r.pos();
    let len = usize::try_from(length)
        .ok()
        .and_then(|l| pn_offset.checked_add(l))
        .filter(|&l| l <= buf.len())
        .ok_or(Malformed)?;
    Ok(Partial {
        header: Header::Long { ty, dcid, scid, token },
        pn_offset,
        len,
    })
}

/// header protection を外す。`pkt` はこのパケットだけのスライス。
/// 戻り値は `(先頭バイト（保護解除後）, packet number 長, 切り詰めた packet number)`。
pub(crate) fn unprotect(pkt: &mut [u8], pn_offset: usize, hp: &HeaderKey) -> Result<(u8, usize, u64), Malformed> {
    let sample_at = pn_offset + 4;
    if pkt.len() < sample_at + SAMPLE_LEN {
        return Err(Malformed);
    }
    let mut sample = [0u8; SAMPLE_LEN];
    sample.copy_from_slice(&pkt[sample_at..sample_at + SAMPLE_LEN]);
    let mask = hp.mask(&sample);
    let long = pkt[0] & 0x80 != 0;
    pkt[0] ^= mask[0] & if long { 0x0f } else { 0x1f };
    let first = pkt[0];
    let pn_len = usize::from(first & 0x3) + 1;
    let mut pn = 0u64;
    for i in 0..pn_len {
        pkt[pn_offset + i] ^= mask[1 + i];
        pn = (pn << 8) | u64::from(pkt[pn_offset + i]);
    }
    Ok((first, pn_len, pn))
}

/// header protection をかける（送信）。`pkt` は暗号化済みのパケット全体。
pub(crate) fn protect(pkt: &mut [u8], pn_offset: usize, pn_len: usize, hp: &HeaderKey) {
    let sample_at = pn_offset + 4;
    let mut sample = [0u8; SAMPLE_LEN];
    sample.copy_from_slice(&pkt[sample_at..sample_at + SAMPLE_LEN]);
    let mask = hp.mask(&sample);
    let long = pkt[0] & 0x80 != 0;
    pkt[0] ^= mask[0] & if long { 0x0f } else { 0x1f };
    for i in 0..pn_len {
        pkt[pn_offset + i] ^= mask[1 + i];
    }
}

/// 切り詰めた packet number を復元する（RFC 9000 A.3）。
pub(crate) fn decode_pn(largest: Option<u64>, truncated: u64, pn_len: usize) -> u64 {
    let expected = largest.map_or(0, |l| l + 1);
    let bits = (pn_len * 8) as u32;
    let win = 1u64 << bits;
    let hwin = win / 2;
    let mask = win - 1;
    let candidate = (expected & !mask) | truncated;
    if candidate + hwin <= expected && candidate < (1u64 << 62) - win {
        candidate + win
    } else if candidate > expected + hwin && candidate >= win {
        candidate - win
    } else {
        candidate
    }
}

/// 送る packet number の長さ（RFC 9000 17.1、A.2）。
pub(crate) fn pn_len(pn: u64, largest_acked: Option<u64>) -> usize {
    let unacked = match largest_acked {
        Some(l) => pn.saturating_sub(l),
        None => pn + 1,
    };
    let range = unacked.saturating_mul(2);
    if range < 1 << 8 {
        1
    } else if range < 1 << 16 {
        2
    } else if range < 1 << 24 {
        3
    } else {
        4
    }
}

/// Version Negotiation パケット（RFC 9000 17.2.1）。
pub(crate) fn version_negotiation(dcid: &[u8], scid: &[u8], random: u8) -> Vec<u8> {
    let mut v = vec![0x80 | (random & 0x7f)];
    v.extend_from_slice(&0u32.to_be_bytes());
    v.push(dcid.len() as u8);
    v.extend_from_slice(dcid);
    v.push(scid.len() as u8);
    v.extend_from_slice(scid);
    v.extend_from_slice(&VERSION_1.to_be_bytes());
    // grease（RFC 9000 15: 0x?a?a?a?a）
    v.extend_from_slice(&[0x1a | (random & 0xf0), 0x2a, 0x3a, 0x4a]);
    v
}

/// Retry パケット（RFC 9000 17.2.5）。
pub(crate) fn retry(dcid: &ConnectionId, scid: &ConnectionId, odcid: &ConnectionId, token: &[u8], random: u8) -> Vec<u8> {
    let mut v = vec![0xf0 | (random & 0x0f)];
    v.extend_from_slice(&VERSION_1.to_be_bytes());
    v.push(dcid.len() as u8);
    v.extend_from_slice(dcid.as_slice());
    v.push(scid.len() as u8);
    v.extend_from_slice(scid.as_slice());
    v.extend_from_slice(token);
    let tag = crate::crypto::retry_tag(odcid, &v);
    v.extend_from_slice(&tag);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pn_decode_rfc_example() {
        // RFC 9000 A.3: largest 0xa82f30ea, truncated 0x9b32 (16 bit) -> 0xa82f9b32
        assert_eq!(decode_pn(Some(0xa82f_30ea), 0x9b32, 2), 0xa82f_9b32);
        assert_eq!(decode_pn(None, 0, 1), 0);
        assert_eq!(decode_pn(Some(255), 0, 1), 256);
        assert_eq!(pn_len(0xac5c02, Some(0xabe8b3)), 2);
        assert_eq!(pn_len(0xace8fe, Some(0xabe8b3)), 3);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse(&[], 8).is_err());
        assert!(parse(&[0x00, 1, 2], 8).is_err());
        assert!(parse(&[0xc0, 0, 0, 0, 1, 21], 8).is_err());
        // Length がはみ出す
        let mut p = vec![0xc0, 0, 0, 0, 1, 0, 0, 0, 0x44, 0x00];
        p.extend_from_slice(&[0; 10]);
        assert!(parse(&p, 8).is_err());
    }
}
