//! playlist を読んで要約を出し、書き出し → 読み直しで同じになるかを確かめる（相互運用試験用）。
//!
//! 使い方: `playlist_check <file.m3u8>... [--rewrite]`
//! `--rewrite` を付けると `<file>.senca.m3u8` に書き出し結果を置く（FFmpeg で再生して確かめるため）。
//! 失敗があれば終了コード 1。

use senca_hls::{MediaPlaylist, MultivariantPlaylist, Playlist};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rewrite = args.iter().any(|a| a == "--rewrite");
    let mut ok = true;
    for path in args.iter().filter(|a| !a.starts_with("--")) {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                println!("FAIL {path}: {e}");
                ok = false;
                continue;
            }
        };
        let r: Result<(String, String), String> = match Playlist::parse(&data) {
            Err(e) => Err(format!("parse: {e}")),
            Ok(Playlist::Media(m)) => (|| {
                let text = m.to_m3u8().map_err(|e| format!("write: {e}"))?;
                let mut again = MediaPlaylist::parse(text.as_bytes()).map_err(|e| format!("reparse: {e}"))?;
                again.version = m.version;
                if again != *m {
                    return Err("roundtrip mismatch".into());
                }
                let parts: usize = m.segments.iter().map(|s| s.parts.len()).sum();
                Ok((
                    format!(
                        "media segments={} target={} msn={} parts={} map={} byterange={} endlist={} type={:?}",
                        m.segments.len(),
                        m.target_duration,
                        m.media_sequence,
                        parts,
                        m.segments.iter().any(|s| s.map.is_some()),
                        m.segments.iter().any(|s| s.byte_range.is_some()),
                        m.end_list,
                        m.playlist_type
                    ),
                    text,
                ))
            })(),
            Ok(Playlist::Multivariant(m)) => (|| {
                let text = m.to_m3u8().map_err(|e| format!("write: {e}"))?;
                let mut again = MultivariantPlaylist::parse(text.as_bytes()).map_err(|e| format!("reparse: {e}"))?;
                again.version = m.version;
                if again != m {
                    return Err("roundtrip mismatch".into());
                }
                Ok((format!("multivariant variants={} renditions={}", m.variants.len(), m.renditions.len()), text))
            })(),
        };
        match r {
            Ok((summary, text)) => {
                println!("OK {path}: {summary}");
                if rewrite && let Err(e) = std::fs::write(format!("{path}.senca.m3u8"), text) {
                    println!("FAIL {path}: {e}");
                    ok = false;
                }
            }
            Err(e) => {
                println!("FAIL {path}: {e}");
                ok = false;
            }
        }
    }
    std::process::exit(if ok { 0 } else { 1 });
}
