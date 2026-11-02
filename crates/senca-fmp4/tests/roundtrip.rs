//! 書いたものを読み戻して同じサンプルになること、FFmpeg が書いた fMP4 を読めること、
//! 壊れた入力でパニックしないことを確かめる。

use bytes::Bytes;
use senca_core::media::{AudioParams, VideoParams};
use senca_core::time::Timebase;
use senca_core::{AccessUnit, Codec, CodecConfig, TrackId};
use senca_fmp4::{
    FragmentWriter, Mp4Error, Mp4Writer, ReadSample, Sample, SampleQueue, TrackRun, TrackSpec, read_file,
    write_init_segment,
};

const AV1_OPUS: &[u8] = include_bytes!("data/av1_opus_small.mp4");
const H264_AAC: &[u8] = include_bytes!("data/h264_aac_frag_small.mp4");

fn h264_config() -> CodecConfig {
    // x264 の 320x240 High の SPS と PPS
    let sps: &[u8] = &[
        0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03,
        0x03, 0xc0, 0xf1, 0x42, 0x99, 0x60,
    ];
    let pps: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];
    let mut rec = vec![1, 0x64, 0, 0x0d, 0xFF, 0xE1, 0, sps.len() as u8];
    rec.extend_from_slice(sps);
    rec.extend_from_slice(&[1, 0, pps.len() as u8]);
    rec.extend_from_slice(pps);
    CodecConfig {
        codec: Codec::H264,
        generation: 0,
        record: Bytes::from(rec),
        video: Some(VideoParams { width: 320, height: 240, profile: 100, compat: 0, level: 13 }),
        audio: None,
    }
}

fn aac_config() -> CodecConfig {
    CodecConfig {
        codec: Codec::Aac,
        generation: 0,
        record: Bytes::from_static(&[0x11, 0x90]),
        video: None,
        audio: Some(AudioParams { sample_rate: 48000, channels: 2, object_type: 2 }),
    }
}

fn sample(dts: i64, duration: u32, cts: i32, key: bool, fill: u8, len: usize) -> Sample {
    Sample { dts, duration, cts_offset: cts, keyframe: key, data: Bytes::from(vec![fill; len]) }
}

fn as_read(track_id: u32, s: &Sample) -> ReadSample {
    ReadSample {
        track_id,
        dts: s.dts,
        duration: s.duration,
        cts_offset: s.cts_offset,
        keyframe: s.keyframe,
        data: s.data.clone(),
    }
}

#[test]
fn fragmented_roundtrip() {
    let specs = vec![TrackSpec::new(1, h264_config()), TrackSpec::new(2, aac_config())];
    assert_eq!((specs[0].timescale, specs[1].timescale), (90000, 48000));
    let mut file = Vec::new();
    write_init_segment(&specs, &mut file).unwrap();
    let mut fw = FragmentWriter::new();
    let mut expect = Vec::new();
    for f in 0..3i64 {
        // B フレーム風: 負の composition offset を含む
        let v: Vec<Sample> = (0..5)
            .map(|i| {
                let n = f * 5 + i;
                sample(n * 3000, 3000, if i == 0 { 6000 } else { -3000 + (i as i32 % 2) * 9000 }, i == 0, n as u8, 100 + i as usize)
            })
            .collect();
        let a: Vec<Sample> = (0..7).map(|i| sample((f * 7 + i) * 1024, 1024, 0, true, 0xA0 + i as u8, 50)).collect();
        fw.write(&[TrackRun { track_id: 1, samples: &v }, TrackRun { track_id: 2, samples: &a }], &mut file).unwrap();
        expect.extend(v.iter().map(|s| as_read(1, s)));
        expect.extend(a.iter().map(|s| as_read(2, s)));
    }
    assert_eq!(fw.sequence(), 4);
    let (init, got) = read_file(&Bytes::from(file)).unwrap();
    assert!(init.fragmented);
    assert_eq!(init.tracks.len(), 2);
    assert_eq!(init.tracks[0].config.as_ref().unwrap(), &specs[0].config);
    assert_eq!(init.tracks[1].config.as_ref().unwrap().record, specs[1].config.record);
    assert_eq!(&init.tracks[0].handler, b"vide");
    assert_eq!(got, expect);
}

