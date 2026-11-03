//! リアルタイムの部屋の統合（ADR-0015、senca-rooms）。
//!
//! - 部屋は制御状態の資源（`Command::PutRoom`）で、グリッドの仕事として 1 つのノードに置く（ADR-0013）。
//!   置かれたノードが自分の `RoomService` に部屋を作り、参加者のピア接続を WebRTC の UDP ソケットで受ける
//!   （WHIP・WHEP と同じソケット。STUN の USERNAME とアドレスで振り分ける）。
//! - シグナリングは `GET /signaling`（WebSocket、subprotocol `senca.rooms.v1`）。最初の `connect` の `room_id` を見て、
//!   部屋が別のノードにあれば `redirect` で案内する（どのノードにつないでもよい）。
//! - 部屋の置き場所が変わったら（ノードの停止・排出）、今の参加者に `redirect` を送り、新しいノードへ入り直させる。
//! - 参加トークンは制御状態の鍵から導いた Ed25519 の鍵で署名する（どのノードでも発行・検査できる）。
//! - 部屋の出来事は SSE（`GET /api/v1/events`）へ流す。

use crate::app::App;
use senca_rooms::host::PeerHost;
use senca_rooms::service::{Action, ConnKey, ConnMeta, RoomService, ServiceConfig};
use senca_rooms::{EventEnvelope, TokenSigner};
use senca_webrtc::{DtlsCertificate, PeerConfig, Transmit};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub const ISSUER: &str = "senca";
pub const AUDIENCE: &str = "senca-rooms";

/// 部屋の核で動かす閉包（API から）。
pub type RoomCall = Box<dyn FnOnce(&mut RoomService, Instant) + Send>;

/// 駆動役への頼みごと。
pub enum RoomCmd {
    Open { meta: ConnMeta, out: mpsc::UnboundedSender<WsOut>, reply: oneshot::Sender<Option<ConnKey>> },
    Text(ConnKey, String),
    Closed(ConnKey),
    /// API から部屋の核を触る（結果は閉包が自分で返す）。
    Call(RoomCall),
    /// 制御状態が変わった: 自分に置かれた部屋を突き合わせる。
    Reconcile,
    /// 参加者のトラックを部屋の外（メディアパイプライン）へ書き出す（ADR-0016）。`rid` はサイマルキャストの層。
    Bridge { room: String, track_id: String, rid: Option<String>, id: u64, tx: mpsc::Sender<BridgeItem> },
    Unbridge { room: String, track_id: String, id: u64 },
}

/// 書き出す AU（設定が変われば `config` が付く）。
pub struct BridgeItem {
    pub au: senca_core::AccessUnit,
    pub config: Option<senca_core::CodecConfig>,
}

/// 書き出しの購読者（部屋の核から見た「ノード」の名前。カスケードの相手と区別する）。
const BRIDGE_NODE: &str = "local-bridge";

type BridgeKey = (String, String);

struct BridgeSink {
    id: u64,
    tx: mpsc::Sender<BridgeItem>,
    waiting_key: bool,
}

/// WebSocket へ送るもの。
pub enum WsOut {
    Text(String),
    Close(u16, String),
}

/// 部屋の出来事（SSE へ）。
#[derive(Clone)]
pub struct RoomEventOut {
    pub tenant: String,
    pub envelope: Arc<EventEnvelope>,
}

/// 参加トークンの署名鍵（制御状態の鍵から導く。全ノードで同じ）。
pub fn signer(app: &App) -> Option<TokenSigner> {
    let key = app.control.lock().unwrap().state.playback_key.clone()?;
    let mut h = Sha256::new();
    h.update(b"senca-rooms-join-token/1\0");
    h.update(key.as_bytes());
    let seed: [u8; 32] = h.finalize().into();
    Some(TokenSigner::ed25519("senca-rooms-1", &seed))
}

