//! 中継の速さを測る（`cargo test -p senca-relay --all-features --release --test bench -- --ignored --nocapture`）。
//!
//! 100 本の stream（4 Mbps の映像 30 fps・GOP 2 秒＋128 kbps の音声 50 fps）を、TCP（loopback）の 1 本の接続で
//! 送り、受け側の RelayHub で 1 人・10 人の購読者に配る。ライブの速さではなく、背圧（送りのキューが空くのを
//! 待つ）をかけながらできるだけ速く流し、Mbit/s・1 Gbit 当たりの CPU 時間・AU 当たりの見出しを出す。
#![cfg(feature = "tokio")]

use bytes::Bytes;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};
use senca_relay::net::{Downstream, EndpointConfig, RelayEndpoint, RelayHub, RelayHubConfig};
use senca_relay::sched::SchedConfig;
use senca_relay::{ConnConfig, StreamEvent, TrackSel};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const STREAMS: usize = 100;
const SECONDS: u64 = 30;

fn cpu_secs() -> f64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let Some(i) = s.rfind(')') else { return 0.0 };
    let f: Vec<&str> = s[i + 2..].split(' ').collect();
    // utime・stime（clock tick。Linux の USER_HZ は 100）
    let t: f64 = f.get(11).and_then(|x| x.parse().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    t / 100.0
}

fn endpoint(
    node: u64,
) -> (
    RelayEndpoint,
    tokio::sync::mpsc::Receiver<senca_relay::net::IncomingSubscribe>,
) {
    RelayEndpoint::new(EndpointConfig {
        conn: ConnConfig {
            node_id: node,
            sched: SchedConfig {
                max_queue_bytes: 256 << 20,
                max_delay_video: Duration::from_secs(600),
                max_delay_audio: Duration::from_secs(600),
                ..Default::default()
            },
            ..Default::default()
        },
        upstream_queue: 1 << 20,
        ..Default::default()
    })
}

struct Result {
    wall: f64,
    cpu: f64,
    wire_bytes: u64,
    payload_bytes: u64,
    aus: u64,
    delivered: u64,
    overhead: f64,
    writes: u64,
    frames: u64,
}

