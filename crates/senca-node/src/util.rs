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
