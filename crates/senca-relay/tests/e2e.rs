//! 2 つ（3 つ）のエンドポイントの間で、FFmpeg で作った本物の H.264/AAC を中継する。
//!
//! FFmpeg は環境変数 `FFMPEG`（既定 `~/tools/ffmpeg`）。無ければ試験を飛ばす。
#![cfg(feature = "tokio")]

use senca_core::{AccessUnit, CodecConfig, MediaKind, TrackId};
use senca_flv::{FlvIngest, FlvReader, IngestEvent};
use senca_relay::net::{
    EndpointConfig, IncomingSubscribe, RelayEndpoint, RelayHub, RelayHubConfig,
};
use senca_relay::wire::same_au;
use senca_relay::{AggregatorConfig, ConnConfig, ErrorCode, StreamEvent, TrackSel};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

fn ffmpeg() -> PathBuf {
    std::env::var_os("FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("tools/ffmpeg")
        })
}

/// FFmpeg で 4 秒の H.264（B フレームつき、GOP 1 秒）＋ AAC の FLV を作り、AU に分ける。
/// 設定と AU。
type Media = (Vec<(TrackId, CodecConfig)>, Vec<AccessUnit>);

fn media() -> Option<&'static Media> {
    static M: OnceLock<Option<Media>> = OnceLock::new();
    M.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("senca-relay-e2e");
        std::fs::create_dir_all(&dir).ok()?;
        let out = dir.join("av.flv");
        if !out.exists() {
            let st = Command::new(ffmpeg())
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=640x360:rate=30",
                    "-f",
                    "lavfi",
                    "-i",
                ])
                .args([
                    "sine=frequency=440:sample_rate=48000",
                    "-t",
                    "4",
                    "-c:v",
                    "libx264",
                    "-g",
                    "30",
                    "-bf",
                    "2",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .args(["-c:a", "aac", "-b:a", "128k", "-f", "flv"])
                .arg(&out)
                .status();
            match st {
                Ok(s) if s.success() => {}
                _ => {
                    eprintln!("FFmpeg が使えないので飛ばす");
                    return None;
                }
            }
        }
        let data = std::fs::read(&out).ok()?;
        let mut r = FlvReader::new();
        r.feed(&data);
        let mut ing = FlvIngest::default();
        let mut evs = Vec::new();
        while let Some(t) = r.next_tag().unwrap() {
            ing.push(t.tag_type, t.timestamp, &t.body, &mut evs)
                .unwrap();
        }
        let mut cfgs = Vec::new();
        let mut aus = Vec::new();
        for e in evs {
            match e {
                IngestEvent::Config { track, config } => cfgs.push((track, config)),
                IngestEvent::AccessUnit(a) => aus.push(a),
                IngestEvent::EndOfSequence { .. } => {}
            }
        }
        assert!(
            aus.iter()
                .filter(|a| a.codec.kind() == MediaKind::Video)
                .count()
                >= 110
        );
        assert!(
            aus.iter()
                .filter(|a| a.codec.kind() == MediaKind::Audio)
                .count()
                >= 180
        );
        Some((cfgs, aus))
    })
    .as_ref()
}

fn source_events(epoch: u64) -> Option<Vec<StreamEvent>> {
    let (cfgs, aus) = media()?;
    let mut v: Vec<StreamEvent> = cfgs
        .iter()
        .map(|(t, c)| StreamEvent::Config(*t, c.clone()))
        .collect();
    v.extend(aus.iter().map(|a| {
        let mut a = a.clone();
        a.epoch = epoch;
        StreamEvent::Au(a)
    }));
    Some(v)
}

fn endpoint(node: u64) -> (RelayEndpoint, mpsc::Receiver<IncomingSubscribe>) {
    RelayEndpoint::new(EndpointConfig {
        conn: ConnConfig {
            node_id: node,
            ..Default::default()
        },
        ..Default::default()
    })
}

async fn link_tcp(a: &RelayEndpoint, b: &RelayEndpoint) {
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
    rb.unwrap();
}

