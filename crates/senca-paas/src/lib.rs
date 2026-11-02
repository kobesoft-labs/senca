//! クラウドのメディア PaaS との連携（REQ-001）。sans-io: HTTP 要求を組み立て、応答を解釈するだけで、
//! 通信・スレッド・時計は持たない（時刻は引数で渡す）。
//!
//! # 方針（REQ-001「設計の方針」）
//!
//! - 連携先は **出入口** として扱う。ここで作るのは外部の受け口（Cloudflare Stream の Live Input、
//!   Amazon IVS の Channel、AWS Elemental MediaLive の Input）だけで、送出そのものは SENCA 自前の
//!   RTMP(S)・SRT・WHIP の実装が行う（[`Ingest::push_target`]）。
//! - **作ったものは SENCA が片付ける。** 作る資源にはすべて `senca:owner=<output id>` のタグ
//!   （Cloudflare は `meta`）を付ける。タグを先に付けて作り、資源 ID を `provider_resource` に記録し、
//!   記録されていないがタグを持つ資源は孤児として消す。タグの無い資源は決して消さない（[`reconcile`]）。
//! - **二重作成を防ぐ。** 外部 API の多くは冪等キーを持たないため、作る前に [`PaasProvider::list_by_tag`]
//!   で同じ owner の資源を探して引き取る（[`reconcile::Action::Adopt`]）。MediaLive の CreateInput には
//!   `requestId` として Idempotency-Key を渡す。
//! - **秘密を出さない。** API トークン・secret access key・stream key・passphrase は [`Secret`] に包み、
//!   `Debug`・エラー・ログに出さない。[`HttpCall`]・[`HttpReply`] の `Debug` も秘密の header と body を伏せる。
//!
//! # 使い方
//!
//! [`provider_for`] で連携先を作り、`create`・`get`・`delete`・`list_by_tag` が返す [`Op`] を
//! [`drive`]（同期の送信関数を渡す）か、呼び出し側の非同期ループで回す。`Op::step` は
//! 前の応答を受け取り、次に送る [`HttpCall`] か結果を返す。
//!
//! 所有権（仕様 8.2）: 外部に副作用を出す操作（create・delete）は、その stream の owner が
//! 最新の fencing token を持つときだけ呼ぶこと。この crate は判断材料（[`reconcile`]）だけを返す。

mod aws;
pub mod call;
pub mod cloudflare;
pub mod error;
pub mod ivs;
pub mod medialive;
pub mod mock;
pub mod provider;
pub mod reconcile;
pub mod secret;
pub mod sigv4;

pub use call::{Endpoint, HttpCall, HttpReply, ReplyReader, Scheme};
pub use error::{ErrorKind, PaasError};
pub use provider::{
    Capabilities, CreateSpec, CreatedResource, DeleteOutcome, Ingest, IngestProtocol, Op, PaasProvider, Playback, PushTarget,
    Step, TAG_CLUSTER, TAG_GENERATION, TAG_OWNER, TaggedResource, drive, provider_for,
};
pub use reconcile::{Action, DeleteReason, Desired, reconcile};
pub use secret::Secret;
pub use sigv4::AwsCredentials;

/// 対応する連携先（`Output.provider` の値）。
pub const PROVIDERS: &[&str] = &[cloudflare::NAME, ivs::NAME, medialive::NAME];
