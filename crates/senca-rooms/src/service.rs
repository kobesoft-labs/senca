//! 部屋の状態機械（sans-io）。
//!
//! ノードは入力を入れ、[`RoomService::poll_action`] で [`Action`] を取り出して実行する。
//!
//! | 入力 | 呼ぶもの |
//! |---|---|
//! | WebSocket が開いた | [`open`](RoomService::open) → [`ConnKey`] |
//! | text frame | [`on_signaling_text`](RoomService::on_signaling_text) |
//! | WebSocket が閉じた | [`on_signaling_closed`](RoomService::on_signaling_closed) |
//! | `AcceptOffer` の結果 | [`on_offer_result`](RoomService::on_offer_result) |
//! | ピア接続の出来事 | [`on_peer_event`](RoomService::on_peer_event) |
//! | ピア接続の統計 | [`on_peer_stats`](RoomService::on_peer_stats) |
//! | 認証 Webhook の応答 | [`on_auth_webhook_result`](RoomService::on_auth_webhook_result) |
//! | 中継（別ノード） | `add_remote_*`・`on_remote_access_unit`・`on_relay_*` |
//! | 時刻 | [`poll_timeout`](RoomService::poll_timeout)・[`handle_timeout`](RoomService::handle_timeout) |
//!
//! 受信の枠（slot）: クライアントの受ける m 行 1 つが枠 1 つ。サーバーは部屋のトラックを枠に割り当て
//! （SFU の送信トラックの送り元を替える）、`slots` で知らせる。枠が足りなければ `renegotiate` で求める。
//! 枠の割り当ては安定（今の割り当てを保ち、空いた枠から使う）。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::Value;
use senca_core::media::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
use senca_webrtc::sfu::{PeerId, Room as SfuRoom, RoomConfig as SfuConfig, RoomOutput};
use senca_webrtc::{Event as PcEvent, SendTrackInfo};

use crate::events::{EventEnvelope, RoomEvent};
use crate::filter::{ForwardingFilter, Subject};
use crate::ids::random_id;
use crate::model::{Kind, Role, RoomSettings, Unfocused};
use crate::signaling::{
    self, Answer, AppMessage, ClientMessage, Connect, Connected, DataChannelSpec, Delivered, DisconnectNotice,
    DisconnectReason, Empty, ErrorCode, ErrorMsg, Notify, ParticipantInfo, PublishedTrack, Push, Redirect,
    Renegotiate, ServerMessage, SimulcastRequest, Slot, SlotCounts, SlotState, Slots, SpotlightRequest,
    TrackInfo, SIGNALING_LABEL,
};
use crate::spotlight::{DetectorConfig, SpeakerDetector, level_from_opus_size};
use crate::token::{JoinClaims, Permissions, TokenSigner, TokenVerifier};
use crate::webhook::{AuthRequest, AuthResponse, auth_decision};

/// シグナリングの接続の番号（ノードの中だけで使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnKey(pub u64);

/// 切断の前に送る誤り。
type Fatal = (ErrorCode, String);

/// 購読者 1 人の望む割り当て（音声, 映像, 注目の組）。
type SlotPlan = (Vec<String>, Vec<String>, Vec<String>);

/// 別のノードのトラックに SFU の中で付ける番号の始まり（ローカルの [`ConnKey`] と重ならない）。
const REMOTE_PEER_BASE: PeerId = 1 << 62;

/// シグナリングの経路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalPath {
    WebSocket,
    /// DataChannel `signaling`。
    DataChannel,
}

/// ピア接続を作るときの条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSpec {
    pub video_codec: Codec,
    pub audio_codec: Codec,
    pub datachannel: bool,
    /// E2EE（SFrame）の部屋。SFU は中身を見ない。
    pub e2ee: bool,
}

/// 別のノードとの中継（ADR-0014。ノード間の 1 本の接続の上の論理チャネル）。
#[derive(Debug, Clone)]
pub enum RelayAction {
    /// このノードの購読者が要るので、`node` のトラックの層を送ってほしい（`rids` が空ならトラック全体）。
    Subscribe { node: String, room_id: String, track_id: String, rids: Vec<String> },
    /// もう要らない。
    Unsubscribe { node: String, room_id: String, track_id: String },
    /// `node` のトラックの層にキーフレームを求める。
    KeyframeRequest { node: String, room_id: String, track_id: String, rid: Option<String> },
    /// このノードのトラックの AU を `node` へ送る。
    Forward {
        node: String,
        room_id: String,
        track_id: String,
        rid: Option<String>,
        au: AccessUnit,
        config: Option<CodecConfig>,
    },
    /// アプリの message を `node` の参加者へ（`to` が空なら部屋の全員）。
    Message {
        node: String,
        room_id: String,
        label: String,
        from: String,
        to: Vec<String>,
        data: Bytes,
        binary: bool,
    },
}

/// ノードが実行すること。
#[derive(Debug, Clone)]
pub enum Action {
    /// シグナリングの message（JSON の text）を送る。`path` が DataChannel なら `signaling` のラベルで。
    Signal { conn: ConnKey, path: SignalPath, text: String },
    /// WebSocket を閉じる（close frame の code・reason）。
    CloseSignaling { conn: ConnKey, code: u16, reason: String },
    /// ピア接続を作る（部屋のコーデックだけを申し出る）。
    CreatePeer { conn: ConnKey, spec: PeerSpec },
    /// クライアントの offer に answer を作り、結果を [`RoomService::on_offer_result`] へ返す。
    AcceptOffer { conn: ConnKey, sdp: String },
    /// trickle の候補（空文字列は end-of-candidates）。
    AddCandidate { conn: ConnKey, candidate: String },
    ClosePeer { conn: ConnKey },
    /// SFU の転送（送信トラックへ AU）。
    Forward { conn: ConnKey, track: TrackId, au: AccessUnit, config: Option<CodecConfig> },
    /// 送り手の受信トラックにキーフレームを求める。
    RequestKeyframe { conn: ConnKey, track: TrackId },
    /// 帯域の探り（上の層へ上げるのに要る速さ）。
    SetDesiredBitrate { conn: ConnKey, bps: u64 },
    /// DataChannel（ラベル）で送る。
    SendData { conn: ConnKey, label: String, data: Bytes, binary: bool },
    /// 許していない DataChannel を閉じる。
    CloseDataChannel { conn: ConnKey, id: u16 },
    /// 認証 Webhook へ POST し、結果を [`RoomService::on_auth_webhook_result`] へ返す（本文は署名して送る）。
    AuthWebhook { conn: ConnKey, request_id: String, body: String },
    /// SSE と イベント Webhook（[`crate::webhook::Outbox`]）へ。
    Event(EventEnvelope),
    Relay(RelayAction),
}

/// 認証 Webhook の使い方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthWebhookMode {
    /// 使わない（トークンだけ）。
    #[default]
    Off,
    /// トークンの代わりに使う（トークンがあれば検証してクレームも渡す）。
    Only,
    /// トークンを検証した後にも問い合わせる。
    AfterToken,
}

/// 設定。
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub node_id: String,
    pub token_verifier: Option<TokenVerifier>,
    pub auth_webhook: AuthWebhookMode,
    pub auth_timeout: Duration,
    /// トークンも Webhook も無い参加を許す（開発用）。
    pub allow_anonymous: bool,
    /// 知らない部屋への参加で部屋を作る。
    pub auto_create_rooms: bool,
    pub default_settings: RoomSettings,
    pub max_rooms: usize,
    /// このノードの参加者の上限。超えたら `overflow_redirect` へ、無ければ `room_full`。
    pub max_connections: usize,
    pub overflow_redirect: Option<String>,
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
    /// WebSocket が開いてから `connect` までの期限。
    pub connect_timeout: Duration,
    /// 参加してからピア接続がつながるまでの期限。
    pub peer_timeout: Duration,
    /// 1 秒あたりの message の上限（接続ごと）。
    pub max_messages_per_sec: u32,
    /// 種類ごとの受信の枠の上限。
    pub max_slots_per_kind: u32,
    pub sfu: SfuConfig,
    pub detector: DetectorConfig,
    /// 話者の検出を見直す間隔。
    pub spotlight_tick: Duration,
    /// REST で参加トークンを発行する鍵と iss・aud。
    pub token_signer: Option<TokenSigner>,
    pub token_issuer: String,
    pub token_audience: String,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig {
            node_id: "node".into(),
            token_verifier: None,
            auth_webhook: AuthWebhookMode::Off,
            auth_timeout: Duration::from_secs(5),
            allow_anonymous: false,
            auto_create_rooms: true,
            default_settings: RoomSettings::default(),
            max_rooms: 10_000,
            max_connections: 10_000,
            overflow_redirect: None,
            ping_interval: Duration::from_secs(10),
            pong_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            peer_timeout: Duration::from_secs(30),
            max_messages_per_sec: 50,
            max_slots_per_kind: 32,
            sfu: SfuConfig::default(),
            detector: DetectorConfig::default(),
            spotlight_tick: Duration::from_millis(200),
            token_signer: None,
            token_issuer: "senca".into(),
            token_audience: "senca-rooms".into(),
        }
    }
}

/// 接続の付帯情報（ログ・認証 Webhook へ）。
#[derive(Debug, Clone, Default)]
pub struct ConnMeta {
    pub remote_addr: Option<String>,
    pub user_agent: Option<String>,
    pub origin: Option<String>,
}

/// ピア接続の統計（[`PeerHost`](crate::host::PeerHost) が定期的に入れる）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerStats {
    pub rtt_ms: u64,
    pub bandwidth_bps: u64,
    pub send_bitrate_bps: u64,
    pub retransmitted: u64,
    pub nack_requested: u64,
    pub dropped_srtp: u64,
}

// ------------------------------------------------------------------ 内部の状態

#[derive(Debug)]
pub(crate) enum Phase {
    AwaitConnect,
    Authenticating { request_id: String, deadline: Instant, pending: Box<(Connect, Option<JoinClaims>)> },
    Joined,
}

#[derive(Debug, Clone)]
pub(crate) struct RecvSlot {
    pub mid: String,
    pub kind: Kind,
    pub send_track: Option<TrackId>,
    pub track: Option<String>,
    /// SFU に結んでいるトラック。
    pub bound: Option<String>,
    pub paused: bool,
    /// クライアントの `layer` で選んだ層。
    pub preferred: Option<String>,
    /// 最後に SFU に渡した層の指定。
    pub applied: Option<Option<String>>,
}

