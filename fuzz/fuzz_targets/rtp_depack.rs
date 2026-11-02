//! RTP の depacketizer（H.264・H.265・VP8・VP9・AV1・Opus・AAC（mpeg4-generic・LATM）・G.711）。
//!
//! 入力の形: 先頭 1 バイトで payload format を選び、残りを「長さ 2 バイト・印 1 バイト・payload」の並びとして
//! 順番どおりのパケットにする。印の bit0 は marker、bit1 は直前の欠落（mark_loss）、bit2 は番号を飛ばす。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_rtp::{PayloadFormat, RtpPacket};

fn formats() -> Vec<PayloadFormat> {
    let f = |enc: &str, rate: u32, ch: Option<u16>, fmtp: &str| PayloadFormat {
        encoding: enc.into(),
        clock_rate: rate,
        channels: ch,
        fmtp: senca_rtp::payload::parse_fmtp(fmtp),
    };
    vec![
        f("H264", 90000, None, "packetization-mode=1"),
        f("H264", 90000, None, "packetization-mode=0"),
        f(
            "H264",
            90000,
            None,
            "packetization-mode=1;sprop-parameter-sets=Z0LAHtkDxWhAAAADAEAAAAwDxYuS,aMuMsg==",
        ),
        f("H265", 90000, None, ""),
        f("VP8", 90000, None, ""),
        f("VP9", 90000, None, ""),
        f("AV1", 90000, None, ""),
        f("opus", 48000, Some(2), "sprop-stereo=1"),
        f(
            "mpeg4-generic",
            48000,
            Some(2),
            "streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1190",
        ),
        f("MP4A-LATM", 44100, Some(2), "cpresent=0;config=40002410"),
        f("PCMA", 8000, Some(1), ""),
        f("PCMU", 8000, Some(1), ""),
    ]
}

fuzz_target!(|data: &[u8]| {
    let Some((&sel, mut rest)) = data.split_first() else { return };
    let fmts = formats();
    let fmt = &fmts[sel as usize % fmts.len()];
    let Ok(mut d) = senca_rtp::payload::depacketizer(fmt) else { return };
    let mut seq: u16 = 0xfff0; // 折り返しを含める
    let mut ts: u32 = 0xffff_f000;
    while rest.len() >= 3 {
        let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
        let flags = rest[2];
        rest = &rest[3..];
        let len = len.min(rest.len());
        let (payload, tail) = rest.split_at(len);
        rest = tail;
        if flags & 2 != 0 {
            d.mark_loss();
        }
        if flags & 4 != 0 {
            seq = seq.wrapping_add(1);
        }
        let p = RtpPacket::new(96, seq, ts, 0x1234_5678, flags & 1 != 0, Bytes::copy_from_slice(payload));
        let _ = d.push(&p);
        while let Some(fr) = d.pop() {
            let _ = fr.payload.len();
        }
        let _ = d.config();
        seq = seq.wrapping_add(1);
        if flags & 1 != 0 {
            ts = ts.wrapping_add(3000);
        }
    }
});
