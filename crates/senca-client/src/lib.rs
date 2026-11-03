//! SENCA の管理 API（`/api/v1`、`docs/api/openapi.yaml`）の非同期（tokio）クライアント。
//!
//! - 型付きのモデル（[`models`]、openapi.yaml から生成）と、資源ごとの操作（`client.streams().create(..)`）
//! - POST には Idempotency-Key を自動で付ける（[`RequestOptions::idempotency_key`] で指定もできる）
//! - 取得・作成・更新は [`Versioned`]（`ETag` と再送の印）を返し、更新は [`IfMatch`] を必ず取る
//! - 一覧は cursor を辿る [`Paginator`]（[`futures_core::Stream`]）
//! - 誤りは problem+json を読んだ [`ApiError`]（[`Error::Api`]）。送り直してよい誤りは指数バックオフで送り直す
//! - SSE（`GET /events`）は [`EventStream`]（`Last-Event-ID` でつなぎ直す）
//! - リアルタイムの部屋（[`rooms`]、`client.rooms()`）と、そのメディアパイプライン（[`media_pipelines`]、`client.media_pipelines()`）
//!
//! HTTP/1.1 は senca-http、TLS は senca-tls（外部の HTTP クライアントに依らない）。
//!
//! ```no_run
//! # async fn f() -> senca_client::Result<()> {
//! use senca_client::{SencaClient, ListOptions, models::{StreamCreate, StreamPatch}};
//! let client = SencaClient::new("http://127.0.0.1:8080", std::env::var("SENCA_TOKEN").unwrap())?;
//! let s = client.streams().create(&StreamCreate { name: "studio-a".into(), ..Default::default() }).await?;
//! let patch = StreamPatch { name: Some("studio-b".into()), ..Default::default() };
//! let s = client.streams().update(&s.id, &patch, &s).await?; // If-Match は前の応答の ETag
//! let mut all = client.streams().list_with(ListOptions::default().limit(50));
//! while let Some(x) = all.next().await {
//!     println!("{}", x?.name);
//! }
//! client.streams().delete(&s.id, s.if_match()).await?;
//! # Ok(()) }
//! ```

pub mod audit;
pub mod client;
pub mod error;
pub mod media_pipelines;
pub mod models;
pub mod ops;
pub mod pagination;
pub mod resources;
pub mod rooms;
pub mod sse;
pub mod transport;

pub use audit::{Audit, AuditQuery};
pub use client::{ClientBuilder, IfMatch, RequestOptions, RetryPolicy, SencaClient, Versioned, new_idempotency_key};
pub use error::{ApiError, Error, Result};
pub use models::ProblemCode;
pub use pagination::{ListOptions, Page, Paginator};
pub use media_pipelines::MediaPipelines;
pub use rooms::Rooms;
pub use resources::{Inputs, Nodes, Outputs, Pipelines, Recordings, Schedules, Secrets, Streams, Tokens};
pub use sse::{EventStream, EventsOptions, ServerEvent, SseEvent, SseParser};

/// `*Patch` の `Option<Option<T>>`（省く・null・値）を serde で読み書きする。
pub(crate) mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(x) => x.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }

    #[cfg(test)]
    mod tests {
        use crate::models::{SnapshotPolicy, StreamPatch};

        #[test]
        fn tri_state() {
            let p = StreamPatch::default();
            assert_eq!(serde_json::to_string(&p).unwrap(), "{}");
            let p = StreamPatch { snapshots: Some(None), ..Default::default() };
            assert_eq!(serde_json::to_string(&p).unwrap(), r#"{"snapshots":null}"#);
            let back: StreamPatch = serde_json::from_str(r#"{"snapshots":null}"#).unwrap();
            assert_eq!(back.snapshots, Some(None));
            let back: StreamPatch = serde_json::from_str("{}").unwrap();
            assert_eq!(back.snapshots, None);
            let _ = SnapshotPolicy::default;
        }
    }
}
