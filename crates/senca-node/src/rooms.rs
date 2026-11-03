//! リアルタイムの部屋の統合（ADR-0015、senca-rooms）。
//!
//! - 部屋は制御状態の資源（`Command::PutRoom`）で、グリッドの仕事として 1 つのノードに置く（ADR-0013）。
//!   置かれたノードが自分の `RoomService` に部屋を作り、参加者のピア接続を WebRTC の UDP ソケットで受ける
//!   （WHIP・WHEP と同じソケット。STUN の USERNAME とアドレスで振り分ける）。
//! - シグナリングは `GET /signaling`（WebSocket、subprotocol `senca.rooms.v1`）。最初の `connect` の `room_id` を見て、
//!   部屋が別のノードにあれば `redirect` で案内する（どのノードにつないでもよい）。
//! - 部屋の置き場所が変わったら（ノードの停止・排出）、今の参加者に `redirect` を送り、新しいノードへ入り直させる。
//! - 参加トークンは制御状態の鍵から導いた Ed25519 の鍵で署名する（どのノードでも発行・検査できる）。
//! - 部屋の出来事は SSE（`GET /api/v1/events`）へ流し、`--rooms-event-webhook` があれば署名して POST する
//!   （[`senca_rooms::webhook::Outbox`]: 少なくとも 1 回、送り直し、諦めたものは数えて記録する）。
//! - 認証 Webhook（部屋の設定 `auth_webhook_url`、無ければ `--rooms-auth-webhook`）。どちらも無い部屋はトークンだけで決める。
//! - カスケード（[`crate::cascade`]）: 部屋の設定 `cascade`（または `--rooms-cascade`）なら、別のノードにつないだ参加者を
//!   このノードの edge の部屋で受け、home とノード間の中継でつなぐ。

use crate::app::App;
use crate::cascade::{self, Cascade, Role as CascadeRole};
use bytes::Bytes;
use senca_core::{AccessUnit, CodecConfig, TrackId};
use senca_relay::net::{DownEvent, IncomingSubscribe};
use senca_relay::{ErrorCode as RelayErrorCode, StreamEvent, TrackSel};
use senca_rooms::host::PeerHost;
use senca_rooms::service::{Action, AuthWebhookMode, ConnKey, ConnMeta, RoomService, ServiceConfig};
use senca_rooms::webhook::{Outbox, OutboxConfig, Outcome};
use senca_rooms::{EventEnvelope, RelayAction, RoomEvent, TokenSigner};
use senca_webrtc::{DtlsCertificate, PeerConfig, Transmit};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
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
    /// `rids` はサイマルキャストの層（低い順。空ならサイマルキャストでない）。届いている中でいちばん良い層を、
    /// キーフレームで切り替えながら 1 本にして渡す（ブラウザーは負荷・帯域で上の層を止めることがある）。
    Bridge { room: String, track_id: String, rids: Vec<String>, id: u64, tx: mpsc::Sender<BridgeItem> },
    Unbridge { room: String, track_id: String, id: u64 },
    /// 別のノードに置いた部屋の参加者がこのノードにつないだ: edge の部屋を用意する（カスケード）。
    EnsureEdge { room: String },
    /// 相手のノード（Raft の番号）から届いたカスケードの message。
    CascadeIn { peer: u64, payload: Bytes },
    /// 相手のノードがこのノードの参加者のトラックを購読した（`@room/<部屋>/<トラック>`）。
    RelayIncoming(IncomingSubscribe),
    /// 購読した別のノードのトラックの AU。
    RemoteAu { room: String, track: String, rid: Option<String>, au: AccessUnit, config: Option<CodecConfig> },
    /// 書き出しのために購読した別のノードの参加者のトラックの AU（カスケードの参加者の書き出し）。
    BridgeAu { room: String, track: String, rid: Option<String>, au: AccessUnit, config: Option<CodecConfig> },
    /// 相手が受けるトラックの層を変えた・キーフレームを求めた・購読をやめた。
    DownTracks { node: String, room: String, track: String, rids: Vec<String> },
    DownKeyframe { room: String, track: String, rid: Option<String> },
    DownClosed { node: String, room: String, track: String, id: u64 },
    /// 認証 Webhook・イベント Webhook の結果。
    AuthResult { conn: ConnKey, request_id: String, status: Option<u16>, body: Bytes },
    EventResult { event_id: String, outcome: Outcome },
    /// 状態（GET /nodes の補足）。
    Status(std::sync::mpsc::Sender<Value>),
}

/// 書き出しのための購読の相手の名前の後ろ（部屋の核の購読者の名前。カスケードの相手と区別する）。
const EXPORT_SUFFIX: &str = "#export";

/// 部屋の購読の優先度（中継のスケジューラーの重み。配信の写しの 128 より上）。
const ROOM_RELAY_PRIORITY: u8 = 192;
/// 参加者のいなくなった edge の部屋を畳むまで。
const EDGE_IDLE: Duration = Duration::from_secs(20);
/// 認証 Webhook の応答の上限。
const AUTH_MAX_BODY: usize = 64 * 1024;

type TrackKey = (String, String, String);

/// 上流（別のノードのトラックの購読）への頼みごと。
enum UpCtl {
    Tracks(TrackSel),
    Keyframe(Option<String>),
}

struct DownHandle {
    id: u64,
    tx: mpsc::UnboundedSender<StreamEvent>,
    rids: Vec<String>,
}

struct EdgeState {
    generation: u64,
    empty_since: Option<Instant>,
}

/// Webhook の設定（ノード）。
struct Hooks {
    auth_url: Option<String>,
    event_url: Option<String>,
    secret: Option<Vec<u8>>,
}

fn rid_of(rids: &[String], t: TrackId) -> Option<String> {
    rids.get((t.0 as usize).checked_sub(1)?).cloned()
}

/// 層の選び方 → rid（空 = トラック全体）。
fn sel_to_rids(sel: &TrackSel, rids: &[String]) -> Vec<String> {
    match sel {
        TrackSel::All => Vec::new(),
        TrackSel::Only(_) if rids.is_empty() => Vec::new(),
        TrackSel::Only(ts) => ts.iter().filter_map(|t| rid_of(rids, *t)).collect(),
    }
}

