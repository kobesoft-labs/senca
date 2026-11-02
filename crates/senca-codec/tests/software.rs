//! ソフトウェアのプロバイダー（libvpx・SVT-AV1・dav1d・libopus）の往復試験。
//!
//! ライブラリは `SENCA_CODEC_LIB_DIR`（またはシステムの既定）から読み込む。見つからないプロバイダーの試験は
//! 「skipped」と出して通す（ライブラリの無い CI でも落ちない）。`tools/interop/codec_sw.sh` は
//! ライブラリの場所を設定し、`SENCA_CODEC_REQUIRE_SW=1` で「無ければ失敗」にして実行する。

use bytes::Bytes;
use senca_codec::{
    AudioEncoderConfig, AudioFrame, CodecProvider, DecoderConfig, EncoderConfig, ErrorKind, Frame,
    PixelFormat, Policy, ProbeOptions, ProviderId, ProviderRegistry, RateControl, SampleFormat,
    VideoEncoderConfig, VideoFrame,
};
use senca_core::{AccessUnit, Codec, Timebase, TrackId};
use std::sync::Arc;

fn registry() -> ProviderRegistry {
    ProviderRegistry::probe(&ProbeOptions::from_env())
}

fn provider(r: &ProviderRegistry, id: ProviderId) -> Option<Arc<dyn CodecProvider>> {
    let p = r.get(id)?.clone();
    if p.status().available {
        Some(p)
    } else {
        if std::env::var("SENCA_CODEC_REQUIRE_SW").as_deref() == Ok("1") {
            panic!(
                "{} is required but unavailable: {}",
                id.as_str(),
                p.status().detail
            );
        }
        eprintln!(
            "skipped: {} unavailable: {}",
            id.as_str(),
            p.status().detail
        );
        None
    }
}

