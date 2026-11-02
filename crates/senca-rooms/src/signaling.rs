//! シグナリング v1（`docs/realtime/signaling.md`）。WebSocket の text frame 1 つに JSON の message 1 つ。
//! 接続後は DataChannel `signaling` に同じ形で載せ替えられる。
//!
//! - すべての message は `type` を持つ。未知の `type`・未知のフィールドは `bad_message`（受けた側は無視せず誤りを返す）。
//! - 版は `connect.version`（今は 1）。サーバーは `connected.version` で答える。版を上げるのは意味の変わる変更のときだけ。
//! - SDP は常にクライアントが offer を出し、サーバーが answer を返す（glare が起きない）。受信の枠（m 行）が
//!   足りなくなるとサーバーが `renegotiate` で求め、クライアントが足して offer し直す。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::filter::ForwardingFilter;
use crate::model::{
    AudioCodec, FieldError, Kind, Role, Unfocused, VideoCodec, label_matches, valid_rid, valid_room_id,
};

pub const PROTOCOL_VERSION: u32 = 1;
/// WebSocket の subprotocol。
pub const SUBPROTOCOL: &str = "senca.rooms.v1";
/// 1 つの message の上限（SDP を含む）。
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// metadata（JSON にした長さ）の上限。
pub const MAX_METADATA_BYTES: usize = 8 * 1024;
/// アプリの message の本文の上限。
pub const MAX_APP_MESSAGE_BYTES: usize = 64 * 1024;
/// 接続後のシグナリングに使う DataChannel のラベル。
pub const SIGNALING_LABEL: &str = "signaling";

fn yes() -> bool {
    true
}

// ------------------------------------------------------------------ クライアント → サーバー

/// サイマルキャストで送る層（低い順）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulcastRequest {
    pub rids: Vec<String>,
    /// 受けるときの最初の層（無ければ帯域で自動）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_rid: Option<String>,
}

/// 受け手としてのスポットライト（部屋の設定の範囲で人数・層を選ぶ）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotlightRequest {
    pub number: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_rid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unfocused: Option<Unfocused>,
}

/// 音声の希望。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioRequest {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<AudioCodec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
}

/// 映像の希望。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoRequest {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<VideoCodec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
}

/// DataChannel の向き（参加者から見て）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DataDirection {
    #[default]
    Sendrecv,
    Sendonly,
    Recvonly,
}

impl DataDirection {
    pub fn sends(self) -> bool {
        matches!(self, DataDirection::Sendrecv | DataDirection::Sendonly)
    }

    pub fn recvs(self) -> bool {
        matches!(self, DataDirection::Sendrecv | DataDirection::Recvonly)
    }
}

/// 使う DataChannel（クライアントが同じラベルで開く。`#` で始まるものはアプリの message で部屋に配る）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataChannelSpec {
    pub label: String,
    #[serde(default)]
    pub direction: DataDirection,
    #[serde(default = "yes")]
    pub ordered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retransmits: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_packet_life_time: Option<u32>,
}

/// シグナリングの経路の希望。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SignalingOptions {
    /// つながったら DataChannel `signaling` へ切り替える。
    #[serde(default)]
    pub data_channel: bool,
    /// 切り替えの後に WebSocket を閉じても切断とみなさない。
    #[serde(default)]
    pub close_websocket: bool,
}

/// SDK の情報（ログ・統計用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdkInfo {
    pub name: String,
    pub version: String,
}

/// `connect`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connect {
    pub version: u32,
    pub room_id: String,
    /// 参加トークン（認証 Webhook だけで認可するなら省ける）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 同じ bundle_id の接続どうしは互いのトラックを受けない（同じ人のカメラと画面共有など）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// 認証 Webhook へ渡す metadata（他の参加者には見せない）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default = "yes")]
    pub multistream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulcast: Option<SimulcastRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spotlight: Option<SpotlightRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_channels: Vec<DataChannelSpec>,
    #[serde(default)]
    pub signaling: SignalingOptions,
    #[serde(default)]
    pub e2ee: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_filter: Option<ForwardingFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<SdkInfo>,
}

/// SDP（`offer`・`answer`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sdp {
    pub sdp: String,
}

