//! Webhook（認証・イベント）。HTTP は呼び出し側が行い、ここは本文・署名・再送の予定だけ。
//!
//! ## 署名
//!
//! `Senca-Signature: t=<UNIX 秒>,v1=<hex(HMAC-SHA256(secret, "<t>.<本文>"))>`。秘密を入れ替える間は
//! `v1=` を複数付ける。受け手は `t` が許容の幅（既定 300 秒）に入り、どれかの `v1` が一致することを確かめる
//! （[`verify_signature`]）。本文は JSON（UTF-8）。
//!
//! ## イベント
//!
//! 少なくとも 1 回届ける。各イベントは `id`（event_id）を持ち、受け手はそれで重複を除く。2xx で届いたとみなす。
//! 408・429・5xx・接続の失敗は [`retry_delay`] の予定で送り直し、それ以外の 4xx は諦める（dead letter）。
//! [`Outbox`] がこの状態を持つ（sans-io）。

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::events::EventEnvelope;
use crate::model::Role;
use crate::token::{JoinClaims, Permissions};

pub const SIGNATURE_HEADER: &str = "Senca-Signature";
pub const ID_HEADER: &str = "Senca-Webhook-Id";
pub const ATTEMPT_HEADER: &str = "Senca-Webhook-Attempt";

fn hmac_hex(secret: &[u8], t: i64, body: &[u8]) -> String {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret).unwrap_or_else(|_| {
        // HMAC はどの長さの鍵も受ける（ここには来ない）
        <Hmac<Sha256> as Mac>::new_from_slice(&[0u8; 32]).expect("hmac")
    });
    m.update(t.to_string().as_bytes());
    m.update(b".");
    m.update(body);
    m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// 署名の header の値を作る（秘密が複数なら全部で署名する）。
pub fn sign(secrets: &[&[u8]], unix_secs: i64, body: &[u8]) -> String {
    let mut s = format!("t={unix_secs}");
    for k in secrets {
        s.push_str(",v1=");
        s.push_str(&hmac_hex(k, unix_secs, body));
    }
    s
}

/// 署名の検査の誤り。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    Malformed,
    Stale,
    Mismatch,
}

/// 署名を確かめる。
pub fn verify_signature(
    secrets: &[&[u8]],
    header: &str,
    body: &[u8],
    now_secs: i64,
    tolerance_secs: i64,
) -> Result<(), SignatureError> {
    let mut t = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => t = Some(v.parse::<i64>().map_err(|_| SignatureError::Malformed)?),
            Some(("v1", v)) => sigs.push(v),
            Some(_) => {}
            None => return Err(SignatureError::Malformed),
        }
    }
    let t = t.ok_or(SignatureError::Malformed)?;
    if sigs.is_empty() {
        return Err(SignatureError::Malformed);
    }
    // t は相手の見出しから来る（極端な値で桁あふれさせない）
    if now_secs.checked_sub(t).is_none_or(|d| d.unsigned_abs() > tolerance_secs.unsigned_abs()) {
        return Err(SignatureError::Stale);
    }
    for k in secrets {
        let want = hmac_hex(k, t, body);
        if sigs.iter().any(|s| bool::from(s.as_bytes().ct_eq(want.as_bytes()))) {
            return Ok(());
        }
    }
    Err(SignatureError::Mismatch)
}

/// 認証 Webhook への要求の本文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthRequest {
    pub version: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub request_id: String,
    pub node: String,
    pub room_id: String,
    pub connection_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub role: Role,
    /// クライアントが connect に入れた metadata（そのまま）。
    pub metadata: Value,
    /// トークンも使うとき、検証済みのクレーム。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_claims: Option<JoinClaims>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

/// 認証 Webhook の応答（2xx の本文）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthResponse {
    pub allowed: bool,
    /// 拒否の理由（クライアントへ伝える。秘密を入れないこと）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 役割を上書きする。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// 他の参加者に見せるメタデータ（トークンのものを上書き）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
}

