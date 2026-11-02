//! transport-wide congestion control の feedback（draft-holmer-rmcat-transport-wide-cc-extensions-01 3.1）。
//!
//! FCI: base sequence number(16)・packet status count(16)・reference time(24, 64 ms 単位の符号付き)・
//! feedback packet count(8)・packet chunk の列・recv delta の列。

use crate::error::{Error, Result};

/// recv delta の 1 単位（250 µs）。
pub const TWCC_DELTA_US: i64 = 250;
/// reference time の 1 単位（64 ms）。
const REF_TIME_US: i64 = 64_000;

/// パケット 1 つの状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwccPacketStatus {
    NotReceived,
    /// 受信した。値は直前の受信時刻（最初は reference time）からの差（250 µs 単位）。
    Received(i32),
}

/// transport-cc feedback。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportCc {
    pub sender_ssrc: u32,
    pub media_ssrc: u32,
    pub base_sequence: u16,
    /// 24 ビットの符号付き（64 ms 単位）。
    pub reference_time: i32,
    pub feedback_count: u8,
    /// base_sequence から連続する各パケットの状態。
    pub packets: Vec<TwccPacketStatus>,
}

fn symbol(s: &TwccPacketStatus) -> u8 {
    match s {
        TwccPacketStatus::NotReceived => 0,
        TwccPacketStatus::Received(d) if (0..=255).contains(d) => 1,
        TwccPacketStatus::Received(_) => 2,
    }
}

impl TransportCc {
    /// FCI を解析する。
    pub fn parse(sender_ssrc: u32, media_ssrc: u32, fci: &[u8]) -> Result<TransportCc> {
        if fci.len() < 8 {
            return Err(Error::Truncated);
        }
        let base_sequence = u16::from_be_bytes([fci[0], fci[1]]);
        let count = u16::from_be_bytes([fci[2], fci[3]]) as usize;
        let rt = u32::from_be_bytes([0, fci[4], fci[5], fci[6]]);
        let reference_time = ((rt << 8) as i32) >> 8;
        let feedback_count = fci[7];
        let mut pos = 8usize;
        let mut symbols: Vec<u8> = Vec::with_capacity(count.min(fci.len() * 14));
        while symbols.len() < count {
            let c = fci.get(pos..pos + 2).ok_or(Error::Truncated)?;
            pos += 2;
            let chunk = u16::from_be_bytes([c[0], c[1]]);
            let left = count - symbols.len();
            if chunk & 0x8000 == 0 {
                let s = ((chunk >> 13) & 3) as u8;
                if s == 3 {
                    return Err(Error::Invalid("transport-cc の予約された状態"));
                }
                let run = (chunk & 0x1fff) as usize;
                if run == 0 {
                    return Err(Error::Invalid("transport-cc の run length 0"));
                }
                symbols.extend(std::iter::repeat_n(s, run.min(left)));
            } else if chunk & 0x4000 == 0 {
                for i in 0..14.min(left) {
                    symbols.push(((chunk >> (13 - i)) & 1) as u8);
                }
            } else {
                for i in 0..7.min(left) {
                    let s = ((chunk >> (12 - 2 * i)) & 3) as u8;
                    if s == 3 {
                        return Err(Error::Invalid("transport-cc の予約された状態"));
                    }
                    symbols.push(s);
                }
            }
        }
        let mut packets = Vec::with_capacity(symbols.len());
        for s in symbols {
            packets.push(match s {
                0 => TwccPacketStatus::NotReceived,
                1 => {
                    let d = *fci.get(pos).ok_or(Error::Truncated)?;
                    pos += 1;
                    TwccPacketStatus::Received(d as i32)
                }
                _ => {
                    let d = fci.get(pos..pos + 2).ok_or(Error::Truncated)?;
                    pos += 2;
                    TwccPacketStatus::Received(i16::from_be_bytes([d[0], d[1]]) as i32)
                }
            });
        }
        // 残りは 0 の padding だけ許す（4 バイト境界まで）
        if fci.len() - pos > 3 {
            return Err(Error::Invalid("transport-cc の後ろに余分なデータ"));
        }
        Ok(TransportCc {
            sender_ssrc,
            media_ssrc,
            base_sequence,
            reference_time,
            feedback_count,
            packets,
        })
    }

