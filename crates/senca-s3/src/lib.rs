//! S3 互換のオブジェクトストレージ（AWS S3・MinIO・Cloudflare R2 など）の sans-io の部品。I/O・時計を持たない。
//!
//! - [`sigv4`] … AWS Signature Version 4 の署名（HMAC-SHA256 は RustCrypto）。
//! - [`request`] … 宛先（パス形式・仮想ホスト形式）、保存先の場所（`s3://bucket/prefix`）、署名付きの要求の組み立て。
//! - [`responses`] … ListObjectsV2・誤り・分割アップロードの応答（XML）、`Content-Range`、部分の分け方。
//! - [`xml`] … 上限付きの小さな XML の読み手。
//!
//! 送受信は呼び出し側（`senca-node` の HTTP クライアント）が行う。

pub mod request;
pub mod responses;
pub mod sigv4;
pub mod xml;

pub use request::{Endpoint, Location, Op, Prepared, prepare};
pub use responses::{ErrorResponse, ListObjectsV2, Object, ResponseError};
pub use sigv4::{Credentials, EMPTY_SHA256, UNSIGNED_PAYLOAD, sha256_hex};
