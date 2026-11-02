//! 送信側の帯域推定（draft-ietf-rmcat-gcc-02 の考え方、RFC 8698 も参照）。
//!
//! 相手の transport-wide-cc feedback（[`crate::twcc::SendHistory::on_feedback`] の結果）から:
//!
//! 1. **遅延勾配**: 送信時刻 5 ms ごとの packet の組で、組の間の到着間隔と送信間隔の差（遅延の変化）を積み上げ、
//!    平滑化した値の直近 20 組への回帰直線の傾きを見る（trendline）。傾き × 組の数 × 4 を適応する閾値 γ と比べ、
//!    過剰（overuse）・過少（underuse）・通常を決める。γ は |傾き| に向かって動く（上げ 0.0087、下げ 0.039、6〜600 ms）。
//! 2. **AIMD**: 過剰なら「相手に届いた速さ」（acked bitrate）の 0.85 倍に下げて保つ。通常なら上げる
//!    （リンク容量の見積もりから遠いときは毎秒 8 % の乗算、近いときは往復時間ごとに約 1 packet の加算）。
//!    上げるのは届いた速さの 1.5 倍まで（送るものが少ないときに推定だけが膨らまないように）。
//! 3. **損失**: feedback の未着の割合が 10 % を超えたら `(1 - 0.5 × 損失率)` 倍に下げ、2 % 未満なら毎秒 8 % まで上げる。
//!
//! 4. **探り（probe）**: 送り手が短い padding の塊（[`BandwidthEstimator::start_probe`]）を送り、その組の
//!    送った速さと届いた速さの小さい方（届いた速さが 9 割未満なら届いた速さの 0.95 倍）が今の推定より高ければ
//!    推定をそこまで上げる。大きく下げた後や、送るものが少ない（app-limited）ときの回復を速める。
//!
//! 推定 = min(遅延による値, 損失による値)。transport-wide-cc の feedback が 2 秒以上来ず REMB があれば REMB を使う。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::config::BweConfig;
use crate::twcc::PacketResult;

/// 遅延勾配の判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandwidthUsage {
    Normal,
    Underusing,
    Overusing,
}

const GROUP_SPAN: Duration = Duration::from_millis(5);
const WINDOW: usize = 20;
const SMOOTHING: f64 = 0.9;
const THRESHOLD_GAIN: f64 = 4.0;
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const BETA: f64 = 0.85;

#[derive(Debug, Clone, Copy)]
struct Group {
    first_send: Instant,
    last_send: Instant,
    last_arrival_us: i64,
    first_arrival_us: i64,
}

/// trendline の検出器。
#[derive(Debug)]
struct Trendline {
    current: Option<Group>,
    prev: Option<Group>,
    first_arrival_us: Option<i64>,
    accumulated: f64,
    smoothed: f64,
    history: VecDeque<(f64, f64)>,
    num_deltas: u32,
    threshold: f64,
    last_threshold_update_us: Option<i64>,
    prev_trend: f64,
    time_over_using: f64,
    overuse_counter: u32,
    state: BandwidthUsage,
}

impl Trendline {
    fn new() -> Self {
        Trendline {
            current: None,
            prev: None,
            first_arrival_us: None,
            accumulated: 0.0,
            smoothed: 0.0,
            history: VecDeque::with_capacity(WINDOW),
            num_deltas: 0,
            threshold: 12.5,
            last_threshold_update_us: None,
            prev_trend: 0.0,
            time_over_using: -1.0,
            overuse_counter: 0,
            state: BandwidthUsage::Normal,
        }
    }