    /// 受信時刻（µs、任意の基準）の列から作る。`arrivals[i]` は `base_sequence + i` の受信時刻。
    pub fn from_arrivals(
        sender_ssrc: u32,
        media_ssrc: u32,
        base_sequence: u16,
        feedback_count: u8,
        arrivals: &[Option<i64>],
    ) -> Result<TransportCc> {
        if arrivals.len() > u16::MAX as usize {
            return Err(Error::Limit("transport-cc のパケット数"));
        }
        let first = arrivals.iter().flatten().next().copied().unwrap_or(0);
        let reference_time = first.div_euclid(REF_TIME_US);
        if !(-(1 << 23)..(1 << 23)).contains(&reference_time) {
            return Err(Error::Invalid("reference time が 24 ビットに収まらない"));
        }
        let mut last = reference_time * REF_TIME_US;
        let mut packets = Vec::with_capacity(arrivals.len());
        for a in arrivals {
            packets.push(match a {
                None => TwccPacketStatus::NotReceived,
                Some(t) => {
                    let d = (t - last).div_euclid(TWCC_DELTA_US);
                    let d = i16::try_from(d)
                        .map_err(|_| Error::Invalid("recv delta が 16 ビットに収まらない"))?;
                    last += d as i64 * TWCC_DELTA_US;
                    TwccPacketStatus::Received(d as i32)
                }
            });
        }
        Ok(TransportCc {
            sender_ssrc,
            media_ssrc,
            base_sequence,
            reference_time: reference_time as i32,
            feedback_count,
            packets,
        })
    }

