//! REST の資源（ADR-0015）。HTTP・認証・Idempotency はノードの共通の仕組み（`/api/v1`、仕様 10.1）が持ち、
//! ここは要求の型・検査・状態の変更・応答の型だけ。
//!
//! | 操作 | パス | ここの関数 |
//! |---|---|---|
//! | 部屋を作る | `POST /rooms` | [`RoomService::api_create_room`] |
//! | 一覧 | `GET /rooms?limit&cursor` | [`RoomService::api_list_rooms`] |
//! | 取得 | `GET /rooms/{id}`（`ETag`） | [`RoomService::api_get_room`] |
//! | 更新 | `PATCH /rooms/{id}`（`If-Match` 必須） | [`RoomService::api_update_room`] |
//! | 削除 | `DELETE /rooms/{id}` | [`RoomService::api_delete_room`] |
//! | 参加者 | `GET /rooms/{id}/participants`・`GET …/{connection_id}` | `api_list_participants`・`api_get_participant` |
//! | 参加者の更新 | `PATCH /rooms/{id}/participants/{connection_id}` | [`RoomService::api_update_participant`] |
//! | 切断 | `DELETE /rooms/{id}/participants/{connection_id}` | [`RoomService::api_disconnect_participant`] |
//! | 送る | `POST /rooms/{id}/messages` | [`RoomService::api_post_message`] |
//! | 統計 | `GET /rooms/{id}/stats` | [`RoomService::api_stats`] |
//! | 参加トークン | `POST /rooms/{id}/tokens` | [`RoomService::api_issue_token`] |
//!
//! 誤りは problem+json（[`ApiError::to_problem`]）。`code` は senca-control の ProblemCode の slug と同じ。

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::events::RoomEvent;
use crate::model::{FieldError, Kind, Role, RoomSettings, valid_room_id};
use crate::service::{ConnKey, Origin, Phase, RoomService, RoomState, SignalPath, participant_info, wire_slot};
use crate::signaling::{
    self, DisconnectReason, Notify, ParticipantInfo, ServerMessage, Slot, SlotState, TrackInfo,
};
use crate::token::{Audience, JoinClaims, Permissions};

/// API の誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    /// problem の種類（`invalid-request`・`not-found`・`conflict`・`precondition-failed`・
    /// `precondition-required`・`capacity-unavailable`・`invalid-config`・`unsupported-codec`）。
    pub code: &'static str,
    pub title: String,
    pub errors: Vec<FieldError>,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, title: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            title: title.into(),
            errors: Vec::new(),
        }
    }

    pub fn not_found() -> ApiError {
        ApiError::new(404, "not-found", "見つかりません")
    }

    pub fn invalid(title: impl Into<String>, errors: Vec<FieldError>) -> ApiError {
        ApiError {
            errors,
            ..ApiError::new(400, "invalid-request", title)
        }
    }

    /// problem+json の本文。
    pub fn to_problem(&self) -> Value {
        let mut v = json!({
            "type": format!("https://senca.dev/problems/{}", self.code),
            "title": self.title,
            "status": self.status,
            "code": self.code.replace('-', "_").to_ascii_uppercase(),
        });
        if !self.errors.is_empty() {
            v["errors"] = serde_json::to_value(&self.errors).unwrap_or(Value::Null);
        }
        v
    }
}

/// `POST /rooms`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CreateRoom {
    /// 省けば作る。
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub settings: Option<RoomSettings>,
    #[serde(default)]
    pub metadata: Value,
}

/// `PATCH /rooms/{id}`（設定は全体を置き換える）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateRoom {
    #[serde(default)]
    pub settings: Option<RoomSettings>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// サーバーでの合成ができるか（E2EE なら理由）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compositing {
    pub allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 部屋。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomView {
    pub id: String,
    pub settings: RoomSettings,
    pub metadata: Value,
    pub generation: u64,
    /// `ETag` の値（`"{generation}"`）。
    pub etag: String,
    /// UNIX ミリ秒。
    pub created_at: i64,
    pub auto_created: bool,
    pub participants: u32,
    pub local_participants: u32,
    pub remote_participants: u32,
    pub tracks: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<String>,
    pub focused: Vec<String>,
    pub compositing: Compositing,
}

/// 参加者。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipantView {
    #[serde(flatten)]
    pub info: ParticipantInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_at: Option<i64>,
    pub peer_connected: bool,
    /// `websocket`・`datachannel`・`remote`。
    pub signaling: String,
    pub subscriptions: Vec<Slot>,
}

/// `PATCH /rooms/{id}/participants/{connection_id}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateParticipant {
    pub metadata: Value,
}

