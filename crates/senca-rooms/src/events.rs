//! 部屋のイベント。SSE（`/api/v1/events`）と Webhook に同じ形で出す（ADR-0015）。
//!
//! ```json
//! {"id":"01h…","version":1,"at":1767222000123,"node":"node-a","room_id":"daily",
//!  "type":"connection.created","data":{"participant":{…}}}
//! ```
//!
//! `id` はイベントごとに一意（受け手は重複を除く）。`at` は UNIX ミリ秒。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::RoomSettings;
use crate::signaling::{DisconnectReason, ParticipantInfo, TrackInfo};

/// イベントの中身（`type` と `data`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum RoomEvent {
    #[serde(rename = "room.created")]
    RoomCreated { settings: Box<RoomSettings>, auto_created: bool },
    #[serde(rename = "room.updated")]
    RoomUpdated { generation: u64 },
    #[serde(rename = "room.destroyed")]
    RoomDestroyed { reason: String },
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
        duration_ms: u64,
    },
    #[serde(rename = "track.published")]
    TrackPublished { track: TrackInfo },
    #[serde(rename = "track.unpublished")]
    TrackUnpublished { track_id: String, connection_id: String },
    #[serde(rename = "spotlight.changed")]
    SpotlightChanged { focused: Vec<String> },
}

impl RoomEvent {
    pub fn type_name(&self) -> &'static str {
        match self {
            RoomEvent::RoomCreated { .. } => "room.created",
            RoomEvent::RoomUpdated { .. } => "room.updated",
            RoomEvent::RoomDestroyed { .. } => "room.destroyed",
            RoomEvent::ConnectionCreated { .. } => "connection.created",
            RoomEvent::ConnectionUpdated { .. } => "connection.updated",
            RoomEvent::ConnectionDestroyed { .. } => "connection.destroyed",
            RoomEvent::TrackPublished { .. } => "track.published",
            RoomEvent::TrackUnpublished { .. } => "track.unpublished",
            RoomEvent::SpotlightChanged { .. } => "spotlight.changed",
        }
    }
}

/// 封筒（SSE の `data:` と Webhook の本文）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: String,
    pub version: u32,
    /// UNIX ミリ秒。
    pub at: i64,
    pub node: String,
    pub room_id: String,
    #[serde(flatten)]
    pub event: RoomEvent,
}

impl EventEnvelope {
    /// SSE の frame（`id:`・`event:`・`data:`）。
    pub fn to_sse(&self) -> String {
        format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            self.id,
            self.event.type_name(),
            serde_json::to_string(self).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;
    use serde_json::json;

    #[test]
    fn envelope_shape_and_round_trip() {
        let e = EventEnvelope {
            id: "e1".into(),
            version: 1,
            at: 1_767_222_000_123,
            node: "n1".into(),
            room_id: "daily".into(),
            event: RoomEvent::ConnectionCreated {
                participant: ParticipantInfo {
                    connection_id: "c1".into(),
                    client_id: Some("alice".into()),
                    bundle_id: "c1".into(),
                    role: Role::Sendrecv,
                    metadata: json!({"name": "Alice"}),
                    tracks: vec![],
                    node: None,
                },
            },
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "connection.created");
        assert_eq!(v["data"]["participant"]["client_id"], "alice");
        assert_eq!(v["room_id"], "daily");
        let back: EventEnvelope = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
        let sse = e.to_sse();
        assert!(sse.starts_with("id: e1\nevent: connection.created\ndata: {"));
        assert!(sse.ends_with("\n\n"));
    }
}
