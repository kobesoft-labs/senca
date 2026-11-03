//! ノード間の中継の統合（ADR-0014、senca-relay）。
//!
//! - ノードの組ごとに 1 本の接続（mTLS。証明書が無いときは平文の TCP）。どちらでも、中継の握手の前にクラスタの
//!   合言葉による相互の挑戦応答（`cluster_auth`、用途 relay）で互いの番号を確かめる。
//! - 所有者のノード: 相手からの購読を受け、手元の hub の購読をそのまま中継のチャネルへ流す。
//! - 所有者でないノード: 視聴（HLS・RTMP/SRT/RTSP play・WHEP）や処理の入力が来たら、その stream の
//!   「写し（mirror）」を作る。写しは中継の購読を送り手として手元の hub へつなぐので、パッケージャー・
//!   GOP キャッシュ・購読者への配り方は所有者のノードと同じ。同じ stream を見る購読者が何人いても上流は 1 本。
//! - 写しは録画・字幕・所有権の主張をしない（それらは所有者のノードの仕事）。
//! - 使われなくなった写しは片付ける。所有権がこのノードへ移ったら、写しを止めて本物の送り手に譲る。
//! - 部屋のカスケード（[`crate::cascade`]）も同じ接続に載せる: 在室・アプリの message は App の枠（topic
//!   `senca.rooms/1`）、参加者のトラックは stream id `@room/<部屋>/<トラック>` の購読（部屋の係が答える）。

use crate::app::App;
use crate::cluster::{CLUSTER_SERVER_NAME, ClusterTls};
use crate::config::ClusterConfig;
use crate::hub::{HubEvent, HubHandle};
use senca_relay::net::{DownEvent, IncomingSubscribe, RelayEndpoint, RelayHub, RelayHubConfig, RelayStream};
use senca_relay::{ErrorCode, StreamEvent, TrackSel};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::cluster_auth::{AuthKey, Purpose};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::watch;

/// 写しを使われないまま置いておく時間。
const MIRROR_IDLE: Duration = Duration::from_secs(30);
/// ソケットの送りのバッファ（混雑は核のスケジューラーで賢く捨てたいので、カーネルに溜めすぎない）。
const SEND_BUFFER: u32 = 512 << 10;
/// 写しを作ってから最初の再生リストが出るまで待つ上限。
pub const MIRROR_WARMUP: Duration = Duration::from_secs(6);
/// 写しのノードが所有者から字幕のファイルを取るときに付ける印（所有者の側では視聴者に数えず、取り次ぎもしない）。
pub const PROXY_HEADER: &str = "x-senca-relay-proxy";
/// 取ってきた字幕を覚えておく長さ（プレイリスト・セグメント）と数。
const CAPTION_TTL_PLAYLIST_MS: u64 = 1000;
const CAPTION_TTL_SEGMENT_MS: u64 = 30_000;
const CAPTION_CACHE_MAX: usize = 1024;

struct Mirror {
    /// 作った順の番号（後片付けで新しい写しの印を消さない）。
    generation: u64,
    created_ms: i64,
    last_access: Arc<AtomicI64>,
    stop: watch::Sender<bool>,
    /// 後片付けが済んだ。
    done: watch::Receiver<bool>,
    /// 上流が終わった時刻（0 = 流れている）。終わった写しの hub は、見ている人が ENDLIST を受け取れるよう
    /// しばらく残し、使われなくなるか所有者がまた配信を始めたら片付ける。
    ended_ms: Arc<AtomicI64>,
    hub: Arc<crate::hub::HubShared>,
    /// 写しを作ったときの所有権の token（新しい publish で変わる。変わったら終わった写しを作り直す）。
    owner_token: u64,
}