/// `POST /rooms/{id}/messages`。`label` があれば DataChannel（そのラベルを開いている参加者）、
/// 無ければシグナリングの `push`。`to` は connection_id（空なら全員）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostMessage {
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub data: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageResult {
    pub delivered: u32,
}

fn default_ttl() -> u32 {
    3600
}

/// `POST /rooms/{id}/tokens`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueToken {
    pub role: Role,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub permissions: Option<Permissions>,
    #[serde(default = "default_ttl")]
    pub ttl_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenView {
    pub token: String,
    pub room_id: String,
    /// UNIX 秒。
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerStats {
    pub rid: Option<String>,
    pub bitrate_bps: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackStats {
    pub track_id: String,
    pub kind: Kind,
    pub layers: Vec<LayerStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionStats {
    pub mid: String,
    pub track_id: Option<String>,
    pub state: SlotState,
    /// 今配っている層（`null` は層の無いトラック・まだ）。
    pub rid: Option<String>,
}

/// 参加者の統計。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipantStats {
    pub connection_id: String,
    pub rtt_ms: u64,
    pub bandwidth_bps: u64,
    pub send_bitrate_bps: u64,
    pub retransmitted_packets: u64,
    pub nack_requested: u64,
    pub received_bytes: u64,
    pub forwarded_bytes: u64,
    pub forwarded_frames: u64,
    pub published: Vec<TrackStats>,
    pub subscriptions: Vec<SubscriptionStats>,
    /// クライアントが送った getStats の要約。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsView {
    pub room_id: String,
    pub at: i64,
    pub participants: Vec<ParticipantStats>,
}

/// 一覧の頁。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// 続きの頁の位置（無ければ null。REST の他の一覧と同じく、いつも載せる）。
    #[serde(default)]
    pub next_cursor: Option<String>,
}

fn page<T>(mut items: Vec<(String, T)>, limit: usize, cursor: Option<&str>) -> Result<Page<T>, ApiError> {
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::invalid("limit は 1〜1000", vec![FieldError::new("limit", "1〜1000")]));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    let mut last = None;
    let mut more = false;
    for (id, it) in items.into_iter().filter(|(id, _)| cursor.is_none_or(|c| id.as_str() > c)) {
        if out.len() == limit {
            // 続きが本当にあるときだけ next_cursor を付ける
            more = true;
            break;
        }
        last = Some(id);
        out.push(it);
    }
    Ok(Page {
        items: out,
        next_cursor: if more { last } else { None },
    })
}

pub(crate) fn room_view(room: &RoomState) -> RoomView {
    RoomView {
        id: room.id.clone(),
        settings: room.settings.clone(),
        metadata: room.metadata.clone(),
        generation: room.generation,
        etag: format!("\"{}\"", room.generation),
        created_at: room.created_at_ms,
        auto_created: room.auto_created,
        participants: (room.members.len() + room.remote.len()) as u32,
        local_participants: room.members.len() as u32,
        remote_participants: room.remote.len() as u32,
        tracks: room.tracks.len() as u32,
        redirect: room.redirect.clone(),
        focused: room.focus.clone(),
        compositing: match room.settings.compositing_allowed() {
            Ok(()) => Compositing { allowed: true, reason: None },
            Err(r) => Compositing {
                allowed: false,
                reason: Some(r.into()),
            },
        },
    }
}

impl RoomService {
    fn room_or_404(&self, id: &str) -> Result<&RoomState, ApiError> {
        self.rooms.get(id).ok_or_else(ApiError::not_found)
    }

    pub fn api_create_room(&mut self, now: Instant, req: CreateRoom) -> Result<RoomView, ApiError> {
        let id = match req.id {
            Some(id) => {
                if !valid_room_id(&id) {
                    return Err(ApiError::invalid(
                        "id の形が違います",
                        vec![FieldError::new("id", "1〜128 文字の英数字と -_.:@")],
                    ));
                }
                id
            }
            None => self.out.id(),
        };
        let settings = req.settings.unwrap_or_else(|| self.cfg.default_settings.clone());
        settings
            .validate()
            .map_err(|e| ApiError::invalid("設定が不正です", e))?;
        if self.rooms.contains_key(&id) {
            return Err(ApiError::new(409, "conflict", "同じ ID の部屋があります"));
        }
        if self.rooms.len() >= self.cfg.max_rooms {
            return Err(ApiError::new(503, "capacity-unavailable", "部屋が多すぎます"));
        }
        self.create_room(now, &id, settings, req.metadata, false);
        Ok(room_view(self.room_or_404(&id)?))
    }

