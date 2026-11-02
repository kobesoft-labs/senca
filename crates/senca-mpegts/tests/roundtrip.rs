//! FFmpeg が作った TS を分離し、多重化し直してもう一度分離すると同じ AU になることを確かめる。
//! あわせて、壊れた入力でパニックしないこと・欠落を検出することを確かめる。

use bytes::Bytes;
use senca_core::time::Timebase;
use senca_core::{AccessUnit, Codec, CodecConfig, TrackId};
use senca_mpegts::{DemuxEvent, DemuxerConfig, MuxerConfig, TsDemuxer, TsMuxer};

const H264_AAC: &[u8] = include_bytes!("data/h264_aac_small.ts");
const H265_OPUS: &[u8] = include_bytes!("data/h265_opus_small.ts");

#[derive(Default, Debug)]
struct Out {
    configs: Vec<(TrackId, CodecConfig)>,
    aus: Vec<AccessUnit>,
    pcrs: usize,
    added: Vec<(TrackId, u16, Codec)>,
    events: Vec<DemuxEvent>,
}

fn demux_with(data: &[u8], chunk: usize, cfg: DemuxerConfig) -> (Out, TsDemuxer) {
    let mut d = TsDemuxer::new(cfg);
    let mut o = Out::default();
    for c in data.chunks(chunk.max(1)) {
        d.push(c);
        drain(&mut d, &mut o);
    }
    d.flush();
    drain(&mut d, &mut o);
    (o, d)
}

fn demux(data: &[u8], chunk: usize) -> Out {
    demux_with(data, chunk, DemuxerConfig::default()).0
}

fn drain(d: &mut TsDemuxer, o: &mut Out) {
    while let Some(ev) = d.poll() {
        o.events.push(ev.clone());
        match ev {
            DemuxEvent::Config { track, config } => o.configs.push((track, config)),
            DemuxEvent::AccessUnit(au) => o.aus.push(au),
            DemuxEvent::Pcr { .. } => o.pcrs += 1,
            DemuxEvent::StreamAdded { track, pid, codec } => o.added.push((track, pid, codec)),
        }
    }
}

fn mux(o: &Out, cfg: MuxerConfig) -> Vec<u8> {
    let mut m = TsMuxer::new(cfg);
    let mut out = Vec::new();
    // 分離したときの順（設定は AU より先）で入れ直す
    for ev in &o.events {
        match ev {
            DemuxEvent::Config { track, config } => {
                if m.pid(*track).is_some() {
                    m.update_config(*track, config).unwrap();
                } else {
                    m.add_track(*track, config).unwrap();
                }
            }
            DemuxEvent::AccessUnit(au) => m.push(au, &mut out).unwrap(),
            _ => {}
        }
    }
    out
}

/// トラックごとに同じ AU 列か（トラック間の並びは PES の組み方で変わる）。
fn assert_same_aus(a: &[AccessUnit], b: &[AccessUnit]) {
    assert_eq!(a.len(), b.len());
    let mut tracks: Vec<TrackId> = a.iter().map(|x| x.track).collect();
    tracks.sort();
    tracks.dedup();
    for t in tracks {
        let xs: Vec<_> = a.iter().filter(|x| x.track == t).collect();
        let ys: Vec<_> = b.iter().filter(|x| x.track == t).collect();
        assert_eq!(xs.len(), ys.len());
        same_track(&xs, &ys);
    }
}

fn same_track(a: &[&AccessUnit], b: &[&AccessUnit]) {
    for (x, y) in a.iter().zip(b) {
        assert_eq!((x.track, x.codec, x.pts, x.dts, x.keyframe), (y.track, y.codec, y.pts, y.dts, y.keyframe));
        assert_eq!(x.payload, y.payload, "payload of {:?} pts {}", x.track, x.pts);
        assert_eq!(x.timebase, Timebase::MPEG);
    }
}

#[test]
fn h264_aac_fixture() {
    let o = demux(H264_AAC, 188 * 7);
    assert_eq!(o.added.iter().map(|a| a.2).collect::<Vec<_>>(), vec![Codec::H264, Codec::Aac]);
    let video: Vec<_> = o.aus.iter().filter(|a| a.codec == Codec::H264).collect();
    let audio: Vec<_> = o.aus.iter().filter(|a| a.codec == Codec::Aac).collect();
    assert_eq!(video.len(), 20);
    assert!(audio.len() >= 37, "{}", audio.len());
    assert!(video[0].keyframe && video[10].keyframe && !video[1].keyframe);
    // B フレームがあるので DTS と PTS が違う AU がある
    assert!(video.iter().any(|a| a.pts != a.dts));
    // 音声の PTS は 1024 サンプル（48 kHz → 1920）ずつ
    for w in audio.windows(2) {
        assert_eq!(w[1].pts - w[0].pts, 1920);
    }
    let vc = &o.configs.iter().find(|c| c.1.codec == Codec::H264).unwrap().1;
    let v = vc.video.unwrap();
    assert_eq!((v.width, v.height), (64, 48));
    let ac = &o.configs.iter().find(|c| c.1.codec == Codec::Aac).unwrap().1;
    assert_eq!(&ac.record[..], &[0x11, 0x88]); // LC, 48 kHz, mono
    // 長さ前置で、AUD・SPS・PPS は入っていない
    for a in &video {
        let mut p = 0;
        while p < a.payload.len() {
            let n = u32::from_be_bytes(a.payload[p..p + 4].try_into().unwrap()) as usize;
            let t = a.payload[p + 4] & 0x1F;
            assert!(!(7..=9).contains(&t));
            p += 4 + n;
        }
        assert_eq!(p, a.payload.len());
    }
    assert!(o.pcrs > 0);

    let ts = mux(&o, MuxerConfig::default());
    assert_eq!(ts.len() % 188, 0);
    let o2 = demux(&ts, 1000);
    assert_same_aus(&o.aus, &o2.aus);
    assert_eq!(o.configs[0].1.record, o2.configs[0].1.record);
}

