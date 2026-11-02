//! FFmpeg（libx265）の出力との突き合わせ。期待値は ffprobe の出力（tests/data/README.md）。

use senca_h265::*;
use std::path::PathBuf;

fn data(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Annex B の NAL 列をアクセスユニットに分ける（7.4.2.4.4 の簡略版）。
fn access_units(stream: &[u8]) -> Vec<Vec<&[u8]>> {
    let mut aus: Vec<Vec<&[u8]>> = Vec::new();
    let mut cur: Vec<&[u8]> = Vec::new();
    let mut seen_vcl = false;
    for nal in annexb_nals(stream) {
        let ty = nal_unit_type(nal).unwrap();
        let starts = if ty.is_vcl() {
            seen_vcl
                && SliceSegmentHeaderStart::parse(nal)
                    .unwrap()
                    .first_slice_segment_in_pic
        } else {
            seen_vcl
                && matches!(
                    ty,
                    NalUnitType::Aud
                        | NalUnitType::Vps
                        | NalUnitType::Sps
                        | NalUnitType::Pps
                        | NalUnitType::PrefixSei
                )
        };
        if starts {
            aus.push(std::mem::take(&mut cur));
            seen_vcl = false;
        }
        seen_vcl |= ty.is_vcl();
        cur.push(nal);
    }
    if !cur.is_empty() {
        aus.push(cur);
    }
    aus
}

struct Expect {
    file: &'static str,
    profile: u8,
    level: u8,
    width: u32,
    height: u32,
    chroma: u8,
    bit_depth: u8,
    fps: (u32, u32),
    reorder: bool,
    keyframes: &'static [bool],
    codecs: &'static str,
}

const CASES: &[Expect] = &[
    Expect {
        file: "hevc_320x240",
        profile: 1,
        level: 60,
        width: 320,
        height: 240,
        chroma: 1,
        bit_depth: 8,
        fps: (30, 1),
        reorder: true,
        keyframes: &[true, false, false, false, false, false, false, false, false],
        codecs: "hvc1.1.6.L60.90",
    },
    Expect {
        file: "hevc_1918x1078",
        profile: 1,
        level: 120,
        width: 1918,
        height: 1078,
        chroma: 1,
        bit_depth: 8,
        fps: (25, 1),
        reorder: true,
        keyframes: &[true],
        codecs: "hvc1.1.6.L120.90",
    },
    Expect {
        file: "hevc_main10",
        profile: 2,
        level: 60,
        width: 176,
        height: 144,
        chroma: 1,
        bit_depth: 10,
        fps: (30, 1),
        reorder: true,
        keyframes: &[true, false],
        codecs: "hvc1.2.4.L60.90",
    },
    Expect {
        file: "hevc_444",
        profile: 4,
        level: 60,
        width: 176,
        height: 144,
        chroma: 3,
        bit_depth: 8,
        fps: (30, 1),
        reorder: true,
        keyframes: &[true, false],
        codecs: "hvc1.4.10.L60.9E.8",
    },
];

#[test]
fn matches_ffprobe() {
    for c in CASES {
        let stream = data(&format!("{}.h265", c.file));
        let nals: Vec<&[u8]> = annexb_nals(&stream).collect();
        let find = |ty| *nals.iter().find(|n| nal_unit_type(n) == Some(ty)).unwrap();
        let (vps_nal, sps_nal, pps_nal) = (
            find(NalUnitType::Vps),
            find(NalUnitType::Sps),
            find(NalUnitType::Pps),
        );
        let vps = Vps::parse(vps_nal).unwrap();
        let sps = Sps::parse(sps_nal).unwrap();
        let pps = Pps::parse(pps_nal).unwrap();
        assert_eq!(vps.ptl, sps.ptl, "{}", c.file);
        assert_eq!(pps.pps_seq_parameter_set_id, sps.sps_seq_parameter_set_id);
        assert_eq!(sps.ptl.general_profile_idc, c.profile, "{}", c.file);
        assert_eq!(sps.ptl.general_level_idc, c.level, "{}", c.file);
        assert_eq!((sps.width, sps.height), (c.width, c.height), "{}", c.file);
        assert_eq!(sps.chroma_format_idc, c.chroma, "{}", c.file);
        assert_eq!(sps.bit_depth_luma, c.bit_depth, "{}", c.file);
        assert_eq!(sps.frame_rate(), Some(c.fps), "{}", c.file);
        assert_eq!(sps.may_reorder(), c.reorder, "{}", c.file);
        assert_eq!(sps.codecs_string(), c.codecs, "{}", c.file);
        assert_eq!(parse_codecs_string(c.codecs).unwrap(), sps.ptl);

        let aus = access_units(&stream);
        let keys: Vec<bool> = aus
            .iter()
            .map(|au| contains_irap(au.iter().copied()))
            .collect();
        assert_eq!(keys, c.keyframes, "{}", c.file);

        // FFmpeg の mp4 muxer が作った hvcC と同じバイト列になる
        let rec = HevcDecoderConfigurationRecord::from_parameter_sets(
            &[vps_nal],
            &[sps_nal],
            &[pps_nal],
            &[],
        )
        .unwrap();
        let ffmpeg_hvcc = data(&format!("{}.hvcC", c.file));
        assert_eq!(&rec.to_bytes().unwrap()[..], &ffmpeg_hvcc[..], "{}", c.file);
        let parsed = HevcDecoderConfigurationRecord::parse(&ffmpeg_hvcc).unwrap();
        assert_eq!(parsed, rec);
        let v = parsed.codec_config(1).unwrap().video.unwrap();
        assert_eq!(
            (v.width, v.height, v.profile, v.level),
            (c.width, c.height, c.profile, c.level)
        );
    }
}

#[test]
fn odd_size_uses_conformance_window() {
    let stream = data("hevc_1918x1078.h265");
    let sps = annexb_nals(&stream)
        .find(|n| nal_unit_type(n) == Some(NalUnitType::Sps))
        .map(|n| Sps::parse(n).unwrap())
        .unwrap();
    assert_eq!(
        (
            sps.pic_width_in_luma_samples,
            sps.pic_height_in_luma_samples
        ),
        (1920, 1088)
    );
    assert_eq!(sps.conformance_window.right, 1);
    assert_eq!(sps.conformance_window.bottom, 5);
}

#[test]
fn length_prefixed_roundtrip_on_real_stream() {
    let stream = data("hevc_320x240.h265");
    let lp = annexb_to_length_prefixed(&stream).unwrap();
    let a: Vec<&[u8]> = annexb_nals(&stream).collect();
    let b: Vec<&[u8]> = length_prefixed_nals(&lp, 4)
        .unwrap()
        .map(|n| n.unwrap())
        .collect();
    assert_eq!(a, b);
    assert!(is_keyframe_length_prefixed(&lp, 4).unwrap());
    for nal in &a {
        let rbsp = remove_emulation_prevention(&nal[2..]);
        assert_eq!(&add_emulation_prevention(&rbsp)[..], &nal[2..]);
    }
}

#[test]
fn corrupted_real_input_never_panics() {
    for f in ["hevc_320x240", "hevc_444"] {
        let stream = data(&format!("{f}.h265"));
        for nal in annexb_nals(&stream).take(4) {
            for cut in 0..nal.len().min(128) {
                let _ = Vps::parse(&nal[..cut]);
                let _ = Sps::parse(&nal[..cut]);
                let _ = Pps::parse(&nal[..cut]);
                let _ = SliceSegmentHeaderStart::parse(&nal[..cut]);
            }
            for i in 0..nal.len().min(80) {
                for x in [0x01u8, 0x5A, 0xFF] {
                    let mut m = nal.to_vec();
                    m[i] ^= x;
                    let _ = Vps::parse(&m);
                    let _ = Sps::parse(&m);
                }
            }
        }
        let hvcc = data(&format!("{f}.hvcC"));
        for cut in 0..hvcc.len() {
            assert!(HevcDecoderConfigurationRecord::parse(&hvcc[..cut]).is_err());
        }
    }
}
