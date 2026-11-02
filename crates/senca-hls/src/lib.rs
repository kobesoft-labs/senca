//! HLS / LL-HLS（RFC 8216 と Apple の LL-HLS 拡張）。I/O を持たない。
//!
//! - [`media`] — media playlist の型・書き出し・厳格な解析
//! - [`multivariant`] — multivariant playlist の型・書き出し・解析（[`Playlist::parse`] で種類を自動判定）
//! - [`segmenter`] — AU から segment / part の境界を決める状態機械と、その状態からの playlist 生成・
//!   blocking reload の判定
//!
//! 時間はマイクロ秒の整数で持ち、書き出した playlist を読み直すと同じ値になる。

mod attr;
pub mod error;
mod lines;
pub mod media;
pub mod multivariant;
pub mod segmenter;

pub use error::HlsError;
pub use lines::ParseLimits;
pub use media::{ByteRange, Map, MediaPlaylist, MediaSegment, OpenSegment, Part, PlaylistType, PreloadHint, PreloadHintType, RenditionReport, ServerControl};
pub use multivariant::{ClosedCaptions, MediaType, MultivariantPlaylist, Playlist, Rendition, VariantStream};
pub use segmenter::{BlockingDecision, HlsQuery, PlaylistKind, SegmentEvent, Segmenter, SegmenterConfig, SimpleUris, SkipRequest, UriScheme};

#[cfg(test)]
mod fuzz {
    //! 壊れた playlist でパニック・無限ループしないこと。
    use super::*;

    const SEEDS: &[&str] = &[
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:4\n#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0\n#EXT-X-PART-INF:PART-TARGET=0.5\n#EXT-X-MEDIA-SEQUENCE:5\n#EXT-X-MAP:URI=\"i.mp4\",BYTERANGE=\"10@0\"\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00.000Z\n#EXT-X-PART:DURATION=0.5,URI=\"p.0\",INDEPENDENT=YES\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:100@10\ns.mp4\n#EXT-X-DISCONTINUITY\n#EXT-X-PART:DURATION=0.5,URI=\"p.1\"\n#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"p.2\"\n#EXT-X-RENDITION-REPORT:URI=\"r.m3u8\",LAST-MSN=6,LAST-PART=1\n",
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"n\",DEFAULT=YES,URI=\"a.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=100,CODECS=\"avc1.64001f,mp4a.40.2\",RESOLUTION=640x360,FRAME-RATE=30.000,AUDIO=\"a\"\nv.m3u8\n",
    ];

    #[test]
    fn mutated_playlists() {
        let mut x = 0xdead_beef_cafe_f00du64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for seed in SEEDS {
            assert!(Playlist::parse(seed.as_bytes()).is_ok(), "{seed}");
            let b = seed.as_bytes();
            for n in 0..b.len() {
                let _ = Playlist::parse(&b[..n]);
            }
        }
        for _ in 0..30_000 {
            let mut v = SEEDS[(rnd() % SEEDS.len() as u64) as usize].as_bytes().to_vec();
            for _ in 0..1 + rnd() % 5 {
                let i = (rnd() % v.len() as u64) as usize;
                match rnd() % 5 {
                    0 => v[i] = rnd() as u8,
                    1 => {
                        v.remove(i);
                    }
                    2 => v.insert(i, b"\",=:\n\r#@.0-"[(rnd() % 11) as usize]),
                    3 => v.truncate(i),
                    _ => {
                        let t = v[i..].to_vec();
                        v.extend_from_slice(&t);
                    }
                }
                if v.is_empty() {
                    break;
                }
            }
            if let Ok(p) = Playlist::parse(&v) {
                // 読めたものは書き出して読み直すと同じ
                match p {
                    Playlist::Media(m) => {
                        if let Ok(t) = m.to_m3u8() {
                            let mut again = MediaPlaylist::parse(t.as_bytes()).unwrap();
                            again.version = m.version;
                            assert_eq!(again, *m, "{t}");
                        }
                    }
                    Playlist::Multivariant(m) => {
                        if let Ok(t) = m.to_m3u8() {
                            let mut again = MultivariantPlaylist::parse(t.as_bytes()).unwrap();
                            again.version = m.version;
                            assert_eq!(again, m, "{t}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn limits() {
        let mut s = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:2\n");
        s.push_str(&"#EXTINF:2,\na.ts\n".repeat(150_000));
        assert_eq!(MediaPlaylist::parse(s.as_bytes()), Err(HlsError::TooLarge));
        let long = format!("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\n{}\n", "a".repeat(20_000));
        assert_eq!(MediaPlaylist::parse(long.as_bytes()), Err(HlsError::TooLarge));
    }
}
