//! RTCP（RFC 3550 6）と feedback（RFC 4585・RFC 5104・REMB・transport-cc）。
//!
//! - [`parse_compound`]: 複合パケット（先頭は SR か RR、padding は最後のパケットだけ）
//! - [`parse_reduced`]: reduced-size RTCP（RFC 5506。先頭の制約なし）
//! - [`build_compound`]: 複数のパケットを 1 つの複合パケットにする
//!
//! 未知の packet type・FMT は [`RtcpPacket::Unknown`] として中身を残す（捨てるかは呼び出し側）。

mod twcc;

pub use twcc::{TWCC_DELTA_US, TransportCc, TwccPacketStatus};

use crate::error::{Error, Result};
use bytes::Bytes;

pub const PT_SR: u8 = 200;
pub const PT_RR: u8 = 201;
pub const PT_SDES: u8 = 202;
pub const PT_BYE: u8 = 203;
pub const PT_APP: u8 = 204;
/// RTPFB（RFC 4585 6.2）。
pub const PT_RTPFB: u8 = 205;
/// PSFB（RFC 4585 6.3）。
pub const PT_PSFB: u8 = 206;

/// 複合パケットに入れられるパケット数の上限。
pub const MAX_PACKETS: usize = 64;
/// 報告ブロック・SDES チャンク・BYE の SSRC の上限（5 ビット）。
pub const MAX_COUNT: usize = 31;

/// 受信報告ブロック（RFC 3550 6.4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReportBlock {
    pub ssrc: u32,
    pub fraction_lost: u8,
    /// 24 ビットの符号付き。
    pub cumulative_lost: i32,
    pub highest_seq: u32,
    pub jitter: u32,
    pub lsr: u32,
    pub dlsr: u32,
}

impl ReportBlock {
    fn parse(d: &[u8]) -> ReportBlock {
        let lost24 = u32::from_be_bytes([0, d[5], d[6], d[7]]);
        let cumulative_lost = ((lost24 << 8) as i32) >> 8;
        ReportBlock {
            ssrc: be32(&d[0..]),
            fraction_lost: d[4],
            cumulative_lost,
            highest_seq: be32(&d[8..]),
            jitter: be32(&d[12..]),
            lsr: be32(&d[16..]),
            dlsr: be32(&d[20..]),
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.push(self.fraction_lost);
        let l = self.cumulative_lost.clamp(-0x80_0000, 0x7F_FFFF) as u32;
        out.extend_from_slice(&l.to_be_bytes()[1..]);
        out.extend_from_slice(&self.highest_seq.to_be_bytes());
        out.extend_from_slice(&self.jitter.to_be_bytes());
        out.extend_from_slice(&self.lsr.to_be_bytes());
        out.extend_from_slice(&self.dlsr.to_be_bytes());
    }
}

/// 送信者報告（SR）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SenderReport {
    pub ssrc: u32,
    /// 64 ビットの NTP 時刻（上位 32 ビットが秒）。
    pub ntp: u64,
    pub rtp_timestamp: u32,
    pub packet_count: u32,
    pub octet_count: u32,
    pub reports: Vec<ReportBlock>,
    /// profile 固有の拡張（そのまま残す）。
    pub extension: Bytes,
}

/// 受信者報告（RR）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceiverReport {
    pub ssrc: u32,
    pub reports: Vec<ReportBlock>,
    pub extension: Bytes,
}

/// SDES の項目の種類。
pub mod sdes_type {
    pub const CNAME: u8 = 1;
    pub const NAME: u8 = 2;
    pub const EMAIL: u8 = 3;
    pub const PHONE: u8 = 4;
    pub const LOC: u8 = 5;
    pub const TOOL: u8 = 6;
    pub const NOTE: u8 = 7;
    pub const PRIV: u8 = 8;
}

/// SDES のチャンク（SSRC と項目の列）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SdesChunk {
    pub ssrc: u32,
    pub items: Vec<(u8, Bytes)>,
}

impl SdesChunk {
    /// CNAME 1 つのチャンク。
    pub fn cname(ssrc: u32, cname: &str) -> SdesChunk {
        SdesChunk {
            ssrc,
            items: vec![(sdes_type::CNAME, Bytes::copy_from_slice(cname.as_bytes()))],
        }
    }