/// ノードのシグナリングの URL（`http://h:p` → `ws://h:p/signaling`）。
pub fn signaling_url(api_base: &str) -> String {
    let b = api_base.trim_end_matches('/');
    let b = if let Some(r) = b.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = b.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        b.to_string()
    };
    format!("{b}/signaling")
}

/// 部屋の置き場所。
pub enum Location {
    Here,
    /// 別のノード（シグナリングの URL、API の URL）。
    Elsewhere { signaling: String, api: String },
    /// 置き場所を決めている途中・置けない。
    Pending,
    /// 無い部屋。
    Unknown,
}

pub fn locate(app: &App, room: &str) -> Location {
    let core = app.control.lock().unwrap();
    if !core.state.rooms.contains_key(room) {
        return Location::Unknown;
    }
    let wid = format!("{}{room}", senca_control::workloads::ROOM);
    match core.state.grid.assignments.get(&wid) {
        Some(a) if a.node == app.node_id => Location::Here,
        Some(a) => match core.state.nodes.get(&a.node).and_then(|n| n.media_addr.clone()) {
            Some(api) => Location::Elsewhere { signaling: signaling_url(&api), api },
            None => Location::Pending,
        },
        None => Location::Pending,
    }
}

/// 部屋の置き場所が決まるまで少し待つ（作ったばかりの部屋）。
pub async fn locate_wait(app: &App, room: &str, wait: Duration) -> Location {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let l = locate(app, room);
        if !matches!(l, Location::Pending) || tokio::time::Instant::now() >= deadline {
            return l;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 駆動役の中の部屋の係（WebRTC の駆動役のタスクが持つ）。
pub struct RoomsHost {
    app: Arc<App>,
    cert: DtlsCertificate,
    local: SocketAddr,
    svc: Option<(RoomService, PeerHost)>,
    outs: HashMap<ConnKey, mpsc::UnboundedSender<WsOut>>,
    /// 作った部屋の世代（制御状態の）。
    applied: HashMap<String, u64>,
    /// 部屋のテナント（消した後に出る出来事も、テナントの SSE へ届ける）。
    tenants: HashMap<String, String>,
    /// 自動に作った部屋が空になった時刻。
    empty_since: HashMap<String, Instant>,
    next_sweep: Instant,
    warned_relay: bool,
    /// (部屋, トラック) → 書き出し先
    /// (部屋, トラック) → 書き出し先（取りこぼした先は次のキーフレームまで映像を送らない）
    bridges: HashMap<BridgeKey, Vec<BridgeSink>>,
    /// 書き出す映像の層（HLS を区切れるよう、定期的にキーフレームを求める）。
    bridge_rids: HashMap<BridgeKey, Option<String>>,
    next_bridge_key: Instant,
}

/// 書き出す映像にキーフレームを求める間隔（HLS のセグメントの目標長。WHIP と同じ）。
const BRIDGE_KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

impl RoomsHost {
    pub fn new(app: Arc<App>, cert: DtlsCertificate, local: SocketAddr) -> RoomsHost {
        RoomsHost {
            app,
            cert,
            local,
            svc: None,
            outs: HashMap::new(),
            applied: HashMap::new(),
            tenants: HashMap::new(),
            empty_since: HashMap::new(),
            next_sweep: Instant::now(),
            warned_relay: false,
            bridges: HashMap::new(),
            bridge_rids: HashMap::new(),
            next_bridge_key: Instant::now(),
        }
    }

    /// 部屋の核（鍵が決まるまでは作れない: クラスタの初期化の前）。
    fn ensure(&mut self, now: Instant) -> Option<&mut (RoomService, PeerHost)> {
        if self.svc.is_none() {
            let signer = signer(&self.app)?;
            let cfg = ServiceConfig {
                node_id: self.app.node_id.clone(),
                token_verifier: Some(signer.verifier(ISSUER, AUDIENCE)),
                token_signer: Some(signer),
                token_issuer: ISSUER.into(),
                token_audience: AUDIENCE.into(),
                // 部屋は制御状態で作る（参加で勝手に作らない。テナントが決まらないため）
                auto_create_rooms: false,
                ..Default::default()
            };
            let svc = RoomService::new(cfg, now, crate::util::now_ms());
            let base = PeerConfig {
                certificate: Some(self.cert.clone()),
                unix_time_ns_at_start: Some(crate::util::now_ms() * 1_000_000),
                ..Default::default()
            };
            let host = PeerHost::new(base, self.local);
            self.svc = Some((svc, host));
        }
        self.svc.as_mut()
    }

    pub fn handle_datagram(&mut self, now: Instant, from: SocketAddr, data: &[u8]) -> bool {
        match self.svc.as_mut() {
            Some((_, host)) => host.handle_datagram(now, from, data),
            None => false,
        }
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        let (svc, host) = self.svc.as_ref()?;
        let bridge = (!self.bridge_rids.is_empty()).then_some(self.next_bridge_key);
        [svc.poll_timeout(), host.poll_timeout(), Some(self.next_sweep), bridge].into_iter().flatten().min()
    }

    pub fn handle_cmd(&mut self, now: Instant, c: RoomCmd) {
        match c {
            RoomCmd::Open { meta, out, reply } => {
                let k = self.ensure(now).map(|(svc, _)| svc.open(now, meta));
                if let Some(k) = k {
                    self.outs.insert(k, out);
                }
                let _ = reply.send(k);
            }
            RoomCmd::Text(k, t) => {
                if let Some((svc, _)) = self.svc.as_mut() {
                    svc.on_signaling_text(now, k, &t);
                }
            }
            RoomCmd::Closed(k) => {
                self.outs.remove(&k);
                if let Some((svc, _)) = self.svc.as_mut() {
                    svc.on_signaling_closed(now, k);
                }
            }
            RoomCmd::Call(f) => match self.ensure(now) {
                Some((svc, _)) => f(svc, now),
                // 鍵がまだ無い: 閉包を捨てる（呼び手は 503 になる）
                None => drop(f),
            },
            RoomCmd::Reconcile => self.reconcile(now),
            RoomCmd::Bridge { room, track_id, rid, id, tx } => {
                let Some((svc, _)) = self.svc.as_mut() else { return };
                let key = (room.clone(), track_id.clone());
                let first = !self.bridges.contains_key(&key);
                self.bridge_rids.insert(key.clone(), rid.clone());
                self.bridges.entry(key).or_default().push(BridgeSink { id, tx, waiting_key: true });
                if first {
                    // 最初の書き出しで購読する（キーフレームも求める）
                    svc.on_relay_subscribe(now, BRIDGE_NODE, &room, &track_id, Some(rid.into_iter().collect()));
                } else {
                    svc.on_relay_keyframe_request(now, &room, &track_id, None);
                }
            }
            RoomCmd::Unbridge { room, track_id, id } => {
                let key = (room.clone(), track_id.clone());
                let empty = match self.bridges.get_mut(&key) {
                    Some(v) => {
                        v.retain(|b| b.id != id);
                        v.is_empty()
                    }
                    None => false,
                };
                if empty {
                    self.bridges.remove(&key);
                    self.bridge_rids.remove(&key);
                    if let Some((svc, _)) = self.svc.as_mut() {
                        svc.on_relay_subscribe(now, BRIDGE_NODE, &room, &track_id, None);
                    }
                }
            }
        }
    }

    /// 自分に置かれた部屋を作り、置き場所が変わった部屋の参加者を案内し、消えた部屋を消す。
    fn reconcile(&mut self, now: Instant) {
        let (mine, moved): (Vec<senca_control::model::Room>, Vec<(String, Option<String>)>) = {
            let core = self.app.control.lock().unwrap();
            let st = &core.state;
            let wid = |id: &str| format!("{}{id}", senca_control::workloads::ROOM);
            let mine = st
                .rooms
                .values()
                .filter(|r| st.grid.assignments.get(&wid(&r.id)).is_some_and(|a| a.node == self.app.node_id))
                .cloned()
                .collect();
            // 手元にあるが、もう自分に置かれていない部屋（新しい置き場所の URL。消えた部屋は None）
            let moved = self
                .applied
                .keys()
                .filter(|id| st.grid.assignments.get(&wid(id)).is_none_or(|a| a.node != self.app.node_id))
                .map(|id| {
                    let to = st
                        .grid
                        .assignments
                        .get(&wid(id))
                        .and_then(|a| st.nodes.get(&a.node))
                        .and_then(|n| n.media_addr.clone())
                        .map(|a| signaling_url(&a));
                    (id.clone(), if st.rooms.contains_key(id) { to } else { None })
                })
                .collect();
            (mine, moved)
        };
        if self.ensure(now).is_none() {
            return;
        }
        let RoomsHost { app, svc, applied, empty_since, tenants, .. } = self;
        let Some((svc, _)) = svc.as_mut() else { return };
        for r in &mine {
            let settings = serde_json::from_value(r.settings.clone()).ok();
            match applied.get(&r.id) {
                None => {
                    let req = senca_rooms::api::CreateRoom { id: Some(r.id.clone()), settings, metadata: r.metadata.clone() };
                    match svc.api_create_room(now, req) {
                        Ok(_) => {
                            app.log(&format!("rooms: room {} hosted here", r.id));
                            applied.insert(r.id.clone(), r.generation);
                            tenants.insert(r.id.clone(), r.tenant.clone());
                        }
                        Err(e) => app.log(&format!("rooms: room {} could not be created: {}", r.id, e.title)),
                    }
                }
                Some(g) if *g != r.generation => {
                    let req = senca_rooms::api::UpdateRoom { settings, metadata: Some(r.metadata.clone()) };
                    if let Err(e) = svc.api_update_room(now, &r.id, None, req) {
                        app.log(&format!("rooms: room {} update failed: {}", r.id, e.title));
                    }
                    applied.insert(r.id.clone(), r.generation);
                }
                Some(_) => {}
            }
        }
        for (id, to) in moved {
            match to {
                Some(url) => {
                    let n = svc.redirect_room(now, &id, url.clone());
                    app.log(&format!("rooms: room {id} moved to {url} ({n} participants redirected)"));
                }
                None => app.log(&format!("rooms: room {id} deleted")),
            }
            let _ = svc.api_delete_room(now, &id);
            applied.remove(&id);
            empty_since.remove(&id);
        }
    }

    /// 自動に作った部屋が空のまま `empty_timeout_secs` 経ったら消す（制御状態から）。
    fn sweep(&mut self, now: Instant) {
        self.next_sweep = now + Duration::from_secs(5);
        let Some((svc, _)) = self.svc.as_ref() else { return };
        let autos: Vec<(String, String, u64)> = {
            let core = self.app.control.lock().unwrap();
            self.applied
                .keys()
                .filter_map(|id| core.state.rooms.get(id))
                .filter(|r| r.auto)
                .map(|r| (r.id.clone(), r.tenant.clone(), r.settings.get("empty_timeout_secs").and_then(|v| v.as_u64()).unwrap_or(60)))
                .collect()
        };
        for (id, tenant, secs) in autos {
            let empty = svc.api_get_room(&id).map(|v| v.participants == 0).unwrap_or(false);
            if !empty {
                self.empty_since.remove(&id);
                continue;
            }
            let since = *self.empty_since.entry(id.clone()).or_insert(now);
            if now.duration_since(since) >= Duration::from_secs(secs) {
                self.empty_since.remove(&id);
                let app = self.app.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = app.propose(senca_control::state::Command::DeleteRoom { tenant, id });
                });
            }
        }
    }

    /// 時間・出来事を進め、送る datagram を返す。
    pub fn drive(&mut self, now: Instant) -> Vec<Transmit> {
        if now >= self.next_sweep {
            self.sweep(now);
        }
        let Some((svc, host)) = self.svc.as_mut() else { return Vec::new() };
        // 書き出す映像: ブラウザーはキーフレームを求められたときしか送らないので、定期的に求める
        if now >= self.next_bridge_key {
            self.next_bridge_key = now + BRIDGE_KEYFRAME_INTERVAL;
            for ((room, track), rid) in &self.bridge_rids {
                svc.on_relay_keyframe_request(now, room, track, rid.as_deref());
            }
        }
        host.handle_timeout(now);
        if svc.poll_timeout().is_some_and(|t| t <= now) {
            svc.handle_timeout(now);
        }
        let mut acts = Vec::new();
        host.pump(now, svc, |a| acts.push(a));
        for a in acts {
            match a {
                Action::Signal { conn, text, .. } => {
                    if let Some(o) = self.outs.get(&conn) {
                        let _ = o.send(WsOut::Text(text));
                    }
                }
                Action::CloseSignaling { conn, code, reason } => {
                    if let Some(o) = self.outs.remove(&conn) {
                        let _ = o.send(WsOut::Close(code, reason));
                    }
                }
                Action::Event(e) => {
                    let tenant = self.tenants.get(&e.room_id).cloned().or_else(|| self.app.control.lock().unwrap().state.rooms.get(&e.room_id).map(|r| r.tenant.clone()));
                    if matches!(e.event, senca_rooms::RoomEvent::RoomDestroyed { .. }) && !self.applied.contains_key(&e.room_id) {
                        self.tenants.remove(&e.room_id);
                    }
                    if let Some(tenant) = tenant {
                        let _ = self.app.room_events.send(RoomEventOut { tenant, envelope: Arc::new(e) });
                    }
                }
                Action::AuthWebhook { conn, request_id, .. } => {
                    // 認証 Webhook はこのノードでは使わない設定（トークンで認める）。来たら拒む
                    svc.on_auth_webhook_result(now, conn, &request_id, None, b"");
                }
                Action::Relay(senca_rooms::RelayAction::Forward { node, room_id, track_id, au, config, .. }) if node == BRIDGE_NODE => {
                    let key = (room_id, track_id);
                    let mut lost = false;
                    if let Some(v) = self.bridges.get_mut(&key) {
                        v.retain(|b| !b.tx.is_closed());
                        let video = au.codec.kind() == senca_core::MediaKind::Video;
                        for b in v.iter_mut() {
                            if video && b.waiting_key {
                                if !au.keyframe {
                                    continue;
                                }
                                b.waiting_key = false;
                            }
                            if b.tx.try_send(BridgeItem { au: au.clone(), config: config.clone() }).is_err() {
                                lost = true;
                                b.waiting_key = video;
                            }
                        }
                    }
                    // 書き出しが追いつかない: 次のキーフレームからやり直せるよう求める
                    if lost {
                        svc.on_relay_keyframe_request(now, &key.0, &key.1, None);
                    }
                }
                Action::Relay(_) if !self.warned_relay => {
                    self.warned_relay = true;
                    self.app.log("rooms: cascading across nodes is not enabled; the room stays on one node");
                }
                _ => {}
            }
        }
        let mut tx = Vec::new();
        while let Some(t) = host.poll_transmit() {
            tx.push(t);
        }
        tx
    }
}

