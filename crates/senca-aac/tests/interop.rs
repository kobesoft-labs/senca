//! FFmpeg（aac エンコーダー、adts muxer）の出力との突き合わせ。期待値は ffprobe の出力（tests/data/README.md）。

use bytes::Bytes;
use senca_aac::*;
use std::path::PathBuf;

fn data(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

struct Expect {
    file: &'static str,
    sample_rate: u32,
    channels: u8,
    /// ffprobe -show_entries packet=size（ADTS ヘッダーを含む）
    sizes: &'static [u16],
}

const CASES: &[Expect] = &[
    Expect {
        file: "aac_44k_mono",
        sample_rate: 44100,
        channels: 1,
        sizes: &[158, 168, 68, 77, 84, 76, 85, 114, 89, 95, 101, 114, 100, 79],
    },
    Expect {
        file: "aac_48k_stereo",
        sample_rate: 48000,
        channels: 2,
        sizes: &[
            137, 231, 125, 128, 136, 140, 149, 142, 166, 170, 164, 175, 152, 161, 175, 14,
        ],
    },
    Expect {
        file: "aac_48k_51",
        sample_rate: 48000,
        channels: 6,
        sizes: &[212, 421, 136, 183, 182, 185, 185, 182, 188, 329, 26],
    },
];

#[test]
fn matches_ffprobe() {
    for c in CASES {
        let stream = Bytes::from(data(&format!("{}.aac", c.file)));
        let frames = split_adts(&stream).unwrap();
        let sizes: Vec<u16> = frames.iter().map(|f| f.header.frame_length).collect();
        assert_eq!(sizes, c.sizes, "{}", c.file);
        for f in &frames {
            assert_eq!(f.payload.len() + 7, f.header.frame_length as usize);
            assert!(f.header.protection_absent);
        }
        let (asc, raws) = adts_to_raw(&stream).unwrap();
        assert_eq!(raws.len(), c.sizes.len());
        assert_eq!(asc.sampling_frequency, c.sample_rate, "{}", c.file);
        assert_eq!(asc.channels, c.channels, "{}", c.file);
        assert_eq!(asc.object_type, 2);
        assert_eq!(asc.codecs_string(), "mp4a.40.2");

        // FFmpeg の mp4 muxer が書いた AudioSpecificConfig と同じバイト列になる
        let ffmpeg_asc = data(&format!("{}.asc", c.file));
        assert_eq!(asc.to_bytes().unwrap(), ffmpeg_asc, "{}", c.file);
        assert_eq!(AudioSpecificConfig::parse(&ffmpeg_asc).unwrap(), asc);
        let cc = codec_config(&ffmpeg_asc, 0).unwrap();
        assert_eq!(cc.audio.unwrap().channels, c.channels);

        // raw_data_block に ADTS を付け直すと、buffer_fullness 以外は元と同じ
        let mut rebuilt = Vec::new();
        for (raw, f) in raws.iter().zip(&frames) {
            let mut h = AdtsHeader::for_payload(&asc, raw.len()).unwrap();
            h.buffer_fullness = f.header.buffer_fullness;
            h.mpeg2 = f.header.mpeg2;
            rebuilt.extend_from_slice(&h.to_bytes().unwrap());
            rebuilt.extend_from_slice(raw);
        }
        assert_eq!(rebuilt, &stream[..], "{}", c.file);

        // 任意の位置で区切って流し込んでも同じフレームが出る
        for chunk in [1usize, 7, 100, 4096] {
            let mut r = AdtsReader::new();
            let mut got = Vec::new();
            for part in stream.chunks(chunk) {
                r.push(part).unwrap();
                while let Some(f) = r.next_frame().unwrap() {
                    got.push(f.payload);
                }
            }
            assert_eq!(got, raws);
        }
    }
}

#[test]
fn corrupted_real_input_never_panics() {
    let stream = data("aac_48k_stereo.aac");
    for cut in 0..300.min(stream.len()) {
        let _ = split_adts(&Bytes::copy_from_slice(&stream[..cut]));
    }
    for i in 0..64 {
        let mut m = stream.clone();
        m[i] ^= 0xA5;
        let _ = split_adts(&Bytes::from(m));
    }
}