    pub fn api_get_room(&self, id: &str) -> Result<RoomView, ApiError> {
        Ok(room_view(self.room_or_404(id)?))
    }

    pub fn api_list_rooms(&self, limit: usize, cursor: Option<&str>) -> Result<Page<RoomView>, ApiError> {
        page(
            self.rooms.values().map(|r| (r.id.clone(), room_view(r))).collect(),
            limit,
            cursor,
        )
    }

    /// 設定・metadata を変える。`if_match` は `ETag` の世代（必須）。
    pub fn api_update_room(&mut self, now: Instant, id: &str, if_match: Option<u64>, req: UpdateRoom) -> Result<RoomView, ApiError> {
        let room = self.room_or_404(id)?;
        let Some(gen_) = if_match else {
            return Err(ApiError::new(428, "precondition-required", "If-Match が要ります"));
        };
        if gen_ != room.generation {
            return Err(ApiError::new(412, "precondition-failed", "部屋が変わっています"));
        }
        if let Some(s) = &req.settings {
            s.validate().map_err(|e| ApiError::invalid("設定が不正です", e))?;
            if s.e2ee != room.settings.e2ee && !(room.members.is_empty() && room.remote.is_empty()) {
                return Err(ApiError::new(409, "conflict", "参加者がいる間は e2ee を変えられません"));
            }
            if s.video_codec != room.settings.video_codec && !room.tracks.is_empty() {
                return Err(ApiError::new(409, "conflict", "トラックがある間はコーデックを変えられません"));
            }
        }
        let RoomService { rooms, conns, out, cfg, .. } = self;
        let Some(room) = rooms.get_mut(id) else { return Err(ApiError::not_found()) };
        if let Some(s) = req.settings {
            let spot_changed = s.spotlight != room.settings.spotlight;
            room.settings = s;
            if spot_changed {
                room.next_tick = room.settings.spotlight.as_ref().map(|_| now);
                if room.settings.spotlight.is_none() {
                    room.focus.clear();
                }
                // 参加者のスポットライトの希望を新しい設定に合わせる
                for k in room.members.clone() {
                    if let Some(c) = conns.get_mut(&k) {
                        c.spotlight = room.settings.spotlight.as_ref().map(|sp| signaling::SpotlightRequest {
                            number: c.spotlight.as_ref().map_or(sp.number, |x| x.number.min(sp.number)),
                            focus_rid: sp.focus_rid.clone(),
                            unfocused: Some(sp.unfocused),
                        });
                        for slot in c.slots.iter_mut() {
                            slot.applied = None;
                        }
                    }
                }
            }
        }
        if let Some(m) = req.metadata {
            room.metadata = m;
        }
        room.generation += 1;
        crate::service::rebalance_pub(now, room, conns, out, cfg);
        out.event(now, id, RoomEvent::RoomUpdated { generation: room.generation });
        Ok(room_view(room))
    }

    pub fn api_delete_room(&mut self, now: Instant, id: &str) -> Result<(), ApiError> {
        self.room_or_404(id)?;
        self.destroy_room(now, id, "deleted");
        Ok(())
    }

    fn participant_view(&self, room: &RoomState, k: ConnKey) -> Option<ParticipantView> {
        let c = self.conns.get(&k)?;
        if !matches!(c.phase, Phase::Joined) {
            return None;
        }
        Some(ParticipantView {
            info: participant_info(room, c),
            joined_at: Some(c.joined_at_ms),
            peer_connected: c.peer_connected,
            signaling: match c.path {
                SignalPath::WebSocket => "websocket".into(),
                SignalPath::DataChannel => "datachannel".into(),
            },
            subscriptions: c.slots.iter().map(|s| wire_slot(room, s)).collect(),
        })
    }

    fn remote_view(p: &ParticipantInfo) -> ParticipantView {
        ParticipantView {
            info: p.clone(),
            joined_at: None,
            peer_connected: true,
            signaling: "remote".into(),
            subscriptions: Vec::new(),
        }
    }

    pub fn api_list_participants(&self, room_id: &str, limit: usize, cursor: Option<&str>) -> Result<Page<ParticipantView>, ApiError> {
        let room = self.room_or_404(room_id)?;
        let mut v: Vec<(String, ParticipantView)> = room
            .members
            .iter()
            .filter_map(|k| self.participant_view(room, *k))
            .map(|p| (p.info.connection_id.clone(), p))
            .collect();
        v.extend(room.remote.values().map(|p| (p.connection_id.clone(), Self::remote_view(p))));
        page(v, limit, cursor)
    }

