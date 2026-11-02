//! 決定的なシミュレーション（sans-io の接続 2 つを、試験の中のトークンバケットの回線でつなぐ）。
//!
//! - 帯域が足りなくなったとき: 音声は途切れず、映像は壊れた GOP を送らず、帯域が戻ればキーフレームから戻る。
//! - 同じ優先度のチャネルは帯域を等分する。
//! - 小さな AU をまとめると、書き込みの回数が減る。

use bytes::{Bytes, BytesMut};
use senca_core::{AccessUnit, Codec, CodecConfig, MediaKind, Timebase, TrackId};
use senca_relay::sched::SchedConfig;
use senca_relay::{
    ConnConfig, ConnEvent, Connection, DownstreamId, StreamEvent, TrackSel, UpstreamId,
};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// 片方向の回線: 速さ（bit/s）のトークンバケットと、伝搬の遅れ。
struct Link {
    rate_bps: f64,
    tokens: f64,
    burst: f64,
    delay: Duration,
    flight: VecDeque<(Instant, Bytes)>,
    sent: u64,
}

impl Link {
    fn new(rate_bps: f64, delay: Duration) -> Link {
        Link {
            rate_bps,
            tokens: 0.0,
            burst: 3000.0,
            delay,
            flight: VecDeque::new(),
            sent: 0,
        }
    }
    /// 1 ステップ: 送り手から、回線が受け付けるだけ取る（取れない間は送り手の中で待つ＝背圧）。
    fn step(&mut self, from: &mut Connection, now: Instant, dt: Duration) {
        self.tokens = (self.tokens + self.rate_bps / 8.0 * dt.as_secs_f64())
            .min(self.burst.max(self.rate_bps / 8.0 * 0.004));
        while self.tokens > 0.0 {
            let mut out = Vec::new();
            let n = from.poll_transmit(now, &mut out);
            if n == 0 {
                break;
            }
            self.tokens -= n as f64;
            self.sent += n as u64;
            for c in out {
                self.flight.push_back((now + self.delay, c));
            }
        }
    }
    fn deliver(&mut self, to: &mut Connection, now: Instant) {
        let mut buf = BytesMut::new();
        while self.flight.front().is_some_and(|f| f.0 <= now) {
            buf.extend_from_slice(&self.flight.pop_front().unwrap().1);
        }
        if !buf.is_empty() {
            to.handle_input(&mut buf, now);
        }
    }
}

fn au(track: u32, codec: Codec, seq: u64, ms: i64, key: bool, len: usize) -> AccessUnit {
    AccessUnit {
        track: TrackId(track),
        codec,
        epoch: 0,
        sequence: seq,
        config_generation: 1,
        pts: ms,
        dts: ms,
        duration: 0,
        timebase: Timebase::MILLIS,
        keyframe: key,
        discontinuity: false,
        capture_time_ns: None,
        payload: Bytes::from(vec![(seq & 0xff) as u8; len]),
    }
}

fn cfg(codec: Codec) -> CodecConfig {
    CodecConfig {
        codec,
        generation: 1,
        record: Bytes::from_static(b"cfg"),
        video: None,
        audio: None,
    }
}

struct Sim {
    t0: Instant,
    now: Instant,
    tx: Connection,
    rx: Connection,
    fwd: Link,
    back: Link,
    downs: Vec<DownstreamId>,
    ups: Vec<UpstreamId>,
    /// 受けた AU（チャネル、届いた時刻 ms、AU）。
    got: Vec<(usize, i64, AccessUnit)>,
}

impl Sim {
    fn new(channels: usize, rate_bps: f64, sched: SchedConfig) -> Sim {
        let t0 = Instant::now();
        // 回線の手前で溜めすぎない（ソケットの送りバッファを小さくしたのと同じ）
        let tx = Connection::new(
            ConnConfig {
                node_id: 2,
                transmit_max: 8 << 10,
                sched,
                ..Default::default()
            },
            t0,
        );
        let rx = Connection::new(
            ConnConfig {
                node_id: 1,
                ..Default::default()
            },
            t0,
        );
        let mut s = Sim {
            t0,
            now: t0,
            tx,
            rx,
            fwd: Link::new(rate_bps, Duration::from_millis(20)),
            back: Link::new(1e9, Duration::from_millis(20)),
            downs: Vec::new(),
            ups: Vec::new(),
            got: Vec::new(),
        };
        for i in 0..channels {
            let up = s.rx.subscribe(&format!("s{i}"), TrackSel::All, 0).unwrap();
            s.ups.push(up);
        }
        for _ in 0..100 {
            s.step(Duration::from_millis(1));
        }
        assert_eq!(s.downs.len(), channels);
        for &d in &s.downs.clone() {
            s.tx.send_config(d, TrackId(0), cfg(Codec::H264), s.now);
            s.tx.send_config(d, TrackId(1), cfg(Codec::Aac), s.now);
        }
        s
    }