    /// 届いた packet（到着順）を 1 つ入れる。
    fn on_packet(&mut self, send: Instant, arrival_us: i64) {
        match &mut self.current {
            None => {
                self.current = Some(Group {
                    first_send: send,
                    last_send: send,
                    first_arrival_us: arrival_us,
                    last_arrival_us: arrival_us,
                });
            }
            Some(g) => {
                if send < g.first_send {
                    return; // 並べ替わった古い packet
                }
                let same = send.duration_since(g.first_send) <= GROUP_SPAN
                    // burst: 到着が 5 ms 以内に続き、送信より詰まって届いたものも同じ組
                    || (arrival_us - g.last_arrival_us < 5000
                        && (arrival_us - g.last_arrival_us)
                            < send.saturating_duration_since(g.last_send).as_micros() as i64
                        && arrival_us - g.first_arrival_us < 100_000);
                if same {
                    if send > g.last_send {
                        g.last_send = send;
                    }
                    g.last_arrival_us = g.last_arrival_us.max(arrival_us);
                    return;
                }
                let done = *g;
                *g = Group {
                    first_send: send,
                    last_send: send,
                    first_arrival_us: arrival_us,
                    last_arrival_us: arrival_us,
                };
                if let Some(p) = self.prev {
                    let send_delta = done
                        .last_send
                        .saturating_duration_since(p.last_send)
                        .as_secs_f64()
                        * 1000.0;
                    let arrival_delta = (done.last_arrival_us - p.last_arrival_us) as f64 / 1000.0;
                    if arrival_delta >= 0.0 || arrival_delta > -3000.0 {
                        self.update(
                            arrival_delta - send_delta,
                            arrival_delta,
                            done.last_arrival_us,
                        );
                    }
                }
                self.prev = Some(done);
            }
        }
    }

    fn update(&mut self, delay_var_ms: f64, arrival_delta_ms: f64, arrival_us: i64) {
        // 大きすぎる変化（時計の飛びなど）は抑える
        let d = delay_var_ms.clamp(-1000.0, 1000.0);
        self.num_deltas = (self.num_deltas + 1).min(1000);
        let first = *self.first_arrival_us.get_or_insert(arrival_us);
        self.accumulated += d;
        self.smoothed = SMOOTHING * self.smoothed + (1.0 - SMOOTHING) * self.accumulated;
        self.history
            .push_back(((arrival_us - first) as f64 / 1000.0, self.smoothed));
        if self.history.len() > WINDOW {
            self.history.pop_front();
        }
        let trend = if self.history.len() == WINDOW {
            linear_fit_slope(&self.history).unwrap_or(self.prev_trend)
        } else {
            self.prev_trend
        };
        self.detect(trend, arrival_delta_ms.max(0.0), arrival_us);
    }

    fn detect(&mut self, trend: f64, ts_delta_ms: f64, now_us: i64) {
        if self.num_deltas < 2 {
            return;
        }
        let modified = self.num_deltas.min(60) as f64 * trend * THRESHOLD_GAIN;
        if modified > self.threshold {
            if self.time_over_using < 0.0 {
                self.time_over_using = ts_delta_ms / 2.0;
            } else {
                self.time_over_using += ts_delta_ms;
            }
            self.overuse_counter += 1;
            if self.time_over_using > 10.0 && self.overuse_counter > 1 && trend >= self.prev_trend {
                self.time_over_using = 0.0;
                self.overuse_counter = 0;
                self.state = BandwidthUsage::Overusing;
            }
        } else if modified < -self.threshold {
            self.time_over_using = -1.0;
            self.overuse_counter = 0;
            self.state = BandwidthUsage::Underusing;
        } else {
            self.time_over_using = -1.0;
            self.overuse_counter = 0;
            self.state = BandwidthUsage::Normal;
        }
        self.prev_trend = trend;
        // 閾値の適応
        let last = self.last_threshold_update_us.unwrap_or(now_us);
        if modified.abs() <= self.threshold + 15.0 {
            let k = if modified.abs() < self.threshold {
                K_DOWN
            } else {
                K_UP
            };
            let dt = ((now_us - last) as f64 / 1000.0).clamp(0.0, 100.0);
            self.threshold += k * (modified.abs() - self.threshold) * dt;
            self.threshold = self.threshold.clamp(6.0, 600.0);
        }
        self.last_threshold_update_us = Some(now_us);
    }
}

fn linear_fit_slope(points: &VecDeque<(f64, f64)>) -> Option<f64> {
    let n = points.len() as f64;
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
    let (mx, my) = (sx / n, sy / n);
    let (mut num, mut den) = (0.0, 0.0);
    for (x, y) in points {
        num += (x - mx) * (y - my);
        den += (x - mx) * (x - mx);
    }
    if den == 0.0 { None } else { Some(num / den) }
}

/// 相手に届いた速さ（到着時刻で 500 ms の窓）。
#[derive(Debug, Default)]
struct AckedBitrate {
    samples: VecDeque<(i64, usize)>,
}

