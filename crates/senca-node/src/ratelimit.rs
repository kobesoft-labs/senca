//! 要求の量の上限（トークンバケット）。超えたら 429（`RATE_LIMITED`、`Retry-After`）。
//!
//! - 送り元の IP ごと（`/api/v1` の全体。認証の前に数える）
//! - 主体ごと（API トークンの ID・JWT の subject。認証の後に数える）
//! - 認証の失敗（IP ごと、1 分あたり）。使い切った IP は、正しい資格情報でも 429（総当たりを止める）
//! - 接続の開始（IP ごと: `GET /signaling`・`POST /whip`・`POST /whep`・`GET /api/v1/events`）
//!
//! どれも 0 で無効。表は上限（`MAX_KEYS`）を超えたら、満タンに戻ったものから捨てる（無制限に増やさない）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 鍵の数の上限（超えたら古い・満タンのものを捨てる）。
const MAX_KEYS: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// 1 秒あたりに戻る量。
    pub per_sec: f64,
    /// 貯められる量（一度に通せる数）。
    pub burst: f64,
}

impl Rate {
    pub fn per_second(n: u32) -> Option<Rate> {
        (n > 0).then(|| Rate { per_sec: n as f64, burst: (n as f64 * 2.0).max(1.0) })
    }

    pub fn per_minute(n: u32) -> Option<Rate> {
        (n > 0).then(|| Rate { per_sec: n as f64 / 60.0, burst: n as f64 })
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

/// 1 種類の上限（鍵ごとのバケット）。
pub struct Limiter {
    rate: Option<Rate>,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Limiter {
    pub fn new(rate: Option<Rate>) -> Limiter {
        Limiter { rate, buckets: Mutex::new(HashMap::new()) }
    }

    #[cfg(test)]
    pub fn enabled(&self) -> bool {
        self.rate.is_some()
    }

    fn refill(rate: Rate, b: &mut Bucket, now: Instant) {
        let dt = now.saturating_duration_since(b.at).as_secs_f64();
        b.tokens = (b.tokens + dt * rate.per_sec).min(rate.burst);
        b.at = now;
    }

    /// 1 つ使う。足りなければ、次に使えるまでの時間。
    pub fn take(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let Some(rate) = self.rate else { return Ok(()) };
        let mut m = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= MAX_KEYS && !m.contains_key(key) {
            Self::prune(rate, &mut m, now);
        }
        let b = m.entry(key.to_string()).or_insert(Bucket { tokens: rate.burst, at: now });
        Self::refill(rate, b, now);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(Self::wait(rate, b.tokens))
        }
    }

    /// 使わずに、使い切っているかだけを見る（認証の失敗の上限: 使い切った IP を断る）。
    pub fn exhausted(&self, key: &str, now: Instant) -> Option<Duration> {
        let rate = self.rate?;
        let mut m = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let b = m.get_mut(key)?;
        Self::refill(rate, b, now);
        (b.tokens < 1.0).then(|| Self::wait(rate, b.tokens))
    }

    fn wait(rate: Rate, tokens: f64) -> Duration {
        Duration::from_secs_f64(((1.0 - tokens) / rate.per_sec).max(0.0))
    }

    /// 満タンに戻ったものを捨てる。それでも多ければ、いちばん古いものから半分を捨てる。
    fn prune(rate: Rate, m: &mut HashMap<String, Bucket>, now: Instant) {
        m.retain(|_, b| {
            Self::refill(rate, b, now);
            b.tokens < rate.burst
        });
        if m.len() >= MAX_KEYS {
            let mut v: Vec<(String, Instant)> = m.iter().map(|(k, b)| (k.clone(), b.at)).collect();
            v.sort_by_key(|x| x.1);
            for (k, _) in v.into_iter().take(MAX_KEYS / 2) {
                m.remove(&k);
            }
        }
    }
}

/// ノードの上限の全体。
pub struct RateLimits {
    pub ip: Limiter,
    pub principal: Limiter,
    pub auth_failures: Limiter,
    pub connects: Limiter,
}

impl RateLimits {
    pub fn from_config(c: &crate::config::Config) -> RateLimits {
        RateLimits {
            ip: Limiter::new(Rate::per_second(c.rate_limit_ip)),
            principal: Limiter::new(Rate::per_second(c.rate_limit_token)),
            auth_failures: Limiter::new(Rate::per_minute(c.rate_limit_auth_failures)),
            connects: Limiter::new(Rate::per_second(c.rate_limit_connect)),
        }
    }
}

/// `Retry-After` の秒（切り上げ、1 以上）。
pub fn retry_after_secs(d: Duration) -> u64 {
    (d.as_secs_f64().ceil() as u64).max(1)
}

/// 送り元のアドレス（`ip:port`・`[v6]:port`）から IP の部分。
pub fn peer_ip(peer: &str) -> &str {
    if let Some(r) = peer.strip_prefix('[') {
        return r.split(']').next().unwrap_or(r);
    }
    match peer.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => peer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_refills_at_the_rate() {
        let l = Limiter::new(Some(Rate { per_sec: 2.0, burst: 3.0 }));
        let t = Instant::now();
        for _ in 0..3 {
            assert!(l.take("a", t).is_ok());
        }
        let w = l.take("a", t).unwrap_err();
        assert!(w <= Duration::from_millis(500) && w > Duration::from_millis(400), "{w:?}");
        // 別の鍵は別のバケット
        assert!(l.take("b", t).is_ok());
        assert!(l.take("a", t + Duration::from_millis(500)).is_ok());
        assert!(l.take("a", t + Duration::from_millis(500)).is_err());
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1500)), 2);
    }

    #[test]
    fn disabled_and_exhausted() {
        let off = Limiter::new(Rate::per_second(0));
        assert!(!off.enabled());
        assert!((0..1000).all(|_| off.take("x", Instant::now()).is_ok()));
        let l = Limiter::new(Rate::per_minute(2));
        let t = Instant::now();
        assert!(l.exhausted("ip", t).is_none());
        l.take("ip", t).unwrap();
        l.take("ip", t).unwrap();
        let w = l.exhausted("ip", t).unwrap();
        assert!(w > Duration::from_secs(29) && w <= Duration::from_secs(30), "{w:?}");
    }

    #[test]
    fn peer_ips() {
        assert_eq!(peer_ip("127.0.0.1:5000"), "127.0.0.1");
        assert_eq!(peer_ip("[::1]:5000"), "::1");
        assert_eq!(peer_ip("10.0.0.1"), "10.0.0.1");
    }

    #[test]
    fn table_is_bounded() {
        let l = Limiter::new(Rate::per_second(1));
        let t = Instant::now();
        for i in 0..(MAX_KEYS + 10) {
            let _ = l.take(&format!("k{i}"), t);
        }
        assert!(l.buckets.lock().unwrap().len() <= MAX_KEYS);
    }
}