/// 元のノード: 知っている stream の購読を受け、全部を積んで、相手がやめるまで持つ。
fn serve(
    mut rx: mpsc::Receiver<IncomingSubscribe>,
    streams: HashMap<String, Vec<StreamEvent>>,
    served: Arc<AtomicUsize>,
) {
    tokio::spawn(async move {
        while let Some(inc) = rx.recv().await {
            let Some(evs) = streams.get(&inc.stream_id).cloned() else {
                inc.reject(ErrorCode::NOT_FOUND, "no such stream");
                continue;
            };
            served.fetch_add(1, Ordering::SeqCst);
            let mut d = inc.accept(None);
            tokio::spawn(async move {
                d.send_all(&evs);
                while d.next_event().await.is_some() {}
            });
        }
    });
}

/// 設定と AU をトラックごとに集める。
#[derive(Default, Debug)]
struct Got {
    cfgs: Vec<(TrackId, CodecConfig)>,
    aus: HashMap<TrackId, Vec<AccessUnit>>,
    discontinuities: usize,
    n: usize,
}

impl Got {
    fn add(&mut self, e: StreamEvent) {
        match e {
            StreamEvent::Config(t, c) => self.cfgs.push((t, c)),
            StreamEvent::Au(a) => {
                self.n += 1;
                self.aus.entry(a.track).or_default().push(a)
            }
            StreamEvent::Discontinuity => self.discontinuities += 1,
            _ => {}
        }
    }
}

fn per_track(aus: &[AccessUnit]) -> HashMap<TrackId, Vec<AccessUnit>> {
    let mut m: HashMap<TrackId, Vec<AccessUnit>> = HashMap::new();
    for a in aus {
        m.entry(a.track).or_default().push(a.clone());
    }
    m
}

