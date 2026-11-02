//! HLS の playlist（multivariant・media）の解析。解析できたものは書き出して読み直す。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_hls::{MediaPlaylist, MultivariantPlaylist, Playlist};

fuzz_target!(|data: &[u8]| {
    let _ = Playlist::parse(data);
    if let Ok(m) = MultivariantPlaylist::parse(data) {
        if let Ok(s) = m.to_m3u8() {
            let _ = MultivariantPlaylist::parse(s.as_bytes());
        }
    }
    let _ = MediaPlaylist::parse(data);
    // 問い合わせ（_HLS_msn など）の解析
    if let Ok(s) = std::str::from_utf8(data) {
        let pairs: Vec<(&str, &str)> = s
            .split('&')
            .take(64)
            .map(|kv| kv.split_once('=').unwrap_or((kv, "")))
            .collect();
        let _ = senca_hls::HlsQuery::from_pairs(pairs);
    }
});
