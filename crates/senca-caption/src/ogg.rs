//! 最小限の Ogg の書き出し（RFC 3533）と Ogg Opus（RFC 7845）。
//!
//! 1 本の論理ストリームだけを書く。パケットは lacing で 255 バイトずつに分け、1 ページは 255 セグメントまで。
//! ページの CRC は多項式 0x04C11DB7・初期値 0・反転なし・最終 XOR なし（RFC 3533 6）。

use crate::error::{Error, Result};
use bytes::Bytes;
use senca_opus::OpusHead;

/// RFC 3533 の CRC-32 の表。
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut k = 0;
        while k < 8 {
            r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04C1_1DB7 } else { r << 1 };
            k += 1;
        }
        t[i] = r;
        i += 1;
    }
    t
};

/// Ogg のページの CRC（CRC の欄を 0 にしたページ全体に対して計算する）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc = (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}

/// header_type の旗。
pub const FLAG_CONTINUED: u8 = 0x01;
pub const FLAG_BOS: u8 = 0x02;
pub const FLAG_EOS: u8 = 0x04;

/// 書き出すファイル全体の上限。
const MAX_STREAM_BYTES: usize = 64 << 20;
/// 1 ページの中身の目安（これを超えたらページを閉じる）。
const PAGE_TARGET_BYTES: usize = 4096;

#[derive(Debug, Clone, Default)]
struct Page {
    flags: u8,
    granule: Option<i64>,
    lacing: Vec<u8>,
    body: Vec<u8>,
}

/// 1 本の論理ストリームの書き手。ページは [`OggWriter::finish`] でまとめて書き出す（最後のページに EOS を立てるため）。
#[derive(Debug, Clone)]
pub struct OggWriter {
    serial: u32,
    pages: Vec<Page>,
    cur: Page,
    total: usize,
}

impl OggWriter {
    pub fn new(serial: u32) -> Self {
        OggWriter { serial, pages: Vec::new(), cur: Page { flags: FLAG_BOS, ..Page::default() }, total: 0 }
    }

    /// パケットを 1 つ足す。`granule` はこのパケットの終わりの granule position。
    /// `flush` ならこのパケットの後でページを閉じる（識別ヘッダー・コメントヘッダー用）。
    pub fn write_packet(&mut self, packet: &[u8], granule: i64, flush: bool) -> Result<()> {
        self.total = self.total.saturating_add(packet.len() + packet.len() / 255 + 1);
        if self.total > MAX_STREAM_BYTES {
            return Err(Error::TooLarge("ogg stream"));
        }
        let mut rest = packet;
        let mut first = true;
        loop {
            if self.cur.lacing.len() == 255 {
                // ページが埋まった。パケットの途中なら次のページは続き（CONTINUED）
                self.close_page(!first);
            }
            let n = rest.len().min(255);
            self.cur.lacing.push(n as u8);
            self.cur.body.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            first = false;
            if n < 255 {
                break;
            }
            // ちょうど 255 の倍数の長さなら、終わりを示す 0 のセグメントが要る（次の周回で n = 0 を足す）
        }
        self.cur.granule = Some(granule);
        if flush || self.cur.body.len() >= PAGE_TARGET_BYTES {
            self.close_page(false);
        }
        Ok(())
    }

    fn close_page(&mut self, next_continued: bool) {
        if self.cur.lacing.is_empty() {
            return;
        }
        let next_flags = if next_continued { FLAG_CONTINUED } else { 0 };
        let page = std::mem::replace(&mut self.cur, Page { flags: next_flags, ..Page::default() });
        self.pages.push(page);
    }

    /// 書き終える。最後のページに EOS を立てて、全ページのバイト列を返す。
    pub fn finish(mut self) -> Result<Vec<u8>> {
        self.close_page(false);
        let Some(last) = self.pages.last_mut() else {
            return Err(Error::InvalidInput("ogg stream without packets"));
        };
        last.flags |= FLAG_EOS;
        let mut out = Vec::with_capacity(self.total + self.pages.len() * 282);
        for (seq, p) in self.pages.iter().enumerate() {
            write_page(&mut out, p, self.serial, seq as u32);
        }
        Ok(out)
    }
}

fn write_page(out: &mut Vec<u8>, p: &Page, serial: u32, seq: u32) {
    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0); // stream_structure_version
    out.push(p.flags);
    out.extend_from_slice(&p.granule.unwrap_or(-1).to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    let crc_pos = out.len();
    out.extend_from_slice(&[0; 4]);
    out.push(p.lacing.len() as u8);
    out.extend_from_slice(&p.lacing);
    out.extend_from_slice(&p.body);
    let crc = crc32(&out[start..]);
    out[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());
}

/// OpusTags（RFC 7845 5.2）。vendor だけでコメントなし。
pub fn opus_tags(vendor: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + vendor.len());
    v.extend_from_slice(b"OpusTags");
    v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    v.extend_from_slice(vendor.as_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}

