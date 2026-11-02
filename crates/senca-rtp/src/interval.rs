//! RTCP の送信間隔（RFC 3550 6.3.1・附属書 A.7）。
//!
//! 乱数は呼び出し側から渡す（sans-io）。

use std::time::Duration;

/// 間隔の計算に使う参加者の状態。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntervalParams {
    /// 参加者の数（自分を含む）。
    pub members: u32,
    /// 送信者の数（自分を含む）。
    pub senders: u32,
    /// RTCP に使う帯域（octet/s。通常はセッション帯域の 5%）。
    pub rtcp_bandwidth: f64,
    /// 自分が最近送信したか。
    pub we_sent: bool,
    /// RTCP パケットの平均の大きさ（octet、UDP/IP ヘッダーを含む）。
    pub avg_rtcp_size: f64,
    /// 最初の送信か（最小間隔を半分にする）。
    pub initial: bool,
    /// 最小間隔を 360 / (セッション帯域 kbit/s) 秒に縮める（6.2 の任意の縮小）。`None` は 5 秒。
    pub reduced_minimum: Option<f64>,
}

/// 送信間隔を求める。`random` は [0, 1) の一様乱数（0.5〜1.5 倍の揺らぎに使う）。
pub fn rtcp_interval(p: &IntervalParams, random: f64) -> Duration {
    const RTCP_MIN_TIME: f64 = 5.0;
    const SENDER_FRACTION: f64 = 0.25;
    const COMPENSATION: f64 = std::f64::consts::E - 1.5;
    let mut min_time = p.reduced_minimum.unwrap_or(RTCP_MIN_TIME);
    if p.initial {
        min_time /= 2.0;
    }
    let mut n = p.members.max(1) as f64;
    let mut bw = p.rtcp_bandwidth.max(1.0);
    if p.senders as f64 <= p.members as f64 * SENDER_FRACTION {
        if p.we_sent {
            bw *= SENDER_FRACTION;
            n = p.senders.max(1) as f64;
        } else {
            bw *= 1.0 - SENDER_FRACTION;
            n -= p.senders as f64;
            n = n.max(1.0);
        }
    }
    let mut t = p.avg_rtcp_size.max(1.0) * n / bw;
    if t < min_time {
        t = min_time;
    }
    let r = random.clamp(0.0, 1.0);
    t = t * (r + 0.5) / COMPENSATION;
    Duration::from_secs_f64(t.clamp(0.0, 3600.0))
}

/// avg_rtcp_size の更新（6.3.3）: avg = 1/16 * size + 15/16 * avg。
pub fn update_avg_rtcp_size(avg: f64, packet_size: usize) -> f64 {
    packet_size as f64 / 16.0 + avg * 15.0 / 16.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> IntervalParams {
        IntervalParams {
            members: 2,
            senders: 1,
            rtcp_bandwidth: 2000.0,
            we_sent: false,
            avg_rtcp_size: 100.0,
            initial: false,
            reduced_minimum: None,
        }
    }

    #[test]
    fn minimum_and_randomization() {
        let p = base();
        // 小さな会議では最小 5 秒、揺らぎ 0.5〜1.5 倍を補正係数 e-1.5 で割る
        let lo = rtcp_interval(&p, 0.0).as_secs_f64();
        let hi = rtcp_interval(&p, 1.0).as_secs_f64();
        assert!((lo - 2.5 / 1.218281828).abs() < 1e-3, "{lo}");
        assert!((hi - 7.5 / 1.218281828).abs() < 1e-3, "{hi}");
        let init = rtcp_interval(&IntervalParams { initial: true, ..p }, 0.5).as_secs_f64();
        assert!((init - 2.5 / 1.218281828).abs() < 1e-3);
    }

    #[test]
    fn scales_with_members() {
        let p = IntervalParams {
            members: 10_000,
            senders: 1,
            ..base()
        };
        // 受信者側: 帯域の 75% を 9999 人で分ける
        let t = rtcp_interval(&p, 0.5).as_secs_f64();
        let want = 100.0 * 9999.0 / 1500.0 / 1.218281828;
        assert!((t - want).abs() < 0.01, "{t} {want}");
        // 送信者は 25% を送信者数で分ける
        let s = rtcp_interval(&IntervalParams { we_sent: true, ..p }, 0.5).as_secs_f64();
        assert!((s - 5.0 / 1.218281828).abs() < 1e-3);
    }

    #[test]
    fn avg_size() {
        assert_eq!(update_avg_rtcp_size(100.0, 260), 110.0);
    }
}