    /// CNAME の値。
    pub fn get_cname(&self) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|(t, _)| *t == sdes_type::CNAME)
            .map(|(_, v)| &v[..])
    }
}

/// SDES。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sdes {
    pub chunks: Vec<SdesChunk>,
}

/// BYE。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bye {
    pub ssrcs: Vec<u32>,
    pub reason: Option<Bytes>,
}

/// APP。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct App {
    pub subtype: u8,
    pub ssrc: u32,
    pub name: [u8; 4],
    /// 長さは 4 の倍数。
    pub data: Bytes,
}

/// Generic NACK（RFC 4585 6.2.1）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Nack {
    pub sender_ssrc: u32,
    pub media_ssrc: u32,
    /// (PID, BLP) の組。
    pub pairs: Vec<(u16, u16)>,
}

impl Nack {
    /// 失われた番号の列から作る（並びは問わない。近いものを BLP にまとめる）。
    pub fn from_sequences(sender_ssrc: u32, media_ssrc: u32, seqs: &[u16]) -> Nack {
        let mut sorted: Vec<u16> = seqs.to_vec();
        // 折り返しをまたぐ並びも近い順になるよう、最初の値からの距離で並べる
        if let Some(&first) = seqs.first() {
            sorted.sort_by_key(|s| s.wrapping_sub(first).wrapping_add(0x8000));
        }
        sorted.dedup();
        let mut pairs: Vec<(u16, u16)> = Vec::new();
        for s in sorted {
            if let Some((pid, blp)) = pairs.last_mut() {
                let d = s.wrapping_sub(*pid);
                if (1..=16).contains(&d) {
                    *blp |= 1 << (d - 1);
                    continue;
                }
            }
            pairs.push((s, 0));
        }
        Nack {
            sender_ssrc,
            media_ssrc,
            pairs,
        }
    }

    /// 失われた番号を並べる。
    pub fn sequences(&self) -> Vec<u16> {
        let mut out = Vec::new();
        for &(pid, blp) in &self.pairs {
            out.push(pid);
            for i in 0..16 {
                if blp & (1 << i) != 0 {
                    out.push(pid.wrapping_add(i + 1));
                }
            }
        }
        out
    }
}

/// Picture Loss Indication（RFC 4585 6.3.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pli {
    pub sender_ssrc: u32,
    pub media_ssrc: u32,
}

/// Full Intra Request（RFC 5104 4.3.1）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fir {
    pub sender_ssrc: u32,
    /// (SSRC, command sequence number)。
    pub entries: Vec<(u32, u8)>,
}

/// Receiver Estimated Max Bitrate（draft-alvestrand-rmcat-remb）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Remb {
    pub sender_ssrc: u32,
    /// bit/s。
    pub bitrate: u64,
    pub ssrcs: Vec<u32>,
}

/// 解釈しなかったパケット。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPacket {
    pub packet_type: u8,
    /// 先頭バイトの下位 5 ビット（RC・SC・FMT など）。
    pub count: u8,
    /// 共通ヘッダーの後ろ（padding を除く）。
    pub payload: Bytes,
}

/// RTCP パケット 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtcpPacket {
    SenderReport(SenderReport),
    ReceiverReport(ReceiverReport),
    Sdes(Sdes),
    Bye(Bye),
    App(App),
    Nack(Nack),
    TransportCc(TransportCc),
    Pli(Pli),
    Fir(Fir),
    Remb(Remb),
    Unknown(UnknownPacket),
}

fn be32(d: &[u8]) -> u32 {
    u32::from_be_bytes([d[0], d[1], d[2], d[3]])
}

fn need(d: &[u8], n: usize) -> Result<()> {
    if d.len() < n {
        Err(Error::Truncated)
    } else {
        Ok(())
    }
}

fn parse_reports(body: &Bytes, start: usize, count: usize) -> Result<(Vec<ReportBlock>, Bytes)> {
    let end = start + count * 24;
    need(body, end)?;
    let reports = (0..count)
        .map(|i| ReportBlock::parse(&body[start + i * 24..]))
        .collect();
    Ok((reports, body.slice(end..)))
}

