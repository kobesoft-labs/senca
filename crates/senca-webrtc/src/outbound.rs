//! 送信トラック（1 つの m 行、1 つの SSRC と RTX の SSRC）。
//!
//! AU → RTP。SSRC・番号・timestamp はこのトラックが持ち、入力の切り替え（simulcast の層・別の送り手）があっても
//! 続いた流れに見えるよう書き換える（切り替えはキーフレームでだけ行う）。送った packet は再送のために覚える。

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use senca_core::media::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
use senca_core::time::{Rescale, Timebase};
use senca_rtp::rtcp::SenderReport;
use senca_rtp::rtx::{RetransmissionHistory, rtx_wrap};
use senca_rtp::{RtpPacket, RtpSender, payload};
use senca_sdp::NegotiatedCodec;

use crate::inbound::{codec_of, payload_format};

struct SenderState {
    codec: Codec,
    clock: u32,
    rtp: RtpSender,
}

/// 送信の結果。
#[derive(Debug, Default)]
pub(crate) struct Packetized {
    pub packets: Vec<RtpPacket>,
    /// キーフレームが要る（入力の切り替え・始まりがキーフレームでない）。
    pub keyframe_needed: bool,
}

pub(crate) struct OutboundTrack {
    pub track: TrackId,
    pub mid: String,
    /// simulcast で送るときの層（RFC 8851 の rid）。
    pub rid: Option<String>,
    pub kind: MediaKind,
    pub ssrc: u32,
    pub rtx_ssrc: Option<u32>,
    pub codecs: Vec<NegotiatedCodec>,
    /// 交渉の結果、送ってよい。
    pub active: bool,
    sender: Option<SenderState>,
    next_seq: u16,
    ts_base: u32,
    source: Option<(TrackId, u64)>,
    /// 時計の単位で AU の pts に足す値。
    offset: i64,
    last_pts: Option<i64>,
    last_send: Option<Instant>,
    waiting_keyframe: bool,
    history: RetransmissionHistory,
    rtx_seq: u16,
    resent: BTreeMap<u16, Instant>,
    resent_order: VecDeque<u16>,
    pub nack_enabled: bool,
    pub packets_sent: u64,
    pub retransmitted: u64,
}

fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    u32::from_be_bytes(b)
}

impl OutboundTrack {
    pub fn new(
        track: TrackId,
        mid: &str,
        rid: Option<String>,
        kind: MediaKind,
        ssrc: u32,
        rtx_ssrc: Option<u32>,
        history: usize,
    ) -> Self {
        let r = rand_u32();
        OutboundTrack {
            track,
            mid: mid.to_string(),
            rid,
            kind,
            ssrc,
            rtx_ssrc,
            codecs: Vec::new(),
            active: false,
            sender: None,
            next_seq: (r >> 16) as u16 & 0x7fff,
            ts_base: rand_u32(),
            source: None,
            offset: 0,
            last_pts: None,
            last_send: None,
            waiting_keyframe: kind == MediaKind::Video,
            history: RetransmissionHistory::new(if kind == MediaKind::Video {
                history
            } else {
                64
            }),
            rtx_seq: r as u16,
            resent: BTreeMap::new(),
            resent_order: VecDeque::new(),
            nack_enabled: false,
            packets_sent: 0,
            retransmitted: 0,
        }
    }

    /// 交渉の結果を入れる。
    pub fn set_codecs(&mut self, codecs: Vec<NegotiatedCodec>, active: bool) {
        self.nack_enabled = codecs.iter().any(|c| c.has_feedback("nack"));
        self.codecs = codecs;
        self.active = active;
        // 交渉が変わったら packetizer を作り直す（番号は続ける）
        if let Some(s) = self.sender.take() {
            self.next_seq = s.rtp.next_sequence();
        }
    }

    /// 送れるコーデック。
    pub fn negotiated_codecs(&self) -> Vec<Codec> {
        let mut v: Vec<Codec> = self.codecs.iter().filter_map(codec_of).collect();
        v.dedup();
        v
    }

    fn pick(&self, codec: Codec) -> Option<&NegotiatedCodec> {
        let mut found = self.codecs.iter().filter(|c| codec_of(c) == Some(codec));
        if codec == Codec::H264 {
            // packetization-mode=1 を選ぶ（FU-A を使う）
            let all: Vec<&NegotiatedCodec> = found.collect();
            return all
                .iter()
                .find(|c| c.remote_fmtp_get("packetization-mode") == Some("1"))
                .or(all.first())
                .copied();
        }
        found.next()
    }