#[test]
fn progressive_roundtrip_and_faststart() {
    let specs = vec![TrackSpec::new(1, h264_config()), TrackSpec::new(2, aac_config())];
    let mut w = Mp4Writer::new(specs).unwrap();
    let mut order = Vec::new();
    for i in 0..30i64 {
        let v = sample(i * 3600, 3600, if i % 3 == 0 { 7200 } else { 3600 }, i % 10 == 0, i as u8, 200 + i as usize);
        w.add(1, &v).unwrap();
        order.push((1, v));
        // 音声は 2 サンプルずつのチャンク
        for k in 0..2 {
            let n = i * 2 + k;
            let a = sample(n * 1920 / 2, 960, 0, true, 0x80 | (n as u8 & 0x7F), 60);
            w.add(2, &a).unwrap();
            order.push((2, a));
        }
    }
    let mut file = w.finish().unwrap();
    // moov が mdat より前
    let moov = file.windows(4).position(|x| x == b"moov").unwrap();
    let mdat = file.windows(4).position(|x| x == b"mdat").unwrap();
    assert!(moov < mdat);
    for (_, s) in &order {
        file.extend_from_slice(&s.data);
    }
    let (init, got) = read_file(&Bytes::from(file)).unwrap();
    assert!(!init.fragmented);
    for tid in [1u32, 2] {
        let want: Vec<_> = order.iter().filter(|(t, _)| *t == tid).map(|(_, s)| s).collect();
        let have: Vec<_> = got.iter().filter(|s| s.track_id == tid).collect();
        assert_eq!(want.len(), have.len());
        // 読み手は elst の media_time だけ DTS をずらす
        let shift = have[0].dts - want[0].dts;
        for (a, b) in want.iter().zip(&have) {
            assert_eq!(a.data, b.data);
            assert_eq!(a.duration, b.duration);
            assert_eq!(a.cts_offset, b.cts_offset);
            assert_eq!(a.keyframe, b.keyframe);
            assert_eq!(a.dts + shift, b.dts);
        }
        if tid == 1 {
            assert_eq!(shift, -7200); // 最初の表示時刻（PTS の最小 = 7200）が 0 になる
        }
    }
}

#[test]
fn reads_ffmpeg_av1_opus() {
    let data = Bytes::from_static(AV1_OPUS);
    let (init, samples) = read_file(&data).unwrap();
    let v = init.tracks.iter().find(|t| t.config.as_ref().unwrap().codec == Codec::Av1).unwrap();
    let a = init.tracks.iter().find(|t| t.config.as_ref().unwrap().codec == Codec::Opus).unwrap();
    let vc = v.config.as_ref().unwrap();
    assert_eq!(vc.record[0], 0x81);
    assert_eq!((vc.video.unwrap().width, vc.video.unwrap().height), (64, 48));
    let ac = a.config.as_ref().unwrap();
    assert_eq!(&ac.record[..8], b"OpusHead");
    let vs: Vec<_> = samples.iter().filter(|s| s.track_id == v.track_id).collect();
    assert_eq!(vs.len(), 20);
    assert!(vs[0].keyframe && vs[10].keyframe && !vs[1].keyframe);
    for w in vs.windows(2) {
        assert_eq!(w[0].dts + w[0].duration as i64, w[1].dts);
    }
    assert!(samples.iter().filter(|s| s.track_id == a.track_id).count() >= 40);
    rewrite_and_compare(&init, &samples);
}

#[test]
fn reads_ffmpeg_h264_aac() {
    let data = Bytes::from_static(H264_AAC);
    let (init, samples) = read_file(&data).unwrap();
    let v = init.tracks.iter().find(|t| t.config.as_ref().unwrap().codec == Codec::H264).unwrap();
    let vs: Vec<_> = samples.iter().filter(|s| s.track_id == v.track_id).collect();
    assert_eq!(vs.len(), 20);
    assert!(vs.iter().any(|s| s.cts_offset != 0));
    let vc = v.config.as_ref().unwrap();
    assert_eq!(vc.video.unwrap().width, 64);
    let a = init.tracks.iter().find(|t| t.config.as_ref().unwrap().codec == Codec::Aac).unwrap();
    assert_eq!(a.config.as_ref().unwrap().audio.unwrap().sample_rate, 48000);
    rewrite_and_compare(&init, &samples);
}

/// 読んだものを fMP4 に書き直し、もう一度読んで同じになるか。
fn rewrite_and_compare(init: &senca_fmp4::InitSegment, samples: &[ReadSample]) {
    let specs: Vec<TrackSpec> = init
        .tracks
        .iter()
        .map(|t| {
            let mut s = TrackSpec::new(t.track_id, t.config.clone().unwrap());
            s.timescale = t.timescale;
            s
        })
        .collect();
    let mut file = Vec::new();
    write_init_segment(&specs, &mut file).unwrap();
    let per: Vec<Vec<Sample>> = specs
        .iter()
        .map(|sp| {
            samples
                .iter()
                .filter(|s| s.track_id == sp.track_id)
                .map(|s| Sample { dts: s.dts, duration: s.duration, cts_offset: s.cts_offset, keyframe: s.keyframe, data: s.data.clone() })
                .collect()
        })
        .collect();
    let runs: Vec<TrackRun<'_>> = specs.iter().zip(&per).map(|(sp, s)| TrackRun { track_id: sp.track_id, samples: s }).collect();
    FragmentWriter::new().write(&runs, &mut file).unwrap();
    let (init2, got) = read_file(&Bytes::from(file)).unwrap();
    for (t1, t2) in init.tracks.iter().zip(&init2.tracks) {
        assert_eq!(t1.config, t2.config);
        let a: Vec<_> = samples.iter().filter(|s| s.track_id == t1.track_id).collect();
        let b: Vec<_> = got.iter().filter(|s| s.track_id == t1.track_id).collect();
        assert_eq!(a, b);
    }
}

