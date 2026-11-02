//! FFmpeg（libx264）の出力との突き合わせ。期待値は ffprobe の出力（tests/data/README.md）。

use senca_h264::*;
use std::path::PathBuf;

fn data(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Annex B の NAL 列をアクセスユニットに分ける（7.4.1.2.3 の簡略版: VCL の後に AUD・SEI・SPS・PPS、
/// または first_mb_in_slice = 0 の VCL が来たら新しい AU）。
fn access_units(stream: &[u8]) -> Vec<Vec<&[u8]>> {
    let mut aus: Vec<Vec<&[u8]>> = Vec::new();
    let mut cur: Vec<&[u8]> = Vec::new();
    let mut seen_vcl = false;
    for nal in annexb_nals(stream) {
        let ty = nal_unit_type(nal).unwrap();
        let starts = if ty.is_vcl() {
            seen_vcl && SliceHeader::parse(nal, None).unwrap().is_first_slice()
        } else {
            seen_vcl
                && matches!(
                    ty,
                    NalUnitType::Aud | NalUnitType::Sei | NalUnitType::Sps | NalUnitType::Pps
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
    constraint: u8,
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
        file: "h264_320x240_high",
        profile: 100,
        constraint: 0,
        level: 13,
        width: 320,
        height: 240,
        chroma: 1,
        bit_depth: 8,
        fps: (30, 1),
        reorder: true,
        keyframes: &[
            true, false, false, false, false, false, false, false, true, false, false, false,
            false, false, false,
        ],
        codecs: "avc1.64000d",
    },
    Expect {
        file: "h264_1918x1078",
        profile: 66,
        constraint: 0xC0,
        level: 40,
        width: 1918,
        height: 1078,
        chroma: 1,
        bit_depth: 8,
        fps: (25, 1),
        reorder: false,
        keyframes: &[true],
        codecs: "avc1.42c028",
    },
    Expect {
        file: "h264_444",
        profile: 244,
        constraint: 0,
        level: 11,
        width: 176,
        height: 144,
        chroma: 3,
        bit_depth: 8,
        fps: (30, 1),
        reorder: false,
        keyframes: &[true, false],
        codecs: "avc1.f4000b",
    },
    Expect {
        file: "h264_10bit",
        profile: 110,
        constraint: 0,
        level: 11,
        width: 176,
        height: 144,
        chroma: 1,
        bit_depth: 10,
        fps: (30, 1),
        reorder: false,
        keyframes: &[true, false],
        codecs: "avc1.6e000b",
    },
    Expect {
        file: "h264_baseline",
        profile: 66,
        constraint: 0xC0,
        level: 11,
        width: 176,
        height: 144,
        chroma: 1,
        bit_depth: 8,
        fps: (30, 1),
        reorder: false,
        keyframes: &[true, false, false],
        codecs: "avc1.42c00b",
    },
    Expect {
        file: "h264_interlaced",
        profile: 77,
        constraint: 0x40,
        level: 30,
        width: 718,
        height: 476,
        chroma: 1,
        bit_depth: 8,
        fps: (25, 1),
        reorder: false,
        keyframes: &[true, false],
        codecs: "avc1.4d401e",
    },
];

#[test]
fn interlaced_sps_fields() {
    let stream = data("h264_interlaced.h264");
    let sps = annexb_nals(&stream)
        .find(|n| nal_unit_type(n) == Some(NalUnitType::Sps))
        .map(|n| Sps::parse(n).unwrap())
        .unwrap();
    assert!(!sps.frame_mbs_only);
    assert!(sps.mb_adaptive_frame_field);
    // 476 = 2 * 15 * 16 - 4（CropUnitY = 4）
    assert_eq!(sps.pic_height_in_map_units, 15);
    assert_eq!(sps.crop.bottom, 1);
}

#[test]
fn matches_ffprobe() {
    for c in CASES {
        let stream = data(&format!("{}.h264", c.file));
        let nals: Vec<&[u8]> = annexb_nals(&stream).collect();
        let sps_nal = nals
            .iter()
            .find(|n| nal_unit_type(n) == Some(NalUnitType::Sps))
            .unwrap();
        let pps_nal = nals
            .iter()
            .find(|n| nal_unit_type(n) == Some(NalUnitType::Pps))
            .unwrap();
        let sps = Sps::parse(sps_nal).unwrap();
        assert_eq!(sps.profile_idc, c.profile, "{}", c.file);
        assert_eq!(sps.constraint_flags, c.constraint, "{}", c.file);
        assert_eq!(sps.level_idc, c.level, "{}", c.file);
        assert_eq!((sps.width, sps.height), (c.width, c.height), "{}", c.file);
        assert_eq!(sps.chroma_format_idc, c.chroma, "{}", c.file);
        assert_eq!(sps.bit_depth_luma, c.bit_depth, "{}", c.file);
        assert_eq!(sps.frame_rate(), Some(c.fps), "{}", c.file);
        assert_eq!(sps.may_reorder(), c.reorder, "{}", c.file);
        assert!(sps.vui.is_some(), "{}", c.file);
        assert_eq!(
            sps.codecs_string().to_ascii_lowercase(),
            c.codecs,
            "{}",
            c.file
        );
        let pps = Pps::parse(pps_nal).unwrap();
        assert_eq!(pps.seq_parameter_set_id, sps.seq_parameter_set_id);

        // AU の数とキーフレームの位置が ffprobe の packet と一致する
        let aus = access_units(&stream);
        let keys: Vec<bool> = aus
            .iter()
            .map(|au| contains_idr(au.iter().copied()))
            .collect();
        assert_eq!(keys, c.keyframes, "{}", c.file);
        for au in &aus {
            for nal in au.iter().filter(|n| nal_unit_type(n).unwrap().is_vcl()) {
                let sh = SliceHeader::parse(nal, Some(&sps)).unwrap();
                assert_eq!(sh.pic_parameter_set_id, pps.pic_parameter_set_id);
                if sh.nal_unit_type == NalUnitType::IdrSlice {
                    assert_eq!(sh.frame_num, Some(0));
                }
            }
        }

        // FFmpeg の mp4 muxer が作った avcC と同じバイト列になる
        let rec =
            AvcDecoderConfigurationRecord::from_parameter_sets(&[sps_nal], &[pps_nal]).unwrap();
        let ffmpeg_avcc = data(&format!("{}.avcC", c.file));
        assert_eq!(&rec.to_bytes().unwrap()[..], &ffmpeg_avcc[..], "{}", c.file);
        let parsed = AvcDecoderConfigurationRecord::parse(&ffmpeg_avcc).unwrap();
        assert_eq!(parsed, rec);
        let cc = parsed.codec_config(1).unwrap();
        let v = cc.video.unwrap();
        assert_eq!(
            (v.width, v.height, v.profile, v.compat, v.level),
            (c.width, c.height, c.profile, c.constraint, c.level)
        );
        if !matches!(c.profile, 66 | 77 | 88) {
            let ext = parsed.ext.as_ref().unwrap();
            assert_eq!(ext.chroma_format, c.chroma);
            assert_eq!(ext.bit_depth_luma, c.bit_depth);
        }
    }
}

#[test]
fn annexb_length_prefixed_roundtrip_on_real_stream() {
    let stream = data("h264_320x240_high.h264");
    let lp = annexb_to_length_prefixed(&stream).unwrap();
    let nals_a: Vec<&[u8]> = annexb_nals(&stream).collect();
    let nals_b: Vec<&[u8]> = length_prefixed_nals(&lp, 4)
        .unwrap()
        .map(|n| n.unwrap())
        .collect();
    assert_eq!(nals_a, nals_b);
    let back = length_prefixed_to_annexb(&lp, 4).unwrap();
    assert_eq!(annexb_nals(&back).collect::<Vec<_>>(), nals_a);
    // emulation prevention の除去と再挿入で元に戻る
    for nal in &nals_a {
        let rbsp = remove_emulation_prevention(&nal[1..]);
        assert_eq!(&add_emulation_prevention(&rbsp)[..], &nal[1..]);
    }
}

#[test]
fn corrupted_real_input_never_panics() {
    let stream = data("h264_320x240_high.h264");
    let nals: Vec<&[u8]> = annexb_nals(&stream).take(4).collect();
    for nal in nals {
        for cut in 0..nal.len().min(64) {
            let _ = Sps::parse(&nal[..cut]);
            let _ = Pps::parse(&nal[..cut]);
            let _ = SliceHeader::parse(&nal[..cut], None);
        }
        // ビット反転
        for i in 0..nal.len().min(40) {
            let mut m = nal.to_vec();
            m[i] ^= 0x5A;
            let _ = Sps::parse(&m);
            let _ = SliceHeader::parse(&m, None);
        }
    }
    let avcc = data("h264_320x240_high.avcC");
    for cut in 0..avcc.len() {
        let _ = AvcDecoderConfigurationRecord::parse(&avcc[..cut]);
    }
}