#[derive(Debug, Clone)]
struct OfferPlan {
    publish: Vec<(String, Kind, Vec<String>)>,
    recv: Vec<(String, Kind)>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ConnStats {
    pub peer: PeerStats,
    pub forwarded_bytes: u64,
    pub forwarded_aus: u64,
    pub received_bytes: u64,
    pub client_reports: Option<Value>,
    pub net_level: Option<u8>,
    pub net_notified: Option<Instant>,
}

#[derive(Debug)]
pub(crate) struct Conn {
    pub key: ConnKey,
    pub meta: ConnMeta,
    pub phase: Phase,
    pub opened_at: Instant,
    pub connection_id: String,
    pub room_id: String,
    pub role: Role,
    pub client_id: Option<String>,
    pub bundle_id: String,
    pub metadata: Value,
    pub multistream: bool,
    pub spotlight: Option<SpotlightRequest>,
    pub simulcast: Option<SimulcastRequest>,
    pub perms: Permissions,
    pub data_channels: Vec<DataChannelSpec>,
    pub dc_ids: BTreeMap<u16, String>,
    pub path: SignalPath,
    pub ws_open: bool,
    pub want_dc_signaling: bool,
    pub close_ws_ok: bool,
    pub filter: Option<ForwardingFilter>,
    pub peer_created: bool,
    pub peer_connected: bool,
    pub joined_at: Instant,
    pub joined_at_ms: i64,
    negotiating: Option<OfferPlan>,
    pub answered: bool,
    pub renegotiate_sent: Option<SlotCounts>,
    pub slots: Vec<RecvSlot>,
    /// mid → track_id
    pub published: BTreeMap<String, String>,
    pub next_ping: Instant,
    pub pong_deadline: Option<Instant>,
    pub rate: (Instant, u32),
    pub stats: ConnStats,
}

#[derive(Debug, Clone)]
pub(crate) enum Origin {
    Local { conn: ConnKey, mid: String },
    Remote { node: String },
}

#[derive(Debug)]
pub(crate) struct Track {
    pub info: TrackInfo,
    /// 持ち主の client_id。
    pub client_id: Option<String>,
    pub seq: u64,
    pub origin: Origin,
    pub codec: Option<Codec>,
    pub sfu_peer: PeerId,
    pub group: String,
    /// 層（rid, 送り手の受信トラック）。
    pub layers: Vec<(Option<String>, TrackId)>,
    /// このトラックを要る別のノード → 層（空はトラック全体）。
    pub relay_out: BTreeMap<String, Vec<String>>,
    /// 別のノードのトラック: 今求めている層（`None` は求めていない）。
    pub relay_in: Option<Vec<String>>,
    /// 層ごとの今の設定（後から購読した相手に、最初の AU と一緒に渡す）。
    pub configs: HashMap<TrackId, CodecConfig>,
    /// まだ設定を渡していない購読の相手。
    pub relay_cfg_pending: BTreeSet<String>,
}

#[derive(Debug)]
pub(crate) struct RoomState {
    pub id: String,
    pub settings: RoomSettings,
    pub metadata: Value,
    pub generation: u64,
    pub created_at_ms: i64,
    pub auto_created: bool,
    pub members: Vec<ConnKey>,
    pub remote: BTreeMap<String, ParticipantInfo>,
    pub tracks: BTreeMap<String, Track>,
    pub layer_map: HashMap<(PeerId, TrackId), String>,
    pub track_seq: u64,
    pub sfu: SfuRoom,
    pub detector: SpeakerDetector,
    pub focus: Vec<String>,
    pub next_tick: Option<Instant>,
    pub redirect: Option<String>,
    pub empty_since: Option<Instant>,
}

/// 出力の箱と時計。
#[derive(Debug)]
pub(crate) struct Out {
    pub actions: VecDeque<Action>,
    pub node: String,
    pub start: Instant,
    pub unix_ms_at_start: i64,
    pub fallback: u64,
}

impl Out {
    pub fn unix_ms(&self, now: Instant) -> i64 {
        self.unix_ms_at_start + now.saturating_duration_since(self.start).as_millis() as i64
    }

    pub fn id(&mut self) -> String {
        random_id(&mut self.fallback)
    }

    pub fn send(&mut self, c: &Conn, msg: &ServerMessage) {
        if c.path == SignalPath::WebSocket && !c.ws_open {
            return;
        }
        self.actions.push_back(Action::Signal {
            conn: c.key,
            path: c.path,
            text: msg.to_json(),
        });
    }

    pub fn event(&mut self, now: Instant, room_id: &str, event: RoomEvent) {
        let env = EventEnvelope {
            id: self.id(),
            version: 1,
            at: self.unix_ms(now),
            node: self.node.clone(),
            room_id: room_id.to_string(),
            event,
        };
        self.actions.push_back(Action::Event(env));
    }

    fn error(&mut self, c: &Conn, code: ErrorCode, message: impl Into<String>, fatal: bool, request: Option<&str>) {
        self.send(
            c,
            &ServerMessage::Error(ErrorMsg {
                code,
                message: message.into(),
                fatal,
                request: request.map(str::to_string),
            }),
        );
    }
}

/// 部屋の核。
#[derive(Debug)]
pub struct RoomService {
    pub(crate) cfg: ServiceConfig,
    pub(crate) out: Out,
    pub(crate) conns: BTreeMap<ConnKey, Conn>,
    pub(crate) by_connection_id: HashMap<String, ConnKey>,
    pub(crate) rooms: BTreeMap<String, RoomState>,
    next_key: u64,
    next_remote_peer: PeerId,
}

fn level_of(bps: u64, rtt_ms: u64) -> u8 {
    let b = match bps {
        x if x >= 1_000_000 => 0,
        x if x >= 500_000 => 1,
        x if x >= 200_000 => 2,
        _ => 3,
    };
    let r = match rtt_ms {
        x if x <= 150 => 0,
        x if x <= 300 => 1,
        x if x <= 600 => 2,
        _ => 3,
    };
    b.max(r)
}

pub(crate) fn participant_info(room: &RoomState, c: &Conn) -> ParticipantInfo {
    ParticipantInfo {
        connection_id: c.connection_id.clone(),
        client_id: c.client_id.clone(),
        bundle_id: c.bundle_id.clone(),
        role: c.role,
        metadata: c.metadata.clone(),
        tracks: room
            .tracks
            .values()
            .filter(|t| matches!(&t.origin, Origin::Local { conn, .. } if *conn == c.key))
            .map(|t| t.info.clone())
            .collect(),
        node: None,
    }
}

impl RoomService {
    /// `now` の時点の UNIX 時刻（ミリ秒）を渡す（トークンの期限・イベントの時刻に使う）。
    pub fn new(cfg: ServiceConfig, now: Instant, unix_ms: i64) -> RoomService {
        RoomService {
            out: Out {
                actions: VecDeque::new(),
                node: cfg.node_id.clone(),
                start: now,
                unix_ms_at_start: unix_ms,
                fallback: unix_ms as u64 | 1,
            },
            cfg,
            conns: BTreeMap::new(),
            by_connection_id: HashMap::new(),
            rooms: BTreeMap::new(),
            next_key: 1,
            next_remote_peer: REMOTE_PEER_BASE,
        }
    }

    pub fn config(&self) -> &ServiceConfig {
        &self.cfg
    }

    /// 次の出力。
    pub fn poll_action(&mut self) -> Option<Action> {
        self.out.actions.pop_front()
    }

    /// 参加者の connection_id から番号を引く。
    pub fn conn_key(&self, connection_id: &str) -> Option<ConnKey> {
        self.by_connection_id.get(connection_id).copied()
    }

    /// 番号から connection_id（参加していれば）。
    pub fn connection_id(&self, key: ConnKey) -> Option<&str> {
        self.conns
            .get(&key)
            .filter(|c| matches!(c.phase, Phase::Joined))
            .map(|c| c.connection_id.as_str())
    }

    /// 参加者の数（このノード）。
    pub fn connection_count(&self) -> usize {
        self.conns.values().filter(|c| matches!(c.phase, Phase::Joined)).count()
    }

    // ------------------------------------------------------------------ シグナリング

    /// WebSocket が開いた。
    pub fn open(&mut self, now: Instant, meta: ConnMeta) -> ConnKey {
        let key = ConnKey(self.next_key);
        self.next_key += 1;
        self.conns.insert(
            key,
            Conn {
                key,
                meta,
                phase: Phase::AwaitConnect,
                opened_at: now,
                connection_id: String::new(),
                room_id: String::new(),
                role: Role::Recvonly,
                client_id: None,
                bundle_id: String::new(),
                metadata: Value::Null,
                multistream: true,
                spotlight: None,
                simulcast: None,
                perms: Permissions::default(),
                data_channels: Vec::new(),
                dc_ids: BTreeMap::new(),
                path: SignalPath::WebSocket,
                ws_open: true,
                want_dc_signaling: false,
                close_ws_ok: false,
                filter: None,
                peer_created: false,
                peer_connected: false,
                joined_at: now,
                joined_at_ms: 0,
                negotiating: None,
                answered: false,
                renegotiate_sent: None,
                slots: Vec::new(),
                published: BTreeMap::new(),
                next_ping: now + self.cfg.ping_interval,
                pong_deadline: None,
                rate: (now, 0),
                stats: ConnStats::default(),
            },
        );
        key
    }

    /// WebSocket が閉じた。
    pub fn on_signaling_closed(&mut self, now: Instant, key: ConnKey) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        c.ws_open = false;
        if matches!(c.phase, Phase::Joined) && c.path == SignalPath::DataChannel && c.close_ws_ok {
            return;
        }
        self.disconnect(now, key, DisconnectReason::WebsocketClosed, None);
    }

