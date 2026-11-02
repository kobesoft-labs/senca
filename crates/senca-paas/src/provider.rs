//! 連携先の共通の形（[`PaasProvider`]・[`Op`]）と、作った受け口（[`CreatedResource`]）。

use crate::call::{HttpCall, HttpReply};
use crate::error::{ErrorKind, PaasError};
use crate::secret::Secret;
use std::collections::BTreeMap;

/// SENCA が作った資源に付けるタグ（Cloudflare は `meta`）。値は出力 ID。これが無い資源は決して消さない。
pub const TAG_OWNER: &str = "senca:owner";
/// SENCA のクラスタの識別子（同じアカウントを複数のクラスタで使うとき、互いの資源を消さないため）。
pub const TAG_CLUSTER: &str = "senca:cluster";
/// 出力の generation（作り直しの判別と監査用）。
pub const TAG_GENERATION: &str = "senca:generation";

/// 1 つの操作が送る要求の数の上限（一覧のページ送りを含む）。
pub const MAX_STEPS: usize = 256;

/// 操作の一歩。
#[derive(Debug)]
pub enum Step<T> {
    /// この要求を送り、応答を次の [`Op::step`] に渡す。`checkpoint` があれば、送る **前に**
    /// `provider_resource` に記録する（途中で落ちても作りかけの資源を見つけて片付けられるように）。
    Call { call: HttpCall, checkpoint: Option<String> },
    /// 終わり。
    Done(T),
}

/// 複数の要求からなる操作（sans-io の状態機械）。最初は `reply = None` で呼び、以後は直前の
/// [`Step::Call`] の応答を渡す。`now_unix` は署名に使う現在時刻（秒）。
pub trait Op<T>: Send {
    fn step(&mut self, reply: Option<&HttpReply>, now_unix: u64) -> Result<Step<T>, PaasError>;
}

/// 作る受け口の指定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSpec {
    /// 持ち主の出力 ID（`senca:owner` タグの値）。
    pub owner: String,
    /// 連携先に見せる名前（英数字・`-`・`_` に直して使う）。
    pub name: String,
    /// 二重作成を防ぐ鍵（Idempotency-Key。MediaLive の `requestId` に使う）。同じ出力の同じ generation で同じ値にする。
    pub idempotency_key: String,
    /// 追加のタグ（`senca:cluster`・`senca:generation` など）。`senca:owner` は自動で付ける。
    pub tags: BTreeMap<String, String>,
}

impl CreateSpec {
    /// 出力から作る。名前は `senca-<出力 ID>`、冪等キーは `<出力 ID>-<generation>`。
    pub fn for_output(output_id: &str, generation: u64, cluster: Option<&str>) -> Self {
        let mut tags = BTreeMap::new();
        tags.insert(TAG_GENERATION.to_string(), generation.to_string());
        if let Some(c) = cluster {
            tags.insert(TAG_CLUSTER.to_string(), c.to_string());
        }
        CreateSpec {
            owner: output_id.to_string(),
            name: format!("senca-{output_id}"),
            idempotency_key: format!("{output_id}-{generation}"),
            tags,
        }
    }

    /// `senca:owner` を含むすべてのタグ。
    pub fn all_tags(&self) -> BTreeMap<String, String> {
        let mut t = self.tags.clone();
        t.insert(TAG_OWNER.to_string(), self.owner.clone());
        t
    }

    /// 連携先の名前に使える形（英数字・`-`・`_`、128 文字まで）。
    pub fn safe_name(&self) -> String {
        self.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(128).collect()
    }

    pub(crate) fn validate(&self) -> Result<(), PaasError> {
        let ok = |s: &str| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| !c.is_control());
        if !ok(&self.owner) || !ok(&self.idempotency_key) || self.tags.iter().any(|(k, v)| !ok(k) || v.len() > 256) {
            return Err(PaasError::config("CreateSpec の owner・idempotency_key・tags が不正"));
        }
        Ok(())
    }
}

/// 送り方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestProtocol {
    Rtmp,
    Rtmps,
    Srt,
    Whip,
}

