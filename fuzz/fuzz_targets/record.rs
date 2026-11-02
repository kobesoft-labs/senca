//! 録画の索引（JSON Lines）の読み込みと VOD playlist の書き出し。
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(idx) = senca_record::parse(data) {
        let _ = (idx.duration_ms(), idx.bytes());
        let _ = idx.vod_playlist(|s| format!("{s:?}").len().to_string(), Some("init.mp4".into()));
    }
});
