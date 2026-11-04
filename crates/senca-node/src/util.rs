//! 小物。

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn rfc3339(ms: i64) -> String {
    senca_core::rfc3339::format_millis(ms)
}

/// 同期の文脈（API のスレッド・専用のスレッド・tokio のワーカー）から非同期の仕事を待つ。
/// ワーカーの上では `block_in_place` で他の仕事を逃がしてから待つ。
pub fn block_on<F: std::future::Future>(rt: &tokio::runtime::Handle, f: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(_) => tokio::task::block_in_place(|| rt.block_on(f)),
        Err(_) => rt.block_on(f),
    }
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

/// `senca-node reset-password` の HTTP（平文の HTTP/1.1、1 要求ごとに接続）。応答の状態と JSON。
fn plain_http(
    api: &str,
    token: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<&serde_json::Value>,
) -> Result<(u16, serde_json::Value), String> {
    use std::io::{Read, Write};
    let rest = api.strip_prefix("http://").ok_or("--api は http:// の URL にしてください（ノードの手元の待ち受け）")?;
    let hp = rest.split('/').next().unwrap_or(rest);
    let mut s = std::net::TcpStream::connect(hp).map_err(|e| format!("{hp}: {e}"))?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let b = body.map(|v| v.to_string()).unwrap_or_default();
    let mut req = format!("{method} /api/v1{path} HTTP/1.1\r\nHost: {hp}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {}\r\n", b.len());
    if body.is_some() {
        req.push_str("Content-Type: application/json\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&b);
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    s.read_to_end(&mut out).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("応答の形が違います")?;
    let status: u16 = head.split(' ').nth(1).and_then(|x| x.parse().ok()).ok_or("応答の形が違います")?;
    Ok((status, serde_json::from_str(body).unwrap_or(serde_json::Value::Null)))
}

/// 利用者のパスワードを仮のものにし、MFA を外し、無効なら有効に戻す。`create` なら、いなければ管理者として作る。
/// 仮のパスワードを返す。
pub fn reset_password_via_api(api: &str, token: &str, username: &str, create: bool) -> Result<String, String> {
    let fail = |what: &str, st: u16, v: &serde_json::Value| format!("{what}: HTTP {st} {}", v.get("title").and_then(|x| x.as_str()).unwrap_or(""));
    let mut cursor: Option<String> = None;
    let mut found = None;
    loop {
        let q = cursor.as_ref().map(|c| format!("&cursor={c}")).unwrap_or_default();
        let (st, v) = plain_http(api, token, "GET", &format!("/users?limit=1000{q}"), &[], None)?;
        if st != 200 {
            return Err(fail("GET /users", st, &v));
        }
        if let Some(items) = v.get("items").and_then(|x| x.as_array()) {
            found = items.iter().find(|u| u.get("username").and_then(|x| x.as_str()).is_some_and(|n| n.eq_ignore_ascii_case(username))).cloned();
        }
        cursor = v.get("next_cursor").and_then(|x| x.as_str()).map(str::to_string);
        if found.is_some() || cursor.is_none() {
            break;
        }
    }
    let user = match found {
        Some(u) => u,
        None if create => {
            let (st, v) = plain_http(api, token, "POST", "/users", &[], Some(&serde_json::json!({ "username": username, "roles": ["admin"] })))?;
            if st != 201 {
                return Err(fail("POST /users", st, &v));
            }
            v
        }
        None => return Err(format!("利用者 {username} がいません（作るなら --create-admin）")),
    };
    let id = user.get("id").and_then(|x| x.as_str()).ok_or("利用者の ID がありません")?.to_string();
    if user.get("disabled").and_then(|x| x.as_bool()) == Some(true) {
        let g = user.get("generation").and_then(|x| x.as_u64()).unwrap_or(0);
        let (st, v) =
            plain_http(api, token, "PATCH", &format!("/users/{id}"), &[("If-Match", format!("\"{g}\""))], Some(&serde_json::json!({ "disabled": false })))?;
        if st != 200 {
            return Err(fail("PATCH /users/{id}", st, &v));
        }
    }
    let (st, v) = plain_http(api, token, "DELETE", &format!("/users/{id}/mfa"), &[], None)?;
    if st != 204 {
        return Err(fail("DELETE /users/{id}/mfa", st, &v));
    }
    let (st, v) = plain_http(api, token, "POST", &format!("/users/{id}/password"), &[], Some(&serde_json::json!({})))?;
    if st != 200 {
        return Err(fail("POST /users/{id}/password", st, &v));
    }
    v.get("temporary_password").and_then(|x| x.as_str()).map(str::to_string).ok_or_else(|| "仮のパスワードが返りませんでした".into())
}
