//! 受信ストリーム（1 つの SSRC、simulcast なら 1 つの層）。
//!
//! RTP → ジッターバッファ（並べ替え・期限つきの待ち合わせ）→ depacketizer → AU。
//! NACK の生成、損失時のキーフレーム要求、時刻の写像（安定した時間軸と SR による lip sync）を受け持つ。

use std::time::{Duration, Instant};

use senca_core::media::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
use senca_rtp::payload::{self, PayloadFormat};
use senca_rtp::rtcp::{ReportBlock, SenderReport};
use senca_rtp::{JitterBuffer, JitterOutput, RtpPacket, TrackReceiver};
use senca_sdp::NegotiatedCodec;

use crate::config::PeerConfig;
use crate::nack::NackGenerator;

/// 接続全体で共有する時間軸。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionClock {
    /// 時間軸の 0（最初のメディアが届いた時刻）。
    pub start: Option<Instant>,
    /// SR の壁時計（UNIX ns）で時間軸の 0 に当たる時刻。最初に SR で合わせたストリームが決める。
    pub base_ns: Option<i64>,
}

/// 1 つのストリームの時刻の写像。出力の pts = 入力の（広げた）RTP timestamp + delta。
///
/// 最初は届いた時刻で時間軸に置き、SR（送り手の壁時計）が分かったら lip sync のために delta を合わせる。
/// 合わせる量は一度に飛ばさず、AU ごとに前の AU との間隔の 4 分の 1 までずつ動かす（時刻が戻ったり詰まったりしない）。
#[derive(Debug)]
struct Timeline {
    clock: i64,
    delta: Option<i64>,
    /// 合わせたい delta（SR から）。
    target: Option<i64>,
    epoch: u64,
    synced: bool,
    last_raw_dts: Option<i64>,
    last_dts: Option<i64>,
}

impl Timeline {
    fn new(clock: i64) -> Timeline {
        Timeline {
            clock,
            delta: None,
            target: None,
            epoch: 0,
            synced: false,
            last_raw_dts: None,
            last_dts: None,
        }
    }

    fn reset(&mut self) {
        self.delta = None;
        self.target = None;
        self.synced = false;
        self.last_raw_dts = None;
    }

    fn anchor(&mut self, pts: i64, arrival: Instant, sess: &mut SessionClock) {
        let start = *sess.start.get_or_insert(arrival);
        let el = arrival.saturating_duration_since(start);
        let mut out = (el.as_nanos() as i128 * self.clock as i128 / 1_000_000_000) as i64;
        if let Some(l) = self.last_dts {
            out = out.max(l + 1);
        }
        self.delta = Some(out - pts);
    }

    fn map(&mut self, au: &mut AccessUnit, arrival: Instant, sess: &mut SessionClock) {
        if self.delta.is_none() || au.epoch != self.epoch {
            self.anchor(au.pts, arrival, sess);
            self.epoch = au.epoch;
            self.synced = false;
            self.target = None;
            self.last_raw_dts = None;
        }
        let mut delta = self.delta.unwrap_or(0);
        if let Some(cap) = au.capture_time_ns {
            let cur = au.pts + delta;
            let cur_ns = (cur as i128 * 1_000_000_000 / self.clock as i128) as i64;
            let base = *sess.base_ns.get_or_insert(cap - cur_ns);
            let target = ((cap - base) as i128 * self.clock as i128 / 1_000_000_000) as i64;
            // 最初の SR で合わせる。その後は大きくずれたとき（送り手の時計の飛び）だけ合わせ直す。
            if !self.synced || (target - cur).abs() > self.clock / 5 {
                self.target = Some(target - au.pts);
                self.synced = true;
            }
        }
        if let Some(t) = self.target {
            let pending = t - delta;
            let step = match self.last_raw_dts {
                // 間隔の 4 分の 1 まで（時刻の順と間隔の 75 % 以上を保つ）
                Some(l) => {
                    let lim = ((au.dts - l).max(0)) / 4;
                    pending.clamp(-lim, lim)
                }
                None => pending,
            };
            delta += step;
            if delta == t {
                self.target = None;
            }
        }
        self.last_raw_dts = Some(au.dts);
        let mut pts = au.pts + delta;
        let mut dts = au.dts + delta;
        if let Some(l) = self.last_dts
            && dts <= l
        {
            let shift = l + 1 - dts;
            delta += shift;
            pts += shift;
            dts += shift;
        }
        self.delta = Some(delta);
        self.last_dts = Some(dts);
        au.pts = pts;
        au.dts = dts;
    }
}

/// 受信ストリーム。
pub(crate) struct InboundStream {
    pub track: TrackId,
    pub mid: String,
    pub rid: Option<String>,
    pub kind: MediaKind,
    pub ssrc: u32,
    pub rtx_ssrc: Option<u32>,
    pub payload_type: u8,
    pub codec: Codec,
    receiver: TrackReceiver,
    jitter: JitterBuffer,
    latency: Duration,
    pub nack: Option<NackGenerator>,
    /// PLI・FIR を送れる（交渉済み）。
    pub can_request_keyframe: bool,
    pub use_fir: bool,
    awaiting_keyframe: bool,
    /// キーフレームを求める必要がある（PC が PLI を送ったら下ろす）。
    pub keyframe_needed: bool,
    timeline: Timeline,
    pub last_packet: Instant,
    pub malformed: u64,
    pub received_packets: u64,
}