impl RtcpPacket {
    /// 1 つのパケット（共通ヘッダー付き、padding を含む全体）を解析する。
    fn parse_one(pkt: &Bytes) -> Result<RtcpPacket> {
        let b0 = pkt[0];
        let count = b0 & 0x1f;
        let pt = pkt[1];
        let mut end = pkt.len();
        if b0 & 0x20 != 0 {
            let p = pkt[end - 1] as usize;
            if p == 0 || p > end - 4 {
                return Err(Error::Invalid("RTCP の padding"));
            }
            end -= p;
        }
        let body = pkt.slice(4..end);
        let n = count as usize;
        Ok(match pt {
            PT_SR => {
                need(&body, 24)?;
                let (reports, extension) = parse_reports(&body, 24, n)?;
                RtcpPacket::SenderReport(SenderReport {
                    ssrc: be32(&body),
                    ntp: (be32(&body[4..]) as u64) << 32 | be32(&body[8..]) as u64,
                    rtp_timestamp: be32(&body[12..]),
                    packet_count: be32(&body[16..]),
                    octet_count: be32(&body[20..]),
                    reports,
                    extension,
                })
            }
            PT_RR => {
                need(&body, 4)?;
                let (reports, extension) = parse_reports(&body, 4, n)?;
                RtcpPacket::ReceiverReport(ReceiverReport {
                    ssrc: be32(&body),
                    reports,
                    extension,
                })
            }
            PT_SDES => RtcpPacket::Sdes(parse_sdes(&body, n)?),
            PT_BYE => {
                need(&body, 4 * n)?;
                let ssrcs = (0..n).map(|i| be32(&body[4 * i..])).collect();
                let rest = &body[4 * n..];
                let reason = if rest.is_empty() {
                    None
                } else {
                    let len = rest[0] as usize;
                    need(rest, 1 + len)?;
                    Some(body.slice(4 * n + 1..4 * n + 1 + len))
                };
                RtcpPacket::Bye(Bye { ssrcs, reason })
            }
            PT_APP => {
                need(&body, 8)?;
                RtcpPacket::App(App {
                    subtype: count,
                    ssrc: be32(&body),
                    name: [body[4], body[5], body[6], body[7]],
                    data: body.slice(8..),
                })
            }
            PT_RTPFB | PT_PSFB => {
                need(&body, 8)?;
                let sender_ssrc = be32(&body);
                let media_ssrc = be32(&body[4..]);
                let fci = body.slice(8..);
                match (pt, count) {
                    (PT_RTPFB, 1) => {
                        if !fci.len().is_multiple_of(4) {
                            return Err(Error::Invalid("NACK の FCI"));
                        }
                        let pairs = fci
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| {
                                (
                                    u16::from_be_bytes([c[0], c[1]]),
                                    u16::from_be_bytes([c[2], c[3]]),
                                )
                            })
                            .collect();
                        RtcpPacket::Nack(Nack {
                            sender_ssrc,
                            media_ssrc,
                            pairs,
                        })
                    }
                    (PT_RTPFB, 15) => {
                        RtcpPacket::TransportCc(TransportCc::parse(sender_ssrc, media_ssrc, &fci)?)
                    }
                    (PT_PSFB, 1) => RtcpPacket::Pli(Pli {
                        sender_ssrc,
                        media_ssrc,
                    }),
                    (PT_PSFB, 4) => {
                        if !fci.len().is_multiple_of(8) {
                            return Err(Error::Invalid("FIR の FCI"));
                        }
                        let entries = fci
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|c| (be32(c), c[4]))
                            .collect();
                        RtcpPacket::Fir(Fir {
                            sender_ssrc,
                            entries,
                        })
                    }
                    (PT_PSFB, 15) if fci.starts_with(b"REMB") => {
                        need(&fci, 8)?;
                        let num = fci[4] as usize;
                        let exp = fci[5] >> 2;
                        let mantissa =
                            ((fci[5] & 3) as u64) << 16 | (fci[6] as u64) << 8 | fci[7] as u64;
                        need(&fci, 8 + 4 * num)?;
                        let bitrate = mantissa
                            .checked_shl(exp as u32)
                            .filter(|v| v >> exp == mantissa)
                            .unwrap_or(u64::MAX);
                        RtcpPacket::Remb(Remb {
                            sender_ssrc,
                            bitrate,
                            ssrcs: (0..num).map(|i| be32(&fci[8 + 4 * i..])).collect(),
                        })
                    }
                    _ => RtcpPacket::Unknown(UnknownPacket {
                        packet_type: pt,
                        count,
                        payload: body,
                    }),
                }
            }
            _ => RtcpPacket::Unknown(UnknownPacket {
                packet_type: pt,
                count,
                payload: body,
            }),
        })
    }

    /// 共通ヘッダーを含めて `out` に書く。
    pub fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        let start = out.len();
        out.extend_from_slice(&[0x80, 0, 0, 0]);
        let (count, pt): (usize, u8) = match self {
            RtcpPacket::SenderReport(sr) => {
                out.extend_from_slice(&sr.ssrc.to_be_bytes());
                out.extend_from_slice(&sr.ntp.to_be_bytes());
                out.extend_from_slice(&sr.rtp_timestamp.to_be_bytes());
                out.extend_from_slice(&sr.packet_count.to_be_bytes());
                out.extend_from_slice(&sr.octet_count.to_be_bytes());
                for r in &sr.reports {
                    r.write(out);
                }
                ext4(&sr.extension)?;
                out.extend_from_slice(&sr.extension);
                (sr.reports.len(), PT_SR)
            }
            RtcpPacket::ReceiverReport(rr) => {
                out.extend_from_slice(&rr.ssrc.to_be_bytes());
                for r in &rr.reports {
                    r.write(out);
                }
                ext4(&rr.extension)?;
                out.extend_from_slice(&rr.extension);
                (rr.reports.len(), PT_RR)
            }
            RtcpPacket::Sdes(s) => {
                for c in &s.chunks {
                    let cs = out.len();
                    out.extend_from_slice(&c.ssrc.to_be_bytes());
                    for (t, v) in &c.items {
                        if *t == 0 {
                            return Err(Error::Invalid("SDES の項目の種類 0"));
                        }
                        let len = u8::try_from(v.len())
                            .map_err(|_| Error::Limit("SDES の項目が 255 バイトを超える"))?;
                        out.push(*t);
                        out.push(len);
                        out.extend_from_slice(v);
                    }
                    // 終端の 0 を 1 つ以上、4 バイト境界まで
                    out.push(0);
                    while !(out.len() - cs).is_multiple_of(4) {
                        out.push(0);
                    }
                }
                (s.chunks.len(), PT_SDES)
            }
            RtcpPacket::Bye(b) => {
                for s in &b.ssrcs {
                    out.extend_from_slice(&s.to_be_bytes());
                }
                if let Some(r) = &b.reason {
                    let len = u8::try_from(r.len())
                        .map_err(|_| Error::Limit("BYE の理由が 255 バイトを超える"))?;
                    out.push(len);
                    out.extend_from_slice(r);
                    while !(out.len() - start).is_multiple_of(4) {
                        out.push(0);
                    }
                }
                (b.ssrcs.len(), PT_BYE)
            }
            RtcpPacket::App(a) => {
                out.extend_from_slice(&a.ssrc.to_be_bytes());
                out.extend_from_slice(&a.name);
                ext4(&a.data)?;
                out.extend_from_slice(&a.data);
                (a.subtype as usize, PT_APP)
            }
            RtcpPacket::Nack(n) => {
                out.extend_from_slice(&n.sender_ssrc.to_be_bytes());
                out.extend_from_slice(&n.media_ssrc.to_be_bytes());
                for (pid, blp) in &n.pairs {
                    out.extend_from_slice(&pid.to_be_bytes());
                    out.extend_from_slice(&blp.to_be_bytes());
                }
                (1, PT_RTPFB)
            }
            RtcpPacket::TransportCc(t) => {
                out.extend_from_slice(&t.sender_ssrc.to_be_bytes());
                out.extend_from_slice(&t.media_ssrc.to_be_bytes());
                t.write_fci(out)?;
                (15, PT_RTPFB)
            }
            RtcpPacket::Pli(p) => {
                out.extend_from_slice(&p.sender_ssrc.to_be_bytes());
                out.extend_from_slice(&p.media_ssrc.to_be_bytes());
                (1, PT_PSFB)
            }
            RtcpPacket::Fir(f) => {
                out.extend_from_slice(&f.sender_ssrc.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                for (ssrc, seq) in &f.entries {
                    out.extend_from_slice(&ssrc.to_be_bytes());
                    out.extend_from_slice(&[*seq, 0, 0, 0]);
                }
                (4, PT_PSFB)
            }
            RtcpPacket::Remb(r) => {
                out.extend_from_slice(&r.sender_ssrc.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(b"REMB");
                let n = u8::try_from(r.ssrcs.len())
                    .map_err(|_| Error::Limit("REMB の SSRC が多すぎる"))?;
                let mut exp = 0u32;
                while (r.bitrate >> exp) >= 1 << 18 {
                    exp += 1;
                }
                let m = (r.bitrate >> exp) as u32;
                out.push(n);
                out.push((exp as u8) << 2 | (m >> 16) as u8);
                out.push((m >> 8) as u8);
                out.push(m as u8);
                for s in &r.ssrcs {
                    out.extend_from_slice(&s.to_be_bytes());
                }
                (15, PT_PSFB)
            }
            RtcpPacket::Unknown(u) => {
                ext4(&u.payload)?;
                out.extend_from_slice(&u.payload);
                (u.count as usize, u.packet_type)
            }
        };
        if count > MAX_COUNT {
            return Err(Error::Limit("RTCP の個数が 31 を超える"));
        }
        let len = out.len() - start;
        if !len.is_multiple_of(4) {
            return Err(Error::Invalid("RTCP の長さが 4 の倍数ではない"));
        }
        let words = u16::try_from(len / 4 - 1).map_err(|_| Error::Limit("RTCP が長すぎる"))?;
        out[start] = 0x80 | count as u8;
        out[start + 1] = pt;
        out[start + 2..start + 4].copy_from_slice(&words.to_be_bytes());
        Ok(())
    }
}