    pub fn api_get_participant(&self, room_id: &str, connection_id: &str) -> Result<ParticipantView, ApiError> {
        let room = self.room_or_404(room_id)?;
        if let Some(p) = room.remote.get(connection_id) {
            return Ok(Self::remote_view(p));
        }
        let k = self.local_member(room, connection_id)?;
        self.participant_view(room, k).ok_or_else(ApiError::not_found)
    }

    fn local_member(&self, room: &RoomState, connection_id: &str) -> Result<ConnKey, ApiError> {
        self.by_connection_id
            .get(connection_id)
            .copied()
            .filter(|k| room.members.contains(k))
            .ok_or_else(ApiError::not_found)
    }

    pub fn api_disconnect_participant(&mut self, now: Instant, room_id: &str, connection_id: &str) -> Result<(), ApiError> {
        let room = self.room_or_404(room_id)?;
        let k = self.local_member(room, connection_id)?;
        self.disconnect(now, k, DisconnectReason::Kicked, None);
        Ok(())
    }

    /// 参加者の metadata を変え、部屋の全員に `connection.updated` を知らせる。
    pub fn api_update_participant(&mut self, now: Instant, room_id: &str, connection_id: &str, req: UpdateParticipant) -> Result<ParticipantView, ApiError> {
        if serde_json::to_string(&req.metadata).map(|s| s.len()).unwrap_or(usize::MAX) > signaling::MAX_METADATA_BYTES {
            return Err(ApiError::invalid("metadata は 8 KiB まで", vec![FieldError::new("metadata", "8 KiB まで")]));
        }
        let room = self.room_or_404(room_id)?;
        let k = self.local_member(room, connection_id)?;
        if let Some(c) = self.conns.get_mut(&k) {
            c.metadata = req.metadata.clone();
        }
        let n = ServerMessage::Notify(Notify::ConnectionUpdated {
            connection_id: connection_id.to_string(),
            metadata: req.metadata.clone(),
        });
        let members = self.rooms.get(room_id).map(|r| r.members.clone()).unwrap_or_default();
        for m in members {
            if let Some(c) = self.conns.get(&m) {
                self.out.send(c, &n);
            }
        }
        self.out.event(
            now,
            room_id,
            RoomEvent::ConnectionUpdated {
                connection_id: connection_id.to_string(),
                metadata: req.metadata,
            },
        );
        let room = self.room_or_404(room_id)?;
        self.participant_view(room, k).ok_or_else(ApiError::not_found)
    }

    /// 部屋・参加者へ送る。
    pub fn api_post_message(&mut self, _now: Instant, room_id: &str, req: PostMessage) -> Result<MessageResult, ApiError> {
        let room = self.room_or_404(room_id)?;
        if serde_json::to_string(&req.data).map(|s| s.len()).unwrap_or(usize::MAX) > signaling::MAX_APP_MESSAGE_BYTES {
            return Err(ApiError::invalid("data は 64 KiB まで", vec![FieldError::new("data", "64 KiB まで")]));
        }
        for t in &req.to {
            if !room.remote.contains_key(t) {
                self.local_member(room, t)?;
            }
        }
        match req.label {
            None => Ok(MessageResult {
                delivered: self.push(room_id, &req.to, req.data) as u32,
            }),
            Some(label) => {
                let Value::String(text) = req.data else {
                    return Err(ApiError::invalid(
                        "DataChannel へ送る data は文字列",
                        vec![FieldError::new("data", "文字列")],
                    ));
                };
                if !label.starts_with('#') {
                    return Err(ApiError::invalid("label は `#` で始まる", vec![FieldError::new("label", "`#` で始まる")]));
                }
                let mut n = 0;
                let mut sends = Vec::new();
                for k in &room.members {
                    let Some(c) = self.conns.get(k) else { continue };
                    if !req.to.is_empty() && !req.to.contains(&c.connection_id) {
                        continue;
                    }
                    if c.dc_ids.values().any(|l| *l == label) {
                        sends.push(*k);
                        n += 1;
                    }
                }
                for k in sends {
                    self.out.actions.push_back(crate::service::Action::SendData {
                        conn: k,
                        label: label.clone(),
                        data: bytes::Bytes::from(text.clone()),
                        binary: false,
                    });
                }
                Ok(MessageResult { delivered: n })
            }
        }
    }