fn assert_identical(got: &[AccessUnit], want: &[AccessUnit]) {
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(want) {
        assert!(same_au(g, w), "AU が違う: {g:?} != {w:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relays_real_h264_aac_bit_identical_with_one_upstream() {
    let Some(src) = source_events(0) else { return };
    let (cfgs, aus) = media().unwrap();
    let (a, _ia) = endpoint(1);
    let (b, ib) = endpoint(2);
    let served = Arc::new(AtomicUsize::new(0));
    serve(
        ib,
        HashMap::from([("live".to_string(), src)]),
        served.clone(),
    );
    link_tcp(&a, &b).await;
    let hub = RelayHub::new(
        a.clone(),
        Arc::new(|_: &str| Some(2)),
        RelayHubConfig {
            agg: AggregatorConfig {
                linger: Duration::from_millis(200),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    // 3 人が同じ stream を見る
    let mut subs: Vec<_> = (0..3)
        .map(|_| hub.subscribe("live", TrackSel::All))
        .collect();
    let total = aus.len();
    let mut gots = Vec::new();
    for s in subs.iter_mut() {
        let mut g = Got::default();
        for e in s.initial() {
            g.add(e);
        }
        while g.n < total {
            let e = tokio::time::timeout(Duration::from_secs(10), s.recv())
                .await
                .expect("時間切れ")
                .unwrap();
            g.add(e);
        }
        gots.push(g);
    }
    assert_eq!(served.load(Ordering::SeqCst), 1, "上流の購読は 1 つ");
    let want = per_track(aus);
    for g in &gots {
        assert_eq!(&g.cfgs, cfgs);
        for (t, w) in &want {
            assert_identical(&g.aus[t], w);
        }
    }
    // 後から来た購読者には直近の GOP（キーフレームから）
    let mut late = hub.subscribe("live", TrackSel::All);
    let mut g = Got::default();
    for e in late.initial() {
        g.add(e);
    }
    assert_eq!(&g.cfgs, cfgs);
    let video = &want[&TrackId(0)];
    let last_key = video.iter().rposition(|a| a.keyframe).unwrap();
    assert_identical(&g.aus[&TrackId(0)], &video[last_key..]);
    assert!(g.aus[&TrackId(1)].len() >= 20, "音声も GOP の分だけ");
    let st = &a.stats()[0].1;
    println!(
        "受けた: {} バイト / {} AU、payload {} バイト、AU 当たりの見出し {:.2} バイト、RTT {:?}",
        st.rx_bytes,
        st.rx_aus,
        st.rx_payload_bytes,
        (st.rx_bytes - st.rx_payload_bytes) as f64 / st.rx_aus as f64,
        st.rtt
    );
    assert_eq!(st.rx_gaps, 0);
    // 最後の購読者が去ると linger 後に上流を閉じる
    drop(subs);
    drop(late);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(hub.stats().upstreams, 0);
    assert_eq!(b.stats()[0].1.downstreams, 0, "元のノードの購読も終わる");
}

/// まとめ書きのできないストリーム（tls_io の DuplexStream と同じ）でも同じ物が届く。層の選択も確かめる。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relays_over_duplex_with_track_selection() {
    let Some(src) = source_events(0) else { return };
    let (_, aus) = media().unwrap();
    let (a, _ia) = endpoint(1);
    let (b, ib) = endpoint(2);
    serve(
        ib,
        HashMap::from([("live".to_string(), src)]),
        Arc::new(AtomicUsize::new(0)),
    );
    let (x, y) = tokio::io::duplex(64 << 10);
    let (ra, rb) = tokio::join!(a.connect(x), b.accept(y));
    ra.unwrap();
    rb.unwrap();
    // 音声だけ
    let mut up = a
        .subscribe(2, "live", TrackSel::only([TrackId(1)]), 0)
        .await
        .unwrap();
    let want: Vec<AccessUnit> = aus
        .iter()
        .filter(|a| a.track == TrackId(1))
        .cloned()
        .collect();
    let mut got = Got::default();
    while got.n < want.len() {
        got.add(
            tokio::time::timeout(Duration::from_secs(10), up.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(got.cfgs.len(), 1);
    assert_identical(&got.aus[&TrackId(1)], &want);
    assert!(!got.aus.contains_key(&TrackId(0)));
    // 音声は 5 ms 以内でまとめて書く: 枠の数は AU の数よりずっと少ない
    let st = b.stats()[0].1.clone();
    println!(
        "音声だけ: {} AU を {} 枠・{} 回の書き込みで",
        st.tx_aus, st.tx_frames, st.tx_writes
    );
    assert!(st.tx_frames < st.tx_aus);
}

/// 元のノードが落ちたら、統合側（Resolver）が指す別のノードから購読し直す。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fails_over_to_another_node_on_connection_loss() {
    let Some(src_b) = source_events(0) else {
        return;
    };
    let src_c = source_events(1).unwrap();
    let (a, _ia) = endpoint(1);
    let (b, ib) = endpoint(2);
    let (c, ic) = endpoint(3);
    serve(
        ib,
        HashMap::from([("live".to_string(), src_b)]),
        Arc::new(AtomicUsize::new(0)),
    );
    serve(
        ic,
        HashMap::from([("live".to_string(), src_c)]),
        Arc::new(AtomicUsize::new(0)),
    );
    link_tcp(&a, &b).await;
    link_tcp(&a, &c).await;
    let owner = Arc::new(AtomicU64::new(2));
    let o2 = owner.clone();
    let hub = RelayHub::new(
        a.clone(),
        Arc::new(move |_: &str| Some(o2.load(Ordering::SeqCst))),
        RelayHubConfig::default(),
    );
    let mut s = hub.subscribe("live", TrackSel::All);
    let mut g = Got::default();
    while g.n < 50 {
        g.add(
            tokio::time::timeout(Duration::from_secs(10), s.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    // 元のノード（2）が落ち、所有権が 3 へ移った（ADR-0013）
    owner.store(3, Ordering::SeqCst);
    b.close_peer(1, "node down");
    // 不連続 → 新しい上流の設定 → epoch 1 のキーフレーム
    let mut seen_disc = false;
    let mut seen_cfg = false;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), s.recv())
            .await
            .expect("張り直せない")
            .unwrap();
        match e {
            StreamEvent::Discontinuity => seen_disc = true,
            StreamEvent::Config(..) if seen_disc => seen_cfg = true,
            StreamEvent::Au(au) if au.epoch == 1 => {
                assert!(seen_disc && seen_cfg);
                assert!(
                    au.codec.kind() != MediaKind::Video || au.keyframe,
                    "映像はキーフレームから"
                );
                break;
            }
            _ => {}
        }
    }
    assert_eq!(hub.stats().upstreams, 1);
}

/// GoAway: 古い上流を流したまま新しい上流を張り、張れたら切り替える。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goaway_moves_subscription_make_before_break() {
    let Some(src_b) = source_events(0) else {
        return;
    };
    let src_c = source_events(1).unwrap();
    let (a, _ia) = endpoint(1);
    let (b, ib) = endpoint(2);
    let (c, ic) = endpoint(3);
    let served_b = Arc::new(AtomicUsize::new(0));
    let served_c = Arc::new(AtomicUsize::new(0));
    serve(
        ib,
        HashMap::from([("live".to_string(), src_b)]),
        served_b.clone(),
    );
    serve(
        ic,
        HashMap::from([("live".to_string(), src_c)]),
        served_c.clone(),
    );
    link_tcp(&a, &b).await;
    link_tcp(&a, &c).await;
    let owner = Arc::new(AtomicU64::new(2));
    let o2 = owner.clone();
    let hub = RelayHub::new(
        a.clone(),
        Arc::new(move |_: &str| Some(o2.load(Ordering::SeqCst))),
        RelayHubConfig::default(),
    );
    let mut s = hub.subscribe("live", TrackSel::All);
    let mut g = Got::default();
    while g.n < 20 {
        g.add(
            tokio::time::timeout(Duration::from_secs(10), s.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    owner.store(3, Ordering::SeqCst);
    b.go_away("maintenance");
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), s.recv())
            .await
            .expect("移れない")
            .unwrap();
        if let StreamEvent::Au(au) = e
            && au.epoch == 1
        {
            break;
        }
    }
    assert_eq!(served_c.load(Ordering::SeqCst), 1);
    // 古い購読はやめた（B の接続は残る）
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(b.stats()[0].1.downstreams, 0);
    assert_eq!(a.peers(), vec![2, 3]);
}

/// 同時に両方からつないでも、残るのは 1 本。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_connection_per_node_pair() {
    let (a, _ia) = endpoint(1);
    let (b, ib) = endpoint(2);
    let (x1, y1) = tokio::io::duplex(1 << 16);
    let (x2, y2) = tokio::io::duplex(1 << 16);
    let (r1, r2, r3, r4) = tokio::join!(a.connect(x1), b.accept(y1), b.connect(x2), a.accept(y2));
    let (c1, c2) = (r1.unwrap(), r2.unwrap());
    let _ = (r3.unwrap(), r4.unwrap());
    // どちらの側も、ノード 1 がつないだ接続を残す
    assert!(a.connection(2).unwrap().same_as(&c1));
    assert!(b.connection(1).unwrap().same_as(&c2));
    assert_eq!(a.peers(), vec![2]);
    assert_eq!(b.peers(), vec![1]);
    // 残った接続で両方向に購読できる
    serve(ib, HashMap::new(), Arc::new(AtomicUsize::new(0)));
    let e = a.subscribe(2, "none", TrackSel::All, 0).await.unwrap_err();
    assert!(
        matches!(
            e,
            senca_relay::net::RelayError::Lost(senca_relay::LostReason::Rejected { code: 1, .. })
        ),
        "{e:?}"
    );
}

/// 元のノードが End を積んですぐに下流を手放しても、End は捨てられずに届く（積んだ物を送ってから閉じる）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_is_delivered_even_if_the_downstream_is_dropped_right_after() {
    let Some(src) = source_events(0) else { return };
    let (a, _ia) = endpoint(1);
    let (b, mut ib) = endpoint(2);
    tokio::spawn(async move {
        while let Some(inc) = ib.recv().await {
            let down = inc.accept(None);
            down.send_all(&src);
            down.send(&StreamEvent::End);
            drop(down);
        }
    });
    // 狭い管にして、手放した時点でまだ積んである状態にする
    let (x, y) = tokio::io::duplex(16 << 10);
    let (ra, rb) = tokio::join!(a.connect(x), b.accept(y));
    ra.unwrap();
    rb.unwrap();
    let mut up = a.subscribe(2, "live", TrackSel::All, 0).await.unwrap();
    let mut n = 0usize;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), up.recv()).await.unwrap() {
            Some(StreamEvent::End) => break,
            Some(StreamEvent::Lost(r)) => panic!("lost before End after {n} events: {r:?}"),
            Some(_) => n += 1,
            None => panic!("closed before End after {n} events"),
        }
    }
    assert!(n > 100, "{n}");
}