/// trickle ICE の候補。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// `candidate:...`（空文字列は end-of-candidates）。
    pub candidate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_mline_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_fragment: Option<String>,
}

/// `pong`（`stats` に getStats の要約を載せてよい）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Pong {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<Value>,
}

/// 受けているトラックの層を選ぶ（`rid` が `null` なら帯域で自動）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerRequest {
    pub track_id: String,
    pub rid: Option<String>,
}

/// シグナリングで送るアプリの message（`to` が空なら部屋の全員、DataChannel の `#` と同じ規則）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppMessage {
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
}

/// クライアントの統計（getStats の要約）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientStats {
    pub reports: Value,
}

/// `disconnect`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DisconnectRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// クライアントからの message。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Connect(Box<Connect>),
    Offer(Sdp),
    Candidate(Candidate),
    Pong(Pong),
    Layer(LayerRequest),
    Message(AppMessage),
    Stats(ClientStats),
    Disconnect(DisconnectRequest),
}

impl ClientMessage {
    pub fn type_name(&self) -> &'static str {
        match self {
            ClientMessage::Connect(_) => "connect",
            ClientMessage::Offer(_) => "offer",
            ClientMessage::Candidate(_) => "candidate",
            ClientMessage::Pong(_) => "pong",
            ClientMessage::Layer(_) => "layer",
            ClientMessage::Message(_) => "message",
            ClientMessage::Stats(_) => "stats",
            ClientMessage::Disconnect(_) => "disconnect",
        }
    }
}

// ------------------------------------------------------------------ サーバー → クライアント

/// 誤りの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadMessage,
    UnsupportedVersion,
    AuthFailed,
    Forbidden,
    RoomNotFound,
    RoomFull,
    RoleViolation,
    E2eeMismatch,
    UnsupportedCodec,
    InvalidSdp,
    NegotiationFailed,
    InvalidState,
    RateLimited,
    UnknownTrack,
    UnknownLabel,
    Timeout,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadMessage => "bad_message",
            ErrorCode::UnsupportedVersion => "unsupported_version",
            ErrorCode::AuthFailed => "auth_failed",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::RoomNotFound => "room_not_found",
            ErrorCode::RoomFull => "room_full",
            ErrorCode::RoleViolation => "role_violation",
            ErrorCode::E2eeMismatch => "e2ee_mismatch",
            ErrorCode::UnsupportedCodec => "unsupported_codec",
            ErrorCode::InvalidSdp => "invalid_sdp",
            ErrorCode::NegotiationFailed => "negotiation_failed",
            ErrorCode::InvalidState => "invalid_state",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::UnknownTrack => "unknown_track",
            ErrorCode::UnknownLabel => "unknown_label",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Internal => "internal",
        }
    }

    /// WebSocket を閉じるときの code（4000 + HTTP の状態コードに近いもの）。
    pub fn close_code(self) -> u16 {
        match self {
            ErrorCode::BadMessage | ErrorCode::InvalidSdp | ErrorCode::InvalidState => 4400,
            ErrorCode::UnsupportedVersion | ErrorCode::UnsupportedCodec | ErrorCode::E2eeMismatch => 4422,
            ErrorCode::AuthFailed => 4401,
            ErrorCode::Forbidden | ErrorCode::RoleViolation | ErrorCode::UnknownLabel => 4403,
            ErrorCode::RoomNotFound | ErrorCode::UnknownTrack => 4404,
            ErrorCode::RoomFull => 4503,
            ErrorCode::RateLimited => 4429,
            ErrorCode::Timeout => 4408,
            ErrorCode::NegotiationFailed | ErrorCode::Internal => 4500,
        }
    }
}

/// 切断の理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    /// クライアントの `disconnect`。
    Normal,
    /// WebSocket が閉じた（切り替えの後で閉じてよいときを除く）。
    WebsocketClosed,
    /// pong が来ない。
    PingTimeout,
    /// ICE・DTLS の失敗、つながらない。
    PeerFailed,
    /// API で切断した。
    Kicked,
    /// 部屋を消した。
    RoomDeleted,
    /// 誤りで閉じた（直前の `error`）。
    Error,
    /// 別のノードへ移す（`redirect`）。
    Redirected,
    /// ノードを止める。
    NodeShutdown,
}