#[test]
fn h265_opus_fixture() {
    let o = demux(H265_OPUS, 4096);
    let video: Vec<_> = o.aus.iter().filter(|a| a.codec == Codec::H265).collect();
    let audio: Vec<_> = o.aus.iter().filter(|a| a.codec == Codec::Opus).collect();
    assert_eq!(video.len(), 20);
    assert!(audio.len() >= 39, "{}", audio.len());
    assert!(video[0].keyframe);
    let vc = &o.configs.iter().find(|c| c.1.codec == Codec::H265).unwrap().1;
    assert_eq!((vc.video.unwrap().width, vc.video.unwrap().height), (64, 48));
    let ac = &o.configs.iter().find(|c| c.1.codec == Codec::Opus).unwrap().1;
    assert_eq!(&ac.record[..8], b"OpusHead");
    assert_eq!(ac.record[9], 2);
    // Opus 20 ms = 1800（90 kHz）
    assert_eq!(audio[1].pts - audio[0].pts, 1800);

    let ts = mux(&o, MuxerConfig::default());
    let o2 = demux(&ts, 188);
    assert_same_aus(&o.aus, &o2.aus);
}

#[test]
fn chunking_does_not_matter() {
    let a = demux(H264_AAC, 1);
    let b = demux(H264_AAC, 100_000);
    assert_same_aus(&a.aus, &b.aus);
}

#[test]
fn segments_and_cc_reset() {
    let o = demux(H264_AAC, 188 * 10);
    let cfg = MuxerConfig { reset_cc_on_segment: true, tables_interval: None, ..MuxerConfig::default() };
    let mut m = TsMuxer::new(cfg);
    for (t, c) in o.configs.iter().filter(|(_, c)| c.generation == 0) {
        m.add_track(*t, c).unwrap();
    }
    let mut segs: Vec<Vec<u8>> = Vec::new();
    for au in &o.aus {
        if au.codec == Codec::H264 && au.keyframe {
            segs.push(Vec::new());
            m.start_segment(segs.last_mut().unwrap());
        }
        if let Some(s) = segs.last_mut() {
            m.push(au, s).unwrap();
        }
    }
    assert_eq!(segs.len(), 2);
    // 各セグメントは PAT で始まり、単独で分離でき、CC は 0 から
    for s in &segs {
        assert_eq!(&s[..4], &[0x47, 0x40, 0x00, 0x10]);
        let (o2, d) = demux_with(s, 188, DemuxerConfig::default());
        assert!(o2.aus.iter().any(|a| a.codec == Codec::H264 && a.keyframe));
        assert_eq!(d.stats().cc_errors, 0);
    }
    // 連結すると CC が飛ぶ（リセットしたので）
    let all: Vec<u8> = segs.concat();
    let (_, d) = demux_with(&all, 188, DemuxerConfig::default());
    assert!(d.stats().cc_errors > 0);
}

#[test]
fn packet_loss_marks_discontinuity() {
    // 映像の PID の途中のパケットを 1 つ抜く
    let mut pkts: Vec<&[u8]> = H264_AAC.chunks(188).collect();
    let o = demux(H264_AAC, 188);
    let vpid = o.added.iter().find(|a| a.2 == Codec::H264).unwrap().1;
    let idx = pkts
        .iter()
        .enumerate()
        .filter(|(_, p)| ((p[1] as u16 & 0x1F) << 8 | p[2] as u16) == vpid && p[1] & 0x40 == 0)
        .find(|(i, _)| *i > pkts.len() / 3)
        .unwrap()
        .0;
    pkts.remove(idx);
    let data: Vec<u8> = pkts.concat();
    let (o2, d) = demux_with(&data, 188, DemuxerConfig::default());
    assert_eq!(d.stats().cc_errors, 1);
    let video: Vec<_> = o2.aus.iter().filter(|a| a.codec == Codec::H264).collect();
    assert_eq!(video.len(), 19);
    assert_eq!(video.iter().filter(|a| a.discontinuity).count(), 1);
}