pub(crate) fn payload_format(c: &NegotiatedCodec) -> PayloadFormat {
    PayloadFormat {
        encoding: c.encoding.clone(),
        clock_rate: c.clock_rate,
        channels: c.channels,
        fmtp: c.remote_fmtp.clone(),
    }
}

pub(crate) fn codec_of(c: &NegotiatedCodec) -> Option<Codec> {
    payload_format(c).codec()
}

impl InboundStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: &PeerConfig,
        track: TrackId,
        mid: &str,
        rid: Option<String>,
        ssrc: u32,
        codec: &NegotiatedCodec,
        now: Instant,
    ) -> Result<InboundStream, senca_rtp::Error> {
        let fmt = payload_format(codec);
        let c = fmt.codec().ok_or(senca_rtp::Error::Unsupported("codec"))?;
        let dep = payload::depacketizer(&fmt)?;
        let kind = c.kind();
        let video = kind == MediaKind::Video;
        let nack =
            (video && codec.has_feedback("nack")).then(|| NackGenerator::new(cfg.nack.clone()));
        let pli = codec.has_feedback("nack pli");
        let fir = codec.has_feedback("ccm fir");
        // 音声は待たない（遅れたら捨てる）。映像は再生期限まで待つ。
        let latency = if video {
            cfg.jitter_latency
        } else {
            cfg.jitter_latency.min(Duration::from_millis(60))
        };
        Ok(InboundStream {
            track,
            mid: mid.to_string(),
            rid,
            kind,
            ssrc,
            rtx_ssrc: None,
            payload_type: codec.payload_type,
            codec: c,
            receiver: TrackReceiver::new(track, dep),
            jitter: JitterBuffer::new(latency, cfg.jitter_capacity),
            latency,
            nack,
            can_request_keyframe: video && (pli || fir),
            use_fir: !pli && fir,
            awaiting_keyframe: video,
            keyframe_needed: false,
            timeline: Timeline::new(codec.clock_rate.max(1) as i64),
            last_packet: now,
            malformed: 0,
            received_packets: 0,
        })
    }

    /// payload type（コーデック）が変わったので depacketizer を作り直す。時間軸は保つ。
    pub fn change_codec(&mut self, codec: &NegotiatedCodec) -> Result<(), senca_rtp::Error> {
        let fmt = payload_format(codec);
        let c = fmt.codec().ok_or(senca_rtp::Error::Unsupported("codec"))?;
        self.receiver = TrackReceiver::new(self.track, payload::depacketizer(&fmt)?);
        self.payload_type = codec.payload_type;
        self.codec = c;
        self.awaiting_keyframe = self.kind == MediaKind::Video;
        // 新しい受信器は時刻の拡張をやり直すので、写像も作り直す（出力の時刻は戻さない）
        self.timeline.reset();
        Ok(())
    }

    /// RTP（RTX から戻したものを含む）を入れる。再送で埋まったときは往復時間の見本を返す。
    pub fn push(&mut self, now: Instant, pkt: RtpPacket, from_rtx: bool) -> Option<Duration> {
        self.last_packet = now;
        self.received_packets += 1;
        if !from_rtx {
            self.receiver.observe(&pkt, now);
        }
        let seq = pkt.header.sequence;
        let rtt = self
            .nack
            .as_mut()
            .and_then(|n| n.on_packet(seq, now, now + self.latency));
        let _ = self.jitter.push(pkt, now);
        if self.nack.as_mut().is_some_and(|n| n.take_keyframe_needed()) {
            self.keyframe_needed = true;
        }
        rtt
    }

    /// 出せる AU を取り出す。
    pub fn drain(
        &mut self,
        now: Instant,
        sess: &mut SessionClock,
        out: &mut Vec<(AccessUnit, Option<CodecConfig>)>,
    ) {
        let video = self.kind == MediaKind::Video;
        while let Some(o) = self.jitter.pop(now) {
            match o {
                JitterOutput::Packet(p) => {
                    if self.receiver.push(&p).is_err() {
                        self.malformed += 1;
                    }
                }
                JitterOutput::Lost { .. } => {
                    self.receiver.mark_loss();
                    if video {
                        self.keyframe_needed = true;
                    }
                }
                JitterOutput::Reset => self.receiver.reset(),
            }
        }
        while let Some((mut au, cfg)) = self.receiver.pop() {
            if video && self.awaiting_keyframe {
                if !au.keyframe {
                    self.keyframe_needed = true;
                    continue;
                }
                self.awaiting_keyframe = false;
            }
            self.timeline.map(&mut au, now, sess);
            out.push((au, cfg));
        }
    }

    pub fn on_sender_report(&mut self, sr: &SenderReport, now: Instant) {
        self.receiver.on_sender_report(sr, now);
    }

    pub fn report_block(&mut self, now: Instant) -> ReportBlock {
        self.receiver.stats_mut().report_block(self.ssrc, now)
    }

    /// 次に [`drain`](Self::drain) を呼ぶ時刻（抜けを待っているとき）。
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.jitter.poll_timeout()
    }

    #[cfg(test)]
    pub fn debug(&self) -> String {
        format!(
            "jitter {:?} nack {:?} malformed {} recv {}",
            self.jitter.counters(),
            self.nack.as_ref().map(|n| n.counters()),
            self.malformed,
            self.received_packets
        )
    }

    pub fn nack_timeout(&self, rtt: Duration) -> Option<Instant> {
        self.nack.as_ref().and_then(|n| n.poll_timeout(rtt))
    }
}
