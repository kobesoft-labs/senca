//! ノード間の中継の統合（ADR-0014、senca-relay）。
//!
//! - ノードの組ごとに 1 本の接続（mTLS。証明書が無いときはクラスタの合言葉で確かめる平文の TCP）。
//! - 所有者のノード: 相手からの購読を受け、手元の hub の購読をそのまま中継のチャネルへ流す。
//! - 所有者でないノード: 視聴（HLS・RTMP/SRT/RTSP play・WHEP）や処理の入力が来たら、その stream の
//!   「写し（mirror）」を作る。写しは中継の購読を送り手として手元の hub へつなぐので、パッケージャー・
//!   GOP キャッシュ・購読者への配り方は所有者のノードと同じ。同じ stream を見る購読者が何人いても上流は 1 本。
//! - 写しは録画・字幕・所有権の主張をしない（それらは所有者のノードの仕事）。
//! - 使われなくなった写しは片付ける。所有権がこのノードへ移ったら、写しを止めて本物の送り手に譲る。

use crate::app::App;
use crate::cluster::{CLUSTER_SERVER_NAME, ClusterTls};
use crate::config::ClusterConfig;
use crate::hub::{HubEvent, HubHandle};
use senca_relay::net::{DownEvent, IncomingSubscribe, RelayEndpoint, RelayHub, RelayHubConfig, RelayStream};
use senca_relay::{ErrorCode, StreamEvent, TrackSel};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::oneshot;

/// 写しを使われないまま置いておく時間。
const MIRROR_IDLE: Duration = Duration::from_secs(30);
/// ソケットの送りのバッファ（混雑は核のスケジューラーで賢く捨てたいので、カーネルに溜めすぎない）。
const SEND_BUFFER: u32 = 512 << 10;
/// 写しを作ってから最初の再生リストが出るまで待つ上限。
pub const MIRROR_WARMUP: Duration = Duration::from_secs(6);

struct Mirror {
    last_access: Arc<AtomicI64>,
    stop: Option<oneshot::Sender<oneshot::Sender<()>>>,
}

pub struct Relay {
    pub hub: RelayHub,
    pub endpoint: RelayEndpoint,
    mirrors: Mutex<HashMap<String, Mirror>>,
}

/// 中継の待ち受け（既定は Raft の待ち受けの次の番号）。
pub fn listen_addr(cc: &ClusterConfig) -> SocketAddr {
    cc.relay_listen.unwrap_or_else(|| SocketAddr::new(cc.listen.ip(), cc.listen.port().wrapping_add(1)))
}

fn peer_addr(p: &crate::config::ClusterPeer) -> SocketAddr {
    p.relay.unwrap_or_else(|| SocketAddr::new(p.addr.ip(), p.addr.port().wrapping_add(1)))
}

fn auth_line(secret: &str) -> [u8; 64] {
    let h = senca_control::secret::hash(&format!("senca-relay/1:{secret}"));
    let mut out = [0u8; 64];
    out.copy_from_slice(&h.as_bytes()[..64]);
    out
}

fn tune(tcp: &TcpStream) {
    let _ = tcp.set_nodelay(true);
}

async fn dial(addr: SocketAddr, tls: Option<ClusterTls>, secret: Arc<String>) -> Result<Box<dyn RelayStream>, String> {
    let sock = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }.map_err(|e| e.to_string())?;
    let _ = sock.set_send_buffer_size(SEND_BUFFER);
    let tcp = tokio::time::timeout(Duration::from_secs(5), sock.connect(addr))
        .await
        .map_err(|_| format!("relay connect {addr}: timeout"))?
        .map_err(|e| format!("relay connect {addr}: {e}"))?;
    tune(&tcp);
    match tls {
        Some(t) => Ok(Box::new(crate::tls_io::connect(tcp, t.client, CLUSTER_SERVER_NAME).await?)),
        None => {
            let mut tcp = tcp;
            tcp.write_all(&auth_line(&secret)).await.map_err(|e| e.to_string())?;
            Ok(Box::new(tcp))
        }
    }
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