impl AckedBitrate {
    fn on_packet(&mut self, arrival_us: i64, size: usize) {
        self.samples.push_back((arrival_us, size));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| arrival_us - t > 500_000)
            || self.samples.len() > 4096
        {
            self.samples.pop_front();
        }
    }

    fn bitrate(&self) -> Option<u64> {
        let (first, last) = (self.samples.front()?, self.samples.back()?);
        let span = (last.0 - first.0).max(0);
        if span < 150_000 || self.samples.len() < 5 {
            return None;
        }
        // 最初の packet は窓の外の時間に属するので除く
        let bytes: usize = self.samples.iter().skip(1).map(|(_, s)| *s).sum();
        Some((bytes as u64 * 8 * 1_000_000) / span as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateState {
    Hold,
    Increase,
    Decrease,
}

/// 送信側の帯域推定。
#[derive(Debug)]
pub struct BandwidthEstimator {
    cfg: BweConfig,
    trend: Trendline,
    acked: AckedBitrate,
    state: RateState,
    delay_rate: f64,
    loss_rate: f64,
    link_capacity: Option<f64>,
    link_var: f64,
    last_change: Option<Instant>,
    last_loss_decrease: Option<Instant>,
    last_loss_increase: Option<Instant>,
    loss_window: (u32, u32),
    loss_fraction: f64,
    rtt: Duration,
    remb: Option<(u64, Instant)>,
    last_twcc: Option<Instant>,
    avg_packet_bits: f64,
    next_probe_id: u32,
    probes: HashMap<u32, Probe>,
}

/// 探りの組の集計。
#[derive(Debug, Clone)]
struct Probe {
    started: Instant,
    first_send: Option<Instant>,
    last_send: Option<Instant>,
    first_arrival: Option<i64>,
    last_arrival: Option<i64>,
    bytes: usize,
    last_size: usize,
    first_size: usize,
    received: u32,
    done: bool,
}

impl BandwidthEstimator {
    pub fn new(cfg: BweConfig) -> Self {
        let start = cfg.start_bps.clamp(cfg.min_bps, cfg.max_bps) as f64;
        BandwidthEstimator {
            trend: Trendline::new(),
            acked: AckedBitrate::default(),
            state: RateState::Increase,
            delay_rate: start,
            loss_rate: cfg.max_bps as f64,
            link_capacity: None,
            link_var: 0.4,
            last_change: None,
            last_loss_decrease: None,
            last_loss_increase: None,
            loss_window: (0, 0),
            loss_fraction: 0.0,
            rtt: Duration::from_millis(100),
            remb: None,
            last_twcc: None,
            avg_packet_bits: 1200.0 * 8.0,
            next_probe_id: 1,
            probes: HashMap::new(),
            cfg,
        }
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt.clamp(Duration::from_millis(1), Duration::from_secs(5));
    }

    /// 今の推定（bit/s）。
    pub fn estimate(&self, now: Instant) -> u64 {
        let twcc_fresh = self
            .last_twcc
            .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(2));
        if !twcc_fresh && let Some((r, _)) = self.remb {
            return r.clamp(self.cfg.min_bps, self.cfg.max_bps);
        }
        (self.delay_rate.min(self.loss_rate) as u64).clamp(self.cfg.min_bps, self.cfg.max_bps)
    }

    /// 遅延勾配の判定（試験・観測用）。
    pub fn usage(&self) -> BandwidthUsage {
        self.trend.state
    }

    /// 相手に届いた速さ（分かれば）。
    pub fn acked_bitrate(&self) -> Option<u64> {
        self.acked.bitrate()
    }

    /// 直近の損失率。
    pub fn loss_fraction(&self) -> f64 {
        self.loss_fraction
    }

    /// 探りの組を始める。返した番号を、その組の packet の送信記録に付ける。
    pub fn start_probe(&mut self, now: Instant) -> u32 {
        let id = self.next_probe_id;
        self.next_probe_id = self.next_probe_id.wrapping_add(1).max(1);
        self.probes
            .retain(|_, p| now.saturating_duration_since(p.started) < Duration::from_secs(5));
        if self.probes.len() < 16 {
            self.probes.insert(
                id,
                Probe {
                    started: now,
                    first_send: None,
                    last_send: None,
                    first_arrival: None,
                    last_arrival: None,
                    bytes: 0,
                    last_size: 0,
                    first_size: 0,
                    received: 0,
                    done: false,
                },
            );
        }
        id
    }

    fn on_probe_packet(&mut self, now: Instant, r: &PacketResult) {
        let (Some(id), Some(a)) = (r.probe, r.arrival_us) else {
            return;
        };
        let Some(p) = self.probes.get_mut(&id) else {
            return;
        };
        if p.done {
            return;
        }
        p.received += 1;
        p.bytes += r.size;
        if p.first_send.is_none_or(|t| r.send_time < t) {
            p.first_send = Some(r.send_time);
            p.first_size = r.size;
        }
        if p.last_send.is_none_or(|t| r.send_time >= t) {
            p.last_send = Some(r.send_time);
        }
        if p.first_arrival.is_none_or(|t| a < t) {
            p.first_arrival = Some(a);
        }
        if p.last_arrival.is_none_or(|t| a >= t) {
            p.last_arrival = Some(a);
            p.last_size = r.size;
        }
        // 5 packet 以上・送った幅 15 ms 以上で判定する（libwebrtc の ProbeBitrateEstimator と同じ考え方）
        let (Some(fs), Some(ls), Some(fa), Some(la)) =
            (p.first_send, p.last_send, p.first_arrival, p.last_arrival)
        else {
            return;
        };
        let send_span = ls.saturating_duration_since(fs).as_secs_f64();
        let recv_span = (la - fa) as f64 / 1e6;
        if p.received < 5 || send_span < 0.015 || recv_span <= 0.0 {
            return;
        }
        p.done = true;
        let send_rate = (p.bytes - p.last_size) as f64 * 8.0 / send_span;
        let recv_rate = (p.bytes - p.first_size) as f64 * 8.0 / recv_span;
        let mut rate = send_rate.min(recv_rate);
        if recv_rate < 0.9 * send_rate {
            rate = recv_rate * 0.95;
        }
        if rate > self.delay_rate {
            self.delay_rate = rate.min(self.cfg.max_bps as f64);
            self.link_capacity = None;
            self.state = RateState::Hold;
            self.last_change = Some(now);
            if self.loss_rate < self.delay_rate {
                self.loss_rate = self.delay_rate;
            }
        }
    }

    /// REMB を受けた。
    pub fn on_remb(&mut self, now: Instant, bps: u64) {
        self.remb = Some((bps, now));
    }

    /// RR の fraction lost（transport-wide-cc が無いときの損失の手がかり）。
    pub fn on_receiver_report(&mut self, now: Instant, fraction_lost: u8) {
        if self.last_twcc.is_some() {
            return;
        }
        self.apply_loss(now, fraction_lost as f64 / 256.0);
    }

    /// transport-wide-cc の feedback（送信記録と突き合わせた結果）を入れる。
    pub fn on_feedback(&mut self, now: Instant, results: &[PacketResult]) {
        if results.is_empty() {
            return;
        }
        self.last_twcc = Some(now);
        let mut received: Vec<&PacketResult> =
            results.iter().filter(|r| r.arrival_us.is_some()).collect();
        let lost = results.len() - received.len();
        received.sort_by_key(|r| (r.arrival_us, r.send_time));
        for r in &received {
            self.on_probe_packet(now, r);
        }
        for r in &received {
            let a = r.arrival_us.unwrap_or_default();
            self.acked.on_packet(a, r.size);
            self.avg_packet_bits = 0.95 * self.avg_packet_bits + 0.05 * (r.size as f64 * 8.0);
            self.trend.on_packet(r.send_time, a);
        }
        // 損失は 20 packet 以上まとまってから判断する
        self.loss_window.0 += lost as u32;
        self.loss_window.1 += results.len() as u32;
        if self.loss_window.1 >= 20 {
            let f = self.loss_window.0 as f64 / self.loss_window.1 as f64;
            self.loss_window = (0, 0);
            self.apply_loss(now, f);
        }
        self.update_delay(now);
    }

    fn apply_loss(&mut self, now: Instant, f: f64) {
        self.loss_fraction = f;
        let current = self.estimate(now) as f64;
        if f > 0.10 {
            let wait = Duration::from_millis(300) + self.rtt;
            if self
                .last_loss_decrease
                .is_none_or(|t| now.saturating_duration_since(t) >= wait)
            {
                self.loss_rate = current * (1.0 - 0.5 * f);
                self.last_loss_decrease = Some(now);
            }
        } else if f < 0.02 {
            let elapsed = self.last_loss_increase.map_or(1.0, |t| {
                now.saturating_duration_since(t).as_secs_f64().min(1.0)
            });
            if self.loss_rate < self.cfg.max_bps as f64 {
                self.loss_rate =
                    self.loss_rate.max(current) * 1.08f64.powf(elapsed) + 1000.0 * elapsed;
                if self.loss_rate > self.delay_rate * 1.5 {
                    // 損失の少ない間は遅延による値が決める
                    self.loss_rate = self.cfg.max_bps as f64;
                }
            }
            self.last_loss_increase = Some(now);
        }
    }

    fn update_delay(&mut self, now: Instant) {
        let acked = self.acked.bitrate().map(|b| b as f64);
        let usage = self.trend.state;
        match usage {
            BandwidthUsage::Overusing => {
                // 下げた直後は、遅延の勾配が追いつくまで（往復時間 + 300 ms）続けて下げない
                if self.state != RateState::Decrease
                    || self.last_change.is_none_or(|t| {
                        now.saturating_duration_since(t) >= self.rtt + Duration::from_millis(300)
                    })
                {
                    let base = acked.unwrap_or(self.delay_rate);
                    // 届いた速さが下がり続ける間に下げすぎないよう、リンク容量の見積もりも下限に使う
                    let floor = self.link_capacity.map_or(0.0, |c| c * 1000.0 * 0.95);
                    let mut new = BETA * base.max(floor.min(self.delay_rate));
                    if new > self.delay_rate {
                        new = self.delay_rate * BETA;
                    }
                    if let Some(a) = acked {
                        // リンク容量の見積もり（kbit/s、libwebrtc の AimdRateControl と同じ正規化）
                        let ak = a / 1000.0;
                        let cap = self.link_capacity.map_or(ak, |c| 0.95 * c + 0.05 * ak);
                        let err = cap - ak;
                        self.link_var = (0.95 * self.link_var + 0.05 * err * err / cap.max(1.0))
                            .clamp(0.4, 2.5);
                        self.link_capacity = Some(cap);
                    }
                    self.delay_rate = new.max(self.cfg.min_bps as f64);
                    self.state = RateState::Decrease;
                    self.last_change = Some(now);
                }
            }
            BandwidthUsage::Underusing => {
                self.state = RateState::Hold;
                self.last_change = Some(now);
            }
            BandwidthUsage::Normal => {
                if self.state != RateState::Increase {
                    if self.state == RateState::Decrease {
                        self.state = RateState::Hold;
                    } else {
                        self.state = RateState::Increase;
                    }
                    self.last_change = Some(now);
                    return;
                }
                let elapsed = self.last_change.map_or(0.0, |t| {
                    now.saturating_duration_since(t).as_secs_f64().min(1.0)
                });
                self.last_change = Some(now);
                let rate_k = self.delay_rate / 1000.0;
                let near = self.link_capacity.is_some_and(|c| {
                    let std = (self.link_var * c).sqrt();
                    (rate_k - c).abs() <= 3.0 * std
                });
                let new = if near {
                    let response = (self.rtt.as_secs_f64() + 0.1).max(0.1);
                    let per_frame = self.avg_packet_bits.max(8000.0);
                    self.delay_rate + (per_frame / response) * elapsed
                } else {
                    if self.link_capacity.is_some_and(|c| rate_k > c * 1.5) {
                        self.link_capacity = None;
                    }
                    self.delay_rate * 1.08f64.powf(elapsed) + 1000.0 * elapsed
                };
                let cap = acked.map_or(f64::MAX, |a| 1.5 * a + 10_000.0);
                if new > self.delay_rate {
                    self.delay_rate = new.min(cap.max(self.delay_rate));
                }
            }
        }
        self.delay_rate = self
            .delay_rate
            .clamp(self.cfg.min_bps as f64, self.cfg.max_bps as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ボトルネック（容量 `cap` bit/s、FIFO）を通して、推定に従って送る送り手を模擬する。
    fn simulate(cap: f64, secs: u64, start: u64, loss_every: Option<u32>) -> (u64, f64) {
        let t0 = Instant::now();
        let mut bwe = BandwidthEstimator::new(BweConfig {
            start_bps: start,
            ..Default::default()
        });
        bwe.set_rtt(Duration::from_millis(40));
        let mut link_free_us = 0f64;
        let mut pending: Vec<PacketResult> = Vec::new();
        let mut next_fb = 50_000i64;
        let mut t_us = 0i64;
        let mut n = 0u32;
        let mut max_queue = 0f64;
        let size = 1200usize;
        while t_us < secs as i64 * 1_000_000 {
            let now = t0 + Duration::from_micros(t_us as u64);
            let rate = bwe.estimate(now) as f64;
            // 送信: 推定どおりの速さ
            let send_us = t_us as f64;
            let tx = size as f64 * 8.0 / cap * 1e6;
            let start_tx = link_free_us.max(send_us);
            link_free_us = start_tx + tx;
            let queue = start_tx - send_us;
            max_queue = max_queue.max(queue);
            n += 1;
            let lost = loss_every.is_some_and(|k| n.is_multiple_of(k));
            pending.push(PacketResult {
                send_time: now,
                size,
                probe: None,
                arrival_us: (!lost).then_some((link_free_us + 20_000.0) as i64),
            });
            t_us += (size as f64 * 8.0 / rate * 1e6) as i64;
            if t_us >= next_fb {
                next_fb += 50_000;
                // 届いたもの（到着が今より前）だけを報告する
                let now2 = t0 + Duration::from_micros(t_us as u64);
                let (ready, rest): (Vec<_>, Vec<_>) = pending
                    .drain(..)
                    .partition(|p| p.arrival_us.is_none_or(|a| a <= t_us));
                pending = rest;
                bwe.on_feedback(now2, &ready);
            }
        }
        (bwe.estimate(t0 + Duration::from_secs(secs)), max_queue)
    }

    #[test]
    fn converges_below_bottleneck() {
        let (est, _) = simulate(1_000_000.0, 30, 3_000_000, None);
        assert!(
            (550_000..=1_150_000).contains(&est),
            "estimate {est} for a 1 Mbit/s bottleneck"
        );
    }

    #[test]
    fn ramps_up_when_capacity_is_high() {
        let (est, q) = simulate(5_000_000.0, 30, 300_000, None);
        assert!(est > 2_000_000, "estimate {est} did not ramp up");
        assert!(q < 1_000_000.0, "queue delay {q} µs");
    }

    #[test]
    fn heavy_loss_reduces() {
        // 容量は十分だが 4 packet に 1 つ失われる
        let (est, _) = simulate(20_000_000.0, 10, 2_000_000, Some(4));
        assert!(est < 2_000_000, "estimate {est} with 25% loss");
    }

    #[test]
    fn remb_fallback() {
        let t = Instant::now();
        let mut b = BandwidthEstimator::new(BweConfig::default());
        b.on_remb(t, 400_000);
        assert_eq!(b.estimate(t), 400_000);
        b.on_feedback(
            t,
            &[PacketResult {
                send_time: t,
                size: 100,
                probe: None,
                arrival_us: Some(0),
            }],
        );
        assert_ne!(b.estimate(t), 400_000);
        assert_eq!(b.estimate(t + Duration::from_secs(3)), 400_000);
    }

    #[test]
    fn probe_raises_estimate_to_delivered_rate() {
        let t = Instant::now();
        let mut b = BandwidthEstimator::new(BweConfig {
            start_bps: 300_000,
            ..Default::default()
        });
        // 2 Mbit/s で送り、1.2 Mbit/s の回線で届いた
        let id = b.start_probe(t);
        let res: Vec<PacketResult> = (0..20)
            .map(|i| PacketResult {
                send_time: t + Duration::from_micros(i * 1200),
                size: 300,
                probe: Some(id),
                arrival_us: Some(10_000 + i as i64 * 2000),
            })
            .collect();
        b.on_feedback(t + Duration::from_millis(60), &res);
        let e = b.estimate(t + Duration::from_millis(60));
        assert!((1_000_000..=1_250_000).contains(&e), "{e}");
    }

    #[test]
    fn slope() {
        let pts: VecDeque<(f64, f64)> = (0..20).map(|i| (i as f64, 2.0 * i as f64 + 1.0)).collect();
        assert!((linear_fit_slope(&pts).unwrap() - 2.0).abs() < 1e-9);
    }
}