    /// シグナリングの text（WebSocket の text frame、または DataChannel `signaling`）。
    pub fn on_signaling_text(&mut self, now: Instant, key: ConnKey, text: &str) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        // 頻度の制限
        if now.saturating_duration_since(c.rate.0) >= Duration::from_secs(1) {
            c.rate = (now, 0);
        }
        c.rate.1 += 1;
        if c.rate.1 > self.cfg.max_messages_per_sec {
            if c.rate.1 == self.cfg.max_messages_per_sec + 1 {
                self.out.error(c, ErrorCode::RateLimited, "too many messages", false, None);
            }
            if c.rate.1 > self.cfg.max_messages_per_sec * 4 {
                self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::RateLimited, "too many messages".into())));
            }
            return;
        }
        let msg = match signaling::parse_client(text) {
            Ok(m) => m,
            Err(e) => {
                let fatal = matches!(c.phase, Phase::AwaitConnect);
                if fatal {
                    self.disconnect(now, key, DisconnectReason::Error, Some((e.code, e.message)));
                } else {
                    self.out.error(c, e.code, e.message, false, None);
                }
                return;
            }
        };
        let tname = msg.type_name();
        let phase = match c.phase {
            Phase::AwaitConnect => 0,
            Phase::Authenticating { .. } => 1,
            Phase::Joined => 2,
        };
        match (phase, msg) {
            (0, ClientMessage::Connect(cm)) => self.handle_connect(now, key, *cm),
            (0, _) => self.disconnect(
                now,
                key,
                DisconnectReason::Error,
                Some((ErrorCode::InvalidState, "the first message must be connect".into())),
            ),
            (1, ClientMessage::Disconnect(_)) => self.disconnect(now, key, DisconnectReason::Normal, None),
            (1, _) => self.out.error(c, ErrorCode::InvalidState, "authenticating", false, Some(tname)),
            (_, ClientMessage::Connect(_)) => {
                self.out.error(c, ErrorCode::InvalidState, "already connected", false, Some(tname))
            }
            (_, ClientMessage::Offer(o)) => self.handle_offer(now, key, &o.sdp),
            (_, ClientMessage::Candidate(cand)) => {
                if cand.candidate.len() > 1024 {
                    self.out.error(c, ErrorCode::BadMessage, "candidate too long", false, Some(tname));
                } else if c.peer_created {
                    let candidate = cand.candidate.strip_prefix("a=").unwrap_or(&cand.candidate).to_string();
                    self.out.actions.push_back(Action::AddCandidate { conn: key, candidate });
                }
            }
            (_, ClientMessage::Pong(p)) => {
                c.pong_deadline = None;
                if let Some(s) = p.stats
                    && serde_json::to_string(&s).map(|x| x.len()).unwrap_or(usize::MAX) <= 64 * 1024
                {
                    c.stats.client_reports = Some(s);
                }
            }
            (_, ClientMessage::Stats(s)) => {
                if serde_json::to_string(&s.reports).map(|x| x.len()).unwrap_or(usize::MAX) <= 64 * 1024 {
                    c.stats.client_reports = Some(s.reports);
                } else {
                    self.out.error(c, ErrorCode::BadMessage, "stats too large", false, Some(tname));
                }
            }
            (_, ClientMessage::Layer(l)) => self.handle_layer(now, key, &l.track_id, l.rid),
            (_, ClientMessage::Message(m)) => self.handle_app_message(now, key, m),
            (_, ClientMessage::Disconnect(_)) => self.disconnect(now, key, DisconnectReason::Normal, None),
        }
    }

    fn handle_connect(&mut self, now: Instant, key: ConnKey, cm: Connect) {
        if let Err((code, msg)) = cm.validate() {
            return self.disconnect(now, key, DisconnectReason::Error, Some((code, msg)));
        }
        let unix = self.out.unix_ms(now) / 1000;
        let mut claims = None;
        if let Some(tok) = &cm.token {
            let Some(v) = &self.cfg.token_verifier else {
                return self.auth_fail(now, key, "tokens are not accepted on this node");
            };
            match v.verify(tok, unix) {
                Ok(c) => {
                    if c.room != cm.room_id {
                        return self.forbid(now, key, "the token is for another room");
                    }
                    if !cm.role.within(c.role) {
                        return self.forbid(now, key, "the role exceeds the token's role");
                    }
                    if let (Some(a), Some(b)) = (&c.client_id, &cm.client_id)
                        && a != b
                    {
                        return self.forbid(now, key, "client_id differs from the token");
                    }
                    claims = Some(c);
                }
                Err(e) => return self.auth_fail(now, key, &format!("token: {e}")),
            }
        }
        let webhook = match self.cfg.auth_webhook {
            AuthWebhookMode::Off => false,
            AuthWebhookMode::Only => true,
            AuthWebhookMode::AfterToken => {
                if claims.is_none() {
                    return self.auth_fail(now, key, "token required");
                }
                true
            }
        };
        if !webhook && claims.is_none() && !self.cfg.allow_anonymous {
            return self.auth_fail(now, key, "token required");
        }
        if webhook {
            let request_id = self.out.id();
            let connection_id = self.out.id();
            let Some(c) = self.conns.get_mut(&key) else { return };
            c.connection_id = connection_id.clone();
            let req = AuthRequest {
                version: 1,
                kind: "connection.auth".into(),
                request_id: request_id.clone(),
                node: self.cfg.node_id.clone(),
                room_id: cm.room_id.clone(),
                connection_id,
                client_id: claims.as_ref().and_then(|x| x.client_id.clone()).or_else(|| cm.client_id.clone()),
                role: cm.role,
                metadata: cm.metadata.clone().unwrap_or(Value::Null),
                token_claims: claims.clone(),
                remote_addr: c.meta.remote_addr.clone(),
                user_agent: c.meta.user_agent.clone(),
            };
            let body = serde_json::to_string(&req).unwrap_or_default();
            c.phase = Phase::Authenticating {
                request_id: request_id.clone(),
                deadline: now + self.cfg.auth_timeout,
                pending: Box::new((cm, claims)),
            };
            self.out.actions.push_back(Action::AuthWebhook { conn: key, request_id, body });
            return;
        }
        self.admit(now, key, cm, claims, None);
    }

    /// 認証 Webhook の結果（`status` は HTTP の状態コード、届かなければ `None`）。
    pub fn on_auth_webhook_result(&mut self, now: Instant, key: ConnKey, request_id: &str, status: Option<u16>, body: &[u8]) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        let Phase::Authenticating { request_id: rid, .. } = &c.phase else { return };
        if rid != request_id {
            return;
        }
        let Phase::Authenticating { pending, .. } = std::mem::replace(&mut c.phase, Phase::AwaitConnect) else {
            return;
        };
        let (cm, claims) = *pending;
        match auth_decision(status, body) {
            Ok(r) => self.admit(now, key, cm, claims, Some(r)),
            Err(reason) => self.auth_fail(now, key, &reason),
        }
    }

    /// この接続には認証 Webhook が無かった（部屋にもノードにも URL が無い）: トークンだけで決める
    /// （[`AuthWebhookMode::Off`] と同じ扱い。名乗った client_id・bundle_id はトークンに無ければ拒む）。
    pub fn on_auth_webhook_skipped(&mut self, now: Instant, key: ConnKey, request_id: &str) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        let Phase::Authenticating { request_id: rid, .. } = &c.phase else { return };
        if rid != request_id {
            return;
        }
        let Phase::Authenticating { pending, .. } = std::mem::replace(&mut c.phase, Phase::AwaitConnect) else {
            return;
        };
        let (cm, claims) = *pending;
        if claims.is_none() && !self.cfg.allow_anonymous {
            return self.auth_fail(now, key, "token required");
        }
        self.admit(now, key, cm, claims, None);
    }

    fn auth_fail(&mut self, now: Instant, key: ConnKey, msg: &str) {
        self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::AuthFailed, msg.to_string())));
    }

    fn forbid(&mut self, now: Instant, key: ConnKey, msg: &str) {
        self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::Forbidden, msg.to_string())));
    }

    fn redirect(&mut self, now: Instant, key: ConnKey, location: String) {
        if let Some(c) = self.conns.get(&key) {
            self.out.send(c, &ServerMessage::Redirect(Redirect { location }));
        }
        self.disconnect(now, key, DisconnectReason::Redirected, None);
    }

    pub(crate) fn create_room(&mut self, now: Instant, id: &str, settings: RoomSettings, metadata: Value, auto: bool) {
        let mut sfu_cfg = self.cfg.sfu.clone();
        sfu_cfg.max_outputs = sfu_cfg.max_outputs.max(1024);
        let room = RoomState {
            id: id.to_string(),
            settings: settings.clone(),
            metadata,
            generation: 1,
            created_at_ms: self.out.unix_ms(now),
            auto_created: auto,
            members: Vec::new(),
            remote: BTreeMap::new(),
            tracks: BTreeMap::new(),
            layer_map: HashMap::new(),
            track_seq: 0,
            sfu: SfuRoom::new(sfu_cfg),
            detector: SpeakerDetector::new(self.cfg.detector.clone()),
            focus: Vec::new(),
            next_tick: settings.spotlight.as_ref().map(|_| now),
            redirect: None,
            empty_since: Some(now),
        };
        self.rooms.insert(id.to_string(), room);
        self.out.event(
            now,
            id,
            RoomEvent::RoomCreated {
                settings: Box::new(settings),
                auto_created: auto,
            },
        );
    }

    fn admit(&mut self, now: Instant, key: ConnKey, cm: Connect, claims: Option<JoinClaims>, resp: Option<AuthResponse>) {
        // client_id・bundle_id は転送フィルターの条件になるので、利用者が名乗った値をそのまま信じない:
        // トークンだけで認めたときは、トークンに無い値を名乗れない（Webhook は名乗った値を見て認める）。
        let via_webhook = resp.is_some();
        let token_only = claims.is_some() && !via_webhook;
        let resp = resp.unwrap_or_default();
        let role = resp.role.unwrap_or(cm.role);
        let issued_client = resp.client_id.clone().or_else(|| claims.as_ref().and_then(|c| c.client_id.clone()));
        let client_id = match issued_client {
            Some(c) => Some(c),
            None if token_only && cm.client_id.is_some() => {
                return self.forbid(now, key, "client_id must be issued in the token");
            }
            None => cm.client_id.clone(),
        };
        let issued_bundle = resp.bundle_id.clone().or_else(|| claims.as_ref().and_then(|c| c.bundle_id.clone()));
        let bundle_id = match issued_bundle {
            Some(b) => Some(b),
            None if token_only && cm.bundle_id.is_some() => {
                return self.forbid(now, key, "bundle_id must be issued in the token");
            }
            None => cm.bundle_id.clone(),
        };
        let metadata = resp
            .metadata
            .clone()
            .or_else(|| claims.as_ref().and_then(|c| c.metadata.clone()))
            .unwrap_or(Value::Null);
        let perms = resp
            .permissions
            .clone()
            .or_else(|| claims.as_ref().and_then(|c| c.permissions.clone()))
            .unwrap_or_default();
        if self.connection_count() >= self.cfg.max_connections {
            if let Some(url) = self.cfg.overflow_redirect.clone() {
                return self.redirect(now, key, url);
            }
            return self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::RoomFull, "node is full".into())));
        }
        if !self.rooms.contains_key(&cm.room_id) {
            if !self.cfg.auto_create_rooms {
                return self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::RoomNotFound, "no such room".into())));
            }
            if self.rooms.len() >= self.cfg.max_rooms {
                return self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::RoomFull, "too many rooms".into())));
            }
            let s = self.cfg.default_settings.clone();
            self.create_room(now, &cm.room_id, s, Value::Null, true);
        }
        let Some(room) = self.rooms.get(&cm.room_id) else { return };
        if let Some(url) = room.redirect.clone() {
            return self.redirect(now, key, url);
        }
        if room.members.len() + room.remote.len() >= room.settings.max_participants as usize {
            if let Some(url) = self.cfg.overflow_redirect.clone() {
                return self.redirect(now, key, url);
            }
            return self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::RoomFull, "room is full".into())));
        }
        if cm.e2ee != room.settings.e2ee {
            let m = if room.settings.e2ee { "this room requires e2ee" } else { "this room is not e2ee" };
            return self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::E2eeMismatch, m.into())));
        }
        if cm.video.as_ref().and_then(|v| v.codec).is_some_and(|c| c != room.settings.video_codec)
            || cm.audio.as_ref().and_then(|a| a.codec).is_some_and(|c| c != room.settings.audio_codec)
        {
            let m = format!(
                "this room uses {:?}/{:?}",
                room.settings.video_codec, room.settings.audio_codec
            );
            return self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::UnsupportedCodec, m)));
        }
        // DataChannel: 部屋の型と許可に合うものだけ受け付ける
        let accepted: Vec<DataChannelSpec> = cm
            .data_channels
            .iter()
            .filter(|d| {
                d.label == SIGNALING_LABEL
                    || (signaling::label_allowed(&room.settings.data_labels, &d.label) && perms.may_use_label(&d.label))
            })
            .cloned()
            .collect();
        let spotlight = room.settings.spotlight.as_ref().map(|s| {
            let req = cm.spotlight.clone();
            SpotlightRequest {
                number: req.as_ref().map_or(s.number, |r| r.number.min(s.number)),
                focus_rid: req.as_ref().and_then(|r| r.focus_rid.clone()).or_else(|| s.focus_rid.clone()),
                unfocused: Some(req.as_ref().and_then(|r| r.unfocused).unwrap_or(s.unfocused)),
            }
        });
        let settings = room.settings.clone();
        let connection_id = match self.conns.get(&key) {
            Some(c) if !c.connection_id.is_empty() => c.connection_id.clone(),
            _ => self.out.id(),
        };
        let unix_ms = self.out.unix_ms(now);
        let ping = self.cfg.ping_interval;
        let Some(c) = self.conns.get_mut(&key) else { return };
        c.phase = Phase::Joined;
        c.connection_id = connection_id.clone();
        c.room_id = cm.room_id.clone();
        c.role = role;
        c.client_id = client_id;
        c.bundle_id = bundle_id.unwrap_or_else(|| connection_id.clone());
        c.metadata = metadata;
        c.multistream = cm.multistream;
        c.spotlight = spotlight.clone();
        c.simulcast = cm.simulcast.clone();
        c.perms = perms;
        c.data_channels = accepted.clone();
        c.want_dc_signaling = cm.signaling.data_channel && accepted.iter().any(|d| d.label == SIGNALING_LABEL);
        c.close_ws_ok = cm.signaling.close_websocket;
        c.filter = cm.forwarding_filter.clone();
        c.joined_at = now;
        c.joined_at_ms = unix_ms;
        c.next_ping = now + ping;
        c.peer_created = true;
        self.by_connection_id.insert(connection_id.clone(), key);
        self.out.actions.push_back(Action::CreatePeer {
            conn: key,
            spec: PeerSpec {
                video_codec: settings.video_codec.codec(),
                audio_codec: settings.audio_codec.codec(),
                datachannel: true,
                e2ee: settings.e2ee,
            },
        });
        let RoomService { rooms, conns, out, cfg, .. } = self;
        let Some(room) = rooms.get_mut(&cm.room_id) else { return };
        room.members.push(key);
        room.empty_since = None;
        let mut participants: Vec<ParticipantInfo> = room
            .members
            .iter()
            .filter(|k| **k != key)
            .filter_map(|k| conns.get(k))
            .map(|o| participant_info(room, o))
            .collect();
        participants.extend(room.remote.values().cloned());
        let Some(c) = conns.get(&key) else { return };
        let want = wanted_counts(room, conns, c, cfg.max_slots_per_kind);
        let me = participant_info(room, c);
        out.send(
            c,
            &ServerMessage::Connected(Box::new(Connected {
                version: signaling::PROTOCOL_VERSION,
                connection_id: c.connection_id.clone(),
                client_id: c.client_id.clone(),
                bundle_id: c.bundle_id.clone(),
                room_id: room.id.clone(),
                role: c.role,
                multistream: c.multistream,
                e2ee: settings.e2ee,
                video_codec: settings.video_codec,
                audio_codec: settings.audio_codec,
                max_video_bitrate_kbps: settings.max_video_bitrate_kbps,
                spotlight,
                participants,
                recv_slots: want,
                data_channels: accepted,
                signaling_data_channel: c.want_dc_signaling,
                ping_interval_ms: cfg.ping_interval.as_millis() as u64,
                metadata: c.metadata.clone(),
            })),
        );
        let n = ServerMessage::Notify(Notify::ConnectionCreated { participant: me.clone() });
        for k in &room.members {
            if *k != key
                && let Some(o) = conns.get(k)
            {
                out.send(o, &n);
            }
        }
        out.event(now, &room.id, RoomEvent::ConnectionCreated { participant: me });
    }

    // ------------------------------------------------------------------ SDP

    fn handle_offer(&mut self, now: Instant, key: ConnKey, sdp: &str) {
        let Some(c) = self.conns.get(&key) else { return };
        if c.negotiating.is_some() {
            return self.out.error(c, ErrorCode::InvalidState, "an offer is already being processed", false, Some("offer"));
        }
        let plan = match inspect_offer(sdp, c) {
            Ok(p) => p,
            Err((code, msg)) => {
                let fatal = !c.answered;
                if fatal {
                    return self.disconnect(now, key, DisconnectReason::Error, Some((code, msg)));
                }
                return self.out.error(c, code, msg, false, Some("offer"));
            }
        };
        let Some(c) = self.conns.get_mut(&key) else { return };
        c.negotiating = Some(plan);
        self.out.actions.push_back(Action::AcceptOffer { conn: key, sdp: sdp.to_string() });
    }

    /// `AcceptOffer` の結果。`send_tracks` は answer の後の [`PeerConnection::send_tracks`](senca_webrtc::PeerConnection::send_tracks)。
    pub fn on_offer_result(&mut self, now: Instant, key: ConnKey, result: Result<String, String>, send_tracks: &[SendTrackInfo]) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        let Some(plan) = c.negotiating.take() else { return };
        let answer = match result {
            Ok(a) => a,
            Err(e) => {
                if !c.answered {
                    return self.disconnect(now, key, DisconnectReason::Error, Some((ErrorCode::NegotiationFailed, e)));
                }
                return self.out.error(c, ErrorCode::NegotiationFailed, e, false, Some("offer"));
            }
        };
        c.answered = true;
        c.renegotiate_sent = None;
        // 受信の枠
        let recv_mids: BTreeSet<&str> = plan.recv.iter().map(|(m, _)| m.as_str()).collect();
        let mut removed_slots = Vec::new();
        c.slots.retain(|s| {
            let keep = recv_mids.contains(s.mid.as_str());
            if !keep && s.bound.is_some() {
                removed_slots.push(s.send_track);
            }
            keep
        });
        for (mid, kind) in &plan.recv {
            let st = send_tracks.iter().find(|t| t.mid == *mid && t.rid.is_none()).map(|t| t.track);
            match c.slots.iter_mut().find(|s| s.mid == *mid) {
                Some(s) => s.send_track = st.or(s.send_track),
                None => c.slots.push(RecvSlot {
                    mid: mid.clone(),
                    kind: *kind,
                    send_track: st,
                    track: None,
                    bound: None,
                    paused: false,
                    preferred: None,
                    applied: None,
                }),
            }
        }
        // 送るトラック
        let pub_mids: BTreeSet<&str> = plan.publish.iter().map(|(m, _, _)| m.as_str()).collect();
        let gone: Vec<String> = c
            .published
            .iter()
            .filter(|(m, _)| !pub_mids.contains(m.as_str()))
            .map(|(_, t)| t.clone())
            .collect();
        c.published.retain(|m, _| pub_mids.contains(m.as_str()));
        let mut added = Vec::new();
        for (mid, kind, rids) in &plan.publish {
            if !c.published.contains_key(mid) {
                let track_id = format!("{}.{}", c.connection_id, mid);
                c.published.insert(mid.clone(), track_id.clone());
                added.push((mid.clone(), *kind, rids.clone(), track_id));
            }
        }
        let published: Vec<PublishedTrack> = plan
            .publish
            .iter()
            .filter_map(|(mid, kind, rids)| {
                Some(PublishedTrack {
                    mid: mid.clone(),
                    track_id: c.published.get(mid)?.clone(),
                    kind: *kind,
                    rids: rids.clone(),
                })
            })
            .collect();
        let connection_id = c.connection_id.clone();
        let owner_client_id = c.client_id.clone();
        let room_id = c.room_id.clone();
        let RoomService { rooms, conns, out, cfg, .. } = self;
        let Some(room) = rooms.get_mut(&room_id) else { return };
        for st in removed_slots.into_iter().flatten() {
            room.sfu.unsubscribe(key.0, st);
        }
        for t in gone {
            unpublish(now, room, conns, out, &t);
        }
        for (mid, kind, rids, track_id) in added {
            room.track_seq += 1;
            let info = TrackInfo {
                track_id: track_id.clone(),
                connection_id: connection_id.clone(),
                kind,
                rids,
            };
            room.tracks.insert(
                track_id.clone(),
                Track {
                    info: info.clone(),
                    client_id: owner_client_id.clone(),
                    seq: room.track_seq,
                    origin: Origin::Local { conn: key, mid: mid.clone() },
                    codec: None,
                    sfu_peer: key.0,
                    group: mid,
                    layers: Vec::new(),
                    relay_out: BTreeMap::new(),
                    configs: HashMap::new(),
                    relay_cfg_pending: BTreeSet::new(),
                    relay_in: None,
                },
            );
            let n = ServerMessage::Notify(Notify::TrackAdded { track: info.clone() });
            for k in &room.members {
                if *k != key
                    && let Some(o) = conns.get(k)
                {
                    out.send(o, &n);
                }
            }
            out.event(now, &room.id, RoomEvent::TrackPublished { track: info });
        }
        rebalance(now, room, conns, out, cfg, Some(key));
        let Some(c) = conns.get(&key) else { return };
        out.send(
            c,
            &ServerMessage::Answer(Answer {
                sdp: answer,
                slots: c.slots.iter().map(|s| wire_slot(room, s)).collect(),
                published,
            }),
        );
        maybe_renegotiate(room, conns, out, cfg, key);
        drain_sfu(room, conns, out);
    }

    // ------------------------------------------------------------------ ピア接続

    /// ピア接続の出来事。
    pub fn on_peer_event(&mut self, now: Instant, key: ConnKey, ev: PcEvent) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        if !matches!(c.phase, Phase::Joined) {
            return;
        }
        let room_id = c.room_id.clone();
        match &ev {
            PcEvent::Connected => {
                c.peer_connected = true;
            }
            PcEvent::Disconnected { .. } => {
                return self.disconnect(now, key, DisconnectReason::PeerFailed, None);
            }
            PcEvent::DataChannelOpen(info) => {
                let ok_label = if info.label == SIGNALING_LABEL {
                    c.want_dc_signaling
                } else {
                    c.data_channels.iter().any(|d| d.label == info.label)
                };
                if !ok_label {
                    self.out.actions.push_back(Action::CloseDataChannel { conn: key, id: info.id });
                    return;
                }
                c.dc_ids.insert(info.id, info.label.clone());
                if info.label == SIGNALING_LABEL && c.path == SignalPath::WebSocket {
                    self.out.send(c, &ServerMessage::Switched(Empty {}));
                    c.path = SignalPath::DataChannel;
                }
                return;
            }
            PcEvent::DataChannelClosed { id } => {
                if c.dc_ids.remove(id).as_deref() == Some(SIGNALING_LABEL) && c.path == SignalPath::DataChannel {
                    c.path = SignalPath::WebSocket;
                    if !c.ws_open {
                        return self.disconnect(now, key, DisconnectReason::PeerFailed, None);
                    }
                }
                return;
            }
            PcEvent::DataChannelMessage { id, data, binary } => {
                let Some(label) = c.dc_ids.get(id).cloned() else { return };
                let (data, binary) = (data.clone(), *binary);
                if label == SIGNALING_LABEL {
                    if let Ok(t) = std::str::from_utf8(&data) {
                        let t = t.to_string();
                        self.on_signaling_text(now, key, &t);
                    }
                    return;
                }
                return self.route_data(now, key, &label, data, binary, &[]);
            }
            PcEvent::BandwidthEstimate { bps } => {
                c.stats.peer.bandwidth_bps = *bps;
                if let Some(room) = self.rooms.get_mut(&room_id) {
                    room.sfu.set_bandwidth(now, key.0, *bps);
                }
            }
            _ => {}
        }
        let RoomService { rooms, conns, out, .. } = self;
        let Some(room) = rooms.get_mut(&room_id) else { return };
        match ev_media(ev) {
            Some(MediaEv::Track(info)) => {
                let Some(tid) = room
                    .tracks
                    .values()
                    .find(|t| matches!(&t.origin, Origin::Local { conn, mid } if *conn == key && *mid == info.mid))
                    .map(|t| t.info.track_id.clone())
                else {
                    return;
                };
                room.sfu.add_publisher_track(key.0, info.track, info.kind, info.codec, &info.mid, info.rid.as_deref());
                room.layer_map.insert((key.0, info.track), tid.clone());
                if let Some(t) = room.tracks.get_mut(&tid) {
                    t.codec = Some(info.codec);
                    if !t.layers.iter().any(|l| l.1 == info.track) {
                        t.layers.push((info.rid.clone(), info.track));
                    }
                }
                rebind_track(now, room, conns, &tid);
            }
            Some(MediaEv::Au(track, au, config)) => {
                if let Some(c) = conns.get_mut(&key) {
                    c.stats.received_bytes += au.payload.len() as u64;
                }
                if au.codec.kind() == MediaKind::Audio && room.settings.spotlight.is_some() {
                    let ms = (au.duration as f64 * 1000.0 * au.timebase.num as f64 / au.timebase.den.max(1) as f64) as u32;
                    let level = level_from_opus_size(au.payload.len(), if ms == 0 { 20 } else { ms });
                    if let Some(c) = conns.get(&key) {
                        let id = c.connection_id.clone();
                        room.detector.observe(&id, level, now);
                    }
                }
                if let Some(tid) = room.layer_map.get(&(key.0, track)).cloned()
                    && let Some(t) = room.tracks.get_mut(&tid)
                {
                    if let Some(c) = &config {
                        t.configs.insert(track, c.clone());
                    }
                    let rid = t.layers.iter().find(|l| l.1 == track).and_then(|l| l.0.clone());
                    let mut sent_cfg = Vec::new();
                    for (node, rids) in &t.relay_out {
                        if rids.is_empty() || rid.as_ref().is_some_and(|r| rids.contains(r)) {
                            // 後から購読した相手には、今の設定を最初の AU と一緒に渡す（途中から受けても組み立てられる）
                            let cfg = if t.relay_cfg_pending.contains(node) { config.clone().or_else(|| t.configs.get(&track).cloned()) } else { config.clone() };
                            if cfg.is_some() {
                                sent_cfg.push(node.clone());
                            }
                            out.actions.push_back(Action::Relay(RelayAction::Forward {
                                node: node.clone(),
                                room_id: room.id.clone(),
                                track_id: tid.clone(),
                                rid: rid.clone(),
                                au: au.clone(),
                                config: cfg,
                            }));
                        }
                    }
                    for n in sent_cfg {
                        t.relay_cfg_pending.remove(&n);
                    }
                }
                room.sfu.on_access_unit(now, key.0, track, &au, config.as_ref());
            }
            Some(MediaEv::KeyframeRequest(track)) => room.sfu.on_keyframe_request(now, key.0, track),
            None => {}
        }
        drain_sfu(room, conns, out);
    }

    /// ピア接続の統計。
    pub fn on_peer_stats(&mut self, now: Instant, key: ConnKey, s: PeerStats) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        c.stats.peer = s;
        if !c.peer_connected {
            return;
        }
        let level = level_of(s.bandwidth_bps, s.rtt_ms);
        if c.stats.net_level != Some(level)
            && c.stats.net_notified.is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(2))
        {
            c.stats.net_level = Some(level);
            c.stats.net_notified = Some(now);
            self.out.send(
                c,
                &ServerMessage::Notify(Notify::NetworkStatus {
                    level,
                    bandwidth_bps: s.bandwidth_bps,
                    rtt_ms: s.rtt_ms,
                }),
            );
        }
    }

    /// 音量（RFC 6464 の -dBov）が分かるときに入れる（Opus の大きさからの見積もりより優先される）。
    pub fn on_audio_level(&mut self, now: Instant, key: ConnKey, level: u8) {
        let Some(c) = self.conns.get(&key) else { return };
        if let Some(room) = self.rooms.get_mut(&c.room_id) {
            room.detector.observe(&c.connection_id, level, now);
        }
    }

    // ------------------------------------------------------------------ 層・message

    fn handle_layer(&mut self, now: Instant, key: ConnKey, track_id: &str, rid: Option<String>) {
        let Some(c) = self.conns.get_mut(&key) else { return };
        let room_id = c.room_id.clone();
        let Some(room) = self.rooms.get_mut(&room_id) else { return };
        let Some(t) = room.tracks.get(track_id) else {
            return self.out.error(c, ErrorCode::UnknownTrack, "no such track", false, Some("layer"));
        };
        if rid.as_ref().is_some_and(|r| !t.info.rids.contains(r)) {
            return self.out.error(c, ErrorCode::UnknownTrack, "no such layer", false, Some("layer"));
        }
        let mut found = false;
        for s in c.slots.iter_mut().filter(|s| s.track.as_deref() == Some(track_id)) {
            s.preferred = rid.clone();
            found = true;
        }
        if !found {
            return self.out.error(c, ErrorCode::UnknownTrack, "the track is not subscribed", false, Some("layer"));
        }
        apply_layers(now, room, c);
        let RoomService { rooms, conns, out, .. } = self;
        if let Some(room) = rooms.get_mut(&room_id) {
            drain_sfu(room, conns, out);
        }
    }

    fn handle_app_message(&mut self, now: Instant, key: ConnKey, m: AppMessage) {
        let Some(c) = self.conns.get(&key) else { return };
        if let Err(e) = m.validate() {
            return self.out.error(c, ErrorCode::BadMessage, e, false, Some("message"));
        }
        let room_ok = self
            .rooms
            .get(&c.room_id)
            .is_some_and(|r| signaling::label_allowed(&r.settings.data_labels, &m.label));
        if !room_ok || !c.perms.may_use_label(&m.label) {
            return self.out.error(c, ErrorCode::UnknownLabel, "label not allowed", false, Some("message"));
        }
        // DataChannel の経路と同じ規則: 送る向きで宣言したラベルだけ送れる
        if !c.data_channels.iter().any(|d| d.label == m.label && d.direction.sends()) {
            return self.out.error(c, ErrorCode::UnknownLabel, "label not declared for sending", false, Some("message"));
        }
        let (data, binary) = match (&m.data, &m.data_base64) {
            (Some(d), _) => (Bytes::from(d.clone()), false),
            (None, Some(b)) => {
                use base64::Engine;
                match base64::engine::general_purpose::STANDARD.decode(b) {
                    Ok(v) => (Bytes::from(v), true),
                    Err(_) => return self.out.error(c, ErrorCode::BadMessage, "data_base64", false, Some("message")),
                }
            }
            _ => return,
        };
        let to = m.to.clone();
        self.route_signaling_message(now, key, &m.label, data, binary, &to);
    }

    /// シグナリングで届いたアプリの message を配る（受け手にはシグナリングの `message` で）。
    fn route_signaling_message(&mut self, _now: Instant, from: ConnKey, label: &str, data: Bytes, binary: bool, to: &[String]) {
        let Some(c) = self.conns.get(&from) else { return };
        let Some(room) = self.rooms.get(&c.room_id) else { return };
        let msg = delivered(label, Some(&c.connection_id), &data, binary);
        for k in &room.members {
            if *k == from {
                continue;
            }
            let Some(o) = self.conns.get(k) else { continue };
            // 受け手の権限と宣言（受ける向き）も確かめる（DataChannel の経路と同じ）
            let may_recv = o.perms.may_use_label(label) && o.data_channels.iter().any(|d| d.label == label && d.direction.recvs());
            if may_recv && (to.is_empty() || to.contains(&o.connection_id)) {
                self.out.send(o, &msg);
            }
        }
        relay_message(&mut self.out, room, label, &c.connection_id, to, data, binary);
    }

    /// DataChannel で届いたアプリの message を、同じラベルを受ける参加者へ配る。
    fn route_data(&mut self, _now: Instant, from: ConnKey, label: &str, data: Bytes, binary: bool, to: &[String]) {
        let Some(c) = self.conns.get(&from) else { return };
        if data.len() > 1 << 20 {
            return;
        }
        let sends = c.data_channels.iter().any(|d| d.label == label && d.direction.sends());
        if !sends {
            return;
        }
        let Some(room) = self.rooms.get(&c.room_id) else { return };
        for k in &room.members {
            if *k == from {
                continue;
            }
            let Some(o) = self.conns.get(k) else { continue };
            if !to.is_empty() && !to.contains(&o.connection_id) {
                continue;
            }
            if o.perms.may_use_label(label)
                && o.dc_ids.values().any(|l| l == label)
                && o.data_channels.iter().any(|d| d.label == label && d.direction.recvs())
            {
                self.out.actions.push_back(Action::SendData {
                    conn: *k,
                    label: label.to_string(),
                    data: data.clone(),
                    binary,
                });
            }
        }
        relay_message(&mut self.out, room, label, &c.connection_id, to, data, binary);
    }

    // ------------------------------------------------------------------ 切断

    /// 切断する（`error` があれば先に `error` を送る）。
    pub fn disconnect(&mut self, now: Instant, key: ConnKey, reason: DisconnectReason, error: Option<(ErrorCode, String)>) {
        let Some(c) = self.conns.remove(&key) else { return };
        let close_code = match &error {
            Some((code, msg)) => {
                self.out.error(&c, *code, msg.clone(), true, None);
                code.close_code()
            }
            None => 1000,
        };
        self.out.send(&c, &ServerMessage::Disconnect(DisconnectNotice { reason }));
        if c.ws_open {
            self.out.actions.push_back(Action::CloseSignaling {
                conn: key,
                code: close_code,
                reason: reason.as_str().to_string(),
            });
        }
        if c.peer_created {
            self.out.actions.push_back(Action::ClosePeer { conn: key });
        }
        if !matches!(c.phase, Phase::Joined) {
            return;
        }
        self.by_connection_id.remove(&c.connection_id);
        let RoomService { rooms, conns, out, cfg, .. } = self;
        let Some(room) = rooms.get_mut(&c.room_id) else { return };
        let mine: Vec<String> = c.published.values().cloned().collect();
        for t in mine {
            unpublish(now, room, conns, out, &t);
        }
        room.sfu.remove_peer(key.0);
        room.detector.remove(&c.connection_id);
        room.focus.retain(|x| *x != c.connection_id);
        room.members.retain(|k| *k != key);
        let n = ServerMessage::Notify(Notify::ConnectionDestroyed {
            connection_id: c.connection_id.clone(),
            client_id: c.client_id.clone(),
            reason,
        });
        for k in &room.members {
            if let Some(o) = conns.get(k) {
                out.send(o, &n);
            }
        }
        out.event(
            now,
            &room.id,
            RoomEvent::ConnectionDestroyed {
                connection_id: c.connection_id.clone(),
                client_id: c.client_id.clone(),
                reason,
                duration_ms: now.saturating_duration_since(c.joined_at).as_millis() as u64,
            },
        );
        rebalance(now, room, conns, out, cfg, None);
        for k in room.members.clone() {
            maybe_renegotiate(room, conns, out, cfg, k);
        }
        drain_sfu(room, conns, out);
        if room.members.is_empty() && room.remote.is_empty() {
            room.empty_since = Some(now);
        }
    }

    /// 部屋を消す（参加者は `room_deleted` で切断）。
    pub(crate) fn destroy_room(&mut self, now: Instant, room_id: &str, reason: &str) {
        let members = match self.rooms.get(room_id) {
            Some(r) => r.members.clone(),
            None => return,
        };
        for k in members {
            self.disconnect(now, k, DisconnectReason::RoomDeleted, None);
        }
        if let Some(room) = self.rooms.remove(room_id) {
            for t in room.tracks.values() {
                if let Origin::Remote { node } = &t.origin
                    && t.relay_in.is_some()
                {
                    self.out.actions.push_back(Action::Relay(RelayAction::Unsubscribe {
                        node: node.clone(),
                        room_id: room.id.clone(),
                        track_id: t.info.track_id.clone(),
                    }));
                }
            }
            self.out.event(now, room_id, RoomEvent::RoomDestroyed { reason: reason.into() });
        }
    }

    // ------------------------------------------------------------------ 時刻

    /// 次に [`handle_timeout`](Self::handle_timeout) を呼ぶ時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut take = |x: Instant| t = Some(t.map_or(x, |y| y.min(x)));
        for c in self.conns.values() {
            match &c.phase {
                Phase::AwaitConnect => take(c.opened_at + self.cfg.connect_timeout),
                Phase::Authenticating { deadline, .. } => take(*deadline),
                Phase::Joined => {
                    take(c.next_ping);
                    if let Some(d) = c.pong_deadline {
                        take(d);
                    }
                    if !c.peer_connected {
                        take(c.joined_at + self.cfg.peer_timeout);
                    }
                }
            }
        }
        for r in self.rooms.values() {
            if let Some(x) = r.sfu.poll_timeout() {
                take(x);
            }
            if let Some(x) = r.next_tick {
                take(x);
            }
            if r.auto_created
                && let Some(e) = r.empty_since
            {
                take(e + Duration::from_secs(r.settings.empty_timeout_secs as u64));
            }
        }
        t
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        // 接続ごとの期限
        let mut drop: Vec<(ConnKey, DisconnectReason, Option<Fatal>)> = Vec::new();
        for c in self.conns.values_mut() {
            match &c.phase {
                Phase::AwaitConnect if now >= c.opened_at + self.cfg.connect_timeout => {
                    drop.push((c.key, DisconnectReason::Error, Some((ErrorCode::Timeout, "no connect".into()))));
                }
                Phase::Authenticating { deadline, .. } if now >= *deadline => {
                    drop.push((c.key, DisconnectReason::Error, Some((ErrorCode::AuthFailed, "auth webhook timeout".into()))));
                }
                Phase::Joined => {
                    if c.pong_deadline.is_some_and(|d| now >= d) {
                        drop.push((c.key, DisconnectReason::PingTimeout, None));
                        continue;
                    }
                    if !c.peer_connected && now >= c.joined_at + self.cfg.peer_timeout {
                        drop.push((c.key, DisconnectReason::PeerFailed, None));
                        continue;
                    }
                    if now >= c.next_ping {
                        self.out.send(c, &ServerMessage::Ping(Empty {}));
                        if c.pong_deadline.is_none() {
                            c.pong_deadline = Some(now + self.cfg.pong_timeout);
                        }
                        while c.next_ping <= now {
                            c.next_ping += self.cfg.ping_interval;
                        }
                    }
                }
                _ => {}
            }
        }
        for (k, r, e) in drop {
            self.disconnect(now, k, r, e);
        }
        // 部屋ごと
        let mut expired = Vec::new();
        let RoomService { rooms, conns, out, cfg, .. } = self;
        for room in rooms.values_mut() {
            room.sfu.handle_timeout(now);
            if let (Some(sp), Some(tick)) = (room.settings.spotlight.clone(), room.next_tick)
                && now >= tick
            {
                room.next_tick = Some(now + cfg.spotlight_tick);
                let cands = spotlight_candidates(room, conns);
                if room.detector.update(now, &cands, sp.number as usize) {
                    room.focus = room.detector.focus().to_vec();
                    let n = ServerMessage::Notify(Notify::SpotlightChanged { focused: room.focus.clone() });
                    for k in &room.members {
                        if let Some(o) = conns.get(k) {
                            out.send(o, &n);
                        }
                    }
                    out.event(now, &room.id, RoomEvent::SpotlightChanged { focused: room.focus.clone() });
                    rebalance(now, room, conns, out, cfg, None);
                }
            }
            drain_sfu(room, conns, out);
            if room.auto_created
                && room.members.is_empty()
                && room.remote.is_empty()
                && room
                    .empty_since
                    .is_some_and(|e| now >= e + Duration::from_secs(room.settings.empty_timeout_secs as u64))
            {
                expired.push(room.id.clone());
            }
        }
        for id in expired {
            self.destroy_room(now, &id, "empty");
        }
    }

    /// API のプッシュ（`to` が空なら部屋の全員）。届けた数を返す。
    pub fn push(&mut self, room_id: &str, to: &[String], data: Value) -> usize {
        let Some(room) = self.rooms.get(room_id) else { return 0 };
        let msg = ServerMessage::Push(Push { data });
        let mut n = 0;
        for k in &room.members {
            let Some(c) = self.conns.get(k) else { continue };
            if to.is_empty() || to.contains(&c.connection_id) {
                self.out.send(c, &msg);
                n += 1;
            }
        }
        n
    }

    // ------------------------------------------------------------------ カスケード（別のノード）

    /// 部屋への参加をこのノードで受けず、`location` へつなぎ直させる（`None` で戻す）。
    pub fn set_room_redirect(&mut self, room_id: &str, location: Option<String>) -> bool {
        match self.rooms.get_mut(room_id) {
            Some(r) => {
                r.redirect = location;
                true
            }
            None => false,
        }
    }

    /// 部屋を別のノードへ移す（グリッドの配置が変わった）: 新しい参加は `location` へ案内し、今の参加者にも
    /// `redirect` を送って切る（クライアントは案内先へ入り直す）。案内した人数を返す。
    pub fn redirect_room(&mut self, now: Instant, room_id: &str, location: String) -> usize {
        let Some(room) = self.rooms.get_mut(room_id) else { return 0 };
        room.redirect = Some(location.clone());
        let members = room.members.clone();
        for k in &members {
            self.redirect(now, *k, location.clone());
        }
        members.len()
    }

    /// 部屋があるか。
    pub fn has_room(&self, room_id: &str) -> bool {
        self.rooms.contains_key(room_id)
    }

    /// このノードの参加者（公開したトラックつき、`node` はこのノード）。カスケードの在室の同期に使う。
    pub fn local_participants(&self, room_id: &str) -> Option<Vec<ParticipantInfo>> {
        let room = self.rooms.get(room_id)?;
        Some(
            room.members
                .iter()
                .filter_map(|k| self.conns.get(k))
                .map(|c| {
                    let mut p = participant_info(room, c);
                    p.node = Some(self.cfg.node_id.clone());
                    p
                })
                .collect(),
        )
    }

    /// 別のノードの参加者（足したトラックつき）。
    pub fn remote_participants(&self, room_id: &str) -> Option<Vec<ParticipantInfo>> {
        Some(self.rooms.get(room_id)?.remote.values().cloned().collect())
    }

    /// 別のノードの参加者を足す（グリッドの中継で知った）。
    pub fn add_remote_participant(&mut self, now: Instant, room_id: &str, mut p: ParticipantInfo) -> bool {
        if p.node.is_none() || !self.rooms.contains_key(room_id) {
            return false;
        }
        p.tracks.clear();
        let RoomService { rooms, conns, out, .. } = self;
        let Some(room) = rooms.get_mut(room_id) else { return false };
        room.remote.insert(p.connection_id.clone(), p.clone());
        room.empty_since = None;
        let n = ServerMessage::Notify(Notify::ConnectionCreated { participant: p });
        for k in &room.members {
            if let Some(o) = conns.get(k) {
                out.send(o, &n);
            }
        }
        let _ = now;
        true
    }

    /// 別のノードの参加者を外す（そのトラックも）。
    pub fn remove_remote_participant(&mut self, now: Instant, room_id: &str, connection_id: &str) {
        let RoomService { rooms, conns, out, cfg, .. } = self;
        let Some(room) = rooms.get_mut(room_id) else { return };
        let Some(p) = room.remote.remove(connection_id) else { return };
        let tracks: Vec<String> = room
            .tracks
            .values()
            .filter(|t| t.info.connection_id == connection_id)
            .map(|t| t.info.track_id.clone())
            .collect();
        for t in tracks {
            unpublish(now, room, conns, out, &t);
        }
        room.detector.remove(connection_id);
        room.focus.retain(|x| x != connection_id);
        let n = ServerMessage::Notify(Notify::ConnectionDestroyed {
            connection_id: p.connection_id.clone(),
            client_id: p.client_id.clone(),
            reason: DisconnectReason::Normal,
        });
        for k in &room.members {
            if let Some(o) = conns.get(k) {
                out.send(o, &n);
            }
        }
        rebalance(now, room, conns, out, cfg, None);
        drain_sfu(room, conns, out);
        if room.members.is_empty() && room.remote.is_empty() {
            room.empty_since = Some(now);
        }
    }

    /// 別のノードのトラックを足す（`connection_id` は [`add_remote_participant`](Self::add_remote_participant) 済み）。
    pub fn add_remote_track(&mut self, now: Instant, room_id: &str, info: TrackInfo, codec: Codec) -> bool {
        let peer = self.next_remote_peer;
        let RoomService { rooms, conns, out, cfg, .. } = self;
        let Some(room) = rooms.get_mut(room_id) else { return false };
        let Some(p) = room.remote.get_mut(&info.connection_id) else { return false };
        if room.tracks.contains_key(&info.track_id) || info.rids.len() > 4 {
            return false;
        }
        let node = p.node.clone().unwrap_or_default();
        let owner_client_id = p.client_id.clone();
        p.tracks.push(info.clone());
        room.track_seq += 1;
        let rids: Vec<Option<String>> = if info.rids.is_empty() {
            vec![None]
        } else {
            info.rids.iter().cloned().map(Some).collect()
        };
        let mut layers = Vec::new();
        for (i, r) in rids.into_iter().enumerate() {
            let t = TrackId(i as u32 + 1);
            room.sfu.add_publisher_track(peer, t, info.kind.media(), codec, "0", r.as_deref());
            room.layer_map.insert((peer, t), info.track_id.clone());
            layers.push((r, t));
        }
        room.tracks.insert(
            info.track_id.clone(),
            Track {
                info: info.clone(),
                client_id: owner_client_id,
                seq: room.track_seq,
                origin: Origin::Remote { node },
                codec: Some(codec),
                sfu_peer: peer,
                group: "0".into(),
                layers,
                relay_out: BTreeMap::new(),
                relay_in: None,
                configs: HashMap::new(),
                relay_cfg_pending: BTreeSet::new(),
            },
        );
        let n = ServerMessage::Notify(Notify::TrackAdded { track: info });
        for k in &room.members {
            if let Some(o) = conns.get(k) {
                out.send(o, &n);
            }
        }
        rebalance(now, room, conns, out, cfg, None);
        for k in room.members.clone() {
            maybe_renegotiate(room, conns, out, cfg, k);
        }
        drain_sfu(room, conns, out);
        self.next_remote_peer += 1;
        true
    }

    /// 別のノードのトラックを外す。
    pub fn remove_remote_track(&mut self, now: Instant, room_id: &str, track_id: &str) {
        let RoomService { rooms, conns, out, cfg, .. } = self;
        let Some(room) = rooms.get_mut(room_id) else { return };
        if !matches!(room.tracks.get(track_id).map(|t| &t.origin), Some(Origin::Remote { .. })) {
            return;
        }
        unpublish(now, room, conns, out, track_id);
        rebalance(now, room, conns, out, cfg, None);
        drain_sfu(room, conns, out);
    }

    /// 別のノードから届いた AU。
    pub fn on_remote_access_unit(&mut self, now: Instant, room_id: &str, track_id: &str, rid: Option<&str>, au: &AccessUnit, config: Option<&CodecConfig>) {
        let RoomService { rooms, conns, out, .. } = self;
        let Some(room) = rooms.get_mut(room_id) else { return };
        let Some(t) = room.tracks.get(track_id) else { return };
        if !matches!(t.origin, Origin::Remote { .. }) {
            return;
        }
        let Some(&(_, lt)) = t.layers.iter().find(|l| l.0.as_deref() == rid) else { return };
        let peer = t.sfu_peer;
        room.sfu.on_access_unit(now, peer, lt, au, config);
        drain_sfu(room, conns, out);
    }

    /// 別のノードがこのノードのトラックを要る（`rids` が空でトラック全体、`None` でやめる）。
    pub fn on_relay_subscribe(&mut self, now: Instant, node: &str, room_id: &str, track_id: &str, rids: Option<Vec<String>>) {
        let Some(room) = self.rooms.get_mut(room_id) else { return };
        let Some(t) = room.tracks.get_mut(track_id) else { return };
        let Origin::Local { conn, .. } = t.origin else { return };
        match rids {
            Some(r) => {
                t.relay_out.insert(node.to_string(), r.clone());
                t.relay_cfg_pending.insert(node.to_string());
                for (lr, lt) in &t.layers {
                    if r.is_empty() || lr.as_ref().is_some_and(|x| r.contains(x)) {
                        self.out.actions.push_back(Action::RequestKeyframe { conn, track: *lt });
                    }
                }
            }
            None => {
                t.relay_out.remove(node);
                t.relay_cfg_pending.remove(node);
            }
        }
        let _ = now;
    }

    /// 別のノードがこのノードのトラックの層にキーフレームを求めた。
    pub fn on_relay_keyframe_request(&mut self, _now: Instant, room_id: &str, track_id: &str, rid: Option<&str>) {
        let Some(t) = self.rooms.get(room_id).and_then(|r| r.tracks.get(track_id)) else { return };
        let Origin::Local { conn, .. } = t.origin else { return };
        for (lr, lt) in &t.layers {
            if rid.is_none() || lr.as_deref() == rid {
                self.out.actions.push_back(Action::RequestKeyframe { conn, track: *lt });
            }
        }
    }

    /// 別のノードから届いたアプリの message を配る（`binary` でなければシグナリングの `message`、
    /// 受け手が同じラベルの DataChannel を開いていればそちら）。
    #[allow(clippy::too_many_arguments)]
    pub fn on_relay_message(&mut self, _now: Instant, room_id: &str, label: &str, from: &str, to: &[String], data: Bytes, binary: bool) {
        let Some(room) = self.rooms.get(room_id) else { return };
        for k in &room.members {
            let Some(o) = self.conns.get(k) else { continue };
            if !to.is_empty() && !to.contains(&o.connection_id) {
                continue;
            }
            let declared = o.perms.may_use_label(label) && o.data_channels.iter().any(|d| d.label == label && d.direction.recvs());
            if !declared {
                continue;
            }
            if o.dc_ids.values().any(|l| l == label) {
                self.out.actions.push_back(Action::SendData {
                    conn: *k,
                    label: label.to_string(),
                    data: data.clone(),
                    binary,
                });
            } else if !to.is_empty() {
                self.out.send(o, &delivered(label, Some(from), &data, binary));
            }
        }
    }
}