impl DisconnectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DisconnectReason::Normal => "normal",
            DisconnectReason::WebsocketClosed => "websocket_closed",
            DisconnectReason::PingTimeout => "ping_timeout",
            DisconnectReason::PeerFailed => "peer_failed",
            DisconnectReason::Kicked => "kicked",
            DisconnectReason::RoomDeleted => "room_deleted",
            DisconnectReason::Error => "error",
            DisconnectReason::Redirected => "redirected",
            DisconnectReason::NodeShutdown => "node_shutdown",
        }
    }
}

/// 公開されたトラック。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub track_id: String,
    pub connection_id: String,
    pub kind: Kind,
    /// サイマルキャストの層（低い順）。サイマルキャストでなければ空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rids: Vec<String>,
}

/// 参加者。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipantInfo {
    pub connection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub bundle_id: String,
    pub role: Role,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub tracks: Vec<TrackInfo>,
    /// 別のノードにいる参加者（カスケード）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// 受信の枠の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotState {
    /// 何も割り当てていない。
    Empty,
    /// 配っている。
    Active,
    /// 割り当てているが配っていない（スポットライトの外など）。
    Paused,
}

/// 受信の枠（クライアントの受ける m 行 1 つ）と、そこに今流しているトラック。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub mid: String,
    pub kind: Kind,
    pub state: SlotState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// 種類ごとの枠の数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SlotCounts {
    pub audio: u32,
    pub video: u32,
}

/// 送るトラック（クライアントの送る m 行）に付けた ID。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedTrack {
    pub mid: String,
    pub track_id: String,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rids: Vec<String>,
}

/// `connected`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connected {
    pub version: u32,
    pub connection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub bundle_id: String,
    pub room_id: String,
    pub role: Role,
    pub multistream: bool,
    pub e2ee: bool,
    pub video_codec: VideoCodec,
    pub audio_codec: AudioCodec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_video_bitrate_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spotlight: Option<SpotlightRequest>,
    /// 他の参加者（今いる人）。
    pub participants: Vec<ParticipantInfo>,
    /// 最初の offer に入れてほしい受信の枠の数。
    pub recv_slots: SlotCounts,
    /// 受け付けた DataChannel（クライアントが開く）。
    pub data_channels: Vec<DataChannelSpec>,
    pub signaling_data_channel: bool,
    pub ping_interval_ms: u64,
    /// 他の参加者に見せる自分の metadata。
    #[serde(default)]
    pub metadata: Value,
}

/// `answer`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub sdp: String,
    pub slots: Vec<Slot>,
    pub published: Vec<PublishedTrack>,
}

/// `slots`（枠の割り当ての変化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slots {
    pub slots: Vec<Slot>,
}

/// `renegotiate`（受信の枠を種類ごとにこの数まで足して offer し直してほしい）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Renegotiate {
    pub audio: u32,
    pub video: u32,
}

/// 通知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Notify {
    #[serde(rename = "connection.created")]
    ConnectionCreated { participant: ParticipantInfo },
    #[serde(rename = "connection.updated")]
    ConnectionUpdated { connection_id: String, metadata: Value },
    #[serde(rename = "connection.destroyed")]
    ConnectionDestroyed {
        connection_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        reason: DisconnectReason,
    },
    #[serde(rename = "track.added")]
    TrackAdded { track: TrackInfo },
    #[serde(rename = "track.removed")]
    TrackRemoved { track_id: String, connection_id: String },
    #[serde(rename = "spotlight.changed")]
    SpotlightChanged { focused: Vec<String> },
    /// 受けている層が変わった（`rid` は今の層）。
    #[serde(rename = "layer.changed")]
    LayerChanged { track_id: String, mid: String, rid: Option<String> },
    /// 自分の回線の状態（0 よい 〜 3 悪い）。
    #[serde(rename = "network.status")]
    NetworkStatus { level: u8, bandwidth_bps: u64, rtt_ms: u64 },
}

/// API からのプッシュ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Push {
    pub data: Value,
}