    fn ms(&self) -> i64 {
        (self.now - self.t0).as_millis() as i64
    }

    fn step(&mut self, dt: Duration) {
        self.now += dt;
        let now = self.now;
        self.tx.handle_timeout(now);
        self.rx.handle_timeout(now);
        self.fwd.step(&mut self.tx, now, dt);
        self.back.step(&mut self.rx, now, dt);
        self.fwd.deliver(&mut self.rx, now);
        self.back.deliver(&mut self.tx, now);
        while let Some(e) = self.tx.poll_event() {
            if let ConnEvent::SubscribeRequest { id, .. } = e {
                self.tx.accept(id, None, now);
                self.downs.push(id);
            }
        }
        let ms = self.ms();
        while let Some(e) = self.rx.poll_event() {
            if let ConnEvent::Stream(id, StreamEvent::Au(a)) = e {
                let ch = self.ups.iter().position(|u| *u == id).unwrap();
                self.got.push((ch, ms, a));
            }
        }
    }
}

/// 4 Mbps の映像（GOP 2 秒）＋ 128 kbps の音声を、10 Mbps → 2 Mbps（6 秒）→ 10 Mbps の回線で送る。
#[test]
fn congested_link_keeps_audio_and_recovers_video_at_keyframes() {
    let mut s = Sim::new(1, 10e6, SchedConfig::default());
    let d = s.downs[0];
    let start = s.ms();
    let (mut vseq, mut aseq) = (0u64, 0u64);
    let mut next_v = start;
    let mut next_a = start;
    let end = start + 15_000;
    let mut created: HashMap<(u32, u64), i64> = HashMap::new();
    while s.ms() < end {
        let t = s.ms() - start;
        s.fwd.rate_bps = if (3000..9000).contains(&t) { 2e6 } else { 10e6 };
        let now = s.now;
        while s.ms() >= next_v {
            let key = vseq.is_multiple_of(60);
            // 4 Mbps: キーフレーム 60 kB、差分 16 kB 弱
            let len = if key { 60_000 } else { 15_900 };
            s.tx.send_au(d, au(0, Codec::H264, vseq, next_v, key, len), now);
            created.insert((0, vseq), next_v);
            vseq += 1;
            next_v = start + (vseq as i64 * 1000) / 30;
        }
        while s.ms() >= next_a {
            s.tx.send_au(d, au(1, Codec::Aac, aseq, next_a, true, 320), now);
            created.insert((1, aseq), next_a);
            aseq += 1;
            next_a = start + aseq as i64 * 20;
        }
        s.step(Duration::from_millis(1));
    }
    for _ in 0..2000 {
        s.step(Duration::from_millis(1));
    }
    let st = s.tx.stats();
    println!(
        "送った AU {}、捨てた（音声・キー・差分・データ）{:?}、待ち時間の平均 {} µs、RTT {:?}",
        st.sched.sent_aus, st.sched.dropped, st.sched.queue_delay_avg_us, st.rtt
    );
    // 音声: 全部届き、遅れは小さい
    let audio: Vec<&(usize, i64, AccessUnit)> = s
        .got
        .iter()
        .filter(|g| g.2.codec.kind() == MediaKind::Audio)
        .collect();
    assert_eq!(audio.len() as u64, aseq, "音声が欠けた");
    for (i, g) in audio.iter().enumerate() {
        assert_eq!(g.2.sequence, i as u64);
        assert!(!g.2.discontinuity);
    }
    let worst_audio = audio
        .iter()
        .map(|g| g.1 - created[&(1, g.2.sequence)])
        .max()
        .unwrap();
    // 枠は途中で割らないので、音声は回線に乗った最大の枠（60 kB のキーフレーム = 2 Mbps で 240 ms）の後ろで
    // 待ちうる（1 本の TCP の head-of-line。QUIC のストリームに移れば無くなる）。それ以上は待たない
    let hol_ms = 60_000.0 * 8.0 / 2e6 * 1000.0;
    println!(
        "音声の最大の遅れ {worst_audio} ms（伝搬 20 ms、最大の枠の直列化 {hol_ms:.0} ms を含む）"
    );
    assert!(
        (worst_audio as f64) < 20.0 + hol_ms + 40.0,
        "音声が遅れすぎ {worst_audio} ms"
    );
    // 映像: GOP の先頭からの連続だけ
    let video: Vec<&(usize, i64, AccessUnit)> = s
        .got
        .iter()
        .filter(|g| g.2.codec.kind() == MediaKind::Video)
        .collect();
    let mut expect: Option<u64> = None;
    let mut keys_in_congestion = 0;
    for g in &video {
        let a = &g.2;
        if a.keyframe {
            if (start + 3000..start + 9000).contains(&g.1) {
                keys_in_congestion += 1;
            }
        } else {
            assert_eq!(Some(a.sequence), expect, "壊れた GOP（seq {}）", a.sequence);
        }
        expect = Some(a.sequence + 1);
    }
    assert!(st.sched.dropped[2] > 0, "詰まったのに差分を捨てていない");
    assert_eq!(st.sched.dropped[0], 0, "音声を捨てた");
    assert!(keys_in_congestion > 0, "詰まっている間もキーフレームは届く");
    // 帯域が戻った後: 最初のキーフレームから最後まで全部届く
    let recovered = video
        .iter()
        .position(|g| g.2.keyframe && created[&(0, g.2.sequence)] >= start + 9000)
        .expect("戻らない");
    let tail: Vec<u64> = video[recovered..].iter().map(|g| g.2.sequence).collect();
    let first = tail[0];
    assert_eq!(tail, (first..vseq).collect::<Vec<_>>(), "戻った後に欠けた");
    let recover_ms = created[&(0, first)] - (start + 9000);
    println!("帯域が戻ってから {recover_ms} ms 後のキーフレームで映像が戻った");
    assert!(recover_ms <= 2000, "次の GOP で戻る");
    // 捨てた後の AU には不連続の印
    assert!(video.iter().any(|g| g.2.discontinuity));
    assert!(s.rx.stats().rx_gaps > 0);
}