enum MediaEv {
    Track(senca_webrtc::TrackInfo),
    Au(TrackId, AccessUnit, Option<CodecConfig>),
    KeyframeRequest(TrackId),
}

fn ev_media(ev: PcEvent) -> Option<MediaEv> {
    match ev {
        PcEvent::Track(i) => Some(MediaEv::Track(i)),
        PcEvent::AccessUnit { track, au, config } => Some(MediaEv::Au(track, au, config)),
        PcEvent::KeyframeRequest { track } => Some(MediaEv::KeyframeRequest(track)),
        _ => None,
    }
}

fn delivered(label: &str, from: Option<&str>, data: &Bytes, binary: bool) -> ServerMessage {
    let (d, b) = if binary {
        use base64::Engine;
        (None, Some(base64::engine::general_purpose::STANDARD.encode(data)))
    } else {
        (Some(String::from_utf8_lossy(data).into_owned()), None)
    };
    ServerMessage::Message(Delivered {
        label: label.to_string(),
        from: from.map(str::to_string),
        data: d,
        data_base64: b,
    })
}

fn relay_message(out: &mut Out, room: &RoomState, label: &str, from: &str, to: &[String], data: Bytes, binary: bool) {
    let mut nodes: BTreeSet<&str> = BTreeSet::new();
    for p in room.remote.values() {
        if (to.is_empty() || to.contains(&p.connection_id))
            && let Some(n) = &p.node
        {
            nodes.insert(n);
        }
    }
    for node in nodes {
        out.actions.push_back(Action::Relay(RelayAction::Message {
            node: node.to_string(),
            room_id: room.id.clone(),
            label: label.to_string(),
            from: from.to_string(),
            to: to.to_vec(),
            data: data.clone(),
            binary,
        }));
    }
}