fn ext4(b: &[u8]) -> Result<()> {
    if !b.len().is_multiple_of(4) {
        Err(Error::Invalid("長さが 4 の倍数ではない"))
    } else {
        Ok(())
    }
}

fn parse_sdes(body: &Bytes, count: usize) -> Result<Sdes> {
    let mut chunks = Vec::with_capacity(count);
    let mut pos = 0usize;
    for _ in 0..count {
        need(&body[pos..], 4)?;
        let ssrc = be32(&body[pos..]);
        pos += 4;
        let mut items = Vec::new();
        loop {
            let t = *body.get(pos).ok_or(Error::Truncated)?;
            if t == 0 {
                // 終端: 次の 4 バイト境界まで進む
                pos += 1;
                pos = pos.div_ceil(4) * 4;
                if pos > body.len() {
                    return Err(Error::Truncated);
                }
                break;
            }
            let len = *body.get(pos + 1).ok_or(Error::Truncated)? as usize;
            let end = pos + 2 + len;
            if end > body.len() {
                return Err(Error::Truncated);
            }
            if items.len() >= 64 {
                return Err(Error::Limit("SDES の項目が多すぎる"));
            }
            items.push((t, body.slice(pos + 2..end)));
            pos = end;
        }
        chunks.push(SdesChunk { ssrc, items });
    }
    Ok(Sdes { chunks })
}