/// rid → 層の選び方（層は rids の順に TrackId 1, 2, …）。
fn rids_to_sel(want: &[String], rids: &[String]) -> TrackSel {
    if want.is_empty() || rids.is_empty() {
        return TrackSel::All;
    }
    TrackSel::only(want.iter().filter_map(|r| rids.iter().position(|x| x == r)).map(|i| TrackId(i as u32 + 1)))
}

/// ノードの ID → Raft の番号（中継の相手）。
fn raft_of(app: &App, node: &str) -> Option<u64> {
    app.control.lock().unwrap().state.nodes.get(node)?.labels.get("raft_id")?.parse().ok()
}

fn node_of_raft(app: &App, raft: u64) -> Option<String> {
    let r = raft.to_string();
    app.control.lock().unwrap().state.nodes.values().find(|n| n.labels.get("raft_id") == Some(&r)).map(|n| n.id.clone())
}

/// 部屋を置いたノード（グリッドの割り当て）。
fn home_of(app: &App, room: &str) -> Option<String> {
    let wid = format!("{}{room}", senca_control::workloads::ROOM);
    app.control.lock().unwrap().state.grid.assignments.get(&wid).map(|a| a.node.clone())
}

/// 部屋の参加者を、部屋を置いていないこのノードで受けてよいか（カスケード）。
pub fn cascade_here(app: &App, room: &str) -> bool {
    if app.relay.get().is_none() || app.shutting_down() {
        return false;
    }
    let core = app.control.lock().unwrap();
    if core.state.nodes.get(&app.node_id).is_some_and(|n| n.draining) {
        return false;
    }
    core.state
        .rooms
        .get(room)
        .is_some_and(|r| app.cfg.rooms_cascade || r.settings.get("cascade").and_then(|v| v.as_bool()).unwrap_or(false))
}

/// 書き出す AU（設定が変われば `config` が付く）。
pub struct BridgeItem {
    pub au: senca_core::AccessUnit,
    pub config: Option<senca_core::CodecConfig>,
}

/// 書き出しの購読者（部屋の核から見た「ノード」の名前。カスケードの相手と区別する）。
const BRIDGE_NODE: &str = "local-bridge";

type BridgeKey = (String, String);

/// 書き出す層: 届いている中でいちばん良い層（rids の後ろほど良い）を選ぶ。
#[derive(Default)]
struct BridgeLayers {
    rids: Vec<String>,
    current: Option<String>,
    seen: HashMap<String, Instant>,
    /// 最後に届いた設定（キーフレームを待っている書き出し先へ、キーフレームといっしょに渡す）。
    last_cfg: Option<CodecConfig>,
}

/// この時間届かない層は止まったとみなす。
const LAYER_ALIVE: Duration = Duration::from_millis(1500);

impl BridgeLayers {
    fn alive(&self, rid: &str, now: Instant) -> bool {
        self.seen.get(rid).is_some_and(|t| now.saturating_duration_since(*t) < LAYER_ALIVE)
    }
    fn best(&self, now: Instant) -> Option<&String> {
        self.rids.iter().rev().find(|r| self.alive(r, now))
    }
}

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
    /// ICE の候補に載せるアドレス（`crate::webrtc::advertised_addrs`）。
    advertise: Vec<SocketAddr>,
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
    /// 書き出すトラックの層の選び方（HLS を区切れるよう、定期的にキーフレームも求める）。
    bridge_layers: HashMap<BridgeKey, BridgeLayers>,
    next_bridge_key: Instant,
    /// 自分への頼みごと（Webhook・中継のタスクから結果を返す）。
    tx: mpsc::UnboundedSender<RoomCmd>,
    cascade: Cascade,
    /// edge として受けている部屋（home は別のノード）。
    edges: HashMap<String, EdgeState>,
    /// 出来事を外へ出さない部屋（edge を畳んだときの room.destroyed）。
    silenced: HashSet<String>,
    /// 購読した別のノードのトラック（(ノード, 部屋, トラック)）。
    ups: HashMap<TrackKey, mpsc::UnboundedSender<UpCtl>>,
    /// 別のノードに購読されたこのノードのトラック。
    downs: HashMap<TrackKey, DownHandle>,
    /// 書き出しのために購読した別のノードの参加者のトラック（(部屋, トラック)）。
    bridge_ups: HashMap<BridgeKey, mpsc::UnboundedSender<UpCtl>>,
    next_down: u64,
    /// 相手のノード（Raft の番号）ごとの App の送り口。
    app_out: HashMap<u64, mpsc::Sender<Bytes>>,
    hooks: Hooks,
    outbox: Option<Outbox>,
    dead_seen: u64,
    last_drive: Instant,
}

/// 書き出す映像にキーフレームを求める間隔（HLS のセグメントの目標長。WHIP と同じ）。
const BRIDGE_KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