/// 届いたアプリの message（シグナリング経由）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    pub label: String,
    /// 送り手の connection_id（API からなら省く）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Empty {}

/// `redirect`（別のノードへつなぎ直す）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redirect {
    pub location: String,
}

/// `error`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMsg {
    pub code: ErrorCode,
    pub message: String,
    /// 真ならこの後切断する。
    pub fatal: bool,
    /// 誤りの元になった message の `type`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
}

/// `disconnect`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisconnectNotice {
    pub reason: DisconnectReason,
}

/// サーバーからの message。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Connected(Box<Connected>),
    Answer(Answer),
    Slots(Slots),
    Renegotiate(Renegotiate),
    Notify(Notify),
    Push(Push),
    Message(Delivered),
    Ping(Empty),
    Switched(Empty),
    Redirect(Redirect),
    Error(ErrorMsg),
    Disconnect(DisconnectNotice),
}

impl ServerMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| r#"{"type":"error","code":"internal","message":"serialize","fatal":false}"#.into())
    }
}

/// 解析の誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub code: ErrorCode,
    pub message: String,
}

/// 受けた text を解析する。
pub fn parse_client(text: &str) -> Result<ClientMessage, ParseError> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(ParseError {
            code: ErrorCode::BadMessage,
            message: "message too large".into(),
        });
    }
    serde_json::from_str::<ClientMessage>(text).map_err(|e| ParseError {
        code: ErrorCode::BadMessage,
        message: e.to_string(),
    })
}

fn valid_client_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && !s.chars().any(char::is_control)
}

fn json_len(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(usize::MAX)
}