fn split(buf: &Bytes, strict: bool) -> Result<Vec<RtcpPacket>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    if buf.is_empty() {
        return Err(Error::Truncated);
    }
    while pos < buf.len() {
        if out.len() >= MAX_PACKETS {
            return Err(Error::Limit("複合パケットの数"));
        }
        let h = buf.get(pos..pos + 4).ok_or(Error::Truncated)?;
        if h[0] >> 6 != 2 {
            return Err(Error::Invalid("RTCP の version が 2 ではない"));
        }
        let len = (u16::from_be_bytes([h[2], h[3]]) as usize + 1) * 4;
        let end = pos + len;
        if end > buf.len() {
            return Err(Error::Truncated);
        }
        if strict {
            if out.is_empty() && !matches!(h[1], PT_SR | PT_RR) {
                return Err(Error::Invalid("複合パケットの先頭が SR・RR ではない"));
            }
            if h[0] & 0x20 != 0 && end != buf.len() {
                return Err(Error::Invalid("padding は最後のパケットだけ"));
            }
        }
        out.push(RtcpPacket::parse_one(&buf.slice(pos..end))?);
        pos = end;
    }
    Ok(out)
}

/// 複合パケットを解析する（RFC 3550 A.2 の検査: version、先頭が SR/RR、padding の位置、長さの合計）。
pub fn parse_compound(buf: &Bytes) -> Result<Vec<RtcpPacket>> {
    split(buf, true)
}