pub struct Relay {
    pub hub: RelayHub,
    pub endpoint: RelayEndpoint,
    mirrors: Mutex<HashMap<String, Mirror>>,
    next_gen: AtomicU64,
    /// 所有者から取ってきた字幕のファイル（パス → (取った時刻, 本文, Content-Type)）。
    captions: Mutex<HashMap<String, (u64, bytes::Bytes, &'static str)>>,
}

/// 中継の待ち受け（既定は Raft の待ち受けの次の番号）。
pub fn listen_addr(cc: &ClusterConfig) -> SocketAddr {
    cc.relay_listen.unwrap_or_else(|| SocketAddr::new(cc.listen.ip(), cc.listen.port().wrapping_add(1)))
}

fn peer_addr(p: &crate::config::ClusterPeer) -> SocketAddr {
    p.relay.unwrap_or_else(|| SocketAddr::new(p.addr.ip(), p.addr.port().wrapping_add(1)))
}

/// 中継の宛先が Raft の宛先や他のノードの中継とぶつからないか（既定の「Raft + 1」は、同じホストで Raft の番号を
/// 続けて取ると隣のノードの Raft と重なる）。ぶつかれば起動しない（明示の `relay_listen`・`peers[].relay` を求める）。
pub fn check_addrs(cc: &ClusterConfig) -> Result<(), String> {
    use std::collections::HashMap;
    let mut seen: HashMap<SocketAddr, String> = HashMap::new();
    let mut put = |a: SocketAddr, what: String| -> Result<(), String> {
        // 0.0.0.0 などの待ち受けは、同じ番号の宛先とぶつかるものとして比べる
        let key = if a.ip().is_unspecified() { SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), a.port()) } else { a };
        if let Some(prev) = seen.insert(key, what.clone()) {
            return Err(format!(
                "cluster: {what} と {prev} が同じ宛先 {a} です。中継の既定は Raft の番号 + 1 なので、同じホストで Raft の番号を続けて使うときは relay_listen と peers[].relay を明示してください"
            ));
        }
        Ok(())
    };
    for p in &cc.peers {
        put(p.addr, format!("node {} の Raft（peers[].addr）", p.id))?;
    }
    for p in &cc.peers {
        let explicit = if p.relay.is_some() { "peers[].relay" } else { "peers[].relay の既定 = addr + 1" };
        put(peer_addr(p), format!("node {} の中継（{explicit}）", p.id))?;
    }
    // 自分の待ち受け: Raft の待ち受けと中継の待ち受けが同じ番号でない
    let l = listen_addr(cc);
    if l.port() == cc.listen.port() && (l.ip() == cc.listen.ip() || l.ip().is_unspecified() || cc.listen.ip().is_unspecified()) {
        return Err(format!("cluster: relay_listen {l} が listen {} と同じです", cc.listen));
    }
    if cc.relay_listen.is_none() && cc.listen.port() == u16::MAX {
        return Err("cluster: listen の番号が 65535 なので中継の既定（+1）が作れません。relay_listen を書いてください".into());
    }
    Ok(())
}

fn tune(tcp: &TcpStream) {
    let _ = tcp.set_nodelay(true);
}

async fn dial(addr: SocketAddr, tls: Option<ClusterTls>, key: Arc<AuthKey>, me: u64, peer: u64) -> Result<Box<dyn RelayStream>, String> {
    let sock = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }.map_err(|e| e.to_string())?;
    let _ = sock.set_send_buffer_size(SEND_BUFFER);
    let tcp = tokio::time::timeout(Duration::from_secs(5), sock.connect(addr))
        .await
        .map_err(|_| format!("relay connect {addr}: timeout"))?
        .map_err(|e| format!("relay connect {addr}: {e}"))?;
    tune(&tcp);
    let mut s: Box<dyn RelayStream> = match tls {
        Some(t) => Box::new(crate::tls_io::connect(tcp, t.client, CLUSTER_SERVER_NAME).await?),
        None => Box::new(tcp),
    };
    crate::cluster_auth::client(&mut s, &key, me, peer).await.map_err(|e| format!("relay auth to node {peer}: {e}"))?;
    Ok(s)
}

/// 所有者のノード（Raft の番号）。
fn owner_raft_id(app: &App, stream: &str) -> Option<u64> {
    let core = app.control.lock().unwrap();
    let o = core.state.owners.get(stream)?;
    if o.node == app.node_id {
        return None;
    }
    core.state.nodes.get(&o.node)?.labels.get("raft_id")?.parse().ok()
}

/// 今の所有権の token。
fn owner_token(app: &App, stream: &str) -> Option<u64> {
    app.control.lock().unwrap().state.owners.get(stream).map(|o| o.token)
}