impl IngestProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            IngestProtocol::Rtmp => "rtmp",
            IngestProtocol::Rtmps => "rtmps",
            IngestProtocol::Srt => "srt",
            IngestProtocol::Whip => "whip",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "rtmp" => IngestProtocol::Rtmp,
            "rtmps" => IngestProtocol::Rtmps,
            "srt" => IngestProtocol::Srt,
            "whip" => IngestProtocol::Whip,
            _ => return None,
        })
    }
}

/// 受け口（送り先）。`url` は秘密を含まない（状態表示に出してよい）。秘密は別の欄に分ける。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingest {
    pub protocol: IngestProtocol,
    /// 秘密を含まない送り先（`rtmps://host:443/app`・`srt://host:port`・WHIP なら origin）。
    pub url: String,
    /// RTMP の stream key。WHIP では URL 全体（URL そのものが鍵になるため）。
    pub stream_key: Option<Secret>,
    /// SRT の streamid（連携先によっては鍵を兼ねるので秘密として扱う）。
    pub stream_id: Option<Secret>,
    /// SRT の passphrase。
    pub passphrase: Option<Secret>,
}

/// SENCA の送出 runner（`rtmp_push`・`srt_push`・`whip_push`）に渡す形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// `Output.protocol` に相当する runner の名前。
    pub runner: &'static str,
    /// runner に渡す URL。streamid などの秘密を含みうるので [`Secret`]。
    pub url: Secret,
    /// RTMP なら stream key、SRT なら passphrase。
    pub credential: Option<Secret>,
}

impl Ingest {
    /// runner に渡す形にする。
    /// - RTMP(S): url = `rtmp(s)://host:port/app`、credential = stream key
    /// - SRT: url = `srt://host:port?streamid=…`、credential = passphrase
    /// - WHIP: url = WHIP の URL 全体、credential なし
    pub fn push_target(&self) -> PushTarget {
        match self.protocol {
            IngestProtocol::Rtmp | IngestProtocol::Rtmps => PushTarget {
                runner: "rtmp_push",
                url: Secret::new(self.url.trim_end_matches('/')),
                credential: self.stream_key.clone(),
            },
            IngestProtocol::Srt => {
                let mut u = self.url.clone();
                if let Some(id) = &self.stream_id {
                    u.push(if u.contains('?') { '&' } else { '?' });
                    u.push_str("streamid=");
                    u.push_str(&crate::call::uri_encode(id.expose(), false));
                }
                PushTarget { runner: "srt_push", url: Secret::new(u), credential: self.passphrase.clone() }
            }
            IngestProtocol::Whip => PushTarget {
                runner: "whip_push",
                url: self.stream_key.clone().unwrap_or_else(|| Secret::new(self.url.clone())),
                credential: None,
            },
        }
    }
}

/// 視聴の URL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playback {
    /// "hls"・"dash"・"whep"
    pub kind: &'static str,
    pub url: String,
}

/// 作った（または取得した）受け口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedResource {
    /// `Output.provider_resource` に記録する ID。MediaLive は `input=<id>;sg=<id>` の複合（`;` で区切る）。
    pub resource_id: String,
    /// 送り先（連携先の推奨順）。
    pub ingest: Vec<Ingest>,
    pub playback: Vec<Playback>,
    /// `senca:owner` タグの値（取得時）。
    pub owner: Option<String>,
}

impl CreatedResource {
    /// 指定の送り方の受け口（無ければ先頭）。
    pub fn ingest_for(&self, protocol: Option<IngestProtocol>) -> Option<&Ingest> {
        match protocol {
            Some(p) => self.ingest.iter().find(|i| i.protocol == p),
            None => self.ingest.first(),
        }
    }
}

/// 削除の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    /// 既に無かった（冪等な削除）。
    AlreadyGone,
}

/// 一覧で見つけた資源（タグ付きのものだけ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedResource {
    pub resource_id: String,
    pub owner: String,
    pub cluster: Option<String>,
    /// 作り終えた資源か（MediaLive の security group だけが残ったものは `false`）。
    pub complete: bool,
}

/// 連携先ごとの対応範囲（`GET /capabilities` に出す。REQ-001 の 5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub provider: &'static str,
    pub ingest: &'static [IngestProtocol],
    pub video_codecs: &'static [&'static str],
    pub audio_codecs: &'static [&'static str],
}