/// reduced-size RTCP（RFC 5506）を含む任意の RTCP を解析する（先頭の種類・padding の位置を問わない）。
pub fn parse_reduced(buf: &Bytes) -> Result<Vec<RtcpPacket>> {
    split(buf, false)
}

/// 複数のパケットを 1 つにまとめる。
pub fn build_compound(packets: &[RtcpPacket]) -> Result<Bytes> {
    if packets.len() > MAX_PACKETS {
        return Err(Error::Limit("複合パケットの数"));
    }
    let mut out = Vec::new();
    for p in packets {
        p.write(&mut out)?;
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(p: RtcpPacket) {
        let b = build_compound(std::slice::from_ref(&p)).unwrap();
        let got = parse_reduced(&b).unwrap();
        assert_eq!(got, vec![p]);
    }

    #[test]
    fn sr_rr_roundtrip() {
        let rb = ReportBlock {
            ssrc: 9,
            fraction_lost: 12,
            cumulative_lost: -5,
            highest_seq: 0x0001_0005,
            jitter: 77,
            lsr: 0x1234_5678,
            dlsr: 65536,
        };
        rt(RtcpPacket::SenderReport(SenderReport {
            ssrc: 1,
            ntp: 0xE000_0000_8000_0000,
            rtp_timestamp: 90000,
            packet_count: 10,
            octet_count: 1000,
            reports: vec![rb],
            extension: Bytes::new(),
        }));
        rt(RtcpPacket::ReceiverReport(ReceiverReport {
            ssrc: 2,
            reports: vec![rb, rb],
            extension: Bytes::from_static(&[1, 2, 3, 4]),
        }));
    }

    #[test]
    fn compound_sr_sdes_bye() {
        let pkts = vec![
            RtcpPacket::SenderReport(SenderReport {
                ssrc: 0x1111,
                ..Default::default()
            }),
            RtcpPacket::Sdes(Sdes {
                chunks: vec![
                    SdesChunk::cname(0x1111, "user@host"),
                    SdesChunk::cname(0x2222, "abcd"),
                ],
            }),
            RtcpPacket::Bye(Bye {
                ssrcs: vec![0x1111],
                reason: Some(Bytes::from_static(b"bye")),
            }),
        ];
        let b = build_compound(&pkts).unwrap();
        assert_eq!(b.len() % 4, 0);
        let got = parse_compound(&b).unwrap();
        assert_eq!(got, pkts);
        if let RtcpPacket::Sdes(s) = &got[1] {
            assert_eq!(s.chunks[0].get_cname(), Some(&b"user@host"[..]));
        }
        // 先頭が SDES の複合パケットは strict では拒否、reduced では受ける
        let b2 = build_compound(&pkts[1..]).unwrap();
        assert!(parse_compound(&b2).is_err());
        assert_eq!(parse_reduced(&b2).unwrap().len(), 2);
    }

    #[test]
    fn sdes_exact_four_byte_chunk() {
        // CNAME の長さ 2 → ssrc4 + 2 + 2 = 8、終端の 0 で 4 バイト追加
        let s = Sdes {
            chunks: vec![SdesChunk::cname(5, "ab")],
        };
        let b = build_compound(&[RtcpPacket::Sdes(s.clone())]).unwrap();
        assert_eq!(b.len(), 4 + 12);
        assert_eq!(parse_reduced(&b).unwrap(), vec![RtcpPacket::Sdes(s)]);
    }

    #[test]
    fn rfc3550_padding_last_only() {
        let mut b = build_compound(&[RtcpPacket::ReceiverReport(ReceiverReport {
            ssrc: 1,
            ..Default::default()
        })])
        .unwrap()
        .to_vec();
        // padding 4 バイトを付ける
        b[0] |= 0x20;
        b.extend_from_slice(&[0, 0, 0, 4]);
        b[3] += 1;
        let bb = Bytes::from(b.clone());
        assert_eq!(parse_compound(&bb).unwrap().len(), 1);
        // 後ろにもう 1 つ続けると不正
        let mut c = b.clone();
        c.extend_from_slice(&build_compound(&[RtcpPacket::Pli(Pli::default())]).unwrap());
        assert!(parse_compound(&Bytes::from(c)).is_err());
    }

    #[test]
    fn app_and_unknown() {
        rt(RtcpPacket::App(App {
            subtype: 3,
            ssrc: 7,
            name: *b"TEST",
            data: Bytes::from_static(&[1, 2, 3, 4]),
        }));
        rt(RtcpPacket::Unknown(UnknownPacket {
            packet_type: 207,
            count: 0,
            payload: Bytes::from_static(&[0; 8]),
        }));
    }

    #[test]
    fn nack_pli_fir_remb() {
        let n = Nack::from_sequences(1, 2, &[100, 101, 116, 117, 200, 65535, 0]);
        assert_eq!(n.pairs[0], (65535, 1));
        assert_eq!(n.pairs[1], (100, 0b1000_0000_0000_0001));
        assert_eq!(n.pairs[2], (117, 0));
        let mut seqs = n.sequences();
        seqs.sort();
        assert_eq!(seqs, vec![0, 100, 101, 116, 117, 200, 65535]);
        rt(RtcpPacket::Nack(n));
        rt(RtcpPacket::Pli(Pli {
            sender_ssrc: 1,
            media_ssrc: 2,
        }));
        rt(RtcpPacket::Fir(Fir {
            sender_ssrc: 1,
            entries: vec![(5, 1), (6, 255)],
        }));
        rt(RtcpPacket::Remb(Remb {
            sender_ssrc: 1,
            bitrate: 262_143,
            ssrcs: vec![10, 11],
        }));
        // 大きい値は mantissa の精度に丸まる
        let b = build_compound(&[RtcpPacket::Remb(Remb {
            sender_ssrc: 1,
            bitrate: 5_000_001,
            ssrcs: vec![],
        })])
        .unwrap();
        if let RtcpPacket::Remb(r) = &parse_reduced(&b).unwrap()[0] {
            assert!(r.bitrate <= 5_000_001 && r.bitrate > 4_999_900);
        } else {
            panic!()
        }
    }

    #[test]
    fn chrome_remb_bytes() {
        // libwebrtc の REMB（1 SSRC、exp=3 mantissa=0x3_0D40 → 1_600_000）
        let b = Bytes::from_static(&[
            0x8F, 0xCE, 0x00, 0x05, 0, 0, 0, 1, 0, 0, 0, 0, b'R', b'E', b'M', b'B', 0x01, 0x0F,
            0x0D, 0x40, 0xAA, 0xBB, 0xCC, 0xDD,
        ]);
        match &parse_reduced(&b).unwrap()[0] {
            RtcpPacket::Remb(r) => {
                assert_eq!(r.bitrate, 0x3_0D40 << 3);
                assert_eq!(r.ssrcs, vec![0xAABBCCDD]);
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn fuzz_never_panics() {
        let base = build_compound(&[
            RtcpPacket::SenderReport(SenderReport {
                ssrc: 1,
                reports: vec![ReportBlock::default()],
                ..Default::default()
            }),
            RtcpPacket::Sdes(Sdes {
                chunks: vec![SdesChunk::cname(1, "x")],
            }),
            RtcpPacket::Nack(Nack::from_sequences(1, 2, &[1, 3])),
            RtcpPacket::TransportCc(
                TransportCc::from_arrivals(1, 2, 10, 0, &[Some(0), None, Some(1000)]).unwrap(),
            ),
        ])
        .unwrap();
        let mut seed = 99u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..30000 {
            let mut v = base.to_vec();
            let n = 1 + rnd() % 4;
            for _ in 0..n {
                let i = rnd() as usize % v.len();
                v[i] = rnd() as u8;
            }
            let cut = rnd() as usize % (v.len() + 1);
            v.truncate(cut);
            let b = Bytes::from(v);
            let _ = parse_compound(&b);
            if let Ok(pk) = parse_reduced(&b) {
                // 解析できたものは書き戻せる（REMB の丸めを除いて同じになる）
                let _ = build_compound(&pk);
            }
        }
    }
}