pub fn start(app: &Arc<App>, cc: &ClusterConfig, secret: String) -> Result<Arc<Relay>, String> {
    let tls = match (&cc.tls_ca, &cc.tls_cert, &cc.tls_key) {
        (Some(ca), Some(cert), Some(key)) => Some(ClusterTls::load(ca, cert, key)?),
        _ => None,
    };
    let key = Arc::new(AuthKey::derive(&secret, Purpose::Relay));
    let me = cc.node_id;
    let known: Arc<Vec<u64>> = Arc::new(cc.peers.iter().map(|p| p.id).collect());
    let mut ecfg = senca_relay::net::EndpointConfig::default();
    ecfg.conn.node_id = cc.node_id;
    let (endpoint, mut incoming) = RelayEndpoint::new(ecfg);
    let addrs: HashMap<u64, SocketAddr> = cc.peers.iter().map(|p| (p.id, peer_addr(p))).collect();
    {
        let tls = tls.clone();
        let key = key.clone();
        endpoint.set_dialer(Arc::new(move |peer| {
            let addr = addrs.get(&peer).copied();
            let tls = tls.clone();
            let key = key.clone();
            Box::pin(async move {
                let addr = addr.ok_or_else(|| format!("relay: unknown node {peer}"))?;
                dial(addr, tls, key, me, peer).await
            })
        }));
    }
    let weak = Arc::downgrade(app);
    let resolver = Arc::new(move |stream: &str| weak.upgrade().and_then(|a| owner_raft_id(&a, stream)));
    let hub = RelayHub::new(endpoint.clone(), resolver, RelayHubConfig::default());
    let relay = Arc::new(Relay {
        hub,
        endpoint: endpoint.clone(),
        mirrors: Mutex::new(HashMap::new()),
        next_gen: AtomicU64::new(1),
        captions: Mutex::new(HashMap::new()),
    });

    // 待ち受け
    let listen = listen_addr(cc);
    let std_l = std::net::TcpListener::bind(listen).map_err(|e| format!("relay {listen}: {e}"))?;
    std_l.set_nonblocking(true).map_err(|e| e.to_string())?;
    let listener = TcpListener::from_std(std_l).map_err(|e| e.to_string())?;
    {
        let ep = endpoint.clone();
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, peer) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => {
                        // EMFILE などで回り続けない
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                tune(&tcp);
                let ep = ep.clone();
                let tls = tls.clone();
                let key = key.clone();
                let known = known.clone();
                let app = app.clone();
                tokio::spawn(async move {
                    let r = async {
                        let mut s: Box<dyn RelayStream> = match tls {
                            Some(t) => Box::new(crate::tls_io::accept(tcp, t.server).await?),
                            None => Box::new(tcp),
                        };
                        let who = crate::cluster_auth::server(&mut s, &key, me, &|id| known.contains(&id)).await?;
                        let c = ep.accept(s).await.map_err(|e| e.to_string())?;
                        // 中継の握手で名乗った番号が、合言葉で確かめた番号と同じか
                        if c.peer_id() != Some(who) {
                            c.close("node id mismatch");
                            return Err(format!("node {who} introduced itself as {:?}", c.peer_id()));
                        }
                        Ok::<(), String>(())
                    }
                    .await;
                    if let Err(e) = r {
                        app.log(&format!("relay: connection from {peer} refused: {e}"));
                    }
                });
            }
        });
    }
    app.log(&format!("relay listening on {listen}"));

    // 部屋のカスケードの message（部屋の係へ）
    if let Some(mut apps) = endpoint.take_app_messages() {
        let app = app.clone();
        tokio::spawn(async move {
            while let Some(m) = apps.recv().await {
                if m.topic != crate::cascade::TOPIC {
                    continue;
                }
                if let Some(rtc) = app.rtc.get() {
                    rtc.room_cmd(crate::rooms::RoomCmd::CascadeIn { peer: m.peer, payload: m.payload });
                }
            }
        });
    }
    // 相手からの購読
    {
        let app = app.clone();
        let relay = relay.clone();
        tokio::spawn(async move {
            while let Some(inc) = incoming.recv().await {
                serve(&app, &relay, inc);
            }
        });
    }
    // 写しの片付け
    {
        let app = app.clone();
        let relay = relay.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                relay.sweep(&app).await;
            }
        });
    }
    Ok(relay)
}

