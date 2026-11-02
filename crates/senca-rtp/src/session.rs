//! 1 本のトラックの送信（[`RtpSender`]: AU → RTP パケット）と受信（[`TrackReceiver`]: RTP パケット → AU）。
//!
//! どちらも sans-io。受信側はジッターバッファの後ろで使う（順番どおりに入れる）。

use crate::clock::SenderClock;
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use crate::payload::{Depacketizer, Packetizer, Payload};
use crate::rtcp::SenderReport;
use crate::stats::{ReceiverStats, SeqUpdate};
use senca_core::media::{AccessUnit, Codec, CodecConfig, TrackId};
use senca_core::time::{Rescale, RolloverExtender, Timebase};
use std::time::Instant;

/// AU を RTP パケットにする送信側。
pub struct RtpSender {
    packetizer: Box<dyn Packetizer>,
    payload_type: u8,
    ssrc: u32,
    clock_rate: u32,
    sequence: u16,
    timestamp_offset: u32,
    packets: u32,
    octets: u32,
    last_timestamp: Option<u32>,
    scratch: Vec<Payload>,
}

impl RtpSender {
    /// `initial_sequence`・`timestamp_offset` は推測されにくい値（乱数）を呼び出し側で選ぶ（RFC 3550 5.1）。
    pub fn new(
        packetizer: Box<dyn Packetizer>,
        payload_type: u8,
        ssrc: u32,
        initial_sequence: u16,
        timestamp_offset: u32,
    ) -> Self {
        let clock_rate = packetizer.clock_rate().max(1);
        RtpSender {
            packetizer,
            payload_type: payload_type & 0x7f,
            ssrc,
            clock_rate,
            sequence: initial_sequence,
            timestamp_offset,
            packets: 0,
            octets: 0,
            last_timestamp: None,
            scratch: Vec::new(),
        }
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    /// 次に使う番号（RTSP の RTP-Info の seq）。
    pub fn next_sequence(&self) -> u16 {
        self.sequence
    }

    /// 時刻（`pts` は `timebase`）に対応する RTP timestamp。
    pub fn rtp_timestamp(&self, pts: i64, timebase: Timebase) -> Option<u32> {
        let t = pts.rescale(timebase, Timebase::new(1, self.clock_rate))?;
        Some(self.timestamp_offset.wrapping_add(t as u32))
    }

    /// AU を RTP パケットにする。
    pub fn send(
        &mut self,
        au: &AccessUnit,
        config: Option<&CodecConfig>,
    ) -> Result<Vec<RtpPacket>> {
        let ts = self
            .rtp_timestamp(au.pts, au.timebase)
            .ok_or(Error::Invalid("PTS を RTP timestamp に換算できない"))?;
        self.scratch.clear();
        self.packetizer.packetize(au, config, &mut self.scratch)?;
        let mut out = Vec::with_capacity(self.scratch.len());
        for p in self.scratch.drain(..) {
            let pkt = RtpPacket::new(
                self.payload_type,
                self.sequence,
                ts.wrapping_add(p.timestamp_offset),
                self.ssrc,
                p.marker,
                p.data,
            );
            self.sequence = self.sequence.wrapping_add(1);
            self.packets = self.packets.wrapping_add(1);
            self.octets = self.octets.wrapping_add(pkt.payload.len() as u32);
            out.push(pkt);
        }
        self.last_timestamp = Some(ts);
        Ok(out)
    }

    /// 送信者報告。`ntp` は今の壁時計、`unix_ns` と同じ瞬間の RTP timestamp は `rtp_now` で渡す。
    pub fn sender_report(&self, ntp: u64, rtp_now: u32) -> SenderReport {
        SenderReport {
            ssrc: self.ssrc,
            ntp,
            rtp_timestamp: rtp_now,
            packet_count: self.packets,
            octet_count: self.octets,
            reports: Vec::new(),
            extension: bytes::Bytes::new(),
        }
    }
}

/// DTS の見積もり（B フレームのある H.264・H.265 で、RTP に DTS が無いため）。
///
/// 並べ替えのある流れでは、フレーム間隔が一定とみなして `DTS = 最初の PTS − 並べ替えの深さ × 間隔 + n × 間隔` とする。
/// 間隔は SPS の timing から、無ければ観測した PTS の差の最小値から取る。DTS は PTS を超えないよう、単調に増えるよう直す。
/// 並べ替えのない流れ（SPS が並べ替えなしを示す・VP8/VP9/AV1・音声）は DTS = PTS。
#[derive(Debug, Clone, Default)]
struct DtsEstimator {
    reorder: u32,
    fixed_interval: Option<i64>,
    observed: Option<i64>,
    default_interval: i64,
    prev_pts: Option<i64>,
    base: Option<i64>,
    count: i64,
    last_dts: Option<i64>,
}

impl DtsEstimator {
    fn configure(&mut self, cfg: &CodecConfig, clock_rate: u32) {
        let (reorder, fps) = match cfg.codec {
            Codec::H264 => senca_h264::AvcDecoderConfigurationRecord::parse(&cfg.record)
                .and_then(|r| r.parse_sps())
                .map(|s| {
                    let n = if s.may_reorder() {
                        s.max_num_reorder_frames().unwrap_or(2)
                    } else {
                        0
                    };
                    (n, s.frame_rate())
                })
                .unwrap_or((0, None)),
            Codec::H265 => senca_h265::HevcDecoderConfigurationRecord::parse(&cfg.record)
                .and_then(|r| r.parse_sps())
                .map(|s| (s.max_num_reorder_pics, s.frame_rate()))
                .unwrap_or((0, None)),
            _ => (0, None),
        };
        self.reorder = reorder.min(16);
        self.fixed_interval = match fps {
            Some((n, d)) if n > 0 => Some((clock_rate as i64 * d as i64 / n as i64).max(1)),
            _ => None,
        };
        self.default_interval = (clock_rate as i64 / 30).max(1);
    }