#[test]
fn sample_queue_durations() {
    let mut q = SampleQueue::new(90000);
    let au = |dts: i64, pts: i64, dur: u64| AccessUnit {
        track: TrackId(0),
        codec: Codec::H264,
        epoch: 0,
        sequence: 0,
        config_generation: 0,
        pts,
        dts,
        duration: dur,
        timebase: Timebase::MILLIS,
        keyframe: dts == 0,
        discontinuity: false,
        capture_time_ns: None,
        payload: Bytes::from_static(b"x"),
    };
    q.push(&au(0, 80, 0)).unwrap();
    assert!(q.ready().is_empty());
    q.push(&au(40, 40, 0)).unwrap();
    q.push(&au(80, 120, 0)).unwrap();
    assert!(q.push(&au(79, 100, 0)).is_err());
    let r = q.take_ready();
    assert_eq!(r.len(), 2);
    assert_eq!((r[0].dts, r[0].duration, r[0].cts_offset, r[0].keyframe), (0, 3600, 7200, true));
    assert_eq!((r[1].dts, r[1].cts_offset), (3600, 0));
    q.flush();
    assert_eq!(q.take_ready()[0].duration, 3600); // 直前の長さ
    q.push(&au(120, 120, 40)).unwrap();
    q.flush();
    assert_eq!(q.take_ready()[0].duration, 3600); // AU の duration
}

#[test]
fn writer_rejects_bad_input() {
    assert!(matches!(write_init_segment(&[], &mut Vec::new()), Err(Mp4Error::BadConfig(_))));
    let dup = vec![TrackSpec::new(1, aac_config()), TrackSpec::new(1, aac_config())];
    assert_eq!(write_init_segment(&dup, &mut Vec::new()), Err(Mp4Error::DuplicateTrack(1)));
    let mut no_params = h264_config();
    no_params.video = None;
    assert!(write_init_segment(&[TrackSpec::new(1, no_params)], &mut Vec::new()).is_err());
    let mut fw = FragmentWriter::new();
    let neg = [sample(-1, 1, 0, true, 0, 1)];
    assert!(matches!(fw.write(&[TrackRun { track_id: 1, samples: &neg }], &mut Vec::new()), Err(Mp4Error::Timestamp(_))));
    let mut w = Mp4Writer::new(vec![TrackSpec::new(1, aac_config())]).unwrap();
    w.add(1, &sample(10, 1, 0, true, 0, 1)).unwrap();
    assert!(w.add(1, &sample(9, 1, 0, true, 0, 1)).is_err());
    assert_eq!(w.add(2, &sample(11, 1, 0, true, 0, 1)), Err(Mp4Error::UnknownTrack(2)));
}

#[test]
fn vp9_and_opus_entries() {
    let vp9 = CodecConfig {
        codec: Codec::Vp9,
        generation: 0,
        record: Bytes::new(),
        video: Some(VideoParams { width: 640, height: 360, profile: 0, compat: 0, level: 31 }),
        audio: None,
    };
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 6, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 1, 4, 2, 0, 4, 1, 2, 3, 5]);
    let opus = CodecConfig { codec: Codec::Opus, generation: 0, record: Bytes::from(head.clone()), video: None, audio: None };
    let mut file = Vec::new();
    write_init_segment(&[TrackSpec::new(1, vp9), TrackSpec::new(2, opus)], &mut file).unwrap();
    let (init, _) = read_file(&Bytes::from(file)).unwrap();
    let v = init.tracks[0].config.as_ref().unwrap();
    assert_eq!(v.codec, Codec::Vp9);
    assert_eq!(&v.record[..], &[1, 0, 0, 0, 0, 31, 0x82, 1, 1, 1, 0, 0]);
    assert_eq!(init.tracks[1].config.as_ref().unwrap().record, Bytes::from(head));
    assert_eq!(init.tracks[1].timescale, 48000);
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

#[test]
fn corrupted_input_does_not_panic() {
    let mut rng = Lcg(7);
    for fixture in [AV1_OPUS, H264_AAC] {
        for i in (0..fixture.len()).step_by(97) {
            let _ = read_file(&Bytes::copy_from_slice(&fixture[..i]));
        }
        for _ in 0..300 {
            let mut d = fixture.to_vec();
            for _ in 0..1 + rng.next() % 16 {
                let i = rng.next() as usize % d.len();
                d[i] = rng.next() as u8;
            }
            let _ = read_file(&Bytes::from(d));
        }
    }
}