pub fn start(app: &Arc<App>, cc: &ClusterConfig, secret: String) -> Result<Arc<Relay>, String> {
    let tls = match (&cc.tls_ca, &cc.tls_cert, &cc.tls_key) {
        (Some(ca), Some(cert), Some(key)) => Some(ClusterTls::load(ca, cert, key)?),
        _ => None,
    };
    let secret = Arc::new(secret);
    let mut ecfg = senca_relay::net::EndpointConfig::default();
    ecfg.conn.node_id = cc.node_id;
    let (endpoint, mut incoming) = RelayEndpoint::new(ecfg);
    let addrs: HashMap<u64, SocketAddr> = cc.peers.iter().map(|p| (p.id, peer_addr(p))).collect();
    {
        let tls = tls.clone();
        let secret = secret.clone();
        endpoint.set_dialer(Arc::new(move |peer| {
            let addr = addrs.get(&peer).copied();
            let tls = tls.clone();
            let secret = secret.clone();
            Box::pin(async move {
                let addr = addr.ok_or_else(|| format!("relay: unknown node {peer}"))?;
                dial(addr, tls, secret).await
            })
        }));
    }
    let weak = Arc::downgrade(app);
    let resolver = Arc::new(move |stream: &str| weak.upgrade().and_then(|a| owner_raft_id(&a, stream)));
    let hub = RelayHub::new(endpoint.clone(), resolver, RelayHubConfig::default());
    let relay = Arc::new(Relay { hub, endpoint: endpoint.clone(), mirrors: Mutex::new(HashMap::new()) });

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
                let Ok((tcp, peer)) = listener.accept().await else { continue };
                tune(&tcp);
                let ep = ep.clone();
                let tls = tls.clone();
                let secret = secret.clone();
                let app = app.clone();
                tokio::spawn(async move {
                    let r = match tls {
                        Some(t) => match crate::tls_io::accept(tcp, t.server).await {
                            Ok(s) => ep.accept(s).await.map(|_| ()).map_err(|e| e.to_string()),
                            Err(e) => Err(e),
                        },
                        None => {
                            let mut tcp = tcp;
                            let mut got = [0u8; 64];
                            let ok = tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut got)).await.is_ok_and(|r| r.is_ok());
                            if ok && senca_control::secret::verify(
                                    std::str::from_utf8(&got).unwrap_or(""),
                                    &senca_control::secret::hash(std::str::from_utf8(&auth_line(&secret)).unwrap_or("x")),
                                )
                            {
                                ep.accept(tcp).await.map(|_| ()).map_err(|e| e.to_string())
                            } else {
                                Err("bad cluster secret".into())
                            }
                        }
                    };
                    if let Err(e) = r {
                        app.log(&format!("relay: connection from {peer} refused: {e}"));
                    }
                });
            }
        });
    }
    app.log(&format!("relay listening on {listen}"));

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
    let is_mirror = relay.mirrors.lock().unwrap().contains_key(&inc.stream_id);
    let hub = app.hubs.lock().unwrap().get(&inc.stream_id).cloned();
    let owned_here = app.control.lock().unwrap().state.owners.get(&inc.stream_id).is_some_and(|o| o.node == app.node_id);
    let hub = match hub {
        Some(h) if !is_mirror && (owned_here || h.is_live()) => h,
        _ if !owned_here && app.control.lock().unwrap().state.owners.contains_key(&inc.stream_id) => {
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
        down.send_all(&init);
        loop {
            tokio::select! {
                ev = sub.recv() => match ev {
                    Ok(ev) => {
                        let end = matches!(ev, HubEvent::End);
                        if !down.send(&map_event(&ev)) || end {
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

    /// 別のノードが持つ stream の写しをこのノードに用意する（既にあれば使った印を付けるだけ）。
    /// 写しの hub を返す（所有者が分からない・このノードが所有者なら None）。
    pub fn ensure_mirror(self: &Arc<Self>, app: &Arc<App>, stream_id: &str) -> Option<HubHandle> {
        if self.touch(stream_id) {
            return app.hubs.lock().unwrap().get(stream_id).cloned();
        }
        let owner = owner_raft_id(app, stream_id)?;
        let stream = app.control.lock().unwrap().state.streams.get(stream_id).cloned()?;
        let rt = app.rt.get()?.clone();
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
        let last = Arc::new(AtomicI64::new(crate::util::mono_ms() as i64));
        let (stop_tx, stop_rx) = oneshot::channel();
        mirrors.insert(stream_id.to_string(), Mirror { last_access: last, stop: Some(stop_tx) });
        drop(mirrors);
        app.log(&format!("relay: mirroring stream {stream_id} from node {owner}"));
        let me = self.clone();
        let app2 = app.clone();
        let h2 = hub.clone();
        let sid = stream_id.to_string();
        rt.spawn(async move {
            me.run_mirror(&app2, h2, &sid, owner, stop_rx).await;
            me.mirrors.lock().unwrap().remove(&sid);
        });
        Some(hub)
    }

    async fn run_mirror(&self, app: &Arc<App>, hub: HubHandle, sid: &str, owner: u64, mut stop: oneshot::Receiver<oneshot::Sender<()>>) {
        let mut sub = self.hub.subscribe(sid, TrackSel::All);
        let peer = format!("relay:node-{owner}");
        let Ok(mut p) = hub.attach("relay", &peer).await else {
            app.log(&format!("relay: mirror {sid}: hub busy"));
            return;
        };
        let mut pending: std::collections::VecDeque<StreamEvent> = sub.initial().into();
        let done: Option<oneshot::Sender<()>> = loop {
            let ev = match pending.pop_front() {
                Some(ev) => ev,
                None => tokio::select! {
                    ev = sub.recv() => match ev { Some(ev) => ev, None => break None },
                    r = &mut stop => break r.ok(),
                },
            };
            match ev {
                StreamEvent::Config(t, c) => {
                    if p.config(t, c).await.is_err() {
                        break None;
                    }
                }
                StreamEvent::Au(a) => {
                    if p.access_unit(a).await.is_err() {
                        break None;
                    }
                }
                StreamEvent::Discontinuity => {
                    // 新しい epoch（パッケージャーが DISCONTINUITY を入れる）
                    p.detach("upstream discontinuity").await;
                    match hub.attach("relay", &peer).await {
                        Ok(np) => p = np,
                        Err(_) => return,
                    }
                }
                StreamEvent::End => break None,
                StreamEvent::Segment { .. } | StreamEvent::Lost(_) => {}
            }
        };
        p.detach("mirror stopped").await;
        app.log(&format!("relay: mirror of {sid} stopped"));
        if let Some(d) = done {
            let _ = d.send(());
        }
    }

    /// 写しを止める（所有権がこのノードへ移ったとき）。止まるまで待つ。
    pub async fn stop_mirror(&self, stream: &str) {
        let stop = self.mirrors.lock().unwrap().get_mut(stream).and_then(|m| m.stop.take());
        if let Some(s) = stop {
            let (tx, rx) = oneshot::channel();
            if s.send(tx).is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
            }
        }
        self.mirrors.lock().unwrap().remove(stream);
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
            self.stop_mirror(&sid).await;
            let removed = app.hubs.lock().unwrap().remove(&sid);
            if let Some(h) = removed {
                h.shutdown().await;
            }
        }
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