    fn reset(&mut self) {
        self.prev_pts = None;
        self.base = None;
        self.count = 0;
        self.last_dts = None;
    }

    fn estimate(&mut self, pts: i64) -> i64 {
        if self.reorder == 0 {
            self.last_dts = Some(pts);
            return pts;
        }
        if let Some(p) = self.prev_pts {
            let d = (pts - p).abs();
            if d > 0 {
                self.observed = Some(self.observed.map_or(d, |i| i.min(d)));
            }
        }
        self.prev_pts = Some(pts);
        let iv = self
            .fixed_interval
            .or(self.observed)
            .unwrap_or(self.default_interval);
        let base = *self.base.get_or_insert(pts - self.reorder as i64 * iv);
        let mut dts = base + self.count * iv;
        self.count += 1;
        if dts > pts {
            // 間隔の見積もりがずれた: 基準を下げる
            self.base = Some(base - (dts - pts));
            dts = pts;
        }
        if let Some(l) = self.last_dts
            && dts <= l
        {
            dts = l + 1;
        }
        self.last_dts = Some(dts);
        dts
    }
}

/// RTP パケットを AU にする受信側（1 トラック・1 SSRC）。
pub struct TrackReceiver {
    track: TrackId,
    depacketizer: Box<dyn Depacketizer>,
    timestamps: RolloverExtender,
    timebase: Timebase,
    epoch: u64,
    sequence: u64,
    stats: ReceiverStats,
    clock: Option<SenderClock>,
    dts: DtsEstimator,
    config_epoch_bump: bool,
}

impl TrackReceiver {
    pub fn new(track: TrackId, depacketizer: Box<dyn Depacketizer>) -> Self {
        let rate = depacketizer.clock_rate().max(1);
        let mut dts = DtsEstimator {
            default_interval: rate as i64 / 30,
            ..Default::default()
        };
        if let Some(c) = depacketizer.config() {
            dts.configure(c, rate);
        }
        TrackReceiver {
            track,
            depacketizer,
            timestamps: RolloverExtender::new(32),
            timebase: Timebase::new(1, rate),
            epoch: 0,
            sequence: 0,
            stats: ReceiverStats::new(rate),
            clock: None,
            dts,
            config_epoch_bump: false,
        }
    }

    pub fn codec(&self) -> Codec {
        self.depacketizer.codec()
    }

    /// AU の時間軸（1/clock_rate）。
    pub fn timebase(&self) -> Timebase {
        self.timebase
    }

    pub fn config(&self) -> Option<&CodecConfig> {
        self.depacketizer.config()
    }

    pub fn stats(&self) -> &ReceiverStats {
        &self.stats
    }

    pub fn stats_mut(&mut self) -> &mut ReceiverStats {
        &mut self.stats
    }

    /// 統計だけを更新する（ジッターバッファに入れる前、到着した順に呼ぶ）。
    pub fn observe(&mut self, pkt: &RtpPacket, arrival: Instant) -> SeqUpdate {
        self.stats
            .on_packet(pkt.header.sequence, pkt.header.timestamp, arrival)
    }

    /// 順番どおりのパケットを入れる（ジッターバッファの出力）。
    pub fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        self.depacketizer.push(pkt)
    }

    /// ジッターバッファが損失を報告した。
    pub fn mark_loss(&mut self) {
        self.depacketizer.mark_loss();
    }

