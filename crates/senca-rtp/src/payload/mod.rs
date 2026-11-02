//! RTP payload format（packetizer・depacketizer）。内部表現（ADR-0003）の AU と RTP payload の相互変換。
//!
//! - packetizer: AU 1 つ → RTP payload の列（同じ RTP timestamp。映像は最後に marker）。
//!   RTP ヘッダー（SSRC・番号・timestamp）は [`crate::session::RtpSender`] が付ける。
//! - depacketizer: 順番どおりの RTP パケット（[`crate::jitter::JitterBuffer`] の後ろ）→ [`Frame`]。
//!   番号の抜けを自分でも検出し、壊れた AU は捨てて次のフレームに `discontinuity` を立てる。
//!   映像は抜けのあとキーフレームまで捨てる。
//!
//! 対応: H.264（RFC 6184）・H.265（RFC 7798）・VP8（RFC 7741）・VP9（RFC 9628）・AV1（AV1 RTP 仕様）・
//! Opus（RFC 7587）・AAC（RFC 3640 mpeg4-generic AAC-hbr、RFC 6416 MP4A-LATM の受信）・G.711（RFC 3551）。

pub mod aac;
pub mod av1;
pub mod g711;
pub mod h264;
pub mod h265;
mod h26x;
pub mod opus;
pub mod vp8;
pub mod vp9;

use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use bytes::Bytes;
use senca_core::media::{AccessUnit, Codec, CodecConfig};
use std::collections::VecDeque;

/// 既定の RTP payload の最大長（IP/UDP/RTP・SRTP・拡張のヘッダーを引いても 1500 に収まる値）。
pub const DEFAULT_MTU: usize = 1200;
/// 1 つの AU を組み立てるときの上限。
pub const MAX_AU_SIZE: usize = 16 << 20;
/// 取り出されずに溜まったフレームの上限（超えたら古いものから捨てる）。
pub const MAX_QUEUED_FRAMES: usize = 256;

/// packetizer の出力 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub data: Bytes,
    pub marker: bool,
    /// AU の RTP timestamp に足す値（G.711 を分割したときなど。通常は 0）。
    pub timestamp_offset: u32,
}

impl Payload {
    pub fn new(data: Bytes, marker: bool) -> Self {
        Payload {
            data,
            marker,
            timestamp_offset: 0,
        }
    }
}

/// AU を RTP payload にする。
pub trait Packetizer: Send {
    fn codec(&self) -> Codec;
    /// RTP の時計の周波数。
    fn clock_rate(&self) -> u32;
    /// AU を payload の列にする。`config` はその AU に適用する設定（H.264 のパラメーターセットの挿入などに使う）。
    fn packetize(
        &mut self,
        au: &AccessUnit,
        config: Option<&CodecConfig>,
        out: &mut Vec<Payload>,
    ) -> Result<()>;
}

/// depacketizer の出力（内部表現のフレーム 1 つ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// RTP timestamp（折り返す 32 ビットのまま）。
    pub timestamp: u32,
    pub payload: Bytes,
    pub keyframe: bool,
    /// このフレームの直前に欠落がある。
    pub discontinuity: bool,
    /// このフレームから適用する新しい設定（変わったときだけ）。
    pub config: Option<CodecConfig>,
    /// 適用する設定の世代。
    pub config_generation: u32,
    /// 長さ（RTP 単位。不明なら 0）。
    pub duration: u32,
}

/// RTP payload を AU にする。
pub trait Depacketizer: Send {
    fn codec(&self) -> Codec;
    fn clock_rate(&self) -> u32;
    /// 順番どおりのパケットを入れる。壊れた payload は `Err`（状態は保たれ、続けて使える）。
    fn push(&mut self, packet: &RtpPacket) -> Result<()>;
    /// 出来上がったフレームを取り出す。
    fn pop(&mut self) -> Option<Frame>;
    /// 外（ジッターバッファ）で損失が分かったときに知らせる。組み立て中の AU を捨てる。
    fn mark_loss(&mut self);
    /// 今の設定。
    fn config(&self) -> Option<&CodecConfig>;
}

/// 番号の連続を見張る（depacketizer の共通部品）。
#[derive(Debug, Clone, Default)]
pub(crate) struct SeqWatch {
    last: Option<u16>,
}

impl SeqWatch {
    /// 前のパケットから抜けがあれば真。
    pub(crate) fn gap(&mut self, seq: u16) -> bool {
        let gap = self.last.is_some_and(|l| seq != l.wrapping_add(1));
        self.last = Some(seq);
        gap
    }
}

/// 設定の世代を管理する（depacketizer の共通部品）。
#[derive(Debug, Clone, Default)]
pub(crate) struct ConfigState {
    pub(crate) current: Option<CodecConfig>,
    pub(crate) generation: u32,
    /// 次のフレームに付けて知らせる新しい設定。
    pub(crate) announce: Option<CodecConfig>,
}