/// 連携先のアダプター（REQ-001 の `PaasProvider`）。どの操作も [`Op`] を返す。
pub trait PaasProvider: Send + Sync {
    /// `Output.provider` の値。
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// 受け口を作る（タグ付き）。
    fn create(&self, spec: &CreateSpec) -> Box<dyn Op<CreatedResource>>;
    /// 受け口を取得する（送り先と鍵を含む）。無ければ `None`。
    fn get(&self, resource_id: &str) -> Box<dyn Op<Option<CreatedResource>>>;
    /// 受け口を消す。無ければ `AlreadyGone`（冪等）。
    fn delete(&self, resource_id: &str) -> Box<dyn Op<DeleteOutcome>>;
    /// `senca:owner` タグを持つ資源の一覧（`owner` を指定すればその出力の分だけ）。ページ送りは中で行う。
    fn list_by_tag(&self, owner: Option<&str>) -> Box<dyn Op<Vec<TaggedResource>>>;
    /// 記録した資源 ID が作りかけか（[`crate::reconcile::Desired::partial`] に使う）。
    fn is_partial(&self, _resource_id: &str) -> bool {
        false
    }
}

/// 同期の送信関数で操作を最後まで回す。`checkpoint` は要求を送る前に呼ばれる（記録が失敗したら `Err` を返すこと）。
/// 非同期の呼び出し側は同じ手順を自分のループで行えばよい。
pub fn drive<T>(
    op: &mut dyn Op<T>,
    mut now_unix: impl FnMut() -> u64,
    mut send: impl FnMut(&HttpCall) -> Result<HttpReply, PaasError>,
    mut checkpoint: impl FnMut(&str) -> Result<(), PaasError>,
) -> Result<T, PaasError> {
    let mut reply: Option<HttpReply> = None;
    for _ in 0..MAX_STEPS {
        match op.step(reply.as_ref(), now_unix())? {
            Step::Done(v) => return Ok(v),
            Step::Call { call, checkpoint: cp } => {
                if let Some(cp) = cp {
                    checkpoint(&cp)?;
                }
                reply = Some(send(&call)?);
            }
        }
    }
    Err(PaasError::malformed("要求の数が上限を超えた"))
}

/// 連携先を作る。`credential` は秘密値（Cloudflare は `account_id:api_token`、AWS は
/// `access_key_id:secret_access_key[:session_token]`）、`options` は `Output.options`。
///
/// 共通の options: `endpoint`（`http(s)://host[:port]`。試験や VPC endpoint 向けの上書き）。
/// 連携先ごとの options は各 module を参照。
pub fn provider_for(
    provider: &str,
    credential: &Secret,
    options: &BTreeMap<String, String>,
) -> Result<Box<dyn PaasProvider>, PaasError> {
    match provider {
        crate::cloudflare::NAME => Ok(Box::new(crate::cloudflare::CloudflareStream::from_options(credential, options)?)),
        crate::ivs::NAME => Ok(Box::new(crate::ivs::Ivs::from_options(credential, options)?)),
        crate::medialive::NAME => Ok(Box::new(crate::medialive::MediaLive::from_options(credential, options)?)),
        _ => Err(PaasError::config("provider は cloudflare_stream・aws_ivs・aws_medialive")),
    }
}

/// 一歩目以外で応答が無い、一歩目で応答がある、など呼び出し側の誤り。
pub(crate) fn misuse() -> PaasError {
    PaasError::new(ErrorKind::Malformed, "Op の呼び出し順が不正")
}

/// 応答を取り出す（無ければ呼び出し順の誤り）。
pub(crate) fn need(reply: Option<&HttpReply>) -> Result<&HttpReply, PaasError> {
    reply.ok_or_else(misuse)
}

/// 終わった操作（`Done` の後にまた呼ばれたとき）。
pub(crate) fn finished() -> PaasError {
    PaasError::new(ErrorKind::Malformed, "Op は終わっている")
}

/// JSON の文字列を取り出す。
pub(crate) fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

/// 同じ答えを 1 回だけ返す操作（要求が要らない場合）。
pub(crate) struct Ready<T>(pub Option<Result<T, PaasError>>);