impl RoomsHost {
    pub fn new(app: Arc<App>, cert: DtlsCertificate, local: SocketAddr, advertise: Vec<SocketAddr>, tx: mpsc::UnboundedSender<RoomCmd>) -> RoomsHost {
        // 秘密のファイルは起動の検査で読めることを確かめてある
        let secret = app.cfg.webhook_secret().unwrap_or_else(|e| {
            app.log(&format!("rooms: {e}"));
            None
        });
        let hooks = Hooks { auth_url: app.cfg.rooms_auth_webhook.clone(), event_url: app.cfg.rooms_event_webhook.clone(), secret };
        let outbox = match (&hooks.event_url, &hooks.secret) {
            (Some(_), Some(s)) => Some(Outbox::new(OutboxConfig { secrets: vec![s.clone()], ..Default::default() })),
            _ => None,
        };
        RoomsHost {
            tx,
            cascade: Cascade::new(&app.node_id),
            edges: HashMap::new(),
            silenced: HashSet::new(),
            ups: HashMap::new(),
            downs: HashMap::new(),
            bridge_ups: HashMap::new(),
            next_down: 1,
            app_out: HashMap::new(),
            hooks,
            outbox,
            dead_seen: 0,
            last_drive: Instant::now(),
            app,
            cert,
            local,
            advertise,
            svc: None,
            outs: HashMap::new(),
            applied: HashMap::new(),
            tenants: HashMap::new(),
            empty_since: HashMap::new(),
            next_sweep: Instant::now(),
            warned_relay: false,
            bridges: HashMap::new(),
            bridge_layers: HashMap::new(),
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
                // 認証 Webhook は部屋ごとに URL を持てるので、いつも問い合わせの形にし、URL の無い部屋は
                // on_auth_webhook_skipped でトークンだけで決める（Off と同じ扱い）
                auth_webhook: match self.app.cfg.rooms_auth_mode {
                    crate::config::RoomsAuthMode::AfterToken => AuthWebhookMode::AfterToken,
                    crate::config::RoomsAuthMode::Only => AuthWebhookMode::Only,
                },
                ..Default::default()
            };
            let svc = RoomService::new(cfg, now, crate::util::now_ms());
            let base = PeerConfig {
                certificate: Some(self.cert.clone()),
                unix_time_ns_at_start: Some(crate::util::now_ms() * 1_000_000),
                ..Default::default()
            };
            let mut host = PeerHost::new(base, self.local);
            host.set_addresses(self.advertise.clone());
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
        let bridge = (!self.bridge_layers.is_empty()).then_some(self.next_bridge_key);
        // 送り箱は、送れる数が埋まっていると期限の過ぎた物を返し続けるので、回りすぎないよう少し待つ
        let outbox = self.outbox.as_ref().and_then(|o| o.poll_timeout()).map(|t| t.max(self.last_drive + Duration::from_millis(100)));
        [svc.poll_timeout(), host.poll_timeout(), Some(self.next_sweep), bridge, self.cascade.poll_timeout(), outbox]
            .into_iter()
            .flatten()
            .min()
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
            RoomCmd::Bridge { room, track_id, rids, id, tx } => {
                let Some((svc, _)) = self.svc.as_mut() else { return };
                let key = (room.clone(), track_id.clone());
                let first = !self.bridges.contains_key(&key);
                // 別のノードの参加者のトラック（カスケード）: 参加者のいるノードから直接購読する
                let remote = svc
                    .remote_participants(&room)
                    .and_then(|ps| ps.into_iter().find(|p| p.tracks.iter().any(|t| t.track_id == track_id)))
                    .and_then(|p| p.node);
                self.bridge_layers.entry(key.clone()).or_insert_with(|| BridgeLayers { rids: rids.clone(), ..Default::default() });
                self.bridges.entry(key.clone()).or_default().push(BridgeSink { id, tx, waiting_key: true });
                if let Some(node) = remote {
                    match self.bridge_ups.get(&key) {
                        Some(u) if !u.is_closed() => {
                            let _ = u.send(UpCtl::Keyframe(None));
                        }
                        _ => self.subscribe_export(node, room, track_id, rids),
                    }
                    return;
                }
                let Some((svc, _)) = self.svc.as_mut() else { return };
                if first {
                    // 最初の書き出しで購読する（全部の層。キーフレームも求める）
                    svc.on_relay_subscribe(now, BRIDGE_NODE, &room, &track_id, Some(Vec::new()));
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
                    self.bridge_layers.remove(&key);
                    // 別のノードからの購読なら、送り口を落とすと購読が終わる
                    self.bridge_ups.remove(&key);
                    if let Some((svc, _)) = self.svc.as_mut() {
                        svc.on_relay_subscribe(now, BRIDGE_NODE, &room, &track_id, None);
                    }
                }
            }
            RoomCmd::EnsureEdge { room } => self.ensure_edge(now, &room),
            RoomCmd::CascadeIn { peer, payload } => {
                let Some(from) = node_of_raft(&self.app, peer) else { return };
                let Some((svc, _)) = self.svc.as_mut() else { return };
                if let Err(e) = self.cascade.on_payload(now, svc, &from, &payload) {
                    self.app.log(&format!("rooms: {e}"));
                }
            }
            RoomCmd::RelayIncoming(inc) => self.serve_down(now, inc),
            RoomCmd::RemoteAu { room, track, rid, au, config } => {
                if let Some((svc, _)) = self.svc.as_mut() {
                    svc.on_remote_access_unit(now, &room, &track, rid.as_deref(), &au, config.as_ref());
                }
            }
            RoomCmd::BridgeAu { room, track, rid, au, config } => {
                if let Some((svc, _)) = self.svc.as_mut() {
                    // 話者の検出にも入れる（合成の話者の大写し。この参加者の音声はここの SFU を通らないことがある）
                    svc.observe_track_audio(now, &room, &track, &au);
                }
                let key = (room, track);
                if bridge_deliver(&mut self.bridges, &mut self.bridge_layers, now, &key, rid, au, config)
                    && let Some(u) = self.bridge_ups.get(&key)
                {
                    let _ = u.send(UpCtl::Keyframe(None));
                }
            }
            RoomCmd::DownTracks { node, room, track, rids } => {
                if let Some((svc, _)) = self.svc.as_mut()
                    && self.downs.contains_key(&(node.clone(), room.clone(), track.clone()))
                {
                    svc.on_relay_subscribe(now, &node, &room, &track, Some(rids));
                }
            }
            RoomCmd::DownKeyframe { room, track, rid } => {
                if let Some((svc, _)) = self.svc.as_mut() {
                    svc.on_relay_keyframe_request(now, &room, &track, rid.as_deref());
                }
            }
            RoomCmd::DownClosed { node, room, track, id } => {
                let key = (node, room, track);
                if self.downs.get(&key).is_some_and(|d| d.id == id) {
                    self.downs.remove(&key);
                    if let Some((svc, _)) = self.svc.as_mut() {
                        svc.on_relay_subscribe(now, &key.0, &key.1, &key.2, None);
                    }
                }
            }
            RoomCmd::AuthResult { conn, request_id, status, body } => {
                if let Some((svc, _)) = self.svc.as_mut() {
                    svc.on_auth_webhook_result(now, conn, &request_id, status, &body);
                }
            }
            RoomCmd::EventResult { event_id, outcome } => {
                if let Some(o) = self.outbox.as_mut() {
                    o.on_result(now, &event_id, outcome);
                    if o.dead_total() > self.dead_seen {
                        self.dead_seen = o.dead_total();
                        let last = o.dead_letters().last().cloned().unwrap_or_default();
                        self.app.log(&format!("rooms: event webhook gave up on event {last} ({} dead letters so far)", self.dead_seen));
                    }
                }
            }
            RoomCmd::Status(reply) => {
                let mut v = self.cascade.status();
                if let Some(o) = &self.outbox {
                    v["event_webhook"] = serde_json::json!({
                        "delivered": o.delivered(),
                        "pending": o.pending(),
                        "dead_letters": o.dead_total(),
                        "dropped": o.dropped(),
                        "recent_dead_letters": o.dead_letters().collect::<Vec<_>>().into_iter().rev().take(20).collect::<Vec<_>>(),
                    });
                }
                v["relay_upstreams"] = self.ups.len().into();
                v["relay_downstreams"] = self.downs.len().into();
                let _ = reply.send(v);
            }
        }
    }

    /// 別のノードに置いた部屋の参加者を受けるため、このノードに edge の部屋を作る。
    fn ensure_edge(&mut self, now: Instant, room: &str) {
        if self.applied.contains_key(room) || self.edges.contains_key(room) {
            return;
        }
        let Some(r) = self.app.control.lock().unwrap().state.rooms.get(room).cloned() else { return };
        let home = home_of(&self.app, room);
        if home.as_deref() == Some(self.app.node_id.as_str()) {
            // 置き場所がこのノードへ移った（突き合わせで home として作る）
            self.reconcile(now);
            return;
        }
        let RoomsHost { app, svc, edges, tenants, cascade, .. } = self;
        let Some((svc, _)) = svc.as_mut() else { return };
        let settings = serde_json::from_value(r.settings.clone()).ok();
        let req = senca_rooms::api::CreateRoom { id: Some(r.id.clone()), settings, metadata: r.metadata.clone() };
        // edge の印を先に付ける（作ったときの room.created を外へ出さない）
        edges.insert(r.id.clone(), EdgeState { generation: r.generation, empty_since: Some(now) });
        match svc.api_create_room(now, req) {
            Ok(_) => {
                tenants.insert(r.id.clone(), r.tenant.clone());
                cascade.set_role(now, svc, &r.id, CascadeRole::Edge { home: home.clone() });
                app.log(&format!("rooms: room {} joined here as an edge (home node {})", r.id, home.as_deref().unwrap_or("?")));
            }
            Err(e) => {
                edges.remove(&r.id);
                app.log(&format!("rooms: edge for room {} could not be created: {}", r.id, e.title));
            }
        }
    }

    /// edge の部屋を畳む（参加者がいれば room_deleted で切れる）。
    fn drop_edge(&mut self, now: Instant, room: &str, why: &str) {
        if self.edges.remove(room).is_none() {
            return;
        }
        self.silenced.insert(room.to_string());
        if let Some((svc, _)) = self.svc.as_mut() {
            let _ = svc.api_delete_room(now, room);
        }
        self.forget_room(room);
        self.app.log(&format!("rooms: edge for room {room} closed ({why})"));
    }

    /// 部屋のカスケードの状態と、中継の購読を片付ける。
    fn forget_room(&mut self, room: &str) {
        for o in self.cascade.remove(room) {
            self.send_app(o);
        }
        // 送り口を落とすと、下流のタスクが購読を終える（上流は部屋の核の Unsubscribe で外れる）
        self.downs.retain(|k, _| k.1 != room);
        self.ups.retain(|k, _| k.1 != room);
        self.bridge_ups.retain(|k, _| k.0 != room);
    }

    /// カスケードの message を相手のノードへ（相手ごとの送り口。詰まったら捨てる: 在室は定期の同期で戻る）。
    fn send_app(&mut self, o: cascade::Out) {
        let Some(relay) = self.app.relay.get() else { return };
        let Some(raft) = raft_of(&self.app, &o.node) else { return };
        let q = self.app_out.entry(raft).or_insert_with(|| {
            let (tx, mut rx) = mpsc::channel::<Bytes>(1024);
            let ep = relay.endpoint.clone();
            tokio::spawn(async move {
                while let Some(b) = rx.recv().await {
                    let _ = ep.send_app(raft, cascade::TOPIC, b).await;
                }
            });
            tx
        });
        let _ = q.try_send(o.payload);
    }

    /// 相手のノードがこのノードの参加者のトラックを購読した。
    fn serve_down(&mut self, now: Instant, inc: IncomingSubscribe) {
        // 書き出しのための購読は、部屋の核どうしの購読と別の相手として数える（同じノードの組・同じトラックでも重ならない）
        let (parsed, export) = match cascade::parse_stream_id(&inc.stream_id) {
            Some(x) => (Some(x), false),
            None => (cascade::parse_export_stream_id(&inc.stream_id), true),
        };
        let Some((room, track)) = parsed.map(|(r, t)| (r.to_string(), t.to_string())) else {
            inc.reject(RelayErrorCode::BAD_REQUEST, "bad room track id");
            return;
        };
        let Some(node) = node_of_raft(&self.app, inc.peer) else {
            inc.reject(RelayErrorCode::NOT_FOUND, "unknown node");
            return;
        };
        let node = if export { format!("{node}{EXPORT_SUFFIX}") } else { node };
        let Some((svc, _)) = self.svc.as_mut() else {
            inc.reject(RelayErrorCode::NOT_FOUND, "rooms are not ready");
            return;
        };
        let rids = svc
            .local_participants(&room)
            .unwrap_or_default()
            .into_iter()
            .flat_map(|p| p.tracks)
            .find(|t| t.track_id == track)
            .map(|t| t.rids);
        let Some(rids) = rids else {
            inc.reject(RelayErrorCode::NOT_FOUND, "no such track on this node");
            return;
        };
        let want = sel_to_rids(&inc.tracks, &rids);
        let mut down = inc.accept(None);
        let id = self.next_down;
        self.next_down += 1;
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let key = (node.clone(), room.clone(), track.clone());
        self.downs.insert(key, DownHandle { id, tx, rids: rids.clone() });
        svc.on_relay_subscribe(now, &node, &room, &track, Some(want));
        let cmd = self.tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Some(ev) => {
                            down.send(&ev);
                            if down.is_closed() {
                                break;
                            }
                        }
                        None => break,
                    },
                    de = down.next_event() => match de {
                        Some(DownEvent::KeyframeRequest(t)) => {
                            let _ = cmd.send(RoomCmd::DownKeyframe { room: room.clone(), track: track.clone(), rid: rid_of(&rids, t) });
                        }
                        Some(DownEvent::TracksChanged(sel)) => {
                            let r = sel_to_rids(&sel, &rids);
                            let _ = cmd.send(RoomCmd::DownTracks { node: node.clone(), room: room.clone(), track: track.clone(), rids: r });
                        }
                        None => break,
                    },
                }
            }
            let _ = cmd.send(RoomCmd::DownClosed { node, room, track, id });
        });
    }

    /// 別のノードのトラックを購読する（層を変えるだけなら今の購読へ伝える）。切れたらつなぎ直す。
    fn subscribe_up(&mut self, node: String, room: String, track: String, want: Vec<String>) {
        let rids = self.cascade.track_rids(&room, &track).unwrap_or_default();
        let sel = rids_to_sel(&want, &rids);
        let key = (node.clone(), room.clone(), track.clone());
        if let Some(u) = self.ups.get(&key)
            && u.send(UpCtl::Tracks(sel.clone())).is_ok()
        {
            return;
        }
        let Some(relay) = self.app.relay.get().cloned() else {
            if !self.warned_relay {
                self.warned_relay = true;
                self.app.log("rooms: cascading needs the cluster relay; remote tracks are not forwarded");
            }
            return;
        };
        let Some(raft) = raft_of(&self.app, &node) else {
            self.app.log(&format!("rooms: node {node} is unknown; cannot subscribe to {track}"));
            return;
        };
        let (ctx, ctl) = mpsc::unbounded_channel();
        self.ups.insert(key, ctx);
        let sid = cascade::stream_id(&room, &track);
        self.spawn_up(relay, raft, sid, room, track, rids, sel, ctl, false);
    }

    /// 別のノードの参加者のトラックを、書き出しのために購読する（全部の層。層の選び方は書き出しの側で）。
    fn subscribe_export(&mut self, node: String, room: String, track: String, rids: Vec<String>) {
        let Some(relay) = self.app.relay.get().cloned() else {
            self.app.log("rooms: exporting a participant on another node needs the cluster relay");
            return;
        };
        let Some(raft) = raft_of(&self.app, &node) else {
            self.app.log(&format!("rooms: node {node} is unknown; cannot export {track}"));
            return;
        };
        let rids = self.cascade.track_rids(&room, &track).filter(|r| !r.is_empty()).unwrap_or(rids);
        let (ctx, ctl) = mpsc::unbounded_channel();
        self.bridge_ups.insert((room.clone(), track.clone()), ctx);
        let sid = cascade::export_stream_id(&room, &track);
        self.app.log(&format!("rooms: exporting track {track} of room {room} from node {node}"));
        self.spawn_up(relay, raft, sid, room, track, rids, TrackSel::All, ctl, true);
    }

    /// 上流の購読のタスク（切れたらつなぎ直す。送り口が落ちれば終わる）。`export` なら AU は書き出しへ。
    #[allow(clippy::too_many_arguments)]
    fn spawn_up(
        &self,
        relay: Arc<crate::relay::Relay>,
        raft: u64,
        sid: String,
        room: String,
        track: String,
        rids: Vec<String>,
        sel: TrackSel,
        mut ctl: mpsc::UnboundedReceiver<UpCtl>,
        export: bool,
    ) {
        let cmd = self.tx.clone();
        let app = self.app.clone();
        tokio::spawn(async move {
            let mut sel = sel;
            // 続けて失敗している間は最初の 1 回だけ記録する（相手のノードが落ちると、在室の期限まで試し続ける）
            let mut failing = false;
            loop {
                let sub = relay.endpoint.subscribe(raft, &sid, sel.clone(), ROOM_RELAY_PRIORITY);
                let r = tokio::select! {
                    r = sub => r,
                    c = ctl.recv() => match c {
                        None => return,
                        Some(UpCtl::Tracks(s)) => { sel = s; continue; }
                        Some(UpCtl::Keyframe(_)) => continue,
                    },
                };
                match r {
                    Ok(mut up) => {
                        failing = false;
                        let mut cfgs: HashMap<TrackId, CodecConfig> = HashMap::new();
                        loop {
                            tokio::select! {
                                ev = up.recv() => match ev {
                                    Some(StreamEvent::Config(t, c)) => { cfgs.insert(t, c); }
                                    Some(StreamEvent::Au(au)) => {
                                        let config = cfgs.remove(&au.track);
                                        let rid = rid_of(&rids, au.track);
                                        let (room, track) = (room.clone(), track.clone());
                                        let m = if export {
                                            RoomCmd::BridgeAu { room, track, rid, au, config }
                                        } else {
                                            RoomCmd::RemoteAu { room, track, rid, au, config }
                                        };
                                        if cmd.send(m).is_err() {
                                            return;
                                        }
                                    }
                                    Some(StreamEvent::Lost(r)) if r.is_graceful() => {}
                                    Some(StreamEvent::Lost(_)) | Some(StreamEvent::End) | None => break,
                                    Some(_) => {}
                                },
                                c = ctl.recv() => match c {
                                    None => return,
                                    Some(UpCtl::Tracks(s)) => { up.set_tracks(s.clone()); sel = s; }
                                    Some(UpCtl::Keyframe(rid)) => {
                                        let n = rids.len().max(1);
                                        for i in 0..n {
                                            if rid.is_none() || rids.get(i) == rid.as_ref() {
                                                up.request_keyframe(TrackId(i as u32 + 1));
                                            }
                                        }
                                    }
                                },
                            }
                        }
                    }
                    Err(e) => {
                        if !failing {
                            app.log(&format!("rooms: subscribe to {sid} on node {raft} failed: {e} (retrying)"));
                        }
                        failing = true;
                    }
                }
                // つなぎ直す（要らなくなれば送り口が落ちて終わる）
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    c = ctl.recv() => match c {
                        None => return,
                        Some(UpCtl::Tracks(s)) => sel = s,
                        Some(UpCtl::Keyframe(_)) => {}
                    },
                }
            }
        });
    }

    /// 部屋の核の中継の頼みごとを実行する（ブリッジ以外）。
    fn relay_action(&mut self, a: RelayAction) {
        match a {
            RelayAction::Subscribe { node, room_id, track_id, rids } => self.subscribe_up(node, room_id, track_id, rids),
            RelayAction::Unsubscribe { node, room_id, track_id } => {
                self.ups.remove(&(node, room_id, track_id));
            }
            RelayAction::KeyframeRequest { node, room_id, track_id, rid } => {
                if let Some(u) = self.ups.get(&(node, room_id, track_id)) {
                    let _ = u.send(UpCtl::Keyframe(rid));
                }
            }
            RelayAction::Forward { node, room_id, track_id, rid, au, config } => {
                let Some(d) = self.downs.get(&(node, room_id, track_id)) else { return };
                let idx = match &rid {
                    None => 0,
                    Some(r) => match d.rids.iter().position(|x| x == r) {
                        Some(i) => i,
                        None => return,
                    },
                };
                let t = TrackId(idx as u32 + 1);
                if let Some(c) = config {
                    let _ = d.tx.send(StreamEvent::Config(t, c));
                }
                let mut au = au;
                au.track = t;
                let _ = d.tx.send(StreamEvent::Au(au));
            }
            RelayAction::Message { node, room_id, label, from, to, data, binary } => {
                if let Some(o) = self.cascade.message(&node, &room_id, &label, &from, &to, &data, binary) {
                    self.send_app(o);
                }
            }
        }
    }

    /// 認証 Webhook へ問い合わせる（部屋の URL、無ければノードの URL。どちらも無ければトークンだけで決める）。
    fn auth_webhook(&mut self, now: Instant, conn: ConnKey, request_id: String, body: String) {
        let room = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("room_id").and_then(|r| r.as_str()).map(str::to_string)).unwrap_or_default();
        let room_url = self
            .app
            .control
            .lock()
            .unwrap()
            .state
            .rooms
            .get(&room)
            .and_then(|r| r.settings.get("auth_webhook_url").and_then(|u| u.as_str()).map(str::to_string));
        let Some((svc, _)) = self.svc.as_mut() else { return };
        let Some(url) = room_url.or_else(|| self.hooks.auth_url.clone()) else {
            svc.on_auth_webhook_skipped(now, conn, &request_id);
            return;
        };
        let Some(secret) = self.hooks.secret.clone() else {
            // 署名できない: 拒む（認証を飛ばさない）
            self.app.log(&format!("rooms: room {room} has an auth webhook but --rooms-webhook-secret-file is not set; join refused"));
            svc.on_auth_webhook_result(now, conn, &request_id, None, b"");
            return;
        };
        let app = self.app.clone();
        let cmd = self.tx.clone();
        let timeout = svc.config().auth_timeout;
        tokio::spawn(async move {
            let sig = senca_rooms::webhook::sign(&[&secret], crate::util::now_ms() / 1000, body.as_bytes());
            let headers = vec![
                ("Content-Type", "application/json".to_string()),
                (senca_rooms::webhook::SIGNATURE_HEADER, sig),
                (senca_rooms::webhook::ID_HEADER, request_id.clone()),
            ];
            let r = crate::httpc::request(&app.cfg.egress_allow, senca_http::Method::Post, &url, &headers, Some(Bytes::from(body)), AUTH_MAX_BODY, timeout).await;
            let (status, body) = match r {
                Ok(r) => (Some(r.status), r.body),
                Err(e) => {
                    app.log(&format!("rooms: auth webhook for room {room}: {e}"));
                    (None, Bytes::new())
                }
            };
            let _ = cmd.send(RoomCmd::AuthResult { conn, request_id, status, body });
        });
    }

    /// イベント Webhook の送り箱から、今送るものを送る。
    fn deliver_events(&mut self, now: Instant) {
        let (Some(o), Some(url)) = (self.outbox.as_mut(), self.hooks.event_url.clone()) else { return };
        while let Some(d) = o.poll_delivery(now, crate::util::now_ms() / 1000) {
            let app = self.app.clone();
            let cmd = self.tx.clone();
            let url = url.clone();
            tokio::spawn(async move {
                let headers: Vec<(&str, String)> = d.headers.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                let r = crate::httpc::request(&app.cfg.egress_allow, senca_http::Method::Post, &url, &headers, Some(Bytes::from(d.body.clone())), 64 * 1024, Duration::from_secs(10)).await;
                let outcome = match r {
                    Ok(r) => Outcome::Status(r.status),
                    Err(e) => {
                        if d.attempt == 1 {
                            app.log(&format!("rooms: event webhook: {e} (will retry)"));
                        }
                        Outcome::Failed
                    }
                };
                let _ = cmd.send(RoomCmd::EventResult { event_id: d.event_id, outcome });
            });
        }
    }

    /// 自分に置かれた部屋を作り、置き場所が変わった部屋の参加者を案内し（カスケードなら edge として残し）、
    /// 消えた部屋を消す。edge の部屋の home を見直す。
    fn reconcile(&mut self, now: Instant) {
        type Moved = (String, Option<String>, Option<String>, bool);
        type EdgeView = (String, Option<senca_control::model::Room>, Option<String>);
        let (mine, moved, edges): (Vec<senca_control::model::Room>, Vec<Moved>, Vec<EdgeView>) = {
            let core = self.app.control.lock().unwrap();
            let st = &core.state;
            let wid = |id: &str| format!("{}{id}", senca_control::workloads::ROOM);
            let home = |id: &str| st.grid.assignments.get(&wid(id)).map(|a| a.node.clone());
            let mine = st
                .rooms
                .values()
                .filter(|r| st.grid.assignments.get(&wid(&r.id)).is_some_and(|a| a.node == self.app.node_id))
                .cloned()
                .collect();
            let draining = self.app.shutting_down() || st.nodes.get(&self.app.node_id).is_some_and(|n| n.draining);
            let relay = self.app.relay.get().is_some();
            // 手元にあるが、もう自分に置かれていない部屋（新しい置き場所の URL・ノード。消えた部屋は None）。
            // カスケードの部屋で、このノードが退くのでなければ edge として残す
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
                    let room = st.rooms.get(id);
                    let cascade = room.is_some_and(|r| self.app.cfg.rooms_cascade || r.settings.get("cascade").and_then(|v| v.as_bool()).unwrap_or(false));
                    let demote = cascade && relay && !draining && to.is_some();
                    (id.clone(), if room.is_some() { to } else { None }, home(id), demote)
                })
                .collect();
            let edges = self.edges.keys().map(|id| (id.clone(), st.rooms.get(id).cloned(), home(id))).collect();
            (mine, moved, edges)
        };
        if self.ensure(now).is_none() {
            return;
        }
        let mut forget = Vec::new();
        let mut drop_edges = Vec::new();
        {
            let RoomsHost { app, svc, applied, empty_since, tenants, edges: edge_map, cascade, .. } = self;
            let Some((svc, _)) = svc.as_mut() else { return };
            for r in &mine {
                let settings = serde_json::from_value(r.settings.clone()).ok();
                // edge だった部屋の置き場所がこのノードへ移った: 今の参加者のまま home になる
                if let Some(e) = edge_map.remove(&r.id) {
                    applied.insert(r.id.clone(), e.generation);
                    tenants.insert(r.id.clone(), r.tenant.clone());
                    app.log(&format!("rooms: room {} hosted here (was an edge)", r.id));
                }
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
                if applied.contains_key(&r.id) && app.relay.get().is_some() {
                    cascade.set_role(now, svc, &r.id, CascadeRole::Home);
                }
            }
            for (id, to, home, demote) in moved {
                if demote {
                    let g = applied.remove(&id).unwrap_or(0);
                    edge_map.insert(id.clone(), EdgeState { generation: g, empty_since: None });
                    cascade.set_role(now, svc, &id, CascadeRole::Edge { home: home.clone() });
                    app.log(&format!("rooms: room {id} moved to node {}; staying here as an edge", home.as_deref().unwrap_or("?")));
                    continue;
                }
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
                forget.push(id);
            }
            for (id, room, home) in edges {
                let Some(e) = edge_map.get_mut(&id) else { continue };
                match room {
                    None => drop_edges.push((id, "room deleted")),
                    Some(r) => {
                        if r.generation != e.generation {
                            let settings = serde_json::from_value(r.settings.clone()).ok();
                            let req = senca_rooms::api::UpdateRoom { settings, metadata: Some(r.metadata.clone()) };
                            if let Err(err) = svc.api_update_room(now, &id, None, req) {
                                app.log(&format!("rooms: edge room {id} update failed: {}", err.title));
                            }
                            e.generation = r.generation;
                        }
                        if home.as_deref() != Some(app.node_id.as_str()) {
                            cascade.set_role(now, svc, &id, CascadeRole::Edge { home });
                        }
                    }
                }
            }
        }
        for id in forget {
            self.forget_room(&id);
        }
        for (id, why) in drop_edges {
            self.drop_edge(now, &id, why);
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
                    // メディアパイプラインの子（合成・送り出し）も片付ける（書き出した stream・録画は残す）
                    let mps: Vec<senca_control::model::MediaPipeline> =
                        app.control.lock().unwrap().state.media_pipelines.values().filter(|m| m.room == id).cloned().collect();
                    for m in &mps {
                        crate::mediapipes::remove_children(&app, m);
                    }
                    let _ = app.propose(senca_control::state::Command::DeleteRoom { tenant, id });
                });
            }
        }
        // このノードの参加者がいなくなった edge の部屋を畳む
        let mut idle = Vec::new();
        for (id, e) in self.edges.iter_mut() {
            if svc.local_participants(id).is_some_and(|v| !v.is_empty()) {
                e.empty_since = None;
            } else if now.duration_since(*e.empty_since.get_or_insert(now)) >= EDGE_IDLE {
                idle.push(id.clone());
            }
        }
        for id in idle {
            self.drop_edge(now, &id, "no participants here");
        }
    }

    /// 時間・出来事を進め、送る datagram を返す。
    pub fn drive(&mut self, now: Instant) -> Vec<Transmit> {
        if now >= self.next_sweep {
            self.sweep(now);
        }
        self.last_drive = now;
        // カスケード: 期限切れの相手を外し、同期を送る
        let outs = match self.svc.as_mut() {
            Some((svc, _)) => self.cascade.poll(now, svc),
            None => Vec::new(),
        };
        for o in outs {
            self.send_app(o);
        }
        self.deliver_events(now);
        let Some((svc, host)) = self.svc.as_mut() else { return Vec::new() };
        // 書き出す映像: ブラウザーはキーフレームを求められたときしか送らないので、定期的に求める
        if now >= self.next_bridge_key {
            self.next_bridge_key = now + BRIDGE_KEYFRAME_INTERVAL;
            for ((room, track), l) in &self.bridge_layers {
                let up = self.bridge_ups.get(&(room.clone(), track.clone()));
                let mut ask = |rid: Option<&String>| match up {
                    Some(u) => {
                        let _ = u.send(UpCtl::Keyframe(rid.cloned()));
                    }
                    None => svc.on_relay_keyframe_request(now, room, track, rid.map(String::as_str)),
                };
                if l.rids.is_empty() {
                    ask(None);
                    continue;
                }
                // 今の層と、より良い層が届いていればその層にも（キーフレームで上へ切り替える）
                let mut want: Vec<&String> = l.current.iter().collect();
                if let Some(b) = l.best(now)
                    && l.current.as_ref() != Some(b)
                {
                    want.push(b);
                }
                if want.is_empty() {
                    ask(None);
                }
                for r in want {
                    ask(Some(r));
                }
            }
        }
        host.handle_timeout(now);
        if svc.poll_timeout().is_some_and(|t| t <= now) {
            svc.handle_timeout(now);
        }
        let mut acts = Vec::new();
        host.pump(now, svc, |a| acts.push(a));
        // 部屋の核を借りたままでは扱えないもの（中継・Webhook）は後で
        let mut later: Vec<Action> = Vec::new();
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
                    let lifecycle = matches!(e.event, RoomEvent::RoomCreated { .. } | RoomEvent::RoomUpdated { .. } | RoomEvent::RoomDestroyed { .. });
                    let destroyed = matches!(e.event, RoomEvent::RoomDestroyed { .. });
                    // edge の部屋の作成・変更・削除は home の出来事と重なるので外へ出さない
                    let quiet = lifecycle && (self.edges.contains_key(&e.room_id) || self.silenced.contains(&e.room_id));
                    if destroyed {
                        self.silenced.remove(&e.room_id);
                    }
                    if matches!(
                        e.event,
                        RoomEvent::ConnectionCreated { .. }
                            | RoomEvent::ConnectionUpdated { .. }
                            | RoomEvent::ConnectionDestroyed { .. }
                            | RoomEvent::TrackPublished { .. }
                            | RoomEvent::TrackUnpublished { .. }
                    ) {
                        self.cascade.mark_dirty(now, &e.room_id);
                    }
                    if let RoomEvent::TrackUnpublished { track_id, .. } = &e.event {
                        // このノードのトラックを購読していた相手へは、もう送らない
                        self.downs.retain(|k, _| !(k.1 == e.room_id && &k.2 == track_id));
                    }
                    let tenant = self.tenants.get(&e.room_id).cloned().or_else(|| self.app.control.lock().unwrap().state.rooms.get(&e.room_id).map(|r| r.tenant.clone()));
                    if destroyed && !self.applied.contains_key(&e.room_id) && !self.edges.contains_key(&e.room_id) {
                        self.tenants.remove(&e.room_id);
                    }
                    if quiet {
                        continue;
                    }
                    if let Some(o) = self.outbox.as_mut() {
                        o.push(now, &e);
                    }
                    if let Some(tenant) = tenant {
                        let _ = self.app.room_events.send(RoomEventOut { tenant, envelope: Arc::new(e) });
                    }
                }
                a @ Action::AuthWebhook { .. } => later.push(a),
                Action::Relay(senca_rooms::RelayAction::Forward { node, room_id, track_id, rid, au, config }) if node == BRIDGE_NODE => {
                    let key = (room_id, track_id);
                    // 書き出しが追いつかない: 次のキーフレームからやり直せるよう求める
                    if bridge_deliver(&mut self.bridges, &mut self.bridge_layers, now, &key, rid, au, config) {
                        svc.on_relay_keyframe_request(now, &key.0, &key.1, None);
                    }
                }
                a @ Action::Relay(_) => later.push(a),
                _ => {}
            }
        }
        let mut tx = Vec::new();
        while let Some(t) = host.poll_transmit() {
            tx.push(t);
        }
        for a in later {
            match a {
                Action::AuthWebhook { conn, request_id, body } => self.auth_webhook(now, conn, request_id, body),
                Action::Relay(r) => self.relay_action(r),
                _ => {}
            }
        }
        if self.outbox.as_ref().is_some_and(|o| o.poll_timeout().is_some_and(|t| t <= now)) {
            self.deliver_events(now);
        }
        tx
    }
}