/// offer を調べる: 送る m 行（役割・許可）と受ける m 行。
fn inspect_offer(sdp: &str, c: &Conn) -> Result<OfferPlan, (ErrorCode, String)> {
    let d = senca_sdp::SessionDescription::parse(sdp).map_err(|e| (ErrorCode::InvalidSdp, format!("{e:?}")))?;
    let mut plan = OfferPlan { publish: Vec::new(), recv: Vec::new() };
    for (i, m) in d.media.iter().enumerate() {
        if m.port == 0 && !m.has_flag("bundle-only") {
            continue;
        }
        let kind = match m.media.as_str() {
            "audio" => Kind::Audio,
            "video" => Kind::Video,
            _ => continue,
        };
        let Some(mid) = m.mid() else {
            return Err((ErrorCode::InvalidSdp, "m-line without mid".into()));
        };
        let dir = d.effective_direction(i);
        if dir.sends() {
            if !c.role.can_publish() {
                return Err((ErrorCode::RoleViolation, format!("role {} cannot publish (mid {mid})", c.role.as_str())));
            }
            if !c.perms.may_publish(kind) {
                return Err((ErrorCode::Forbidden, format!("publishing {} is not permitted", kind.as_str())));
            }
            let mut rids: Vec<String> = m
                .rids()
                .into_iter()
                .filter(|r| r.direction == senca_sdp::RidDirection::Send)
                .map(|r| r.id)
                .collect();
            if kind == Kind::Video
                && let Some(sc) = &c.simulcast
            {
                // 宣言した順（低い順）に並べる
                rids.sort_by_key(|r| sc.rids.iter().position(|x| x == r).unwrap_or(usize::MAX));
            }
            if rids.len() > 4 {
                return Err((ErrorCode::InvalidSdp, "too many simulcast layers".into()));
            }
            plan.publish.push((mid.to_string(), kind, rids));
        }
        if dir.recvs() {
            plan.recv.push((mid.to_string(), kind));
        }
    }
    if plan.publish.len() > 8 || plan.recv.len() > 128 {
        return Err((ErrorCode::InvalidSdp, "too many m-lines".into()));
    }
    Ok(plan)
}