impl<T: Send> Op<T> for Ready<T> {
    fn step(&mut self, _reply: Option<&HttpReply>, _now: u64) -> Result<Step<T>, PaasError> {
        self.0.take().ok_or_else(finished)?.map(Step::Done)
    }
}

/// 要求 1 回で終わる操作。要求は署名の時刻が決まってから組み立てる。
type BuildFn = Box<dyn FnOnce(u64) -> Result<HttpCall, PaasError> + Send>;
type ParseFn<T> = Box<dyn FnOnce(&HttpReply) -> Result<T, PaasError> + Send>;

pub(crate) struct OneShot<T> {
    build: Option<BuildFn>,
    parse: Option<ParseFn<T>>,
}

impl<T> OneShot<T> {
    pub(crate) fn new(
        build: impl FnOnce(u64) -> Result<HttpCall, PaasError> + Send + 'static,
        parse: impl FnOnce(&HttpReply) -> Result<T, PaasError> + Send + 'static,
    ) -> Self {
        OneShot { build: Some(Box::new(build)), parse: Some(Box::new(parse)) }
    }
}

impl<T: Send> Op<T> for OneShot<T> {
    fn step(&mut self, reply: Option<&HttpReply>, now: u64) -> Result<Step<T>, PaasError> {
        match reply {
            None => {
                let b = self.build.take().ok_or_else(misuse)?;
                Ok(Step::Call { call: b(now)?, checkpoint: None })
            }
            Some(r) => {
                if self.build.is_some() {
                    return Err(misuse());
                }
                let p = self.parse.take().ok_or_else(finished)?;
                p(r).map(Step::Done)
            }
        }
    }
}

/// 連携先が返した ID を path に使ってよいか（英数字と `-`・`_`・`:`・`/`・`.`、256 文字まで）。
pub(crate) fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && s.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':' | b'/' | b'.'))
}

/// タグの対応表を (owner, cluster) にする。
pub(crate) fn owner_of(tags: Option<&serde_json::Value>) -> Option<(String, Option<String>)> {
    let t = tags?.as_object()?;
    let owner = t.get(TAG_OWNER)?.as_str()?.to_string();
    let cluster = t.get(TAG_CLUSTER).and_then(|c| c.as_str()).map(str::to_string);
    Some((owner, cluster))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_for_output() {
        let s = CreateSpec::for_output("out_1/x", 3, Some("c1"));
        assert_eq!(s.idempotency_key, "out_1/x-3");
        assert_eq!(s.safe_name(), "senca-out_1-x");
        let t = s.all_tags();
        assert_eq!(t[TAG_OWNER], "out_1/x");
        assert_eq!(t[TAG_CLUSTER], "c1");
        assert_eq!(t[TAG_GENERATION], "3");
        assert!(s.validate().is_ok());
        assert!(CreateSpec { owner: String::new(), ..s }.validate().is_err());
    }

    #[test]
    fn push_targets() {
        let i = Ingest {
            protocol: IngestProtocol::Rtmps,
            url: "rtmps://live.example.com:443/live/".into(),
            stream_key: Some(Secret::new("KEY")),
            stream_id: None,
            passphrase: None,
        };
        let p = i.push_target();
        assert_eq!((p.runner, p.url.expose(), p.credential.unwrap().expose()), ("rtmp_push", "rtmps://live.example.com:443/live", "KEY"));
        let i = Ingest {
            protocol: IngestProtocol::Srt,
            url: "srt://live.example.com:778".into(),
            stream_key: None,
            stream_id: Some(Secret::new("abc#1")),
            passphrase: Some(Secret::new("PASS")),
        };
        let p = i.push_target();
        assert_eq!(p.runner, "srt_push");
        assert_eq!(p.url.expose(), "srt://live.example.com:778?streamid=abc%231");
        assert_eq!(p.credential.unwrap().expose(), "PASS");
        assert!(!format!("{i:?}").contains("PASS"));
    }

    #[test]
    fn unknown_provider() {
        let e = provider_for("nope", &Secret::new("a:b"), &BTreeMap::new()).err().unwrap();
        assert_eq!(e.kind, ErrorKind::Config);
    }
}