/// Ogg Opus のファイル全体を作る。`packets` は Opus パケット（48 kHz の時間軸で連続しているもの）。
///
/// granule position は pre-skip + それまでのサンプル数（RFC 7845 4）。再生時間は総サンプル数 − pre-skip。
pub fn ogg_opus_file(head: &OpusHead, packets: &[Bytes], serial: u32) -> Result<Vec<u8>> {
    if packets.is_empty() {
        return Err(Error::InvalidInput("no opus packets"));
    }
    let head_bytes = head.to_bytes()?;
    let mut w = OggWriter::new(serial);
    w.write_packet(&head_bytes, 0, true)?;
    w.write_packet(&opus_tags("senca"), 0, true)?;
    let mut granule = head.pre_skip as i64;
    for p in packets {
        granule += senca_opus::packet_samples(p)? as i64;
        w.write_packet(p, granule, false)?;
    }
    w.finish()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 試験用のページ解析: (flags, granule, serial, seq, lacing, body) と CRC の検査。
    /// (flags, granule, serial, seq, lacing, body)。
    pub(crate) type PageParts = (u8, i64, u32, u32, Vec<u8>, Vec<u8>);

    pub(crate) fn parse_pages(data: &[u8]) -> Vec<PageParts> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            assert_eq!(&data[pos..pos + 4], b"OggS");
            assert_eq!(data[pos + 4], 0);
            let flags = data[pos + 5];
            let granule = i64::from_le_bytes(data[pos + 6..pos + 14].try_into().unwrap());
            let serial = u32::from_le_bytes(data[pos + 14..pos + 18].try_into().unwrap());
            let seq = u32::from_le_bytes(data[pos + 18..pos + 22].try_into().unwrap());
            let crc = u32::from_le_bytes(data[pos + 22..pos + 26].try_into().unwrap());
            let n = data[pos + 26] as usize;
            let lacing = data[pos + 27..pos + 27 + n].to_vec();
            let body_len: usize = lacing.iter().map(|&x| x as usize).sum();
            let end = pos + 27 + n + body_len;
            let mut copy = data[pos..end].to_vec();
            copy[22..26].fill(0);
            assert_eq!(crc32(&copy), crc, "page {seq} crc");
            out.push((flags, granule, serial, seq, lacing, data[pos + 27 + n..end].to_vec()));
            pos = end;
        }
        out
    }

    /// ページからパケットを組み立て直す。
    pub(crate) fn packets(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut cur = Vec::new();
        for (_, _, _, _, lacing, body) in parse_pages(data) {
            let mut p = 0;
            for l in lacing {
                cur.extend_from_slice(&body[p..p + l as usize]);
                p += l as usize;
                if l < 255 {
                    out.push(std::mem::take(&mut cur));
                }
            }
        }
        out
    }

    #[test]
    fn crc_known_values() {
        // "OggS" 等の既知値ではなく、多項式の性質で確かめる: 空は 0、1 バイト 0x01 は多項式そのもの
        assert_eq!(crc32(&[]), 0);
        assert_eq!(crc32(&[0x01]), 0x04C1_1DB7);
        // CRC-32/MPEG-2 と違い初期値 0・最終 XOR なし（CRC-32/BZIP2 系の非反転）: "123456789" の値
        assert_eq!(crc32(b"123456789"), 0x89A1_897F);
    }

    #[test]
    fn page_structure_and_lacing() {
        let mut w = OggWriter::new(0x1234);
        w.write_packet(b"head", 0, true).unwrap();
        let big = vec![7u8; 255 * 3]; // ちょうど 255 の倍数 → 0 の終端セグメント
        w.write_packet(&big, 100, false).unwrap();
        let huge = vec![9u8; 255 * 600 + 10]; // 3 ページにまたがる
        w.write_packet(&huge, 200, false).unwrap();
        w.write_packet(b"tail", 300, false).unwrap();
        let data = w.finish().unwrap();
        let pages = parse_pages(&data);
        assert_eq!(pages[0].0, FLAG_BOS);
        assert_eq!(pages[0].1, 0);
        assert_eq!(pages[0].4, vec![4]);
        assert!(pages.iter().all(|p| p.2 == 0x1234));
        assert!(pages.iter().enumerate().all(|(i, p)| p.3 == i as u32));
        assert!(pages.iter().all(|p| p.4.len() <= 255));
        assert_eq!(pages.last().unwrap().0 & FLAG_EOS, FLAG_EOS);
        assert_eq!(pages.iter().filter(|p| p.0 & FLAG_EOS != 0).count(), 1);
        // 続きのページには CONTINUED、途中で終わるパケットのないページの granule は -1
        assert!(pages.iter().skip(1).any(|p| p.0 & FLAG_CONTINUED != 0));
        assert!(pages.iter().any(|p| p.1 == -1));
        let ps = packets(&data);
        assert_eq!(ps.len(), 4);
        assert_eq!(ps[1], big);
        assert_eq!(ps[2], huge);
        assert_eq!(ps[3], b"tail");
        assert_eq!(pages.last().unwrap().1, 300);
    }

    #[test]
    fn empty_stream_rejected() {
        assert!(OggWriter::new(1).finish().is_err());
        let head = OpusHead::new(1, 312, 48000).unwrap();
        assert!(ogg_opus_file(&head, &[], 1).is_err());
    }

    #[test]
    fn ogg_opus_layout() {
        let head = OpusHead::new(2, 312, 48000).unwrap();
        // TOC 0xFC: config 31 (CELT FB 20 ms)、stereo、code 0 → 960 サンプル
        let pkt = Bytes::from_static(&[0xFC, 1, 2, 3]);
        let packets_in: Vec<Bytes> = (0..10).map(|_| pkt.clone()).collect();
        let data = ogg_opus_file(&head, &packets_in, 7).unwrap();
        let pages = parse_pages(&data);
        // 識別ヘッダーは単独の最初のページ、コメントヘッダーは次のページで閉じる
        assert_eq!(pages[0].4.len(), 1);
        assert_eq!(pages[0].0, FLAG_BOS);
        assert!(pages[1].5.starts_with(b"OpusTags"));
        assert_eq!(pages[1].1, 0);
        assert_eq!(pages.last().unwrap().1, 312 + 9600);
        let ps = packets(&data);
        assert_eq!(OpusHead::parse(&ps[0]).unwrap(), head);
        assert_eq!(ps.len(), 12);
    }
}