pub(crate) fn wire_slot(room: &RoomState, s: &RecvSlot) -> Slot {
    let t = s.track.as_ref().and_then(|id| room.tracks.get(id));
    let client_id = t.and_then(|t| t.client_id.clone());
    Slot {
        mid: s.mid.clone(),
        kind: s.kind,
        state: match (&s.track, s.paused) {
            (None, _) => SlotState::Empty,
            (Some(_), true) => SlotState::Paused,
            (Some(_), false) => SlotState::Active,
        },
        track_id: s.track.clone(),
        connection_id: t.map(|t| t.info.connection_id.clone()),
        client_id,
    }
}

/// 送り手の属性（フィルター用）: (connection_id, client_id, bundle_id, metadata)。
fn owner_of<'a>(room: &'a RoomState, conns: &'a BTreeMap<ConnKey, Conn>, t: &Track) -> Option<(&'a str, &'a str, &'a str, &'a Value)> {
    match &t.origin {
        Origin::Local { conn, .. } => {
            let c = conns.get(conn)?;
            Some((&c.connection_id, c.client_id.as_deref().unwrap_or(""), &c.bundle_id, &c.metadata))
        }
        Origin::Remote { .. } => {
            let p = room.remote.get(&t.info.connection_id)?;
            Some((&p.connection_id, p.client_id.as_deref().unwrap_or(""), &p.bundle_id, &p.metadata))
        }
    }
}