    /// 各パケットの（番号, 受信時刻 µs）。時刻は reference time を基準にした絶対値。
    pub fn arrivals(&self) -> Vec<(u16, Option<i64>)> {
        let mut t = self.reference_time as i64 * REF_TIME_US;
        self.packets
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let seq = self.base_sequence.wrapping_add(i as u16);
                match p {
                    TwccPacketStatus::NotReceived => (seq, None),
                    TwccPacketStatus::Received(d) => {
                        t += *d as i64 * TWCC_DELTA_US;
                        (seq, Some(t))
                    }
                }
            })
            .collect()
    }

    /// FCI を書く（4 バイト境界まで 0 で埋める）。
    pub fn write_fci(&self, out: &mut Vec<u8>) -> Result<()> {
        let count = u16::try_from(self.packets.len())
            .map_err(|_| Error::Limit("transport-cc のパケット数"))?;
        if !(-(1 << 23)..(1 << 23)).contains(&self.reference_time) {
            return Err(Error::Invalid("reference time"));
        }
        let start = out.len();
        out.extend_from_slice(&self.base_sequence.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&(self.reference_time as u32).to_be_bytes()[1..]);
        out.push(self.feedback_count);
        let syms: Vec<u8> = self.packets.iter().map(symbol).collect();
        let mut i = 0usize;
        while i < syms.len() {
            let s = syms[i];
            let run = syms[i..]
                .iter()
                .take(0x1fff)
                .take_while(|&&x| x == s)
                .count();
            if run >= 7 || i + run == syms.len() {
                out.extend_from_slice(&(((s as u16) << 13) | run as u16).to_be_bytes());
                i += run;
                continue;
            }
            let window14 = &syms[i..(i + 14).min(syms.len())];
            if window14.iter().all(|&x| x <= 1) && window14.len() == 14 {
                let mut c = 0x8000u16;
                for (k, &x) in window14.iter().enumerate() {
                    c |= (x as u16) << (13 - k);
                }
                out.extend_from_slice(&c.to_be_bytes());
                i += 14;
            } else {
                let window7 = &syms[i..(i + 7).min(syms.len())];
                let mut c = 0xC000u16;
                for (k, &x) in window7.iter().enumerate() {
                    c |= (x as u16) << (12 - 2 * k);
                }
                out.extend_from_slice(&c.to_be_bytes());
                i += window7.len();
            }
        }
        for p in &self.packets {
            if let TwccPacketStatus::Received(d) = p {
                if (0..=255).contains(d) {
                    out.push(*d as u8);
                } else {
                    let v = i16::try_from(*d).map_err(|_| Error::Invalid("recv delta"))?;
                    out.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        while !(out.len() - start).is_multiple_of(4) {
            out.push(0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtcp::{RtcpPacket, build_compound, parse_reduced};
    use bytes::Bytes;

    fn rt(t: TransportCc) {
        let b = build_compound(&[RtcpPacket::TransportCc(t.clone())]).unwrap();
        assert_eq!(parse_reduced(&b).unwrap(), vec![RtcpPacket::TransportCc(t)]);
    }

    #[test]
    fn draft_example_run_length_and_vectors() {
        // draft 3.1.3 の例: run length chunk 「状態 1 が 221 個」→ 0x20DD
        let mut fci = vec![0, 0, 0, 221, 0, 0, 0, 0, 0x20, 0xDD];
        fci.extend(std::iter::repeat_n(1u8, 221));
        let t = TransportCc::parse(1, 2, &fci).unwrap();
        assert_eq!(t.packets.len(), 221);
        assert!(
            t.packets
                .iter()
                .all(|p| *p == TwccPacketStatus::Received(1))
        );
        // 3.1.4 の例: 1 ビットの status vector 0xA640 (1|0|10011001000000)
        let fci = [0, 10, 0, 14, 0, 0, 1, 5, 0xA6, 0x40, 1, 2, 3, 4, 0, 0];
        let t = TransportCc::parse(1, 2, &fci).unwrap();
        let rx: Vec<bool> = t
            .packets
            .iter()
            .map(|p| *p != TwccPacketStatus::NotReceived)
            .collect();
        assert_eq!(
            rx,
            vec![
                true, false, false, true, true, false, false, true, false, false, false, false,
                false, false
            ]
        );
        assert_eq!(t.reference_time, 1);
        assert_eq!(t.feedback_count, 5);
    }

    #[test]
    fn roundtrip_mixed() {
        let arrivals: Vec<Option<i64>> = (0..100)
            .map(|i| match i % 9 {
                0 => None,
                3 => Some(1_000_000 + i * 20_000 + 70_000), // 大きい差
                _ => Some(1_000_000 + i * 20_000),
            })
            .collect();
        let t = TransportCc::from_arrivals(5, 6, 65530, 9, &arrivals).unwrap();
        rt(t.clone());
        // 復元した時刻は 250 µs の精度で一致（負の差も扱う）
        for ((_, got), want) in t.arrivals().iter().zip(&arrivals) {
            match (got, want) {
                (None, None) => {}
                (Some(g), Some(w)) => assert!((g - w).abs() < 250, "{g} {w}"),
                _ => panic!(),
            }
        }
        assert_eq!(t.arrivals()[10].0, 65530u16.wrapping_add(10));
        // 全部届かない・全部同じ
        rt(TransportCc::from_arrivals(1, 1, 0, 0, &[None; 20]).unwrap());
        rt(TransportCc::from_arrivals(1, 1, 0, 0, &[Some(5000); 3]).unwrap());
        rt(TransportCc::from_arrivals(1, 1, 0, 0, &[]).unwrap());
    }

    #[test]
    fn rejects_truncated() {
        let t = TransportCc::from_arrivals(1, 1, 0, 0, &[Some(0), Some(100_000)]).unwrap();
        let b = build_compound(&[RtcpPacket::TransportCc(t)]).unwrap();
        let mut v = b.to_vec();
        v.truncate(v.len() - 4);
        v[3] -= 1;
        assert!(parse_reduced(&Bytes::from(v)).is_err());
    }
}