impl ConfigState {
    /// 新しい設定を入れる。中身（record）が同じなら何もしない。
    pub(crate) fn update(&mut self, mut cfg: CodecConfig) {
        if let Some(cur) = &self.current
            && cur.record == cfg.record
            && cur.video == cfg.video
            && cur.audio == cfg.audio
        {
            return;
        }
        if self.current.is_some() {
            self.generation = self.generation.wrapping_add(1);
        }
        cfg.generation = self.generation;
        self.current = Some(cfg.clone());
        self.announce = Some(cfg);
    }

    pub(crate) fn take_announce(&mut self) -> Option<CodecConfig> {
        self.announce.take()
    }
}

/// フレームの待ち行列（上限つき）。
#[derive(Debug, Default)]
pub(crate) struct FrameQueue {
    q: VecDeque<Frame>,
    dropped: bool,
}

impl FrameQueue {
    pub(crate) fn push(&mut self, f: Frame) {
        if self.q.len() >= MAX_QUEUED_FRAMES {
            self.q.pop_front();
            self.dropped = true;
        }
        self.q.push_back(f);
    }

    pub(crate) fn pop(&mut self) -> Option<Frame> {
        let mut f = self.q.pop_front()?;
        if self.dropped {
            f.discontinuity = true;
            self.dropped = false;
        }
        Some(f)
    }
}

/// fmtp の `key=value; key=value` を分ける。キーは小文字にする。値の前後の空白は除く。
pub fn parse_fmtp(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|kv| {
            let kv = kv.trim();
            if kv.is_empty() {
                return None;
            }
            Some(match kv.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
                None => (kv.to_ascii_lowercase(), String::new()),
            })
        })
        .collect()
}

/// fmtp の値（キーは大文字・小文字を区別しない）。
pub fn fmtp_get<'a>(fmtp: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fmtp.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// fmtp を `key=value;key=value` にする。
pub fn format_fmtp(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                k.clone()
            } else {
                format!("{k}={v}")
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// SDP の rtpmap・fmtp で表した payload format。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadFormat {
    /// rtpmap の encoding name（`H264`・`opus`・`mpeg4-generic` など。大文字・小文字は問わない）。
    pub encoding: String,
    pub clock_rate: u32,
    /// 音声のチャンネル数（rtpmap の 3 つ目）。
    pub channels: Option<u16>,
    /// fmtp の組（[`parse_fmtp`] の結果）。
    pub fmtp: Vec<(String, String)>,
}

impl PayloadFormat {
    /// rtpmap の encoding name からコーデックを決める。
    pub fn codec(&self) -> Option<Codec> {
        let e = self.encoding.to_ascii_lowercase();
        Some(match e.as_str() {
            "h264" => Codec::H264,
            "h265" => Codec::H265,
            "vp8" => Codec::Vp8,
            "vp9" => Codec::Vp9,
            "av1" => Codec::Av1,
            "opus" => Codec::Opus,
            "mpeg4-generic" | "mp4a-latm" => Codec::Aac,
            "pcma" => Codec::Pcma,
            "pcmu" => Codec::Pcmu,
            _ => return None,
        })
    }

    /// `PayloadFormat` の fmtp の値。
    pub fn fmtp(&self, key: &str) -> Option<&str> {
        fmtp_get(&self.fmtp, key)
    }
}

/// SDP の payload format に合う depacketizer を作る。
pub fn depacketizer(fmt: &PayloadFormat) -> Result<Box<dyn Depacketizer>> {
    let e = fmt.encoding.to_ascii_lowercase();
    Ok(match e.as_str() {
        "h264" => Box::new(h264::H264Depacketizer::from_format(fmt)?),
        "h265" => Box::new(h265::H265Depacketizer::from_format(fmt)?),
        "vp8" => Box::new(vp8::Vp8Depacketizer::new()),
        "vp9" => Box::new(vp9::Vp9Depacketizer::new()),
        "av1" => Box::new(av1::Av1Depacketizer::new()),
        "opus" => Box::new(opus::OpusDepacketizer::from_format(fmt)?),
        "mpeg4-generic" => Box::new(aac::AacDepacketizer::from_format(fmt)?),
        "mp4a-latm" => Box::new(aac::LatmDepacketizer::from_format(fmt)?),
        "pcma" => Box::new(g711::G711Depacketizer::new(Codec::Pcma, fmt.clock_rate)),
        "pcmu" => Box::new(g711::G711Depacketizer::new(Codec::Pcmu, fmt.clock_rate)),
        _ => return Err(Error::Unsupported("payload format")),
    })
}