/// 映像の送り手（スポットライトの候補、参加の順）。
fn spotlight_candidates(room: &RoomState, conns: &BTreeMap<ConnKey, Conn>) -> Vec<String> {
    let mut v: Vec<(u64, String)> = Vec::new();
    for t in room.tracks.values() {
        if t.info.kind == Kind::Video && !v.iter().any(|x| x.1 == t.info.connection_id) {
            let ok = match &t.origin {
                Origin::Local { conn, .. } => conns.contains_key(conn),
                Origin::Remote { .. } => true,
            };
            if ok {
                v.push((t.seq, t.info.connection_id.clone()));
            }
        }
    }
    v.sort();
    v.into_iter().map(|x| x.1).collect()
}

/// 購読者 `s` の注目の組（自分は除く）。
fn focus_for(room: &RoomState, s: &Conn) -> Vec<String> {
    let n = s.spotlight.as_ref().map_or(0, |x| x.number as usize);
    room.focus.iter().filter(|x| **x != s.connection_id).take(n).cloned().collect()
}

/// 購読者 `s` に配りたいトラック（種類ごと、優先の順、上限まで）。
fn desired_tracks(room: &RoomState, conns: &BTreeMap<ConnKey, Conn>, s: &Conn, kind: Kind, limit: u32) -> Vec<String> {
    if !s.role.can_subscribe() || !s.perms.subscribe {
        return Vec::new();
    }
    let focus = focus_for(room, s);
    let mut v: Vec<(usize, u64, String, String)> = Vec::new();
    for t in room.tracks.values() {
        if t.info.kind != kind || t.info.connection_id == s.connection_id {
            continue;
        }
        let Some((cid, client, bundle, md)) = owner_of(room, conns, t) else { continue };
        if bundle == s.bundle_id {
            continue;
        }
        let subj = Subject {
            connection_id: cid,
            client_id: client,
            bundle_id: bundle,
            kind,
            metadata: md,
        };
        if room.settings.forwarding_filter.as_ref().is_some_and(|f| !f.allows(&subj))
            || s.filter.as_ref().is_some_and(|f| !f.allows(&subj))
        {
            continue;
        }
        let rank = focus.iter().position(|f| f == cid).unwrap_or(usize::MAX);
        v.push((rank, t.seq, t.info.track_id.clone(), cid.to_string()));
    }
    v.sort();
    if !s.multistream {
        // 1 本だけ: 注目（無ければ最初）の送り手
        if let Some(first) = v.first().cloned() {
            v.retain(|x| x.2 == first.2);
        }
    }
    v.truncate(limit as usize);
    v.into_iter().map(|x| x.2).collect()
}

fn wanted_counts(room: &RoomState, conns: &BTreeMap<ConnKey, Conn>, s: &Conn, limit: u32) -> SlotCounts {
    SlotCounts {
        audio: desired_tracks(room, conns, s, Kind::Audio, limit).len() as u32,
        video: desired_tracks(room, conns, s, Kind::Video, limit).len() as u32,
    }
}