    /// 参加トークンを発行する（`ServiceConfig::token_signer` が要る）。
    pub fn api_issue_token(&mut self, now: Instant, room_id: &str, req: IssueToken) -> Result<TokenView, ApiError> {
        let Some(signer) = self.cfg.token_signer.clone() else {
            return Err(ApiError::new(400, "invalid-config", "トークンの署名鍵が設定されていません"));
        };
        if !valid_room_id(room_id) {
            return Err(ApiError::not_found());
        }
        if !(10..=86_400).contains(&req.ttl_secs) {
            return Err(ApiError::invalid("ttl_secs は 10〜86400", vec![FieldError::new("ttl_secs", "10〜86400")]));
        }
        if req.metadata.as_ref().is_some_and(|m| {
            serde_json::to_string(m).map(|s| s.len()).unwrap_or(usize::MAX) > signaling::MAX_METADATA_BYTES
        }) {
            return Err(ApiError::invalid("metadata は 8 KiB まで", vec![FieldError::new("metadata", "8 KiB まで")]));
        }
        let iat = self.out.unix_ms(now) / 1000;
        let exp = iat + req.ttl_secs as i64;
        let claims = JoinClaims {
            iss: self.cfg.token_issuer.clone(),
            aud: Audience::One(self.cfg.token_audience.clone()),
            exp,
            nbf: None,
            iat: Some(iat),
            jti: Some(self.out.id()),
            sub: None,
            room: room_id.to_string(),
            role: req.role,
            client_id: req.client_id,
            bundle_id: req.bundle_id,
            metadata: req.metadata,
            permissions: req.permissions,
        };
        let token = signer
            .sign(&claims)
            .map_err(|e| ApiError::new(500, "internal-error", e.to_string()))?;
        Ok(TokenView {
            token,
            room_id: room_id.to_string(),
            expires_at: exp,
        })
    }

    fn stats_of(&self, room: &RoomState, now: Instant, k: ConnKey) -> Option<ParticipantStats> {
        let c = self.conns.get(&k)?;
        let published = room
            .tracks
            .values()
            .filter(|t| matches!(&t.origin, Origin::Local { conn, .. } if *conn == k))
            .map(|t| TrackStats {
                track_id: t.info.track_id.clone(),
                kind: t.info.kind,
                layers: room
                    .sfu
                    .layer_bitrates(now, t.sfu_peer, &t.group)
                    .into_iter()
                    .map(|(rid, b)| LayerStats { rid, bitrate_bps: b })
                    .collect(),
            })
            .collect();
        let subscriptions = c
            .slots
            .iter()
            .map(|s| SubscriptionStats {
                mid: s.mid.clone(),
                track_id: s.track.clone(),
                state: wire_slot(room, s).state,
                rid: s
                    .send_track
                    .filter(|_| s.bound.is_some())
                    .and_then(|st| room.sfu.current_layer(k.0, st))
                    .flatten(),
            })
            .collect();
        let p = c.stats.peer;
        Some(ParticipantStats {
            connection_id: c.connection_id.clone(),
            rtt_ms: p.rtt_ms,
            bandwidth_bps: p.bandwidth_bps,
            send_bitrate_bps: p.send_bitrate_bps,
            retransmitted_packets: p.retransmitted,
            nack_requested: p.nack_requested,
            received_bytes: c.stats.received_bytes,
            forwarded_bytes: c.stats.forwarded_bytes,
            forwarded_frames: c.stats.forwarded_aus,
            published,
            subscriptions,
            client: c.stats.client_reports.clone(),
        })
    }

    /// 部屋の統計（このノードの参加者）。
    pub fn api_stats(&self, now: Instant, room_id: &str) -> Result<StatsView, ApiError> {
        let room = self.room_or_404(room_id)?;
        Ok(StatsView {
            room_id: room_id.to_string(),
            at: self.out.unix_ms(now),
            participants: room.members.iter().filter_map(|k| self.stats_of(room, now, *k)).collect(),
        })
    }

    pub fn api_participant_stats(&self, now: Instant, room_id: &str, connection_id: &str) -> Result<ParticipantStats, ApiError> {
        let room = self.room_or_404(room_id)?;
        let k = self.local_member(room, connection_id)?;
        self.stats_of(room, now, k).ok_or_else(ApiError::not_found)
    }

    /// メディアパイプライン（ADR-0016）が部屋の合成・合成の録画を計画するときの検査。E2EE なら 422 と理由。
    pub fn api_plan_composite(&self, room_id: &str) -> Result<(), ApiError> {
        let room = self.room_or_404(room_id)?;
        room.settings
            .compositing_allowed()
            .map_err(|r| ApiError::new(422, "unsupported-codec", r))
    }

    /// 部屋のトラック（API・パイプライン用）。
    pub fn api_tracks(&self, room_id: &str) -> Result<Vec<TrackInfo>, ApiError> {
        Ok(self.room_or_404(room_id)?.tracks.values().map(|t| t.info.clone()).collect())
    }
}