pub fn map_event(ev: &HubEvent) -> StreamEvent {
    match ev {
        HubEvent::Config(t, c) => StreamEvent::Config(*t, c.clone()),
        HubEvent::Au(a) => StreamEvent::Au(a.clone()),
        HubEvent::Discontinuity => StreamEvent::Discontinuity,
        HubEvent::End => StreamEvent::End,
        HubEvent::Segment { seq, start_us, duration_us, discontinuity } => {
            StreamEvent::Segment { seq: *seq, start_us: *start_us, duration_us: *duration_us, discontinuity: *discontinuity }
        }
    }
}

/// 相手のノードからの購読に答える（このノードが送り手を持つ stream だけ）。
fn serve(app: &Arc<App>, relay: &Arc<Relay>, inc: IncomingSubscribe) {
    if inc.stream_id.starts_with(crate::cascade::STREAM_PREFIX) {
        // 部屋の参加者のトラック: 部屋の係が答える
        match app.rtc.get() {
            Some(rtc) => rtc.room_cmd(crate::rooms::RoomCmd::RelayIncoming(inc)),
            None => inc.reject(ErrorCode::NOT_FOUND, "rooms are not enabled on this node"),
        }
        return;
    }
    let is_mirror = relay.is_mirror(&inc.stream_id);
    let hub = app.hubs.lock().unwrap().get(&inc.stream_id).cloned();
    let (owned_here, owned_elsewhere) = {
        let core = app.control.lock().unwrap();
        let o = core.state.owners.get(&inc.stream_id);
        (o.is_some_and(|o| o.node == app.node_id), o.is_some_and(|o| o.node != app.node_id))
    };
    let hub = match hub {
        // 写しは配らない（中継の中継を作らない。相手は所有者から直接受ける）
        Some(h) if !is_mirror && (owned_here || h.is_live()) => h,
        _ if owned_elsewhere => {
            inc.reject(ErrorCode::MOVED, "owned by another node");
            return;
        }
        _ => {
            inc.reject(ErrorCode::NOT_FOUND, "not live on this node");
            return;
        }
    };
    let app = app.clone();
    tokio::spawn(async move {
        let peer = inc.peer;
        let stream = inc.stream_id.clone();
        let mut down = inc.accept(None);
        let mut sub = hub.subscribe();
        let init: Vec<StreamEvent> = sub.initial().iter().map(map_event).collect();
        // send の false は「捨てた」（キーフレーム待ちなど）も含む。閉じたかは is_closed で見る
        down.send_all(&init);
        loop {
            if down.is_closed() {
                break;
            }
            tokio::select! {
                ev = sub.recv() => match ev {
                    Ok(ev) => {
                        let end = matches!(ev, HubEvent::End);
                        down.send(&map_event(&ev));
                        if end {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => sub.lagged(n),
                    Err(_) => {
                        down.send(&StreamEvent::End);
                        break;
                    }
                },
                de = down.next_event() => match de {
                    // 送り手へのキーフレームの要求は、送り手の種類ごとの口が要る（WebRTC は定期の PLI で足りる）
                    Some(DownEvent::KeyframeRequest(_)) | Some(DownEvent::TracksChanged(_)) => {}
                    None => break,
                },
            }
        }
        // down を手放すと、積んである物（End など）を送り終えてから閉じる（senca-relay）
        app.log(&format!("relay: stream {stream} to node {peer} closed"));
    });
}

impl Relay {
    fn touch(&self, stream: &str) -> bool {
        match self.mirrors.lock().unwrap().get(stream) {
            Some(m) => {
                m.last_access.store(crate::util::mono_ms() as i64, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn is_mirror(&self, stream: &str) -> bool {
        self.mirrors.lock().unwrap().contains_key(stream)
    }

    /// 写しを作ってからの待ちの残り（最初の再生リストを待つ。作ってしばらく経てば待たない）。
    pub fn warmup_left(&self, stream: &str) -> Duration {
        let now = crate::util::mono_ms() as i64;
        self.mirrors
            .lock()
            .unwrap()
            .get(stream)
            .map(|m| Duration::from_millis((m.created_ms + MIRROR_WARMUP.as_millis() as i64 - now).max(0) as u64))
            .unwrap_or_default()
    }

    /// 別のノードが持つ stream の写しをこのノードに用意する（既にあれば使った印を付けるだけ）。
    /// 写しの hub を返す（所有者が分からない・このノードが所有者なら None）。
    pub fn ensure_mirror(self: &Arc<Self>, app: &Arc<App>, stream_id: &str) -> Option<HubHandle> {
        // 上流が終わった写しは、所有権が新しくなったら（また配信が始まったら）片付けて作り直す。
        // それまでは終わった hub（ENDLIST）を見せる
        let token = owner_token(app, stream_id);
        let stale = self
            .mirrors
            .lock()
            .unwrap()
            .get(stream_id)
            .is_some_and(|m| m.ended_ms.load(Ordering::Relaxed) > 0 && token.is_some_and(|t| t != m.owner_token));
        if stale && let Some(h) = self.take_ended(app, stream_id) {
            tokio::spawn(async move { h.shutdown().await });
        }
        if self.touch(stream_id) {
            return app.hubs.lock().unwrap().get(stream_id).cloned();
        }
        let owner = owner_raft_id(app, stream_id)?;
        let stream = app.control.lock().unwrap().state.streams.get(stream_id).cloned()?;
        let rt = app.rt.get()?.clone();
        // 順序: mirrors → hubs（逆に取らない）
        let mut mirrors = self.mirrors.lock().unwrap();
        if mirrors.contains_key(stream_id) {
            drop(mirrors);
            self.touch(stream_id);
            return app.hubs.lock().unwrap().get(stream_id).cloned();
        }
        let hub = app.hub(&stream);
        if hub.is_live() {
            // 手元に本物の送り手がいる（所有権の更新がまだ届いていない）
            return Some(hub);
        }
        let now = crate::util::mono_ms() as i64;
        let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let ended_ms = Arc::new(AtomicI64::new(0));
        mirrors.insert(
            stream_id.to_string(),
            Mirror {
                generation,
                created_ms: now,
                last_access: Arc::new(AtomicI64::new(now)),
                stop: stop_tx,
                done: done_rx,
                ended_ms: ended_ms.clone(),
                hub: hub.shared.clone(),
                owner_token: owner_token(app, stream_id).unwrap_or(0),
            },
        );
        drop(mirrors);
        app.log(&format!("relay: mirroring stream {stream_id} from node {owner}"));
        let me = self.clone();
        let app2 = app.clone();
        let h2 = hub.clone();
        let sid = stream_id.to_string();
        rt.spawn(async move {
            let upstream_ended = me.run_mirror(&app2, h2.clone(), &sid, owner, stop_rx).await;
            if upstream_ended {
                // hub は残す（見ている人に ENDLIST を渡す）。片付けは sweep・作り直し・本物の送り手の到着で
                ended_ms.store((crate::util::mono_ms() as i64).max(1), Ordering::Relaxed);
                let _ = done_tx.send(true);
                return;
            }
            // 後片付け: hub を外してから写しの印を消す（その間に来た送り手は stop_mirror で待つ）
            let removed = {
                let mut hubs = app2.hubs.lock().unwrap();
                match hubs.get(&sid) {
                    Some(h) if Arc::ptr_eq(&h.shared, &h2.shared) => hubs.remove(&sid),
                    _ => None,
                }
            };
            if let Some(h) = removed {
                h.shutdown().await;
            }
            {
                let mut mirrors = me.mirrors.lock().unwrap();
                if mirrors.get(&sid).is_some_and(|m| m.generation == generation) {
                    mirrors.remove(&sid);
                }
            }
            let _ = done_tx.send(true);
        });
        Some(hub)
    }

    /// 写しを動かす。上流が終わって止まったら true。
    async fn run_mirror(&self, app: &Arc<App>, hub: HubHandle, sid: &str, owner: u64, mut stop: watch::Receiver<bool>) -> bool {
        let mut sub = self.hub.subscribe(sid, TrackSel::All);
        let peer = format!("relay:node-{owner}");
        let Ok(mut p) = hub.attach("relay", &peer).await else {
            app.log(&format!("relay: mirror {sid}: hub busy"));
            return false;
        };
        let mut pending: std::collections::VecDeque<StreamEvent> = sub.initial().into();
        let why = loop {
            if *stop.borrow() {
                break "stopped";
            }
            let ev = match pending.pop_front() {
                Some(ev) => ev,
                None => tokio::select! {
                    ev = sub.recv() => match ev { Some(ev) => ev, None => break "upstream closed" },
                    _ = stop.changed() => break "stopped",
                },
            };
            match ev {
                StreamEvent::Config(t, c) => {
                    if p.config(t, c).await.is_err() {
                        break "hub closed";
                    }
                }
                StreamEvent::Au(a) => {
                    if p.access_unit(a).await.is_err() {
                        break "hub closed";
                    }
                }
                StreamEvent::Discontinuity => {
                    // 新しい epoch（パッケージャーが DISCONTINUITY を入れる）
                    p.detach("upstream discontinuity").await;
                    match hub.attach("relay", &peer).await {
                        Ok(np) => p = np,
                        Err(_) => {
                            app.log(&format!("relay: mirror of {sid} stopped (re-attach failed)"));
                            return false;
                        }
                    }
                }
                StreamEvent::End => break "upstream ended",
                StreamEvent::Segment { .. } | StreamEvent::Lost(_) => {}
            }
        };
        p.detach("mirror stopped").await;
        app.log(&format!("relay: mirror of {sid} stopped ({why})"));
        why == "upstream ended"
    }

    /// 上流が終わった写しの印と hub を外す（hub は呼び手が shutdown する）。
    fn take_ended(&self, app: &App, stream: &str) -> Option<HubHandle> {
        let mut mirrors = self.mirrors.lock().unwrap();
        let m = mirrors.get(stream)?;
        if m.ended_ms.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let shared = m.hub.clone();
        mirrors.remove(stream);
        let mut hubs = app.hubs.lock().unwrap();
        match hubs.get(stream) {
            Some(h) if Arc::ptr_eq(&h.shared, &shared) => hubs.remove(stream),
            _ => None,
        }
    }

    /// 写しを止め、後片付け（hub を外す）が済むまで待つ。何人が同時に呼んでもみな待つ。
    pub async fn stop_mirror(&self, app: &App, stream: &str) {
        if let Some(h) = self.take_ended(app, stream) {
            h.shutdown().await;
            return;
        }
        let done = {
            let mirrors = self.mirrors.lock().unwrap();
            let Some(m) = mirrors.get(stream) else { return };
            let _ = m.stop.send(true);
            m.done.clone()
        };
        let mut done = done;
        let _ = tokio::time::timeout(Duration::from_secs(5), done.wait_for(|d| *d)).await;
    }

    /// 使われていない写しを片付ける（HLS の取得が途絶え、hub の購読者もいない）。
    async fn sweep(&self, app: &Arc<App>) {
        let now = crate::util::mono_ms() as i64;
        let idle: Vec<String> = self
            .mirrors
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, m)| now - m.last_access.load(Ordering::Relaxed) > MIRROR_IDLE.as_millis() as i64)
            .map(|(k, _)| k.clone())
            .collect();
        for sid in idle {
            let hub = app.hubs.lock().unwrap().get(&sid).cloned();
            if hub.as_ref().is_some_and(|h| h.shared.fanout.receiver_count() > 0) {
                self.touch(&sid);
                continue;
            }
            self.stop_mirror(app, &sid).await;
        }
    }

    /// 字幕のファイルを所有者のノードから取る（`path` は `/s/{exp}/{sig}/live/{stream}/{file}`。署名はどのノードでも通る）。
    /// 字幕は所有者のノードのメモリにだけあるので、写しのノードはこれで取り次ぐ。プレイリストは 1 秒、セグメントは 30 秒覚える。
    /// 所有者のノードの公開 URL（`peers[].api`）は管理者が書いたものなので、外向きの接続の制限（egress）は掛けない。
    pub async fn caption_file(&self, app: &Arc<App>, stream: &str, path: &str) -> Option<(bytes::Bytes, &'static str)> {
        let ttl = if path.ends_with(".vtt") { CAPTION_TTL_SEGMENT_MS } else { CAPTION_TTL_PLAYLIST_MS };
        let now = crate::util::mono_ms();
        if let Some((t, b, ct)) = self.captions.lock().unwrap().get(path)
            && now.saturating_sub(*t) < ttl
        {
            return Some((b.clone(), ct));
        }
        let base = app.owner_base(stream)?;
        let any = ["0.0.0.0/0".to_string(), "::/0".to_string()];
        let r = crate::httpc::request(
            &any,
            senca_http::Method::Get,
            &format!("{}{path}", base.trim_end_matches('/')),
            &[(PROXY_HEADER, "1".to_string())],
            None,
            1 << 20,
            Duration::from_secs(3),
        )
        .await
        .ok()?;
        if r.status != 200 {
            return None;
        }
        let ct = if path.ends_with(".vtt") { "text/vtt" } else { "application/vnd.apple.mpegurl" };
        let mut c = self.captions.lock().unwrap();
        if c.len() >= CAPTION_CACHE_MAX {
            c.retain(|k, (t, _, _)| now.saturating_sub(*t) < if k.ends_with(".vtt") { CAPTION_TTL_SEGMENT_MS } else { CAPTION_TTL_PLAYLIST_MS });
            if c.len() >= CAPTION_CACHE_MAX {
                c.clear();
            }
        }
        c.insert(path.to_string(), (now, r.body.clone(), ct));
        Some((r.body, ct))
    }

    /// 中継の状態（GET /nodes の補足）。
    pub fn status(&self) -> serde_json::Value {
        let peers: Vec<serde_json::Value> = self
            .endpoint
            .stats()
            .into_iter()
            .map(|(id, s)| {
                serde_json::json!({
                    "node": id,
                    "rtt_ms": s.rtt.map(|d| d.as_secs_f64() * 1000.0),
                    "tx_bps": s.tx_bps,
                    "rx_bps": s.rx_bps,
                    "tx_bytes": s.tx_bytes,
                    "rx_bytes": s.rx_bytes,
                    "tx_aus": s.tx_aus,
                    "rx_aus": s.rx_aus,
                    "overhead_per_au": s.tx_overhead_per_au(),
                    "upstreams": s.upstreams,
                    "downstreams": s.downstreams,
                    "going_away": s.going_away || s.peer_going_away,
                })
            })
            .collect();
        let mirrors: Vec<String> = self.mirrors.lock().unwrap().keys().cloned().collect();
        serde_json::json!({ "peers": peers, "mirrors": mirrors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClusterPeer;

    fn cc(peers: &[(u64, &str, Option<&str>)], relay_listen: Option<&str>) -> ClusterConfig {
        ClusterConfig {
            node_id: 1,
            listen: peers[0].1.parse().unwrap(),
            peers: peers.iter().map(|(id, a, r)| ClusterPeer { id: *id, addr: a.parse().unwrap(), api: None, relay: r.map(|r| r.parse().unwrap()) }).collect(),
            secret_file: "/dev/null".into(),
            tls_ca: None,
            tls_cert: None,
            tls_key: None,
            relay_listen: relay_listen.map(|r| r.parse().unwrap()),
        }
    }

    #[test]
    fn consecutive_raft_ports_on_one_host_are_refused() {
        let c = cc(&[(1, "127.0.0.1:7000", None), (2, "127.0.0.1:7001", None)], None);
        let e = check_addrs(&c).unwrap_err();
        assert!(e.contains("relay_listen"), "{e}");
        // 明示すれば通る
        let c = cc(&[(1, "127.0.0.1:7000", Some("127.0.0.1:7100")), (2, "127.0.0.1:7001", Some("127.0.0.1:7101"))], Some("127.0.0.1:7100"));
        check_addrs(&c).unwrap();
        // 別のホストなら既定で足りる
        let c = cc(&[(1, "10.0.0.1:7000", None), (2, "10.0.0.2:7000", None)], None);
        check_addrs(&c).unwrap();
        // 2 つおきなら既定でもぶつからない
        let c = cc(&[(1, "127.0.0.1:7000", None), (2, "127.0.0.1:7002", None)], None);
        check_addrs(&c).unwrap();
    }
}