/// 枠の割り当てを見直し、SFU を結び直し、変わった枠を `slots` で知らせる（`quiet` の接続には送らない）。
fn rebalance(now: Instant, room: &mut RoomState, conns: &mut BTreeMap<ConnKey, Conn>, out: &mut Out, cfg: &ServiceConfig, quiet: Option<ConnKey>) {
    let members = room.members.clone();
    // まず望む割り当てを（不変の借用で）求める
    let mut plans: Vec<(ConnKey, SlotPlan)> = Vec::new();
    for k in &members {
        let Some(s) = conns.get(k) else { continue };
        if !matches!(s.phase, Phase::Joined) {
            continue;
        }
        plans.push((
            *k,
            (
                desired_tracks(room, conns, s, Kind::Audio, cfg.max_slots_per_kind),
                desired_tracks(room, conns, s, Kind::Video, cfg.max_slots_per_kind),
                focus_for(room, s),
            ),
        ));
    }
    let off = room
        .settings
        .spotlight
        .as_ref()
        .is_some_and(|sp| sp.unfocused == Unfocused::Off);
    for (k, (audio, video, focus)) in plans {
        let Some(s) = conns.get_mut(&k) else { continue };
        let before: Vec<Slot> = s.slots.iter().map(|x| wire_slot(room, x)).collect();
        for (kind, want) in [(Kind::Audio, &audio), (Kind::Video, &video)] {
            for slot in s.slots.iter_mut().filter(|x| x.kind == kind) {
                if slot.track.as_ref().is_some_and(|t| !want.contains(t)) {
                    slot.track = None;
                    slot.preferred = None;
                }
            }
            for t in want {
                if s.slots.iter().any(|x| x.track.as_ref() == Some(t)) {
                    continue;
                }
                if let Some(free) = s.slots.iter_mut().find(|x| x.kind == kind && x.track.is_none()) {
                    free.track = Some(t.clone());
                    free.preferred = None;
                }
            }
        }
        let user_spot = s.spotlight.as_ref().is_some_and(|sp| sp.unfocused == Some(Unfocused::Off)) || off;
        for slot in s.slots.iter_mut() {
            slot.paused = match (&slot.track, slot.kind) {
                (Some(t), Kind::Video) if s.spotlight.is_some() && user_spot => {
                    let owner = room.tracks.get(t).map(|x| x.info.connection_id.clone()).unwrap_or_default();
                    !focus.contains(&owner)
                }
                _ => false,
            };
        }
        bind_slots(now, room, s);
        apply_layers(now, room, s);
        let after: Vec<Slot> = s.slots.iter().map(|x| wire_slot(room, x)).collect();
        if Some(k) != quiet && s.answered {
            let changed: Vec<Slot> = after.into_iter().filter(|a| !before.contains(a)).collect();
            if !changed.is_empty() {
                out.send(s, &ServerMessage::Slots(Slots { slots: changed }));
            }
        }
    }
    for k in members {
        if Some(k) != quiet {
            maybe_renegotiate(room, conns, out, cfg, k);
        }
    }
    update_relay_demand(room, conns, out);
}

/// 枠が足りなければ `renegotiate` を送る（offer の処理中なら answer の後で）。
fn maybe_renegotiate(room: &RoomState, conns: &mut BTreeMap<ConnKey, Conn>, out: &mut Out, cfg: &ServiceConfig, k: ConnKey) {
    let Some(s) = conns.get(&k) else { return };
    if !s.answered || s.negotiating.is_some() {
        return;
    }
    let want = wanted_counts(room, conns, s, cfg.max_slots_per_kind);
    let have = |kind| s.slots.iter().filter(|x| x.kind == kind).count() as u32;
    let (ha, hv) = (have(Kind::Audio), have(Kind::Video));
    if want.audio <= ha && want.video <= hv {
        return;
    }
    let r = SlotCounts {
        audio: want.audio.max(ha),
        video: want.video.max(hv),
    };
    if s.renegotiate_sent == Some(r) {
        return;
    }
    let Some(s) = conns.get_mut(&k) else { return };
    s.renegotiate_sent = Some(r);
    out.send(s, &ServerMessage::Renegotiate(Renegotiate { audio: r.audio, video: r.video }));
}

/// 枠を SFU に結ぶ・外す。
fn bind_slots(now: Instant, room: &mut RoomState, s: &mut Conn) {
    for slot in s.slots.iter_mut() {
        let Some(st) = slot.send_track else { continue };
        let want = match (&slot.track, slot.paused) {
            (Some(t), false) if room.tracks.get(t).is_some_and(|x| !x.layers.is_empty()) => Some(t.clone()),
            _ => None,
        };
        if want == slot.bound {
            continue;
        }
        match &want {
            Some(t) => {
                let Some(tr) = room.tracks.get(t) else { continue };
                room.sfu.subscribe(now, s.key.0, st, tr.sfu_peer, &tr.group);
            }
            None => room.sfu.unsubscribe(s.key.0, st),
        }
        slot.bound = want;
        slot.applied = None;
    }
}

/// 層の指定を SFU に渡す: クライアントの `layer` > スポットライト > `simulcast.default_rid` > 自動。
fn apply_layers(now: Instant, room: &mut RoomState, s: &mut Conn) {
    let focus = focus_for(room, s);
    for slot in s.slots.iter_mut() {
        let (Some(t), Some(st)) = (&slot.bound, slot.send_track) else { continue };
        let Some(tr) = room.tracks.get(t) else { continue };
        if tr.info.rids.is_empty() {
            continue;
        }
        let spot = s.spotlight.as_ref().map(|sp| {
            if focus.contains(&tr.info.connection_id) {
                sp.focus_rid.clone()
            } else {
                // 一番低い層
                tr.info.rids.first().cloned()
            }
        });
        let pref = slot
            .preferred
            .clone()
            .or(spot.flatten())
            .or_else(|| s.simulcast.as_ref().and_then(|x| x.default_rid.clone()))
            .filter(|r| tr.info.rids.contains(r));
        if slot.applied.as_ref() != Some(&pref) {
            room.sfu.set_preferred_layer(now, s.key.0, st, pref.as_deref());
            slot.applied = Some(pref);
        }
    }
}

/// トラックの層が増えたので、それを待っていた枠を結ぶ。
fn rebind_track(now: Instant, room: &mut RoomState, conns: &mut BTreeMap<ConnKey, Conn>, track_id: &str) {
    for k in room.members.clone() {
        let Some(s) = conns.get_mut(&k) else { continue };
        if s.slots.iter().any(|x| x.track.as_deref() == Some(track_id)) {
            bind_slots(now, room, s);
            apply_layers(now, room, s);
        }
    }
}

/// トラックを外し、知らせる。
fn unpublish(now: Instant, room: &mut RoomState, conns: &mut BTreeMap<ConnKey, Conn>, out: &mut Out, track_id: &str) {
    let Some(t) = room.tracks.remove(track_id) else { return };
    for (_, lt) in &t.layers {
        room.sfu.remove_publisher_track(t.sfu_peer, *lt);
        room.layer_map.remove(&(t.sfu_peer, *lt));
    }
    if let Origin::Remote { node } = &t.origin {
        if t.relay_in.is_some() {
            out.actions.push_back(Action::Relay(RelayAction::Unsubscribe {
                node: node.clone(),
                room_id: room.id.clone(),
                track_id: track_id.to_string(),
            }));
        }
        if let Some(p) = room.remote.get_mut(&t.info.connection_id) {
            p.tracks.retain(|x| x.track_id != track_id);
        }
    }
    for k in room.members.clone() {
        let Some(s) = conns.get_mut(&k) else { continue };
        for slot in s.slots.iter_mut() {
            if slot.bound.as_deref() == Some(track_id) {
                if let Some(st) = slot.send_track {
                    room.sfu.unsubscribe(k.0, st);
                }
                slot.bound = None;
            }
        }
    }
    let n = ServerMessage::Notify(Notify::TrackRemoved {
        track_id: track_id.to_string(),
        connection_id: t.info.connection_id.clone(),
    });
    for k in &room.members {
        if let Some(o) = conns.get(k)
            && o.connection_id != t.info.connection_id
        {
            out.send(o, &n);
        }
    }
    if matches!(t.origin, Origin::Local { .. }) {
        out.event(
            now,
            &room.id,
            RoomEvent::TrackUnpublished {
                track_id: track_id.to_string(),
                connection_id: t.info.connection_id.clone(),
            },
        );
    }
}

/// 別のノードのトラックで、このノードの購読者が要る層を求め直す。
fn update_relay_demand(room: &mut RoomState, conns: &BTreeMap<ConnKey, Conn>, out: &mut Out) {
    let ids: Vec<String> = room
        .tracks
        .iter()
        .filter(|(_, t)| matches!(t.origin, Origin::Remote { .. }))
        .map(|(k, _)| k.clone())
        .collect();
    for id in ids {
        let mut any = false;
        let mut auto = false;
        let mut rids: BTreeSet<String> = BTreeSet::new();
        for k in &room.members {
            let Some(s) = conns.get(k) else { continue };
            for slot in s.slots.iter().filter(|x| x.bound.as_deref() == Some(id.as_str())) {
                any = true;
                match &slot.applied {
                    Some(Some(r)) => {
                        rids.insert(r.clone());
                    }
                    _ => auto = true,
                }
            }
        }
        let Some(t) = room.tracks.get_mut(&id) else { continue };
        let Origin::Remote { node } = &t.origin else { continue };
        let want = if !any {
            None
        } else if auto || t.info.rids.is_empty() {
            Some(t.info.rids.clone())
        } else {
            Some(rids.into_iter().collect::<Vec<_>>())
        };
        if want == t.relay_in {
            continue;
        }
        match &want {
            Some(r) => out.actions.push_back(Action::Relay(RelayAction::Subscribe {
                node: node.clone(),
                room_id: room.id.clone(),
                track_id: id.clone(),
                rids: r.clone(),
            })),
            None => out.actions.push_back(Action::Relay(RelayAction::Unsubscribe {
                node: node.clone(),
                room_id: room.id.clone(),
                track_id: id.clone(),
            })),
        }
        t.relay_in = want;
    }
}

/// SFU の出力を行動へ。
fn drain_sfu(room: &mut RoomState, conns: &mut BTreeMap<ConnKey, Conn>, out: &mut Out) {
    while let Some(o) = room.sfu.poll_output() {
        match o {
            RoomOutput::Forward { subscriber, track, au, config } => {
                if let Some(c) = conns.get_mut(&ConnKey(subscriber)) {
                    c.stats.forwarded_bytes += au.payload.len() as u64;
                    c.stats.forwarded_aus += 1;
                }
                out.actions.push_back(Action::Forward {
                    conn: ConnKey(subscriber),
                    track,
                    au,
                    config,
                });
            }
            RoomOutput::KeyframeRequest { publisher, track } => {
                if publisher < REMOTE_PEER_BASE {
                    out.actions.push_back(Action::RequestKeyframe { conn: ConnKey(publisher), track });
                } else if let Some(tid) = room.layer_map.get(&(publisher, track))
                    && let Some(t) = room.tracks.get(tid)
                    && let Origin::Remote { node } = &t.origin
                {
                    let rid = t.layers.iter().find(|l| l.1 == track).and_then(|l| l.0.clone());
                    out.actions.push_back(Action::Relay(RelayAction::KeyframeRequest {
                        node: node.clone(),
                        room_id: room.id.clone(),
                        track_id: tid.clone(),
                        rid,
                    }));
                }
            }
            RoomOutput::DesiredBitrate { subscriber, bps } => {
                out.actions.push_back(Action::SetDesiredBitrate { conn: ConnKey(subscriber), bps });
            }
            RoomOutput::LayerChanged { subscriber, track, rid } => {
                if let Some(c) = conns.get(&ConnKey(subscriber))
                    && let Some(slot) = c.slots.iter().find(|s| s.send_track == Some(track))
                    && let Some(tid) = &slot.bound
                {
                    let n = ServerMessage::Notify(Notify::LayerChanged {
                        track_id: tid.clone(),
                        mid: slot.mid.clone(),
                        rid,
                    });
                    out.send(c, &n);
                }
            }
        }
    }
}

/// 設定の変更の後に（API から）。
pub(crate) fn rebalance_pub(now: Instant, room: &mut RoomState, conns: &mut BTreeMap<ConnKey, Conn>, out: &mut Out, cfg: &ServiceConfig) {
    rebalance(now, room, conns, out, cfg, None);
    drain_sfu(room, conns, out);
}
