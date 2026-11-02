//! FFmpeg（libaom-av1、obu muxer）の出力との突き合わせ。期待値は ffprobe の出力（tests/data/README.md）。

use bytes::Bytes;
use senca_av1::*;
use std::path::PathBuf;

fn data(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Low Overhead Bitstream Format を Temporal Delimiter で TU に分ける。
fn temporal_units(stream: &Bytes) -> Vec<Bytes> {
    let mut starts = Vec::new();
    let mut pos = 0;
    for obu in obus(stream) {
        let obu = obu.unwrap();
        if obu.header.obu_type == ObuType::TemporalDelimiter {
            starts.push(pos);
        }
        pos += obu.raw.len();
    }
    starts.push(stream.len());
    starts
        .windows(2)
        .map(|w| stream.slice(w[0]..w[1]))
        .collect()
}

struct Expect {
    file: &'static str,
    profile: u8,
    width: u32,
    height: u32,
    bit_depth: u8,
    subsampling: (bool, bool),
    keyframes: &'static [bool],
    codecs: &'static str,
}

const CASES: &[Expect] = &[
    Expect {
        file: "av1_320x240",
        profile: 0,
        width: 320,
        height: 240,
        bit_depth: 8,
        subsampling: (true, true),
        keyframes: &[true, false, false, false, false, true, false, false, false],
        codecs: "av01.0.00M.08",
    },
    Expect {
        file: "av1_10bit",
        profile: 0,
        width: 176,
        height: 144,
        bit_depth: 10,
        subsampling: (true, true),
        keyframes: &[true, false],
        codecs: "av01.0.00M.10",
    },
    Expect {
        file: "av1_444",
        profile: 1,
        width: 176,
        height: 144,
        bit_depth: 8,
        subsampling: (false, false),
        keyframes: &[true, false],
        codecs: "av01.1.00M.08",
    },
];

#[test]
fn matches_ffprobe() {
    for c in CASES {
        let stream = Bytes::from(data(&format!("{}.obu", c.file)));
        let (sh, sh_obu) = SequenceHeader::find_in(&stream).unwrap().unwrap();
        assert_eq!(sh.seq_profile, c.profile, "{}", c.file);
        assert_eq!(
            (sh.max_frame_width, sh.max_frame_height),
            (c.width, c.height),
            "{}",
            c.file
        );
        assert_eq!(sh.color.bit_depth, c.bit_depth, "{}", c.file);
        assert_eq!(
            (sh.color.subsampling_x, sh.color.subsampling_y),
            c.subsampling,
            "{}",
            c.file
        );
        assert_eq!(sh.codecs_string(), c.codecs, "{}", c.file);

        // TU の数とキーフレームの位置が ffprobe の packet と一致する
        let tus = temporal_units(&stream);
        let keys: Vec<bool> = tus
            .iter()
            .map(|tu| is_keyframe(tu, Some(&sh)).unwrap())
            .collect();
        assert_eq!(keys, c.keyframes, "{}", c.file);
        for tu in &tus {
            let stripped = strip_temporal_delimiters(tu).unwrap();
            // 先頭の TD（2 バイト）を切り出すだけになる
            assert_eq!(stripped.len(), tu.len() - 2);
            assert_eq!(stripped.as_ptr(), tu[2..].as_ptr());
            for obu in obus(&stripped) {
                assert_ne!(obu.unwrap().header.obu_type, ObuType::TemporalDelimiter);
            }
            // TD を除いても判定は同じ
            assert_eq!(
                is_keyframe(&stripped, Some(&sh)).unwrap(),
                is_keyframe(tu, Some(&sh)).unwrap()
            );
        }

        // FFmpeg の mp4 muxer が作った av1C と同じバイト列になる
        let rec = Av1CodecConfigurationRecord::from_sequence_header_obu(sh_obu).unwrap();
        let ffmpeg_av1c = data(&format!("{}.av1C", c.file));
        assert_eq!(&rec.to_bytes().unwrap()[..], &ffmpeg_av1c[..], "{}", c.file);
        let parsed = Av1CodecConfigurationRecord::parse(&ffmpeg_av1c).unwrap();
        assert_eq!(parsed, rec);
        assert_eq!(parsed.codecs_string(), c.codecs);
        let v = parsed.codec_config(0).unwrap().video.unwrap();
        assert_eq!(
            (v.width, v.height, v.profile),
            (c.width, c.height, c.profile)
        );
    }
}

#[test]
fn corrupted_real_input_never_panics() {
    let stream = data("av1_320x240.obu");
    let (_, sh_obu) = SequenceHeader::find_in(&stream).unwrap().unwrap();
    let sh_obu = sh_obu.to_vec();
    for cut in 0..sh_obu.len() {
        let _ = Av1CodecConfigurationRecord::from_sequence_header_obu(&sh_obu[..cut]);
    }
    for i in 0..sh_obu.len() {
        for x in [0x01u8, 0x80, 0xFF] {
            let mut m = sh_obu.clone();
            m[i] ^= x;
            let _ = Av1CodecConfigurationRecord::from_sequence_header_obu(&m);
        }
    }
    for cut in 0..200 {
        let b = Bytes::copy_from_slice(&stream[..cut]);
        let _ = strip_temporal_delimiters(&b);
        let _ = is_keyframe(&b, None);
    }
}
