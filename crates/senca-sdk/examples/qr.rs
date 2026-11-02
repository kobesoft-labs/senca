//! 設定文字列を検査して QR コードの画像にする（管理画面・API と同じ処理）。
//!
//! `qr <config-or-text> <out.png|out.svg> [scale]`
//!
//! 設定文字列（`senca://`・`srt://`・`rtmp://`・`https://`）なら検査して正規形にしてから符号化する。

use senca_sdk::IngestUri;
use senca_sdk::qr::QrCode;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: qr <config-or-text> <out.png|out.svg> [scale]");
        std::process::exit(2);
    }
    let text = match IngestUri::parse(&args[1]) {
        Ok(u) if args[1].starts_with("senca:") => u.to_senca_uri(),
        Ok(_) => args[1].clone(),
        Err(e) if args[1].contains("://") => return Err(format!("invalid config: {e}").into()),
        Err(_) => args[1].clone(),
    };
    let q = QrCode::encode(&text)?;
    let scale = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    if args[2].ends_with(".svg") {
        std::fs::write(&args[2], q.to_svg(4))?;
    } else {
        std::fs::write(&args[2], q.to_png(scale, 4))?;
    }
    println!(
        "version={} mask={} size={} bytes={}",
        q.version(),
        q.mask(),
        q.size(),
        text.len()
    );
    Ok(())
}