/// 設定に合う packetizer と、SDP に書く payload format を作る。`mtu` は RTP payload の最大長。
pub fn packetizer(
    codec: Codec,
    config: Option<&CodecConfig>,
    mtu: usize,
) -> Result<(Box<dyn Packetizer>, PayloadFormat)> {
    let mtu = mtu.clamp(64, 65000);
    Ok(match codec {
        Codec::H264 => {
            let p = h264::H264Packetizer::new(mtu, h264::PacketizationMode::NonInterleaved);
            let f = h264::payload_format(config, h264::PacketizationMode::NonInterleaved)?;
            (Box::new(p), f)
        }
        Codec::H265 => (
            Box::new(h265::H265Packetizer::new(mtu)),
            h265::payload_format(config)?,
        ),
        Codec::Vp8 => (
            Box::new(vp8::Vp8Packetizer::new(mtu)),
            vp8::payload_format(),
        ),
        Codec::Vp9 => (
            Box::new(vp9::Vp9Packetizer::new(mtu)),
            vp9::payload_format(),
        ),
        Codec::Av1 => (
            Box::new(av1::Av1Packetizer::new(mtu)),
            av1::payload_format(),
        ),
        Codec::Opus => (
            Box::new(opus::OpusPacketizer::new()),
            opus::payload_format(config),
        ),
        Codec::Aac => {
            let cfg = config.ok_or(Error::Invalid("AAC には AudioSpecificConfig が要る"))?;
            (
                Box::new(aac::AacPacketizer::new(mtu, cfg)?),
                aac::payload_format(cfg)?,
            )
        }
        Codec::Pcma | Codec::Pcmu => (
            Box::new(g711::G711Packetizer::new(codec, mtu)),
            g711::payload_format(codec),
        ),
        Codec::WebVtt => return Err(Error::Unsupported("WebVTT の RTP")),
    })
}

/// 静的 payload type（RFC 3551）があればその値。
pub fn static_payload_type(codec: Codec) -> Option<u8> {
    match codec {
        Codec::Pcmu => Some(0),
        Codec::Pcma => Some(8),
        _ => None,
    }
}

/// 静的 payload type（RFC 3551）の既定の format。
pub fn static_format(pt: u8) -> Option<PayloadFormat> {
    let (e, rate) = match pt {
        0 => ("PCMU", 8000),
        8 => ("PCMA", 8000),
        _ => return None,
    };
    Some(PayloadFormat {
        encoding: e.into(),
        clock_rate: rate,
        channels: Some(1),
        fmtp: Vec::new(),
    })
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// payload の列を RTP パケットに包んで depacketizer に入れ、出てきたフレームを集める。
    pub(crate) fn run(
        d: &mut dyn Depacketizer,
        payloads: &[Payload],
        ts: u32,
        seq: &mut u16,
    ) -> Vec<Frame> {
        for p in payloads {
            let pkt = RtpPacket::new(
                96,
                *seq,
                ts.wrapping_add(p.timestamp_offset),
                1,
                p.marker,
                p.data.clone(),
            );
            *seq = seq.wrapping_add(1);
            d.push(&pkt).unwrap();
        }
        let mut out = Vec::new();
        while let Some(f) = d.pop() {
            out.push(f);
        }
        out
    }

    /// 乱数（xorshift）。
    pub(crate) struct Rng(pub u32);
    impl Rng {
        pub(crate) fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0
        }
    }

    /// ランダムな payload を入れても panic しない。
    pub(crate) fn fuzz(d: &mut dyn Depacketizer, seeds: &[Payload], iters: usize) {
        let mut r = Rng(0xC0FFEE);
        let mut seq = 0u16;
        for i in 0..iters {
            let base = &seeds[r.next() as usize % seeds.len()];
            let mut v = base.data.to_vec();
            if !v.is_empty() {
                for _ in 0..(r.next() % 3) {
                    let k = r.next() as usize % v.len();
                    v[k] = r.next() as u8;
                }
                v.truncate(r.next() as usize % (v.len() + 1));
            }
            // ときどき番号を飛ばす
            if r.next().is_multiple_of(13) {
                seq = seq.wrapping_add(2);
            }
            let pkt = RtpPacket::new(
                96,
                seq,
                (i / 3) as u32 * 3000,
                1,
                r.next().is_multiple_of(3),
                Bytes::from(v),
            );
            seq = seq.wrapping_add(1);
            let _ = d.push(&pkt);
            while d.pop().is_some() {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmtp_parsing() {
        let f = parse_fmtp(
            "packetization-mode=1; profile-level-id=42e01f;sprop-parameter-sets=Z0IAH5WoFAFuQA==,aM48gA==; ",
        );
        assert_eq!(fmtp_get(&f, "PACKETIZATION-MODE"), Some("1"));
        assert_eq!(
            fmtp_get(&f, "sprop-parameter-sets"),
            Some("Z0IAH5WoFAFuQA==,aM48gA==")
        );
        assert_eq!(
            format_fmtp(&f[..2]),
            "packetization-mode=1;profile-level-id=42e01f"
        );
        assert!(parse_fmtp("").is_empty());
    }

    #[test]
    fn config_state_generations() {
        let mut cs = ConfigState::default();
        let c = CodecConfig {
            codec: Codec::Opus,
            generation: 0,
            record: Bytes::from_static(b"a"),
            video: None,
            audio: None,
        };
        cs.update(c.clone());
        assert_eq!(cs.take_announce().unwrap().generation, 0);
        cs.update(c.clone());
        assert!(cs.take_announce().is_none());
        cs.update(CodecConfig {
            record: Bytes::from_static(b"b"),
            ..c
        });
        assert_eq!(cs.take_announce().unwrap().generation, 1);
    }
}