    /// 送信元が再始動した（番号の大きな飛び）。時刻の拡張をやり直し、epoch を増やす。
    pub fn reset(&mut self) {
        self.depacketizer.mark_loss();
        self.timestamps.reset();
        self.dts.reset();
        self.config_epoch_bump = true;
    }

    /// SR を受け取った（撮影時刻の推定と DLSR に使う）。
    pub fn on_sender_report(&mut self, sr: &SenderReport, now: Instant) {
        self.stats.on_sender_report(sr.ntp, now);
        match &mut self.clock {
            Some(c) => c.update(sr.ntp, sr.rtp_timestamp),
            None => {
                self.clock = Some(SenderClock::new(
                    self.timebase.den,
                    sr.ntp,
                    sr.rtp_timestamp,
                ))
            }
        }
    }

    /// 出来上がった AU と、変わったときは新しい設定を取り出す。
    pub fn pop(&mut self) -> Option<(AccessUnit, Option<CodecConfig>)> {
        let f = self.depacketizer.pop()?;
        if self.config_epoch_bump {
            self.config_epoch_bump = false;
            self.epoch += 1;
        }
        if let Some(c) = &f.config {
            self.dts.configure(c, self.timebase.den);
        }
        let pts = self.timestamps.extend(f.timestamp as u64);
        let dts = self.dts.estimate(pts);
        let capture_time_ns = self.clock.map(|c| c.rtp_to_unix_ns(f.timestamp));
        let au = AccessUnit {
            track: self.track,
            codec: self.depacketizer.codec(),
            epoch: self.epoch,
            sequence: self.sequence,
            config_generation: f.config_generation,
            pts,
            dts,
            duration: f.duration as u64,
            timebase: self.timebase,
            keyframe: f.keyframe,
            discontinuity: f.discontinuity,
            capture_time_ns,
            payload: f.payload,
        };
        self.sequence += 1;
        Some((au, f.config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{self, PayloadFormat};
    use crate::rtcp::SenderReport;
    use bytes::Bytes;

    #[test]
    fn opus_sender_receiver() {
        let (pz, fmt) = payload::packetizer(Codec::Opus, None, 1200).unwrap();
        let mut tx = RtpSender::new(pz, 111, 0x1234, 65535, 0xFFFF_FF00);
        let mut rx = TrackReceiver::new(TrackId(1), payload::depacketizer(&fmt).unwrap());
        let t0 = Instant::now();
        for i in 0..10i64 {
            let au = AccessUnit {
                track: TrackId(0),
                codec: Codec::Opus,
                epoch: 0,
                sequence: 0,
                config_generation: 0,
                pts: i * 20,
                dts: i * 20,
                duration: 20,
                timebase: Timebase::MILLIS,
                keyframe: true,
                discontinuity: false,
                capture_time_ns: None,
                payload: Bytes::from_static(&[0xFC, 1, 2]),
            };
            for p in tx.send(&au, None).unwrap() {
                let p = RtpPacket::parse(p.to_bytes().unwrap()).unwrap();
                rx.observe(&p, t0);
                rx.push(&p).unwrap();
            }
        }
        rx.on_sender_report(
            &SenderReport {
                ssrc: 0x1234,
                ntp: crate::clock::unix_ns_to_ntp(1_000_000_000),
                rtp_timestamp: 0xFFFF_FF00,
                ..Default::default()
            },
            t0,
        );
        let mut aus = Vec::new();
        while let Some((au, _)) = rx.pop() {
            aus.push(au);
        }
        assert_eq!(aus.len(), 10);
        // 折り返しをまたいでも単調
        assert_eq!(aus[9].pts - aus[0].pts, 9 * 960);
        assert_eq!(aus[1].dts, aus[1].pts);
        assert_eq!(aus[3].sequence, 3);
        assert_eq!(aus[0].timebase, Timebase::new(1, 48000));
        assert!(aus[9].capture_time_ns.is_some());
        let _ = PayloadFormat::codec(&fmt);
    }

    #[test]
    fn dts_estimator_monotonic() {
        let mut e = DtsEstimator {
            reorder: 2,
            fixed_interval: Some(3000),
            default_interval: 3000,
            ..Default::default()
        };
        // I P B B の表示時刻（デコード順）
        let pts = [0i64, 9000, 3000, 6000, 18000, 12000, 15000];
        let dts: Vec<i64> = pts.iter().map(|p| e.estimate(*p)).collect();
        for w in dts.windows(2) {
            assert!(w[1] > w[0]);
        }
        assert_eq!(dts, vec![-6000, -3000, 0, 3000, 6000, 9000, 12000]);
        for (d, p) in dts.iter().zip(pts.iter()) {
            assert!(d <= p, "{dts:?}");
        }
    }
}