/// 書き出す AU を書き出し先へ渡す。サイマルキャスト: 選んだ層だけを流す。より良い層（または今の層が止まったら
/// 届いている層）のキーフレームで切り替え、切り替えたら寸法の変わった設定を渡す。取りこぼした先は次のキーフレーム
/// まで映像を送らない。取りこぼしがあれば真（キーフレームを求める）。
fn bridge_deliver(
    bridges: &mut HashMap<BridgeKey, Vec<BridgeSink>>,
    layers: &mut HashMap<BridgeKey, BridgeLayers>,
    now: Instant,
    key: &BridgeKey,
    rid: Option<String>,
    au: AccessUnit,
    config: Option<CodecConfig>,
) -> bool {
    let mut config = config;
    if let Some(l) = layers.get_mut(key)
        && !l.rids.is_empty()
        && au.codec.kind() == senca_core::MediaKind::Video
    {
        let Some(r) = rid else { return false };
        l.seen.insert(r.clone(), now);
        if l.current.as_ref() != Some(&r) {
            let cur_alive = l.current.as_ref().is_some_and(|c| l.alive(c, now));
            let better = l.best(now) == Some(&r);
            if !(au.keyframe && (better || !cur_alive)) {
                return false;
            }
            l.current = Some(r.clone());
            if config.is_none()
                && let Some((w, h)) = crate::media::vpx_keyframe_dims(au.codec, &au.payload)
            {
                config = Some(senca_core::CodecConfig {
                    codec: au.codec,
                    generation: 0,
                    record: bytes::Bytes::new(),
                    video: Some(senca_core::media::VideoParams { width: w, height: h, ..Default::default() }),
                    audio: None,
                });
            }
        }
    }
    let video = au.codec.kind() == senca_core::MediaKind::Video;
    // 設定はキーフレームより前の AU に付いて届くことがある: 覚えておき、待っていた先にはキーフレームといっしょに渡す
    let last = layers.get_mut(key).map(|l| {
        if video && let Some(c) = &config {
            l.last_cfg = Some(c.clone());
        }
        l.last_cfg.clone()
    });
    let mut lost = false;
    if let Some(v) = bridges.get_mut(key) {
        v.retain(|b| !b.tx.is_closed());
        for b in v.iter_mut() {
            let mut cfg = config.clone();
            if video && b.waiting_key {
                if !au.keyframe {
                    continue;
                }
                b.waiting_key = false;
                cfg = cfg.or_else(|| last.clone().flatten());
            }
            if b.tx.try_send(BridgeItem { au: au.clone(), config: cfg }).is_err() {
                lost = true;
                b.waiting_key = video;
            }
        }
    }
    lost
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
                                // カスケード: このノードの edge の部屋で受ける（同じ頼みごとの列なので Open より先に作られる）
                                Location::Elsewhere { .. } if cascade_here(&app, room) => {
                                    rtc.room_cmd(RoomCmd::EnsureEdge { room: room.clone() });
                                }
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