impl Connect {
    /// 部屋の設定に依らない検査。
    pub fn validate(&self) -> Result<(), (ErrorCode, String)> {
        if self.version != PROTOCOL_VERSION {
            return Err((
                ErrorCode::UnsupportedVersion,
                format!("version {} is not supported (use {PROTOCOL_VERSION})", self.version),
            ));
        }
        let bad = |m: &str| Err((ErrorCode::BadMessage, m.to_string()));
        if !valid_room_id(&self.room_id) {
            return bad("room_id: 1〜128 文字の英数字と -_.:@");
        }
        if self.client_id.as_deref().is_some_and(|c| !valid_client_id(c)) {
            return bad("client_id: 1〜255 文字、制御文字なし");
        }
        if self.bundle_id.as_deref().is_some_and(|c| !valid_client_id(c)) {
            return bad("bundle_id: 1〜255 文字、制御文字なし");
        }
        if self.metadata.as_ref().is_some_and(|m| json_len(m) > MAX_METADATA_BYTES) {
            return bad("metadata: 8 KiB まで");
        }
        if let Some(s) = &self.simulcast {
            if s.rids.is_empty() || s.rids.len() > 4 || s.rids.iter().any(|r| !valid_rid(r)) {
                return bad("simulcast.rids: 英数字 1〜16 文字を 1〜4 個");
            }
            let mut u = s.rids.clone();
            u.sort();
            u.dedup();
            if u.len() != s.rids.len() {
                return bad("simulcast.rids: 重複");
            }
            if s.default_rid.as_ref().is_some_and(|d| !s.rids.contains(d)) {
                return bad("simulcast.default_rid: rids のどれか");
            }
        }
        if let Some(s) = &self.spotlight {
            if !(1..=16).contains(&s.number) {
                return bad("spotlight.number: 1〜16");
            }
            if s.focus_rid.as_deref().is_some_and(|r| !valid_rid(r)) {
                return bad("spotlight.focus_rid");
            }
        }
        if self.data_channels.len() > 32 {
            return bad("data_channels: 32 個まで");
        }
        let mut seen = Vec::new();
        for d in &self.data_channels {
            if d.label != SIGNALING_LABEL && (!d.label.starts_with('#') || d.label.len() < 2) {
                return bad("data_channels.label: `#` で始まるか `signaling`");
            }
            if d.label.len() > 64 || d.label.chars().any(char::is_control) {
                return bad("data_channels.label: 64 byte まで");
            }
            if d.max_retransmits.is_some() && d.max_packet_life_time.is_some() {
                return bad("data_channels: max_retransmits と max_packet_life_time は片方だけ");
            }
            if seen.contains(&d.label) {
                return bad("data_channels.label: 重複");
            }
            seen.push(d.label.clone());
        }
        if self.signaling.close_websocket && !self.signaling.data_channel {
            return bad("signaling.close_websocket は data_channel と一緒に");
        }
        if let Some(f) = &self.forwarding_filter
            && let Err(e) = f.validate()
        {
            return Err((
                ErrorCode::BadMessage,
                format!(
                    "forwarding_filter: {}",
                    e.iter().map(|x: &FieldError| format!("{} {}", x.field, x.reason)).collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        if self.token.as_ref().is_some_and(|t| t.len() > 16 * 1024) {
            return bad("token: 16 KiB まで");
        }
        Ok(())
    }

    /// 送る種類（connect の希望。実際に送るものは offer で決まる）。
    pub fn wants(&self, k: Kind) -> bool {
        match k {
            Kind::Audio => self.audio.as_ref().is_none_or(|a| a.enabled),
            Kind::Video => self.video.as_ref().is_none_or(|v| v.enabled),
        }
    }
}

impl AppMessage {
    pub fn validate(&self) -> Result<(), String> {
        if !self.label.starts_with('#') || self.label.len() > 64 {
            return Err("label: `#` で始まる 64 byte まで".into());
        }
        match (&self.data, &self.data_base64) {
            (Some(d), None) if d.len() <= MAX_APP_MESSAGE_BYTES => Ok(()),
            (None, Some(d)) if d.len() <= MAX_APP_MESSAGE_BYTES * 4 / 3 + 4 => Ok(()),
            (Some(_), Some(_)) | (None, None) => Err("data か data_base64 のどちらか 1 つ".into()),
            _ => Err("64 KiB まで".into()),
        }
    }
}

/// `label` を使ってよいか（部屋の設定・許可・宣言）。
pub fn label_allowed(room_patterns: &[String], label: &str) -> bool {
    room_patterns.iter().any(|p| label_matches(p, label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn connect_json(extra: Value) -> String {
        let mut v = json!({"type":"connect","version":1,"room_id":"r1","role":"sendrecv"});
        if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, x) in e {
                m.insert(k.clone(), x.clone());
            }
        }
        v.to_string()
    }

    #[test]
    fn parses_full_connect() {
        let t = connect_json(json!({
            "token": "a.b.c", "client_id": "alice", "bundle_id": "alice-1", "metadata": {"n": 1},
            "multistream": true, "simulcast": {"rids": ["r0","r1","r2"], "default_rid": "r1"},
            "spotlight": {"number": 2, "unfocused": "off"},
            "audio": {"enabled": true, "codec": "opus", "bitrate_kbps": 32},
            "video": {"codec": "vp8", "bitrate_kbps": 1500},
            "data_channels": [{"label": "#chat", "direction": "sendrecv", "ordered": false, "max_retransmits": 0},
                              {"label": "signaling"}],
            "signaling": {"data_channel": true, "close_websocket": true},
            "e2ee": false,
            "forwarding_filter": {"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]}]]},
            "sdk": {"name": "senca-js", "version": "1.0.0"}
        }));
        let m = parse_client(&t).unwrap();
        let ClientMessage::Connect(c) = m else { panic!() };
        c.validate().unwrap();
        assert_eq!(c.simulcast.as_ref().unwrap().rids.len(), 3);
        assert_eq!(c.data_channels[0].max_retransmits, Some(0));
        assert!(!c.data_channels[0].ordered);
        assert!(c.data_channels[1].ordered);
        assert!(c.wants(Kind::Audio) && c.wants(Kind::Video));
        // 往復
        let back = serde_json::to_string(&ClientMessage::Connect(c.clone())).unwrap();
        assert_eq!(parse_client(&back).unwrap(), ClientMessage::Connect(c));
    }

    #[test]
    fn rejects_unknown_fields_and_types() {
        for bad in [
            connect_json(json!({"bogus": 1})),
            connect_json(json!({"audio": {"enabled": true, "x": 1}})),
            r#"{"type":"offer","sdp":"v=0","extra":1}"#.to_string(),
            r#"{"type":"teleport"}"#.to_string(),
            r#"{"sdp":"v=0"}"#.to_string(),
            r#"{"type":"connect","version":1,"room_id":"r","role":"admin"}"#.to_string(),
            "not json".to_string(),
        ] {
            let e = parse_client(&bad).unwrap_err();
            assert_eq!(e.code, ErrorCode::BadMessage, "{bad}");
        }
        let big = format!(r#"{{"type":"offer","sdp":"{}"}}"#, "a".repeat(MAX_MESSAGE_BYTES));
        assert!(parse_client(&big).is_err());
    }

    #[test]
    fn connect_validation() {
        let parse = |extra: Value| -> Connect {
            match parse_client(&connect_json(extra)).unwrap() {
                ClientMessage::Connect(c) => *c,
                _ => unreachable!(),
            }
        };
        assert_eq!(parse(json!({"version": 2})).validate().unwrap_err().0, ErrorCode::UnsupportedVersion);
        for extra in [
            json!({"room_id": "a/b"}),
            json!({"client_id": ""}),
            json!({"metadata": {"x": "a".repeat(9000)}}),
            json!({"simulcast": {"rids": []}}),
            json!({"simulcast": {"rids": ["a", "a"]}}),
            json!({"simulcast": {"rids": ["a-b"]}}),
            json!({"simulcast": {"rids": ["a"], "default_rid": "b"}}),
            json!({"spotlight": {"number": 0}}),
            json!({"data_channels": [{"label": "chat"}]}),
            json!({"data_channels": [{"label": "#a"}, {"label": "#a"}]}),
            json!({"data_channels": [{"label": "#a", "max_retransmits": 1, "max_packet_life_time": 10}]}),
            json!({"signaling": {"close_websocket": true}}),
            json!({"forwarding_filter": {"action": "block", "rules": []}}),
        ] {
            assert_eq!(parse(extra.clone()).validate().unwrap_err().0, ErrorCode::BadMessage, "{extra}");
        }
    }

    #[test]
    fn server_messages_shape() {
        let n = ServerMessage::Notify(Notify::TrackAdded {
            track: TrackInfo {
                track_id: "c1.0".into(),
                connection_id: "c1".into(),
                kind: Kind::Video,
                rids: vec!["r0".into(), "r1".into()],
            },
        });
        let j: Value = serde_json::from_str(&n.to_json()).unwrap();
        assert_eq!(j["type"], "notify");
        assert_eq!(j["event"], "track.added");
        assert_eq!(j["track"]["kind"], "video");
        let back: ServerMessage = serde_json::from_value(j).unwrap();
        assert_eq!(back, n);
        let e = ServerMessage::Error(ErrorMsg {
            code: ErrorCode::RoomFull,
            message: "full".into(),
            fatal: true,
            request: Some("connect".into()),
        });
        assert_eq!(
            e.to_json(),
            r#"{"type":"error","code":"room_full","message":"full","fatal":true,"request":"connect"}"#
        );
        assert_eq!(ServerMessage::Ping(Empty {}).to_json(), r#"{"type":"ping"}"#);
        let r = ServerMessage::Renegotiate(Renegotiate { audio: 2, video: 3 });
        assert_eq!(r.to_json(), r#"{"type":"renegotiate","audio":2,"video":3}"#);
        assert_eq!(ErrorCode::AuthFailed.close_code(), 4401);
    }

    #[test]
    fn app_message_validation() {
        let m = |label: &str, data: Option<&str>, b64: Option<&str>| AppMessage {
            label: label.into(),
            to: vec![],
            data: data.map(str::to_string),
            data_base64: b64.map(str::to_string),
        };
        assert!(m("#chat", Some("hi"), None).validate().is_ok());
        assert!(m("#chat", None, Some("aGk=")).validate().is_ok());
        assert!(m("chat", Some("hi"), None).validate().is_err());
        assert!(m("#chat", Some("hi"), Some("aGk=")).validate().is_err());
        assert!(m("#chat", None, None).validate().is_err());
        assert!(m("#chat", Some(&"x".repeat(MAX_APP_MESSAGE_BYTES + 1)), None).validate().is_err());
    }
}