/// 同じ優先度の 3 チャネルが 6 Mbps の回線を分け合う（各 4 Mbps を出す）。
#[test]
fn channels_share_a_congested_link_fairly() {
    let mut s = Sim::new(3, 6e6, SchedConfig::default());
    let downs = s.downs.clone();
    let start = s.ms();
    let mut seq = 0u64;
    let mut next = start;
    while s.ms() < start + 10_000 {
        let now = s.now;
        while s.ms() >= next {
            for &d in &downs {
                let key = seq.is_multiple_of(60);
                s.tx.send_au(
                    d,
                    au(
                        0,
                        Codec::H264,
                        seq,
                        next,
                        key,
                        if key { 60_000 } else { 15_900 },
                    ),
                    now,
                );
            }
            seq += 1;
            next = start + (seq as i64 * 1000) / 30;
        }
        s.step(Duration::from_millis(1));
    }
    let mut bytes = [0usize; 3];
    for g in &s.got {
        if g.1 >= start + 2000 {
            bytes[g.0] += g.2.payload.len();
        }
    }
    let max = *bytes.iter().max().unwrap() as f64;
    let min = *bytes.iter().min().unwrap() as f64;
    println!("8 秒で届いたバイト数 {bytes:?}");
    assert!(min > 0.0 && max / min < 1.3, "不公平 {bytes:?}");
    // 回線をほぼ使い切る
    let total: usize = bytes.iter().sum();
    assert!(
        total as f64 * 8.0 / 8.0 > 6e6 * 0.85,
        "回線を使い切れていない {total}"
    );
}

/// 100 チャネルの音声（50 fps、ずれた位相）。5 ms まとめると、書き込みの回数と枠の数が大きく減る。
#[test]
fn batching_reduces_writes() {
    let run = |window: Duration| {
        let mut s = Sim::new(
            100,
            1e9,
            SchedConfig {
                batch_window: window,
                ..Default::default()
            },
        );
        let downs = s.downs.clone();
        // 設定の枠を送り終えてから数える
        for _ in 0..10 {
            s.step(Duration::from_millis(1));
        }
        let w0 = s.tx.stats();
        let start = s.ms();
        let mut seqs = vec![0u64; 100];
        for t in 0..1000i64 {
            let now = s.now;
            for (i, &d) in downs.iter().enumerate() {
                // チャネルごとに位相をずらす
                if (t + i as i64) % 20 == 0 {
                    s.tx.send_au(d, au(1, Codec::Opus, seqs[i], start + t, true, 160), now);
                    seqs[i] += 1;
                }
            }
            s.step(Duration::from_millis(1));
        }
        for _ in 0..100 {
            s.step(Duration::from_millis(1));
        }
        let st = s.tx.stats();
        let n: u64 = seqs.iter().sum();
        assert_eq!(s.got.len() as u64, n);
        let worst = s.got.iter().map(|g| g.1 - g.2.dts).max().unwrap();
        (
            st.tx_writes - w0.tx_writes,
            st.tx_frames - w0.tx_frames,
            st.tx_overhead_per_au(),
            n,
            worst,
        )
    };
    let (w0, f0, o0, n, l0) = run(Duration::ZERO);
    let (w5, f5, o5, _, l5) = run(Duration::from_millis(5));
    println!(
        "{n} AU: まとめない {w0} 回・{f0} 枠・AU 当たり {o0:.2} B・最大の遅れ {l0} ms / 5 ms まとめる {w5} 回・{f5} 枠・{o5:.2} B・{l5} ms"
    );
    assert!(w5 * 3 < w0, "書き込みが減らない {w5} vs {w0}");
    assert!(f5 * 3 < f0);
    assert!(o5 < o0);
    assert!(l5 <= l0 + 6, "まとめの待ちは 5 ms 以内");
}