async fn run(subs_per_stream: usize) -> Result {
    let (a, _ia) = endpoint(1);
    let (b, mut ib) = endpoint(2);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (x, y) = tokio::join!(
        async { tokio::net::TcpStream::connect(addr).await.unwrap() },
        async { l.accept().await.unwrap().0 }
    );
    x.set_nodelay(true).unwrap();
    y.set_nodelay(true).unwrap();
    let (ra, rb) = tokio::join!(a.connect(x), b.accept(y));
    ra.unwrap();
    let conn_b = rb.unwrap();
    let hub = RelayHub::new(
        a.clone(),
        Arc::new(|_: &str| Some(2)),
        RelayHubConfig {
            subscriber_queue: 1 << 20,
            ..Default::default()
        },
    );

    let video_per_stream = SECONDS * 30;
    let audio_per_stream = SECONDS * 50;
    let per_stream = video_per_stream + audio_per_stream;
    let expected = per_stream * STREAMS as u64 * subs_per_stream as u64;
    let delivered = Arc::new(AtomicU64::new(0));
    let done = Arc::new(tokio::sync::Notify::new());
    let mut keep = Vec::new();
    for s in 0..STREAMS {
        for _ in 0..subs_per_stream {
            let mut sub = hub.subscribe(&format!("s{s}"), TrackSel::All);
            let d = delivered.clone();
            let done = done.clone();
            keep.push(tokio::spawn(async move {
                let _ = sub.initial();
                while let Some(e) = sub.recv().await {
                    if let StreamEvent::Au(_) = e
                        && d.fetch_add(1, Ordering::Relaxed) + 1 == expected
                    {
                        done.notify_one();
                    }
                }
            }));
        }
    }
    let mut downs: Vec<Downstream> = Vec::new();
    while downs.len() < STREAMS {
        downs.push(ib.recv().await.unwrap().accept(None));
    }
    let pool = Bytes::from(vec![0x5au8; 60_000]);
    let vcfg = CodecConfig {
        codec: Codec::H264,
        generation: 1,
        record: Bytes::from_static(b"avcC"),
        video: None,
        audio: None,
    };
    let acfg = CodecConfig {
        codec: Codec::Aac,
        generation: 1,
        record: Bytes::from_static(&[0x11, 0x90]),
        video: None,
        audio: None,
    };
    for d in &downs {
        d.send(&StreamEvent::Config(TrackId(0), vcfg.clone()));
        d.send(&StreamEvent::Config(TrackId(1), acfg.clone()));
    }
    let mk = |track: u32, codec: Codec, seq: u64, ms: i64, key: bool, len: usize| AccessUnit {
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
        payload: pool.slice(..len),
    };
    let cpu0 = cpu_secs();
    let t0 = Instant::now();
    // 10 ms ごとの時刻で、全 stream の分を積む
    let (mut vseq, mut aseq) = (0u64, 0u64);
    for tick in 0..(SECONDS * 100) as i64 {
        let ms = tick * 10;
        while (vseq as i64 * 1000) / 30 <= ms && vseq < video_per_stream {
            let key = vseq.is_multiple_of(60);
            for d in &downs {
                d.send_au(mk(
                    0,
                    Codec::H264,
                    vseq,
                    (vseq as i64 * 1000) / 30,
                    key,
                    if key { 60_000 } else { 15_900 },
                ));
            }
            vseq += 1;
        }
        while aseq as i64 * 20 <= ms && aseq < audio_per_stream {
            for d in &downs {
                d.send_au(mk(1, Codec::Aac, aseq, aseq as i64 * 20, true, 320));
            }
            aseq += 1;
        }
        conn_b.wait_drained(16 << 20).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(120), async {
        while delivered.load(Ordering::Relaxed) < expected {
            tokio::select! {
                _ = done.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    })
    .await;
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_secs() - cpu0;
    let st = conn_b.stats();
    for k in keep {
        k.abort();
    }
    Result {
        wall,
        cpu,
        wire_bytes: st.tx_bytes,
        payload_bytes: st.tx_payload_bytes,
        aus: st.tx_aus,
        delivered: delivered.load(Ordering::Relaxed),
        overhead: st.tx_overhead_per_au(),
        writes: st.tx_writes,
        frames: st.tx_frames,
    }
}

#[test]
#[ignore]
fn throughput_100_streams_x_4mbps() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    for subs in [1usize, 10] {
        let r = rt.block_on(run(subs));
        let gbit = r.wire_bytes as f64 * 8.0 / 1e9;
        println!(
            "{STREAMS} stream × 4 Mbps（{SECONDS} 秒分。送り・受けの両端が同じプロセス）、stream 当たりの購読者 {subs}: 回線 {:.0} Mbit/s（{:.2} GB を {:.2} 秒）、\
             CPU {:.2} 秒 = 1 Gbit 当たり {:.3} CPU 秒、AU 当たりの見出し {:.2} B（payload 比 {:.3}%）、\
             書き込み {} 回・枠 {}・AU {}、配った AU {}",
            gbit * 1000.0 / r.wall,
            r.wire_bytes as f64 / 1e9,
            r.wall,
            r.cpu,
            r.cpu / gbit,
            r.overhead,
            (r.wire_bytes - r.payload_bytes) as f64 * 100.0 / r.payload_bytes as f64,
            r.writes,
            r.frames,
            r.aus,
            r.delivered
        );
        assert_eq!(r.aus, (SECONDS * 80) * STREAMS as u64, "捨てずに全部送る");
        assert_eq!(r.delivered, r.aus * subs as u64, "全員に届く");
    }
}