#[test]
fn resync_after_garbage() {
    let mut data = vec![0x47, 1, 2, 3, 0x47, 0, 0];
    data.extend_from_slice(&[0xAB; 500]);
    data.extend_from_slice(H264_AAC);
    let (o, d) = demux_with(&data, 333, DemuxerConfig::default());
    assert_eq!(o.aus.iter().filter(|a| a.codec == Codec::H264).count(), 20);
    assert_eq!(d.stats().sync_losses, 0);
    // 途中に 1 バイト挟む → 同期を失って取り戻す
    let mut data = H264_AAC.to_vec();
    data.insert(188 * 30 + 50, 0x00);
    let (o, d) = demux_with(&data, 1000, DemuxerConfig::default());
    assert_eq!(d.stats().sync_losses, 1);
    assert!(o.aus.iter().filter(|a| a.codec == Codec::H264).count() >= 18);
}

#[test]
fn pes_size_limit() {
    let cfg = DemuxerConfig { max_pes_size: 200, ..DemuxerConfig::default() };
    let (o, d) = demux_with(H264_AAC, 188, cfg);
    assert!(d.stats().pes_oversize > 0);
    assert!(o.aus.iter().all(|a| a.payload.len() <= 200));
}

#[test]
fn bad_crc_is_rejected() {
    // PAT の CRC を壊すと番組が見つからない
    let mut data = H264_AAC.to_vec();
    for p in data.chunks_mut(188) {
        let pid = (p[1] as u16 & 0x1F) << 8 | p[2] as u16;
        if pid == 0 {
            p[10] ^= 0xFF;
        }
    }
    let (o, d) = demux_with(&data, 188, DemuxerConfig::default());
    assert!(o.aus.is_empty());
    assert!(d.stats().crc_errors > 0);
}

/// 簡単な擬似乱数（再現できるように固定の種）。
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

#[test]
fn corrupted_input_does_not_panic() {
    let mut rng = Lcg(42);
    for fixture in [H264_AAC, H265_OPUS] {
        for round in 0..200 {
            let mut data = fixture.to_vec();
            let n = 1 + rng.next() as usize % 64;
            for _ in 0..n {
                let i = rng.next() as usize % data.len();
                data[i] = rng.next() as u8;
            }
            if round % 3 == 0 {
                let cut = rng.next() as usize % data.len();
                data.truncate(cut);
            }
            let (o, _) = demux_with(&data, 1 + rng.next() as usize % 2000, DemuxerConfig::default());
            // 分離できた AU は多重化し直せる（壊れた中身でもパニックしない）
            let mut m = TsMuxer::new(MuxerConfig::default());
            let mut out = Vec::new();
            for (t, c) in &o.configs {
                if m.pid(*t).is_none() {
                    let _ = m.add_track(*t, c);
                } else {
                    let _ = m.update_config(*t, c);
                }
            }
            for au in &o.aus {
                let _ = m.push(au, &mut out);
            }
        }
    }
}

#[test]
fn opus_trim_and_multi_packet() {
    // pre_skip 312 の OpusHead → 最初のパケットに start_trim が付く
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
    let cfg = CodecConfig { codec: Codec::Opus, generation: 0, record: Bytes::from(head), video: None, audio: None };
    let mut m = TsMuxer::new(MuxerConfig::default());
    m.add_track(TrackId(0), &cfg).unwrap();
    let mut out = Vec::new();
    let mut sent = Vec::new();
    for i in 0..10u8 {
        // 20 ms の CELT パケット、300 バイト（au_size が 0xFF を跨ぐ）
        let mut p = vec![0xFC];
        p.extend(std::iter::repeat_n(i, 299));
        let au = AccessUnit {
            track: TrackId(0),
            codec: Codec::Opus,
            epoch: 0,
            sequence: i as u64,
            config_generation: 0,
            pts: i as i64 * 960,
            dts: i as i64 * 960,
            duration: 960,
            timebase: Timebase::new(1, 48000),
            keyframe: true,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(p),
        };
        m.push(&au, &mut out).unwrap();
        sent.push(au);
    }
    // 最初の PES に start_trim フラグ（0x7F 0xF0）
    let o = demux(&out, 188);
    assert_eq!(o.aus.len(), 10);
    for (a, s) in o.aus.iter().zip(&sent) {
        assert_eq!(a.payload, s.payload);
        assert_eq!(a.pts, s.pts * 90000 / 48000);
        assert_eq!(a.duration, 1800);
    }
    let first = out.chunks(188).find(|p| (p[1] as u16 & 0x1F) << 8 | p[2] as u16 == 0x100).unwrap();
    let pos = first.windows(2).position(|w| w == [0x7F, 0xF0]);
    assert!(pos.is_some());
}

#[test]
fn rejects_wrong_payload_format() {
    let o = demux(H264_AAC, 188 * 50);
    let mut m = TsMuxer::new(MuxerConfig::default());
    for (t, c) in o.configs.iter().filter(|(_, c)| c.generation == 0) {
        m.add_track(*t, c).unwrap();
    }
    let mut au = o.aus.iter().find(|a| a.codec == Codec::H264).unwrap().clone();
    // Annex B のまま渡すと長さが合わない
    au.payload = Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x10]);
    let mut out = Vec::new();
    assert!(m.push(&au, &mut out).is_err());
    assert!(out.is_empty());
}
