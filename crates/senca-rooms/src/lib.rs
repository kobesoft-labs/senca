//! リアルタイムの部屋（ADR-0015）の sans-io の核。I/O・スレッドを持たず、時刻は引数で受ける。
//!
//! ノード（呼び出し側）は WebSocket・UDP・HTTP を持ち、入力を [`RoomService`] に入れて [`Action`] を実行する。
//! ピア接続（senca-webrtc）を持つ部分は [`host::PeerHost`] にまとめてあり、そのまま使える。
//!
//! - [`signaling`] — シグナリング v1（WebSocket の JSON、接続後は DataChannel `signaling` へ切り替えられる）
//! - [`service`] — 部屋・参加者の状態機械（[`RoomService`]）と出力（[`Action`]）
//! - [`spotlight`] — 話者の検出（ヒステリシス）とスポットライト
//! - [`filter`] — 転送フィルター（購読する相手・トラックを規則で絞る）
//! - [`token`] — 参加トークン（JWT 互換、EdDSA・ES256。HS 系・none は拒否）
//! - [`webhook`] — 認証 Webhook とイベント Webhook（HMAC-SHA256 の署名、再送の予定）
//! - [`events`] — SSE と Webhook に同じ形で出すイベント
//! - [`api`] — REST の資源（`/rooms`・`/rooms/{id}/participants`・`/rooms/{id}/messages`・stats・tokens）
//! - [`model`] — 役割・部屋の設定などの共通の型
//! - [`host`] — [`Action`] を senca-webrtc の [`PeerConnection`](senca_webrtc::PeerConnection) で実行する部品
//!
//! 仕様は `docs/realtime/signaling.md`。

pub mod api;
pub mod events;
pub mod filter;
pub mod host;
pub mod model;
pub mod service;
pub mod signaling;
pub mod spotlight;
pub mod token;
pub mod webhook;

mod ids;

#[cfg(test)]
mod service_tests;

pub use api::ApiError;
pub use events::{EventEnvelope, RoomEvent};
pub use filter::ForwardingFilter;
pub use host::PeerHost;
pub use model::{Kind, Role, RoomSettings};
pub use service::{Action, ConnKey, RelayAction, RoomService, ServiceConfig};
pub use signaling::{ClientMessage, ServerMessage};
pub use token::{JoinClaims, Permissions, TokenSigner, TokenVerifier};