/// `GET /signaling`（WebSocket）。部屋が別のノードにあれば `redirect` を送って閉じる。
pub async fn serve_ws<S>(app: Arc<App>, mut sock: S, h: senca_http::RequestHead, peer: String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    use senca_ws::{AcceptOptions, Connection, Limits, Message, Role as WsRole};
    let Some(rtc) = app.rtc.get().cloned() else {
        let _ = sock.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 29\r\nConnection: close\r\n\r\nwebrtc (--webrtc-udp) is off\n").await;
        return;
    };
    let opts = AcceptOptions { subprotocols: vec![senca_rooms::signaling::SUBPROTOCOL.into()], require_subprotocol: true, ..Default::default() };
    let acc = match senca_ws::handshake::accept(&h, &opts) {
        Ok(a) => a,
        Err(e) => {
            let _ = sock.write_all(&e.response_bytes()).await;
            return;
        }
    };
    if sock.write_all(&acc.response_bytes()).await.is_err() {
        return;
    }
    let mut ws = Connection::new(WsRole::Server, Limits { max_message_size: 256 * 1024, max_frame_size: 256 * 1024, ..Default::default() });
    let meta = ConnMeta { remote_addr: Some(peer), user_agent: h.headers.get_str("user-agent").map(str::to_string), origin: acc.origin.clone() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut key: Option<ConnKey> = None;
    let (otx, mut orx) = mpsc::unbounded_channel::<WsOut>();
    let mut pending_meta = Some(meta);
    let connect_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        // 届いた枠を読み出す
        loop {
            match ws.poll() {
                Ok(Some(Message::Text(t))) => match key {
                    Some(k) => rtc.room_cmd(RoomCmd::Text(k, t)),
                    None => {
                        // 最初の message: 部屋の置き場所を見る
                        let room = serde_json::from_str::<Value>(&t).ok().and_then(|v| v.get("room_id").and_then(|r| r.as_str()).map(str::to_string));
                        if let Some(room) = &room {
                            match locate_wait(&app, room, Duration::from_secs(3)).await {
                                Location::Elsewhere { signaling, .. } => {
                                    let msg = serde_json::json!({ "type": "redirect", "location": signaling }).to_string();
                                    let _ = ws.send_text(&msg);
                                    let _ = ws.close(Some(4300), "redirect");
                                    flush_ws(&mut ws, &mut sock).await;
                                    return;
                                }
                                Location::Pending => {
                                    let msg = serde_json::json!({ "type": "error", "code": "capacity_unavailable", "message": "no node can host this room now", "fatal": true }).to_string();
                                    let _ = ws.send_text(&msg);
                                    let _ = ws.close(Some(4503), "room unavailable");
                                    flush_ws(&mut ws, &mut sock).await;
                                    return;
                                }
                                Location::Here | Location::Unknown => {}
                            }
                        }
                        let (reply, rx) = oneshot::channel();
                        rtc.room_cmd(RoomCmd::Open { meta: pending_meta.take().unwrap_or_default(), out: otx.clone(), reply });
                        match rx.await.ok().flatten() {
                            Some(k) => {
                                key = Some(k);
                                rtc.room_cmd(RoomCmd::Text(k, t));
                            }
                            None => {
                                let _ = ws.close(Some(1013), "rooms not ready");
                                flush_ws(&mut ws, &mut sock).await;
                                return;
                            }
                        }
                    }
                },
                Ok(Some(Message::Close(_))) | Err(_) => {
                    let _ = ws.close(None, "");
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
            }
        }
        if !flush_ws(&mut ws, &mut sock).await || ws.is_closed() {
            break;
        }
        if key.is_none() && tokio::time::Instant::now() >= connect_deadline {
            let _ = ws.close(Some(4408), "connect timeout");
            flush_ws(&mut ws, &mut sock).await;
            break;
        }
        tokio::select! {
            r = sock.read(&mut buf) => match r {
                Ok(0) | Err(_) => ws.feed_eof(),
                Ok(n) => ws.feed(&buf[..n]),
            },
            o = orx.recv() => match o {
                Some(WsOut::Text(t)) => { let _ = ws.send_text(&t); }
                Some(WsOut::Close(code, reason)) => { let _ = ws.close(Some(code), &reason); }
                None => {}
            },
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
    if let Some(k) = key {
        rtc.room_cmd(RoomCmd::Closed(k));
    }
}

/// 送るものを書き出す（失敗したら偽）。
async fn flush_ws<S: AsyncWrite + Unpin>(ws: &mut senca_ws::Connection, sock: &mut S) -> bool {
    while let Some(b) = ws.poll_transmit() {
        if sock.write_all(&b).await.is_err() {
            ws.feed_eof();
            return false;
        }
    }
    sock.flush().await.is_ok()
}
