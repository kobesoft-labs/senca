//! FFmpeg（libopus、ogg muxer）の出力との突き合わせ。期待値は ffprobe の出力（tests/data/README.md）。

use senca_opus::*;
use std::path::PathBuf;

fn data(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// 試験用の最小限の Ogg 解析（RFC 3533）。ページをつなげてパケットの列を返す。
fn ogg_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut cur = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        assert_eq!(&data[pos..pos + 4], b"OggS");
        let nseg = data[pos + 26] as usize;
        let table = &data[pos + 27..pos + 27 + nseg];
        let mut body = pos + 27 + nseg;
        for &lace in table {
            cur.extend_from_slice(&data[body..body + lace as usize]);
            body += lace as usize;
            if lace < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        pos = body;
    }
    packets
}

struct Expect {
    file: &'static str,
    channels: u8,
    frame_samples: u32,
    /// ffprobe -show_entries packet=size
    sizes: &'static [usize],
}

const CASES: &[Expect] = &[
    Expect {
        file: "opus_stereo",
        channels: 2,
        frame_samples: 960,
        sizes: &[
            84, 71, 75, 78, 76, 77, 73, 74, 79, 57, 60, 61, 65, 66, 57, 39,
        ],
    },
    Expect {
        file: "opus_mono_60ms",
        channels: 1,
        frame_samples: 2880,
        sizes: &[179, 171, 163, 121, 122, 68],
    },
];

#[test]
fn matches_ffprobe() {
    for c in CASES {
        let packets = ogg_packets(&data(&format!("{}.opus", c.file)));
        let head = OpusHead::parse(&packets[0]).unwrap();
        assert_eq!(head.channels, c.channels, "{}", c.file);
        assert_eq!(head.pre_skip, 312, "{}", c.file);
        assert_eq!(head.input_sample_rate, 48000, "{}", c.file);
        assert_eq!(head.mapping_family, 0);
        assert_eq!(head.to_bytes().unwrap(), packets[0], "{}", c.file);
        assert_eq!(
            head.codec_config(0).unwrap().audio.unwrap().channels,
            c.channels
        );
        assert!(packets[1].starts_with(b"OpusTags"));

        let audio = &packets[2..];
        let sizes: Vec<usize> = audio.iter().map(|p| p.len()).collect();
        assert_eq!(sizes, c.sizes, "{}", c.file);
        for p in audio {
            let parsed = parse_packet(p).unwrap();
            assert_eq!(parsed.toc.stereo, c.channels == 2, "{}", c.file);
            // ffprobe の packet duration（最後のパケットは末尾の切り詰めで短く表示される）
            assert_eq!(parsed.samples(), c.frame_samples, "{}", c.file);
            assert_eq!(packet_samples(p).unwrap(), c.frame_samples);
            assert_eq!(frame_count(p).unwrap(), parsed.frames.len());
            let total: usize = parsed.frames.iter().map(|f| f.len()).sum();
            assert!(total + parsed.padding < p.len());
        }
    }
}

#[test]
fn corrupted_real_input_never_panics() {
    let packets = ogg_packets(&data("opus_mono_60ms.opus"));
    for p in &packets[2..] {
        for cut in 0..p.len() {
            let _ = parse_packet(&p[..cut]);
        }
        for i in 0..p.len().min(8) {
            let mut m = p.clone();
            m[i] ^= 0xFF;
            let _ = parse_packet(&m);
        }
    }
    for cut in 0..packets[0].len() {
        assert!(OpusHead::parse(&packets[0][..cut]).is_err());
    }
}