    /// AU を RTP にする。
    pub fn packetize(
        &mut self,
        now: Instant,
        au: &AccessUnit,
        config: Option<&CodecConfig>,
        mtu: usize,
    ) -> Result<Packetized, crate::pc::WebRtcError> {
        use crate::pc::WebRtcError;
        let mut out = Packetized::default();
        if !self.active {
            return Err(WebRtcError::InvalidState(
                "このトラックは送信の方向で交渉していない",
            ));
        }
        let video = self.kind == MediaKind::Video;
        if self.sender.as_ref().is_none_or(|s| s.codec != au.codec) {
            let nc = self
                .pick(au.codec)
                .ok_or(WebRtcError::UnsupportedCodec)?
                .clone();
            let (pz, _) = payload::packetizer(au.codec, config, mtu).map_err(WebRtcError::Rtp)?;
            let clock = pz.clock_rate().max(1);
            if let Some(old) = self.sender.take() {
                self.next_seq = old.rtp.next_sequence();
                // 時計が変わるなら写像を作り直す
                if old.clock != clock {
                    self.source = None;
                    self.last_pts = None;
                }
            }
            let _ = payload_format(&nc);
            self.sender = Some(SenderState {
                codec: au.codec,
                clock,
                rtp: RtpSender::new(pz, nc.payload_type, self.ssrc, self.next_seq, self.ts_base),
            });
            if video {
                self.waiting_keyframe = true;
                self.source = None;
            }
        }
        let Some(s) = self.sender.as_mut() else {
            return Ok(out);
        };
        let tb = Timebase::new(1, s.clock);
        let pts = au
            .pts
            .rescale(au.timebase, tb)
            .ok_or(WebRtcError::InvalidState("pts"))?;
        let dts = au
            .dts
            .rescale(au.timebase, tb)
            .ok_or(WebRtcError::InvalidState("dts"))?;
        let key = (au.track, au.epoch);
        if self.source != Some(key) {
            if video && !au.keyframe {
                // 切り替え・始まりはキーフレームから
                out.keyframe_needed = true;
                return Ok(out);
            }
            // 続いた時刻になるよう合わせる: 前に送った時刻 + 経過時間（少なくとも 1 単位）
            self.offset = match (self.last_pts, self.last_send) {
                (Some(lp), Some(ls)) => {
                    let el = now.saturating_duration_since(ls);
                    let step = ((el.as_nanos() as i128 * s.clock as i128) / 1_000_000_000) as i64;
                    lp + step.max(1) - pts
                }
                _ => -pts,
            };
            self.source = Some(key);
            self.waiting_keyframe = false;
        } else if video && self.waiting_keyframe {
            if !au.keyframe {
                out.keyframe_needed = true;
                return Ok(out);
            }
            self.waiting_keyframe = false;
        }
        let mut a = au.clone();
        a.timebase = tb;
        a.pts = pts + self.offset;
        a.dts = dts + self.offset;
        let pkts = s.rtp.send(&a, config).map_err(WebRtcError::Rtp)?;
        self.last_pts = Some(self.last_pts.map_or(a.pts, |l| l.max(a.pts)));
        self.last_send = Some(now);
        for p in &pkts {
            self.history.insert(p.clone());
        }
        self.packets_sent += pkts.len() as u64;
        out.packets = pkts;
        Ok(out)
    }

    /// 次に送る AU はキーフレームから（pacer で packet を捨てたなど）。
    pub fn require_keyframe(&mut self) {
        if self.kind == MediaKind::Video {
            self.waiting_keyframe = true;
        }
    }

    /// NACK に応える packet を作る。同じ番号を往復時間内に何度も送らない。
    pub fn retransmit(&mut self, seq: u16, now: Instant, rtt: Duration) -> Option<RtpPacket> {
        if !self.nack_enabled {
            return None;
        }
        let pkt = self.history.get(seq)?.clone();
        if let Some(t) = self.resent.get(&seq)
            && now.saturating_duration_since(*t) < rtt.max(Duration::from_millis(10))
        {
            return None;
        }
        if self.resent.insert(seq, now).is_none() {
            self.resent_order.push_back(seq);
        }
        while self.resent_order.len() > 1024 {
            if let Some(s) = self.resent_order.pop_front() {
                self.resent.remove(&s);
            }
        }
        self.retransmitted += 1;
        let rtx_pt = self
            .codecs
            .iter()
            .find(|c| c.payload_type == pkt.header.payload_type)
            .and_then(|c| c.rtx_payload_type);
        match (self.rtx_ssrc, rtx_pt) {
            (Some(rs), Some(pt)) => {
                let r = rtx_wrap(&pkt, pt, rs, self.rtx_seq);
                self.rtx_seq = self.rtx_seq.wrapping_add(1);
                Some(r)
            }
            _ => Some(pkt),
        }
    }

    /// 帯域の探りに使う padding だけの RTX packet（RTX が無ければ `None`）。
    pub fn padding_packet(&mut self, size: u8) -> Option<RtpPacket> {
        let rs = self.rtx_ssrc?;
        let pt = self.codecs.first()?.rtx_payload_type?;
        let ts = self.ts_base.wrapping_add(self.last_pts? as u32);
        let mut p = RtpPacket::new(pt, self.rtx_seq, ts, rs, false, bytes::Bytes::new());
        p.padding = size.max(1);
        self.rtx_seq = self.rtx_seq.wrapping_add(1);
        Some(p)
    }

    /// 送信者報告（まだ送っていなければ `None`）。
    pub fn sender_report(&self, now: Instant, ntp: u64) -> Option<SenderReport> {
        let s = self.sender.as_ref()?;
        let (lp, ls) = (self.last_pts?, self.last_send?);
        let el = now.saturating_duration_since(ls);
        let rtp_now = self
            .ts_base
            .wrapping_add(lp as u32)
            .wrapping_add(((el.as_nanos() * s.clock as u128) / 1_000_000_000) as u32);
        Some(s.rtp.sender_report(ntp, rtp_now))
    }
}
