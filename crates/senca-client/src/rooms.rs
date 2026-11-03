//! リアルタイムの部屋の REST（`/rooms`、ADR-0015、`docs/realtime/signaling.md` 11 章）。
//! 型は openapi.yaml からの生成物（[`crate::models`] の `Room`・`RoomCreate`・`RoomPatch`・`Participant` など）。

use crate::client::{IfMatch, RequestOptions, SencaClient, Versioned, encode_segment as seg};
use crate::error::Result;
use crate::models::{
    Participant, ParticipantPatch, Room, RoomCreate, RoomMessage, RoomMessageResult, RoomPatch, RoomStats, RoomToken, RoomTokenRequest,
};
use crate::pagination::{ListOptions, Page, Paginator};

/// 部屋（`/rooms`）。
#[derive(Debug, Clone)]
pub struct Rooms(pub(crate) SencaClient);

impl Rooms {
    /// 作る（`createRoom`）。
    pub async fn create(&self, body: &RoomCreate) -> Result<Versioned<Room>> {
        self.create_with(body, RequestOptions::default()).await
    }
    pub async fn create_with(&self, body: &RoomCreate, opts: RequestOptions) -> Result<Versioned<Room>> {
        self.0.op_post("/rooms".into(), Some(body), opts).await
    }
    /// 一覧（`listRooms`）。
    pub fn list(&self) -> Paginator<Room> {
        self.list_with(ListOptions::default())
    }
    pub fn list_with(&self, opts: ListOptions) -> Paginator<Room> {
        self.0.op_list("/rooms".into(), opts)
    }
    pub async fn list_page(&self, opts: &ListOptions) -> Result<Page<Room>> {
        self.0.op_page("/rooms".into(), opts).await
    }
    /// 取得（`getRoom`、ETag つき）。
    pub async fn get(&self, id: &str) -> Result<Versioned<Room>> {
        self.0.op_get(format!("/rooms/{}", seg(id))).await
    }
    /// 更新（`patchRoom`、If-Match 必須）。
    pub async fn update(&self, id: &str, patch: &RoomPatch, if_match: impl Into<IfMatch>) -> Result<Versioned<Room>> {
        self.0.op_patch(format!("/rooms/{}", seg(id)), patch, if_match.into()).await
    }
    /// 消す（`deleteRoom`。参加者は `room_deleted` で切れる）。
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/rooms/{}", seg(id)), None).await
    }
    /// 参加トークンを発行する（`issueRoomToken`。`token` はこの応答でだけ渡る）。
    pub async fn issue_token(&self, room: &str, req: &RoomTokenRequest) -> Result<Versioned<RoomToken>> {
        self.0.op_post(format!("/rooms/{}/tokens", seg(room)), Some(req), RequestOptions::default()).await
    }
    /// 部屋・参加者へ送る（`postRoomMessage`。`label` があれば DataChannel、無ければ `push`）。
    pub async fn send_message(&self, room: &str, msg: &RoomMessage) -> Result<RoomMessageResult> {
        self.0.op_post(format!("/rooms/{}/messages", seg(room)), Some(msg), RequestOptions::default()).await.map(|v| v.value)
    }
    /// 統計（`getRoomStats`）。
    pub async fn stats(&self, room: &str) -> Result<RoomStats> {
        self.0.op_get(format!("/rooms/{}/stats", seg(room))).await.map(|v| v.value)
    }
    /// 参加者の一覧（`listRoomParticipants`）。
    pub fn participants(&self, room: &str) -> Paginator<Participant> {
        self.participants_with(room, ListOptions::default())
    }
    pub fn participants_with(&self, room: &str, opts: ListOptions) -> Paginator<Participant> {
        self.0.op_list(format!("/rooms/{}/participants", seg(room)), opts)
    }
    /// 参加者（`getRoomParticipant`）。
    pub async fn participant(&self, room: &str, connection_id: &str) -> Result<Participant> {
        self.0.op_get(format!("/rooms/{}/participants/{}", seg(room), seg(connection_id))).await.map(|v| v.value)
    }
    /// 参加者の metadata を変える（`patchRoomParticipant`。他の参加者に `connection.updated`）。
    pub async fn update_participant(&self, room: &str, connection_id: &str, patch: &ParticipantPatch) -> Result<Participant> {
        self.0
            .json(crate::client::Call::new(senca_http::Method::Patch, format!("/rooms/{}/participants/{}", seg(room), seg(connection_id))).json(patch)?)
            .await
            .map(|v| v.value)
    }
    /// 切断する（`disconnectRoomParticipant`、理由は `kicked`）。
    pub async fn disconnect(&self, room: &str, connection_id: &str) -> Result<()> {
        self.0.op_delete(format!("/rooms/{}/participants/{}", seg(room), seg(connection_id)), None).await
    }
}
