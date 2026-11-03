//! 小物。

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn rfc3339(ms: i64) -> String {
    senca_core::rfc3339::format_millis(ms)
}

/// 単調時計（ミリ秒、プロセス起動から）。
pub fn mono_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// ログの設定（`--log-json`・ノード ID）。起動の早いうちに [`init_log`]、ノード ID が分かったら [`set_log_node`]。
static LOG_JSON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LOG_NODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn init_log(json: bool) {
    LOG_JSON.store(json, std::sync::atomic::Ordering::Relaxed);
}

pub fn set_log_node(node: &str) {
    let _ = LOG_NODE.set(node.to_string());
}

pub fn log_json() -> bool {
    LOG_JSON.load(std::sync::atomic::Ordering::Relaxed)
}

/// 1 行のログ。`--log-json` なら標準出力に `{"ts", "node", "msg"}`（ノード ID がまだ無ければ `node` は null）、
/// そうでなければ標準エラーに `時刻 本文`。
pub fn log(msg: &str) {
    if log_json() {
        println!("{}", serde_json::json!({ "ts": rfc3339(now_ms()), "node": LOG_NODE.get(), "msg": msg }));
    } else {
        eprintln!("{} {msg}", rfc3339(now_ms()));
    }
}

/// 子プロセス（codec worker・capture worker）の標準エラーを渡す先。`--log-json` のときだけ JSON のログにまとめる
/// （それ以外は今までどおり親の標準エラーに引き継ぐ）。
pub fn worker_log_sink() -> Option<senca_codec_worker::pool::LogSink> {
    log_json().then(|| senca_codec_worker::pool::LogSink::new(log))
}

/// 要求 ID: クライアントの `X-Request-Id` が 64 文字以下の英数字と `-_.` なら使う。
pub fn request_id(client: Option<&str>) -> String {
    if let Some(c) = client
        && !c.is_empty()
        && c.len() <= 64
        && c.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return c.to_string();
    }
    senca_core::Id128::random().map(|i| format!("req_{i}")).unwrap_or_else(|_| "req_unknown".into())
}