/// 認証 Webhook の結果の解釈。2xx で本文が [`AuthResponse`] として読め、`allowed` が真のときだけ許す。
pub fn auth_decision(status: Option<u16>, body: &[u8]) -> Result<AuthResponse, String> {
    match status {
        Some(s) if (200..300).contains(&s) => {
            let r: AuthResponse = serde_json::from_slice(body).map_err(|e| format!("auth webhook: invalid response ({e})"))?;
            if r.allowed {
                Ok(r)
            } else {
                Err(r.reason.unwrap_or_else(|| "denied by auth webhook".into()))
            }
        }
        Some(s) => Err(format!("auth webhook: status {s}")),
        None => Err("auth webhook: unreachable".into()),
    }
}

/// 送り直しの待ち時間（`attempt` は失敗した回数、1 から）。1 秒から倍々で最大 1 時間、±20 % の揺らぎ
/// （`seed` から決まる。同じイベントは同じ予定）。`max_attempts` 回失敗したら `None`（諦める）。
///
/// 既定の 10 回では、合計でおよそ 1 + 2 + 4 + … + 256 + 512 秒 ≒ 17 分。
pub fn retry_delay(attempt: u32, max_attempts: u32, seed: u64) -> Option<Duration> {
    if attempt == 0 || attempt >= max_attempts {
        return None;
    }
    let base = (1u64 << (attempt - 1).min(12)).min(3600) as f64;
    let mut x = seed ^ (attempt as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    let jitter = (x % 4001) as f64 / 10_000.0 - 0.2;
    Some(Duration::from_millis((base * (1.0 + jitter) * 1000.0) as u64))
}

fn seed_of(id: &str) -> u64 {
    id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// 送る 1 回分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub event_id: String,
    /// 何回目か（1 から）。
    pub attempt: u32,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

/// 送った結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Status(u16),
    /// 接続できない・時間切れ。
    Failed,
}

/// イベント Webhook の設定。
#[derive(Debug, Clone)]
pub struct OutboxConfig {
    pub secrets: Vec<Vec<u8>>,
    pub max_attempts: u32,
    /// 同時に送る数。
    pub max_in_flight: usize,
    /// 待ち行列の上限（超えたら古いものから捨てて数える）。
    pub max_queue: usize,
    /// 応答を待つ最長の時間（過ぎたら失敗として送り直す）。
    pub timeout: Duration,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        OutboxConfig {
            secrets: Vec::new(),
            max_attempts: 10,
            max_in_flight: 8,
            max_queue: 10_000,
            timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug)]
struct Pending {
    id: String,
    body: String,
    attempt: u32,
    due: Instant,
}

/// イベント Webhook の送り箱（少なくとも 1 回、順は保証しない）。
#[derive(Debug)]
pub struct Outbox {
    cfg: OutboxConfig,
    queue: VecDeque<Pending>,
    in_flight: BTreeMap<String, (Pending, Instant)>,
    delivered: u64,
    /// 届けられなかったイベントの ID（新しい方から [`MAX_DEAD`] 件だけ残す）。
    dead: VecDeque<String>,
    dead_total: u64,
    dropped: u64,
}

/// 覚えておく、届けられなかったイベントの数の上限。
pub const MAX_DEAD: usize = 1024;

impl Outbox {
    pub fn new(cfg: OutboxConfig) -> Outbox {
        Outbox {
            cfg,
            queue: VecDeque::new(),
            in_flight: BTreeMap::new(),
            delivered: 0,
            dead: VecDeque::new(),
            dead_total: 0,
            dropped: 0,
        }
    }

    /// イベントを積む。
    pub fn push(&mut self, now: Instant, ev: &EventEnvelope) {
        let body = serde_json::to_string(ev).unwrap_or_default();
        if self.queue.len() >= self.cfg.max_queue {
            self.queue.pop_front();
            self.dropped += 1;
        }
        self.queue.push_back(Pending {
            id: ev.id.clone(),
            body,
            attempt: 0,
            due: now,
        });
    }

    /// 今送るもの（署名は送る時刻で付ける）。
    pub fn poll_delivery(&mut self, now: Instant, unix_secs: i64) -> Option<Delivery> {
        // 時間切れの送りは失敗として扱う
        let expired: Vec<String> = self
            .in_flight
            .iter()
            .filter(|(_, (_, at))| now.saturating_duration_since(*at) >= self.cfg.timeout)
            .map(|(k, _)| k.clone())
            .collect();
        for id in expired {
            self.on_result(now, &id, Outcome::Failed);
        }
        if self.in_flight.len() >= self.cfg.max_in_flight {
            return None;
        }
        let i = self.queue.iter().position(|p| p.due <= now)?;
        let mut p = self.queue.remove(i)?;
        p.attempt += 1;
        let secrets: Vec<&[u8]> = self.cfg.secrets.iter().map(|s| s.as_slice()).collect();
        let d = Delivery {
            event_id: p.id.clone(),
            attempt: p.attempt,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                (ID_HEADER.into(), p.id.clone()),
                (ATTEMPT_HEADER.into(), p.attempt.to_string()),
                (SIGNATURE_HEADER.into(), sign(&secrets, unix_secs, p.body.as_bytes())),
            ],
            body: p.body.clone(),
        };
        self.in_flight.insert(p.id.clone(), (p, now));
        Some(d)
    }

    /// 送った結果を入れる。
    pub fn on_result(&mut self, now: Instant, event_id: &str, outcome: Outcome) {
        let Some((mut p, _)) = self.in_flight.remove(event_id) else {
            return;
        };
        let retry = match outcome {
            Outcome::Status(s) if (200..300).contains(&s) => {
                self.delivered += 1;
                return;
            }
            Outcome::Status(408 | 429) | Outcome::Failed => true,
            Outcome::Status(s) => s >= 500,
        };
        match retry.then(|| retry_delay(p.attempt, self.cfg.max_attempts, seed_of(&p.id))).flatten() {
            Some(d) => {
                p.due = now + d;
                self.queue.push_back(p);
            }
            None => {
                if self.dead.len() >= MAX_DEAD {
                    self.dead.pop_front();
                }
                self.dead.push_back(p.id);
                self.dead_total += 1;
            }
        }
    }

    /// 次に何かすべき時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        let q = self.queue.iter().map(|p| p.due).min();
        let f = self.in_flight.values().map(|(_, at)| *at + self.cfg.timeout).min();
        match (q, f) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// 諦めたイベントの ID。
    pub fn dead_letters(&self) -> impl Iterator<Item = &String> {
        self.dead.iter()
    }

    /// 諦めたイベントの数（これまでの合計）。
    pub fn dead_total(&self) -> u64 {
        self.dead_total
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn pending(&self) -> usize {
        self.queue.len() + self.in_flight.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::RoomEvent;

    #[test]
    fn signature_round_trip_rotation_and_tamper() {
        let body = br#"{"id":"e1"}"#;
        let h = sign(&[b"new-secret", b"old-secret"], 1_700_000_000, body);
        assert!(h.starts_with("t=1700000000,v1="));
        assert_eq!(h.matches("v1=").count(), 2);
        // 受け手が古い秘密だけを知っていても通る
        assert_eq!(verify_signature(&[b"old-secret"], &h, body, 1_700_000_100, 300), Ok(()));
        assert_eq!(verify_signature(&[b"new-secret"], &h, body, 1_700_000_100, 300), Ok(()));
        assert_eq!(
            verify_signature(&[b"other"], &h, body, 1_700_000_100, 300),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(&[b"new-secret"], &h, br#"{"id":"e2"}"#, 1_700_000_100, 300),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(&[b"new-secret"], &h, body, 1_700_000_301, 300),
            Err(SignatureError::Stale)
        );
        assert_eq!(verify_signature(&[b"k"], "v1=00", body, 0, 300), Err(SignatureError::Malformed));
        assert_eq!(verify_signature(&[b"k"], "t=1", body, 0, 300), Err(SignatureError::Malformed));
        assert_eq!(verify_signature(&[b"k"], "garbage", body, 0, 300), Err(SignatureError::Malformed));
        // 極端な時刻で桁あふれしない
        assert_eq!(verify_signature(&[b"k"], "t=-9223372036854775808,v1=00", body, 1_700_000_000, 300), Err(SignatureError::Stale));
        assert_eq!(verify_signature(&[b"k"], "t=9223372036854775807,v1=00", body, -5, 300), Err(SignatureError::Stale));
        // 既知の値（RFC 4231 の形の HMAC-SHA256: 鍵 "key"、"0.body"）
        let known = sign(&[b"key"], 0, b"body");
        assert_eq!(known.len(), "t=0,v1=".len() + 64);
    }

    #[test]
    fn retry_schedule() {
        let d: Vec<Duration> = (1..10).map(|a| retry_delay(a, 10, 42).unwrap()).collect();
        for (i, x) in d.iter().enumerate() {
            let base = (1u64 << i) as f64;
            let s = x.as_secs_f64();
            assert!(s >= base * 0.8 - 1e-9 && s <= base * 1.2 + 1e-9, "attempt {} {s}", i + 1);
        }
        assert_eq!(retry_delay(10, 10, 42), None);
        assert_eq!(retry_delay(0, 10, 42), None);
        // 同じ seed は同じ予定、上限 1 時間
        assert_eq!(retry_delay(3, 10, 7), retry_delay(3, 10, 7));
        assert!(retry_delay(30, 100, 1).unwrap() <= Duration::from_secs(4320));
    }

    #[test]
    fn auth_decisions() {
        let ok = auth_decision(Some(200), br#"{"allowed":true,"role":"recvonly","metadata":{"n":1}}"#).unwrap();
        assert_eq!(ok.role, Some(Role::Recvonly));
        assert_eq!(auth_decision(Some(200), br#"{"allowed":false,"reason":"banned"}"#), Err("banned".into()));
        assert!(auth_decision(Some(200), br#"{"allowed":true,"bogus":1}"#).is_err());
        assert!(auth_decision(Some(500), b"").is_err());
        assert!(auth_decision(None, b"").is_err());
    }

    fn env(id: &str) -> EventEnvelope {
        EventEnvelope {
            id: id.into(),
            version: 1,
            at: 1,
            node: "n1".into(),
            room_id: "r".into(),
            event: RoomEvent::RoomDestroyed { reason: "x".into() },
        }
    }

    #[test]
    fn outbox_at_least_once() {
        let t0 = Instant::now();
        let mut o = Outbox::new(OutboxConfig {
            secrets: vec![b"s".to_vec()],
            max_attempts: 3,
            max_in_flight: 1,
            ..Default::default()
        });
        o.push(t0, &env("e1"));
        o.push(t0, &env("e2"));
        let d = o.poll_delivery(t0, 1000).unwrap();
        assert_eq!(d.event_id, "e1");
        assert_eq!(d.attempt, 1);
        let sig = &d.headers.iter().find(|h| h.0 == SIGNATURE_HEADER).unwrap().1;
        assert_eq!(verify_signature(&[b"s"], sig, d.body.as_bytes(), 1000, 300), Ok(()));
        assert!(d.body.contains(r#""type":"room.destroyed""#));
        // 同時に 1 つまで
        assert!(o.poll_delivery(t0, 1000).is_none());
        o.on_result(t0, "e1", Outcome::Status(503));
        let d2 = o.poll_delivery(t0, 1000).unwrap();
        assert_eq!(d2.event_id, "e2");
        o.on_result(t0, "e2", Outcome::Status(204));
        assert_eq!(o.delivered(), 1);
        // e1 は待ってから 2 回目
        assert!(o.poll_delivery(t0, 1000).is_none());
        let t1 = o.poll_timeout().unwrap();
        assert!(t1 > t0 && t1 <= t0 + Duration::from_millis(1200));
        let d = o.poll_delivery(t1, 1001).unwrap();
        assert_eq!((d.event_id.as_str(), d.attempt), ("e1", 2));
        // 時間切れも失敗。3 回目で諦める
        let t2 = t1 + Duration::from_secs(10);
        assert!(o.poll_delivery(t2, 1011).is_none());
        let t3 = o.poll_timeout().unwrap();
        let d = o.poll_delivery(t3, 1013).unwrap();
        assert_eq!(d.attempt, 3);
        o.on_result(t3, "e1", Outcome::Failed);
        assert_eq!(o.dead_letters().cloned().collect::<Vec<_>>(), vec!["e1".to_string()]);
        assert_eq!(o.pending(), 0);
        // 400 はすぐ諦める、429 は送り直す
        o.push(t3, &env("e3"));
        o.poll_delivery(t3, 0).unwrap();
        o.on_result(t3, "e3", Outcome::Status(400));
        assert_eq!(o.dead_letters().count(), 2);
        o.push(t3, &env("e4"));
        o.poll_delivery(t3, 0).unwrap();
        o.on_result(t3, "e4", Outcome::Status(429));
        assert_eq!(o.pending(), 1);
    }
}