/// 動く模様の I420 フレーム。
fn test_frame(w: u32, h: u32, i: u32) -> VideoFrame {
    let mut d = Vec::with_capacity(PixelFormat::I420.packed_size(w, h));
    for y in 0..h {
        for x in 0..w {
            d.push(((x + y + i * 3) % 200 + 20) as u8);
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    for y in 0..ch {
        for x in 0..cw {
            d.push((128 + (x as i32 - y as i32 + i as i32) % 40) as u8);
        }
    }
    for y in 0..ch {
        for x in 0..cw {
            d.push((100 + (x * 2 + y + i) % 50) as u8);
        }
    }
    VideoFrame::from_packed(
        PixelFormat::I420,
        w,
        h,
        Bytes::from(d),
        i as i64 * 3000,
        Timebase::MPEG,
    )
    .unwrap()
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

fn video_roundtrip(codec: Codec, enc_id: ProviderId, dec_id: ProviderId, w: u32, h: u32) {
    video_roundtrip_with(codec, enc_id, dec_id, w, h, true);
}

fn video_roundtrip_with(
    codec: Codec,
    enc_id: ProviderId,
    dec_id: ProviderId,
    w: u32,
    h: u32,
    low_latency: bool,
) {
    let r = registry();
    let (Some(ep), Some(dp)) = (provider(&r, enc_id), provider(&r, dec_id)) else {
        return;
    };
    let mut cfg = VideoEncoderConfig::new(codec, w, h, 30, 800_000);
    cfg.gop = 5;
    cfg.track = TrackId(3);
    if !low_latency {
        cfg.low_latency = false;
        cfg.b_frames = 3;
        cfg.rate_control = RateControl::Vbr;
    }
    let mut enc = ep.create_encoder(&EncoderConfig::Video(cfg)).unwrap();
    let frames: Vec<VideoFrame> = (0..12).map(|i| test_frame(w, h, i)).collect();
    let mut aus = Vec::new();
    for f in &frames {
        enc.send(&Frame::Video(f.clone())).unwrap();
        while let Some(au) = enc.receive().unwrap() {
            aus.push(au);
        }
    }
    enc.flush().unwrap();
    while let Some(au) = enc.receive().unwrap() {
        aus.push(au);
    }
    assert_eq!(aus.len(), frames.len(), "{codec}: one AU per frame");
    assert!(aus[0].keyframe, "{codec}: first AU is a keyframe");
    assert!(
        aus.iter().skip(1).any(|a| !a.keyframe),
        "{codec}: has inter frames"
    );
    assert!(
        aus.iter()
            .all(|a| a.track == TrackId(3) && a.codec == codec)
    );
    for (i, a) in aus.iter().enumerate() {
        assert_eq!(a.sequence, i as u64);
        assert_eq!(a.pts, i as i64 * 3000, "{codec}: pts preserved");
    }
    let cc = enc.codec_config().expect("codec config");
    assert_eq!(cc.codec, codec);
    if codec == Codec::Av1 {
        assert_eq!(cc.record[0], 0x81, "av1C marker/version");
        for a in &aus {
            let obus = senca_codec::bitstream::av1::parse_obus(&a.payload).unwrap();
            assert!(
                obus.iter()
                    .all(|o| o.obu_type != senca_codec::bitstream::av1::OBU_TEMPORAL_DELIMITER)
            );
        }
    }

    let mut dc = DecoderConfig::new(codec);
    dc.config = Some(cc);
    let mut dec = dp.create_decoder(&dc).unwrap();
    let mut out = Vec::new();
    for a in &aus {
        dec.send(a).unwrap();
        while let Some(f) = dec.receive().unwrap() {
            out.push(f);
        }
    }
    dec.flush().unwrap();
    while let Some(f) = dec.receive().unwrap() {
        out.push(f);
    }
    assert_eq!(out.len(), frames.len(), "{codec}: decoded frame count");
    for (i, (o, f)) in out.iter().zip(&frames).enumerate() {
        let o = o.as_video().unwrap();
        assert_eq!((o.width, o.height, o.format), (w, h, PixelFormat::I420));
        assert_eq!(o.pts, f.pts, "{codec}: frame {i} pts");
        let p = psnr(&o.to_packed().unwrap(), &f.to_packed().unwrap());
        assert!(p > 28.0, "{codec}: frame {i} psnr {p:.1}");
    }
}

#[test]
fn vp8_roundtrip() {
    video_roundtrip(Codec::Vp8, ProviderId::Libvpx, ProviderId::Libvpx, 176, 144);
}

#[test]
fn vp9_roundtrip_odd_size() {
    video_roundtrip(Codec::Vp9, ProviderId::Libvpx, ProviderId::Libvpx, 175, 143);
}

#[test]
fn av1_roundtrip() {
    video_roundtrip(Codec::Av1, ProviderId::SvtAv1, ProviderId::Dav1d, 192, 128);
}

#[test]
fn av1_roundtrip_random_access() {
    video_roundtrip_with(
        Codec::Av1,
        ProviderId::SvtAv1,
        ProviderId::Dav1d,
        192,
        128,
        false,
    );
}

#[test]
fn vp9_roundtrip_good_quality() {
    video_roundtrip_with(
        Codec::Vp9,
        ProviderId::Libvpx,
        ProviderId::Libvpx,
        160,
        96,
        false,
    );
}

#[test]
fn nv12_output_and_input() {
    let r = registry();
    let Some(p) = provider(&r, ProviderId::Libvpx) else {
        return;
    };
    let mut cfg = VideoEncoderConfig::new(Codec::Vp8, 64, 48, 30, 300_000);
    cfg.input_format = PixelFormat::Nv12;
    let mut enc = p.create_encoder(&EncoderConfig::Video(cfg)).unwrap();
    let f = test_frame(64, 48, 0).convert(PixelFormat::Nv12).unwrap();
    enc.send(&Frame::Video(f)).unwrap();
    enc.flush().unwrap();
    let au = enc.receive().unwrap().unwrap();
    let mut dc = DecoderConfig::new(Codec::Vp8);
    dc.output_format = Some(PixelFormat::Nv12);
    let mut dec = p.create_decoder(&dc).unwrap();
    dec.send(&au).unwrap();
    let out = dec.receive().unwrap().unwrap();
    assert_eq!(out.as_video().unwrap().format, PixelFormat::Nv12);
}

#[test]
fn constant_qp_and_keyframe_request() {
    let r = registry();
    let Some(p) = provider(&r, ProviderId::Libvpx) else {
        return;
    };
    let mut cfg = VideoEncoderConfig::new(Codec::Vp9, 64, 64, 30, 0);
    cfg.rate_control = RateControl::ConstQp(30);
    cfg.gop = 0;
    let mut enc = p.create_encoder(&EncoderConfig::Video(cfg)).unwrap();
    let mut keys = Vec::new();
    for i in 0..6 {
        if i == 4 {
            enc.request_keyframe();
        }
        enc.send(&Frame::Video(test_frame(64, 64, i))).unwrap();
        while let Some(au) = enc.receive().unwrap() {
            keys.push(au.keyframe);
        }
    }
    assert_eq!(keys, vec![true, false, false, false, true, false]);
}

#[test]
fn decoders_reject_garbage() {
    let r = registry();
    let garbage: Vec<Bytes> = vec![
        Bytes::from_static(&[0xff; 7]),
        Bytes::from(
            (0..2000u32)
                .map(|i| (i * 7919 % 251) as u8)
                .collect::<Vec<_>>(),
        ),
        Bytes::from_static(&[0x12, 0x00, 0x0a, 0x7f]),
    ];
    for (codec, id) in [
        (Codec::Vp8, ProviderId::Libvpx),
        (Codec::Vp9, ProviderId::Libvpx),
        (Codec::Av1, ProviderId::Dav1d),
    ] {
        let Some(p) = provider(&r, id) else { continue };
        for g in &garbage {
            let mut dec = p.create_decoder(&DecoderConfig::new(codec)).unwrap();
            let au = access_unit(codec, g.clone());
            // エラーでもよいが、落ちない・フレームを出しても大きさは上限内。
            let _ = dec.send(&au);
            let _ = dec.flush();
            while let Ok(Some(f)) = dec.receive() {
                let v = f.as_video().unwrap();
                assert!(v.width <= 7680 && v.height <= 4320);
            }
        }
    }
    if let Some(p) = provider(&r, ProviderId::Opus) {
        let mut dec = p.create_decoder(&DecoderConfig::new(Codec::Opus)).unwrap();
        for g in &garbage {
            let _ = dec.send(&access_unit(Codec::Opus, g.clone()));
        }
    }
}

#[test]
fn decoder_resolution_limit() {
    let r = registry();
    let Some(p) = provider(&r, ProviderId::Libvpx) else {
        return;
    };
    let cfg = VideoEncoderConfig::new(Codec::Vp8, 320, 240, 30, 300_000);
    let mut enc = p.create_encoder(&EncoderConfig::Video(cfg)).unwrap();
    enc.send(&Frame::Video(test_frame(320, 240, 0))).unwrap();
    enc.flush().unwrap();
    let au = enc.receive().unwrap().unwrap();
    let mut dc = DecoderConfig::new(Codec::Vp8);
    dc.max_width = 160;
    dc.max_height = 120;
    let mut dec = p.create_decoder(&dc).unwrap();
    let e = dec.send(&au).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidInput);
}

fn access_unit(codec: Codec, payload: Bytes) -> AccessUnit {
    AccessUnit {
        track: TrackId(0),
        codec,
        epoch: 0,
        sequence: 0,
        config_generation: 0,
        pts: 0,
        dts: 0,
        duration: 0,
        timebase: Timebase::MPEG,
        keyframe: true,
        discontinuity: false,
        capture_time_ns: None,
        payload,
    }
}

#[test]
fn opus_roundtrip() {
    let r = registry();
    let Some(p) = provider(&r, ProviderId::Opus) else {
        return;
    };
    let rate = 48000u32;
    let mut cfg = AudioEncoderConfig::opus(rate, 2, 96_000);
    cfg.track = TrackId(1);
    let mut enc = p.create_encoder(&EncoderConfig::Audio(cfg)).unwrap();
    let head = enc.codec_config().unwrap();
    assert_eq!(&head.record[..8], b"OpusHead");
    // 1 秒の 440 Hz（10 ms ずつ、i16 と f32 を混ぜて入れる）
    let total = rate as usize;
    let mut pcm = Vec::new();
    for n in 0..total {
        let v = (2.0 * std::f32::consts::PI * 440.0 * n as f32 / rate as f32).sin() * 0.5;
        pcm.push(v);
        pcm.push(v);
    }
    let mut aus = Vec::new();
    for (k, chunk) in pcm.chunks(960).enumerate() {
        let (format, data): (SampleFormat, Vec<u8>) = if k % 2 == 0 {
            (
                SampleFormat::F32,
                chunk.iter().flat_map(|v| v.to_ne_bytes()).collect(),
            )
        } else {
            (
                SampleFormat::I16,
                chunk
                    .iter()
                    .flat_map(|v| ((v * 32767.0) as i16).to_ne_bytes())
                    .collect(),
            )
        };
        let f = AudioFrame {
            format,
            sample_rate: rate,
            channels: 2,
            data: Bytes::from(data),
            pts: (k * 480) as i64,
            timebase: Timebase::new(1, rate),
        };
        enc.send(&Frame::Audio(f)).unwrap();
        while let Some(au) = enc.receive().unwrap() {
            aus.push(au);
        }
    }
    enc.flush().unwrap();
    while let Some(au) = enc.receive().unwrap() {
        aus.push(au);
    }
    // 1 秒 = 50 パケット + 符号化器の遅延（lookahead）を出し切る 1 パケット。
    assert_eq!(aus.len(), 51, "20 ms packets");
    assert_eq!(aus[1].pts, 960);
    assert_eq!(aus[0].timebase, Timebase::new(1, 48000));
    assert!(
        aus.iter()
            .all(|a| a.duration == 960 && a.track == TrackId(1))
    );

    let mut dc = DecoderConfig::new(Codec::Opus);
    dc.config = Some(head.clone());
    let mut dec = p.create_decoder(&dc).unwrap();
    let mut decoded = Vec::new();
    for a in &aus {
        dec.send(a).unwrap();
        while let Some(f) = dec.receive().unwrap() {
            let a = f.as_audio().unwrap();
            assert_eq!((a.channels, a.sample_rate), (2, 48000));
            decoded.extend(
                a.data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_ne_bytes(*c)),
            );
        }
    }
    let pre_skip = u16::from_le_bytes([head.record[10], head.record[11]]) as usize;
    assert_eq!(decoded.len() / 2, 51 * 960 - pre_skip);
    assert!(51 * 960 - pre_skip >= 48000, "no samples lost at the end");
    // 相関で確かめる（pre_skip を除いたので位相がそろう）。
    let n = 40000.min(decoded.len() / 2);
    let (mut num, mut da, mut db) = (0f64, 0f64, 0f64);
    for i in 1000..n {
        let a = pcm[2 * i] as f64;
        let b = decoded[2 * i] as f64;
        num += a * b;
        da += a * a;
        db += b * b;
    }
    let corr = num / (da.sqrt() * db.sqrt());
    assert!(corr > 0.95, "correlation {corr}");
}

#[test]
fn registry_policy_with_real_probe() {
    let r = registry();
    // この試験機に GPU は無いので H.264 は 503 相当。ある環境では作れる（それも正しい）。
    let cfg = EncoderConfig::Video(VideoEncoderConfig::new(
        Codec::H264,
        640,
        360,
        30,
        1_000_000,
    ));
    match r.create_encoder(&cfg, &Policy::default()) {
        Ok((id, _)) => assert!(id.is_hardware()),
        Err(e) => assert_eq!(e.kind, ErrorKind::Unavailable),
    }
    assert_eq!(
        r.create_encoder(&cfg, &Policy::royalty_free())
            .err()
            .unwrap()
            .kind,
        ErrorKind::PolicyRejected
    );
    // ソフトウェアの能力に H.264/H.265/AAC は出てこない。
    for c in r.capabilities(&Policy::default()) {
        if !c.hardware {
            assert!(!c.codec.is_patent_pooled(), "{c:?}");
        }
    }
}
