//! 資源ごとの操作（`client.streams().create(...)` の形）。各操作は openapi.yaml の operationId と 1 対 1
//! （対応表は [`crate::ops::OPERATIONS`]）。

use bytes::Bytes;
use senca_http::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::client::{Call, IfMatch, RequestOptions, SencaClient, Versioned, encode_segment as seg};
use crate::error::{Error, Result};
use crate::models::*;
use crate::pagination::{ListOptions, Page, Paginator};
use crate::sse::{EventStream, EventsOptions};

// ------------------------------------------------------------------ 共通の部品

impl SencaClient {
    pub(crate) async fn op_get<T: DeserializeOwned>(&self, path: String) -> Result<Versioned<T>> {
        self.json(Call::new(Method::Get, path)).await
    }

    pub(crate) async fn op_post<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: String,
        body: Option<&B>,
        opts: RequestOptions,
    ) -> Result<Versioned<T>> {
        let mut c = Call::new(Method::Post, path).opts(opts);
        if let Some(b) = body {
            c = c.json(b)?;
        }
        self.json(c).await
    }

    pub(crate) async fn op_patch<B: Serialize + ?Sized, T: DeserializeOwned>(&self, path: String, body: &B, m: IfMatch) -> Result<Versioned<T>> {
        self.json(Call::new(Method::Patch, path).json(body)?.if_match(Some(m))).await
    }

    pub(crate) async fn op_delete(&self, path: String, m: Option<IfMatch>) -> Result<()> {
        self.empty(Call::new(Method::Delete, path).if_match(m)).await
    }

    pub(crate) fn op_list<T: DeserializeOwned + Send + 'static>(&self, path: String, opts: ListOptions) -> Paginator<T> {
        Paginator::new(self.clone(), path, opts)
    }

    pub(crate) async fn op_page<T: DeserializeOwned>(&self, path: String, opts: &ListOptions) -> Result<Page<T>> {
        self.op_page_as(path, opts).await
    }

    /// 一覧の 1 頁を任意の型で読む（`items`・`next_cursor` のほかにフィールドがある一覧）。
    pub(crate) async fn op_page_as<T: DeserializeOwned>(&self, path: String, opts: &ListOptions) -> Result<T> {
        self.json(opts.apply(Call::new(Method::Get, path))).await.map(|v| v.value)
    }

    // -------------------------------------------------------------- 入口

    pub fn streams(&self) -> Streams {
        Streams(self.clone())
    }
    pub fn recordings(&self) -> Recordings {
        Recordings(self.clone())
    }
    pub fn schedules(&self) -> Schedules {
        Schedules(self.clone())
    }
    pub fn tokens(&self) -> Tokens {
        Tokens(self.clone())
    }
    pub fn inputs(&self) -> Inputs {
        Inputs(self.clone())
    }
    pub fn outputs(&self) -> Outputs {
        Outputs(self.clone())
    }
    pub fn secrets(&self) -> Secrets {
        Secrets(self.clone())
    }
    pub fn pipelines(&self) -> Pipelines {
        Pipelines(self.clone())
    }
    pub fn nodes(&self) -> Nodes {
        Nodes(self.clone())
    }
    /// リアルタイムの部屋（`/rooms`）。
    pub fn rooms(&self) -> crate::rooms::Rooms {
        crate::rooms::Rooms(self.clone())
    }
    /// 部屋のメディアパイプライン（書き出し・録画・合成・送り出し・文字起こし）。
    pub fn media_pipelines(&self) -> crate::media_pipelines::MediaPipelines {
        crate::media_pipelines::MediaPipelines(self.clone())
    }
    /// 監査ログ（`/audit`、読み出しだけ）。
    pub fn audit(&self) -> crate::audit::Audit {
        crate::audit::Audit(self.clone())
    }

    /// 能力（`GET /capabilities`）。
    pub async fn capabilities(&self) -> Result<Capabilities> {
        self.op_get("/capabilities".into()).await.map(|v| v.value)
    }

    /// グリッド（ノードの資源・装置・仕事の配置、`GET /grid`）。
    pub async fn grid(&self) -> Result<Grid> {
        self.op_get("/grid".into()).await.map(|v| v.value)
    }

    /// 全ノードのハードウェア（`GET /devices`）。
    pub async fn devices(&self) -> Result<Devices> {
        self.op_get("/devices".into()).await.map(|v| v.value)
    }

    /// イベントの購読（SSE、`GET /events`）。
    ///
    /// ```no_run
    /// # async fn f(client: senca_client::SencaClient) {
    /// use senca_client::{EventsOptions, ServerEvent};
    /// let mut ev = client.events(EventsOptions::default().types(["stream.*"]));
    /// while let Some(Ok(e)) = ev.next().await {
    ///     match e {
    ///         ServerEvent::Event(e) => println!("{} {}", e.event, e.data.resource_id),
    ///         ServerEvent::Resync { .. } => { /* 一覧を読み直す */ }
    ///     }
    /// }
    /// # }
    /// ```
    pub fn events(&self, opts: EventsOptions) -> EventStream {
        crate::sse::start(self.clone(), opts)
    }

    /// 稼働確認（`GET /healthz`、認証なし）。
    pub async fn healthz(&self) -> Result<()> {
        let mut c = Call::new(Method::Get, "/healthz");
        c.outside_api = true;
        c.accept = "text/plain";
        self.empty(c).await
    }

    /// 受け付けられるか（`GET /readyz`、認証なし）。
    pub async fn readyz(&self) -> Result<()> {
        let mut c = Call::new(Method::Get, "/readyz");
        c.outside_api = true;
        c.accept = "text/plain";
        self.empty(c).await
    }

    /// 署名付きの再生パス（`/s/{exp}/{sig}/live|vod/...`）を読む。`url` は再生 URL の応答の絶対 URL か、`/s/` で始まるパス。
    /// 認証ヘッダーは付けない。307（データを持つ別のノードへの案内）は 1 回だけ辿る。
    pub async fn fetch_signed(&self, url: &str) -> Result<Bytes> {
        let mut target = url.to_string();
        for _ in 0..2 {
            let mut c = Call::new(Method::Get, target.clone());
            if target.starts_with('/') {
                if !target.starts_with("/s/") {
                    return Err(Error::InvalidArgument("署名付きのパスは /s/ で始まる".into()));
                }
                c.outside_api = true;
            }
            c.accept = "*/*";
            let resp = self.open(&c, &[], self.inner.timeout).await?;
            if resp.status == 307
                && let Some(loc) = resp.header("location").map(str::to_string)
            {
                target = loc;
                continue;
            }
            let status = resp.status;
            let headers = resp.headers.clone();
            let body = tokio::time::timeout(self.inner.timeout, resp.read_all(1024 * 1024 * 1024)).await.map_err(|_| Error::Timeout)??;
            if !(200..300).contains(&status) {
                return Err(crate::error::ApiError::from_response(status, &headers, &body).into());
            }
            return Ok(body);
        }
        Err(Error::Transport("案内（307）が続きました".into()))
    }

    /// 型の無い要求（この SDK がまだ知らない操作のため）。`path` は `/api/v1` からの相対。
    /// POST には Idempotency-Key を付ける。
    pub async fn request_json(&self, method: &str, path: &str, body: Option<&Value>, if_match: Option<IfMatch>) -> Result<Versioned<Value>> {
        let m = match method.to_ascii_uppercase().as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "PATCH" => Method::Patch,
            "DELETE" => Method::Delete,
            other => return Err(Error::InvalidArgument(format!("method {other}"))),
        };
        let mut c = Call::new(m, path.to_string()).if_match(if_match);
        if let Some(b) = body {
            c = c.json(b)?;
        }
        let (_, headers, body) = self.execute(c).await?;
        let v = if body.is_empty() { Value::Null } else { crate::client::decode(&body)? };
        Ok(crate::client::versioned(v, &headers))
    }
}

/// 配信（`/streams`）。
#[derive(Debug, Clone)]
pub struct Streams(SencaClient);

impl Streams {
    /// 作る（`createStream`）。`ingest.stream_key` はこの応答でだけ渡る。
    pub async fn create(&self, body: &StreamCreate) -> Result<Versioned<Stream>> {
        self.create_with(body, RequestOptions::default()).await
    }
    pub async fn create_with(&self, body: &StreamCreate, opts: RequestOptions) -> Result<Versioned<Stream>> {
        self.0.op_post("/streams".into(), Some(body), opts).await
    }
    /// 一覧（`listStreams`）。頁は必要なだけ読む。
    pub fn list(&self) -> Paginator<Stream> {
        self.list_with(ListOptions::default())
    }
    pub fn list_with(&self, opts: ListOptions) -> Paginator<Stream> {
        self.0.op_list("/streams".into(), opts)
    }
    pub async fn list_page(&self, opts: &ListOptions) -> Result<Page<Stream>> {
        self.0.op_page("/streams".into(), opts).await
    }
    /// 取得（`getStream`、ETag つき）。
    pub async fn get(&self, id: &str) -> Result<Versioned<Stream>> {
        self.0.op_get(format!("/streams/{}", seg(id))).await
    }
    /// 書いたフィールドだけ置き換える（`patchStream`、If-Match 必須）。
    pub async fn update(&self, id: &str, patch: &StreamPatch, if_match: impl Into<IfMatch>) -> Result<Versioned<Stream>> {
        self.0.op_patch(format!("/streams/{}", seg(id)), patch, if_match.into()).await
    }
    /// 消す（`deleteStream`。`if_match` を付ければ版を確かめる）。
    pub async fn delete(&self, id: &str, if_match: Option<IfMatch>) -> Result<()> {
        self.0.op_delete(format!("/streams/{}", seg(id)), if_match).await
    }
    /// stream key を作り直す（`rotateStreamKey`）。
    pub async fn rotate_key(&self, id: &str) -> Result<Versioned<Stream>> {
        self.0.op_post::<(), _>(format!("/streams/{}/keys", seg(id)), None, RequestOptions::default()).await
    }
    /// 署名付きの再生 URL（`createStreamPlaybackUrl`）。
    pub async fn playback_url(&self, id: &str, req: &PlaybackRequest) -> Result<LivePlayback> {
        self.0.op_post(format!("/streams/{}/playback-url", seg(id)), Some(req), RequestOptions::default()).await.map(|v| v.value)
    }
    /// 録画を始める（`startRecording`）。
    pub async fn start_recording(&self, id: &str, req: &RecordingStart) -> Result<Versioned<Recording>> {
        self.0.op_post(format!("/streams/{}/recordings", seg(id)), Some(req), RequestOptions::default()).await
    }
    /// 実行の計画（`planStream`）。
    pub async fn plan(&self, id: &str) -> Result<Plan> {
        self.0.op_post::<(), _>(format!("/streams/{}/plan", seg(id)), None, RequestOptions::default()).await.map(|v| v.value)
    }
    /// 今の静止画（`getStreamSnapshot`。`format` は `jpeg`・`png`）。
    pub async fn snapshot(&self, id: &str, format: Option<&str>) -> Result<Bytes> {
        let mut c = Call::new(Method::Get, format!("/streams/{}/snapshot", seg(id)));
        if let Some(f) = format {
            c = c.query("format", f);
        }
        self.0.bytes(c, "image/jpeg, image/png").await
    }
    /// 保存した静止画の一覧（`listStreamSnapshots`）。
    pub async fn snapshots(&self, id: &str) -> Result<SnapshotList> {
        self.0.op_get(format!("/streams/{}/snapshots", seg(id))).await.map(|v| v.value)
    }
    /// 保存した静止画（`getStreamSnapshotById`）。
    pub async fn snapshot_by_id(&self, id: &str, sid: &str) -> Result<Bytes> {
        self.0.bytes(Call::new(Method::Get, format!("/streams/{}/snapshots/{}", seg(id), seg(sid))), "image/jpeg, image/png").await
    }
    /// 保存した静止画を消す（`deleteStreamSnapshot`）。
    pub async fn delete_snapshot(&self, id: &str, sid: &str) -> Result<()> {
        self.0.op_delete(format!("/streams/{}/snapshots/{}", seg(id), seg(sid)), None).await
    }
}

/// 録画（`/recordings`）。
#[derive(Debug, Clone)]
pub struct Recordings(SencaClient);

impl Recordings {
    /// 一覧（`listRecordings`。`ListOptions::stream` で絞れる）。
    pub fn list(&self) -> Paginator<Recording> {
        self.list_with(ListOptions::default())
    }
    pub fn list_with(&self, opts: ListOptions) -> Paginator<Recording> {
        self.0.op_list("/recordings".into(), opts)
    }
    pub async fn list_page(&self, opts: &ListOptions) -> Result<Page<Recording>> {
        self.0.op_page("/recordings".into(), opts).await
    }
    pub async fn get(&self, id: &str) -> Result<Versioned<Recording>> {
        self.0.op_get(format!("/recordings/{}", seg(id))).await
    }
    pub async fn update(&self, id: &str, patch: &RecordingPatch, if_match: impl Into<IfMatch>) -> Result<Versioned<Recording>> {
        self.0.op_patch(format!("/recordings/{}", seg(id)), patch, if_match.into()).await
    }
    /// 消す（`deleteRecording`、202 で非同期に消す）。
    pub async fn delete(&self, id: &str, if_match: Option<IfMatch>) -> Result<RecordingDeleting> {
        self.0
            .json(Call::new(Method::Delete, format!("/recordings/{}", seg(id))).if_match(if_match))
            .await
            .map(|v| v.value)
    }
    /// 止める（`stopRecording`）。
    pub async fn stop(&self, id: &str) -> Result<Versioned<Recording>> {
        self.0.op_post::<(), _>(format!("/recordings/{}/stop", seg(id)), None, RequestOptions::default()).await
    }
    /// 署名付きの VOD の URL（`createRecordingPlaybackUrl`）。
    pub async fn playback_url(&self, id: &str, req: &PlaybackRequest) -> Result<VodPlayback> {
        self.0.op_post(format!("/recordings/{}/playback-url", seg(id)), Some(req), RequestOptions::default()).await.map(|v| v.value)
    }
    /// サムネイル（`getRecordingThumbnail`）。
    pub async fn thumbnail(&self, id: &str) -> Result<Bytes> {
        self.0.bytes(Call::new(Method::Get, format!("/recordings/{}/thumbnail", seg(id))), "image/jpeg").await
    }
    /// MP4 に書き出す（`exportRecordingMp4`）。
    pub async fn export_mp4(&self, id: &str) -> Result<Bytes> {
        self.0.bytes(Call::new(Method::Get, format!("/recordings/{}/export.mp4", seg(id))), "video/mp4").await
    }
}

/// 予約（`/schedules`）。
#[derive(Debug, Clone)]
pub struct Schedules(SencaClient);

impl Schedules {
    pub async fn create(&self, body: &ScheduleCreate) -> Result<Versioned<Schedule>> {
        self.create_with(body, RequestOptions::default()).await
    }
    pub async fn create_with(&self, body: &ScheduleCreate, opts: RequestOptions) -> Result<Versioned<Schedule>> {
        self.0.op_post("/schedules".into(), Some(body), opts).await
    }
    pub fn list(&self) -> Paginator<Schedule> {
        self.list_with(ListOptions::default())
    }
    pub fn list_with(&self, opts: ListOptions) -> Paginator<Schedule> {
        self.0.op_list("/schedules".into(), opts)
    }
    pub async fn list_page(&self, opts: &ListOptions) -> Result<Page<Schedule>> {
        self.0.op_page("/schedules".into(), opts).await
    }
    /// 作らずに衝突を調べる（`checkSchedule`）。
    pub async fn check(&self, body: &ScheduleCreate) -> Result<ScheduleCheck> {
        self.0.op_post("/schedules/check".into(), Some(body), RequestOptions::default()).await.map(|v| v.value)
    }
    pub async fn get(&self, id: &str) -> Result<Versioned<Schedule>> {
        self.0.op_get(format!("/schedules/{}", seg(id))).await
    }
    pub async fn update(&self, id: &str, patch: &SchedulePatch, if_match: impl Into<IfMatch>) -> Result<Versioned<Schedule>> {
        self.0.op_patch(format!("/schedules/{}", seg(id)), patch, if_match.into()).await
    }
    pub async fn delete(&self, id: &str, if_match: Option<IfMatch>) -> Result<()> {
        self.0.op_delete(format!("/schedules/{}", seg(id)), if_match).await
    }
    /// 実行の履歴（`listScheduleRuns`）。
    pub async fn runs(&self, id: &str) -> Result<ScheduleRunList> {
        self.0.op_get(format!("/schedules/{}/runs", seg(id))).await.map(|v| v.value)
    }
}

/// API トークン（`/tokens`）。
#[derive(Debug, Clone)]
pub struct Tokens(SencaClient);

impl Tokens {
    /// 作る（`createToken`）。`token` はこの応答でだけ渡る。
    pub async fn create(&self, body: &TokenCreate) -> Result<Versioned<Token>> {
        self.0.op_post("/tokens".into(), Some(body), RequestOptions::default()).await
    }
    pub async fn create_with(&self, body: &TokenCreate, opts: RequestOptions) -> Result<Versioned<Token>> {
        self.0.op_post("/tokens".into(), Some(body), opts).await
    }
    pub fn list(&self) -> Paginator<Token> {
        self.list_with(ListOptions::default())
    }
    pub fn list_with(&self, opts: ListOptions) -> Paginator<Token> {
        self.0.op_list("/tokens".into(), opts)
    }
    pub async fn list_page(&self, opts: &ListOptions) -> Result<Page<Token>> {
        self.0.op_page("/tokens".into(), opts).await
    }
    pub async fn get(&self, id: &str) -> Result<Token> {
        self.0.op_get(format!("/tokens/{}", seg(id))).await.map(|v| v.value)
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/tokens/{}", seg(id)), None).await
    }
}

/// 入力（pull。作成と一覧は stream の下、他は `/inputs/{id}`）。
#[derive(Debug, Clone)]
pub struct Inputs(SencaClient);

impl Inputs {
    pub async fn create(&self, stream_id: &str, body: &InputCreate) -> Result<Versioned<Input>> {
        self.create_with(stream_id, body, RequestOptions::default()).await
    }
    pub async fn create_with(&self, stream_id: &str, body: &InputCreate, opts: RequestOptions) -> Result<Versioned<Input>> {
        self.0.op_post(format!("/streams/{}/inputs", seg(stream_id)), Some(body), opts).await
    }
    pub fn list(&self, stream_id: &str) -> Paginator<Input> {
        self.list_with(stream_id, ListOptions::default())
    }
    pub fn list_with(&self, stream_id: &str, opts: ListOptions) -> Paginator<Input> {
        self.0.op_list(format!("/streams/{}/inputs", seg(stream_id)), opts)
    }
    pub async fn get(&self, id: &str) -> Result<Versioned<Input>> {
        self.0.op_get(format!("/inputs/{}", seg(id))).await
    }
    pub async fn update(&self, id: &str, patch: &IoPatch, if_match: impl Into<IfMatch>) -> Result<Versioned<Input>> {
        self.0.op_patch(format!("/inputs/{}", seg(id)), patch, if_match.into()).await
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/inputs/{}", seg(id)), None).await
    }
}

/// 出力（push・PaaS。作成と一覧は stream の下、他は `/outputs/{id}`）。
#[derive(Debug, Clone)]
pub struct Outputs(SencaClient);

impl Outputs {
    pub async fn create(&self, stream_id: &str, body: &OutputCreate) -> Result<Versioned<Output>> {
        self.create_with(stream_id, body, RequestOptions::default()).await
    }
    pub async fn create_with(&self, stream_id: &str, body: &OutputCreate, opts: RequestOptions) -> Result<Versioned<Output>> {
        self.0.op_post(format!("/streams/{}/outputs", seg(stream_id)), Some(body), opts).await
    }
    pub fn list(&self, stream_id: &str) -> Paginator<Output> {
        self.list_with(stream_id, ListOptions::default())
    }
    pub fn list_with(&self, stream_id: &str, opts: ListOptions) -> Paginator<Output> {
        self.0.op_list(format!("/streams/{}/outputs", seg(stream_id)), opts)
    }
    pub async fn get(&self, id: &str) -> Result<Versioned<Output>> {
        self.0.op_get(format!("/outputs/{}", seg(id))).await
    }
    pub async fn update(&self, id: &str, patch: &IoPatch, if_match: impl Into<IfMatch>) -> Result<Versioned<Output>> {
        self.0.op_patch(format!("/outputs/{}", seg(id)), patch, if_match.into()).await
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/outputs/{}", seg(id)), None).await
    }
}

/// 秘密値（`/secrets`。値は読めない）。
#[derive(Debug, Clone)]
pub struct Secrets(SencaClient);

impl Secrets {
    pub async fn create(&self, body: &SecretCreate) -> Result<Versioned<Secret>> {
        self.0.op_post("/secrets".into(), Some(body), RequestOptions::default()).await
    }
    pub async fn create_with(&self, body: &SecretCreate, opts: RequestOptions) -> Result<Versioned<Secret>> {
        self.0.op_post("/secrets".into(), Some(body), opts).await
    }
    pub fn list(&self) -> Paginator<Secret> {
        self.0.op_list("/secrets".into(), ListOptions::default())
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/secrets/{}", seg(id)), None).await
    }
}

/// パイプライン（`/pipelines`）。
#[derive(Debug, Clone)]
pub struct Pipelines(SencaClient);

impl Pipelines {
    pub async fn create(&self, body: &PipelineCreate) -> Result<Versioned<Pipeline>> {
        self.create_with(body, RequestOptions::default()).await
    }
    pub async fn create_with(&self, body: &PipelineCreate, opts: RequestOptions) -> Result<Versioned<Pipeline>> {
        self.0.op_post("/pipelines".into(), Some(body), opts).await
    }
    pub fn list(&self) -> Paginator<Pipeline> {
        self.list_with(ListOptions::default())
    }
    pub fn list_with(&self, opts: ListOptions) -> Paginator<Pipeline> {
        self.0.op_list("/pipelines".into(), opts)
    }
    pub async fn list_page(&self, opts: &ListOptions) -> Result<Page<Pipeline>> {
        self.0.op_page("/pipelines".into(), opts).await
    }
    pub async fn get(&self, id: &str) -> Result<Versioned<Pipeline>> {
        self.0.op_get(format!("/pipelines/{}", seg(id))).await
    }
    pub async fn update(&self, id: &str, patch: &PipelinePatch, if_match: impl Into<IfMatch>) -> Result<Versioned<Pipeline>> {
        self.0.op_patch(format!("/pipelines/{}", seg(id)), patch, if_match.into()).await
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/pipelines/{}", seg(id)), None).await
    }
    /// グラフを検証する（`validatePipeline`。保存前のグラフも渡せる）。
    pub async fn validate(&self, id: &str, graph: &PipelineGraph) -> Result<PipelineValidation> {
        self.0.op_post(format!("/pipelines/{}/validate", seg(id)), Some(graph), RequestOptions::default()).await.map(|v| v.value)
    }
    /// スイッチャー・ミキサーの操作（`controlPipeline`。`{"op":"take","node":"mix"}` など）。
    pub async fn control(&self, id: &str, control: &PipelineControl) -> Result<PipelineControlResult> {
        self.0.op_post(format!("/pipelines/{}/control", seg(id)), Some(control), RequestOptions::default()).await.map(|v| v.value)
    }
    /// 板書の変化の静止画の一覧（`listPipelineSnapshots`）。
    pub async fn snapshots(&self, id: &str) -> Result<SnapshotList> {
        self.0.op_get(format!("/pipelines/{}/snapshots", seg(id))).await.map(|v| v.value)
    }
    /// 板書の変化の静止画（`getPipelineSnapshotById`）。
    pub async fn snapshot_by_id(&self, id: &str, sid: &str) -> Result<Bytes> {
        self.0.bytes(Call::new(Method::Get, format!("/pipelines/{}/snapshots/{}", seg(id), seg(sid))), "image/jpeg").await
    }
    /// 板書の変化の静止画を消す（`deletePipelineSnapshot`）。
    pub async fn delete_snapshot(&self, id: &str, sid: &str) -> Result<()> {
        self.0.op_delete(format!("/pipelines/{}/snapshots/{}", seg(id), seg(sid)), None).await
    }
}

/// ノード（`/nodes`）。
#[derive(Debug, Clone)]
pub struct Nodes(SencaClient);

impl Nodes {
    /// 一覧（`listNodes`。頁に分けない）。
    pub async fn list(&self) -> Result<Vec<Node>> {
        self.0.op_get::<NodeList>("/nodes".into()).await.map(|v| v.value.items)
    }
    pub async fn get(&self, id: &str) -> Result<Node> {
        self.0.op_get(format!("/nodes/{}", seg(id))).await.map(|v| v.value)
    }
    /// 新しい仕事を受けないようにする（`drainNode`）。
    pub async fn drain(&self, id: &str) -> Result<Node> {
        self.0.op_post::<(), _>(format!("/nodes/{}/drain", seg(id)), None, RequestOptions::default()).await.map(|v| v.value)
    }
    /// drain を解く（`undrainNode`）。
    pub async fn undrain(&self, id: &str) -> Result<Node> {
        self.0.op_post::<(), _>(format!("/nodes/{}/undrain", seg(id)), None, RequestOptions::default()).await.map(|v| v.value)
    }
    /// 装置を使わないようにする（`disableDevice`）。
    pub async fn disable_device(&self, node: &str, device: &str) -> Result<DeviceHealth> {
        self.0
            .op_post::<(), _>(format!("/nodes/{}/devices/{}/disable", seg(node), seg(device)), None, RequestOptions::default())
            .await
            .map(|v| v.value)
    }
    /// 装置をまた使う（`enableDevice`）。
    pub async fn enable_device(&self, node: &str, device: &str) -> Result<DeviceHealth> {
        self.0
            .op_post::<(), _>(format!("/nodes/{}/devices/{}/enable", seg(node), seg(device)), None, RequestOptions::default())
            .await
            .map(|v| v.value)
    }
}

// ------------------------------------------------------------------ ログインと利用者（ADR-0017）

impl SencaClient {
    /// ログインと本人の操作（`/auth/...`）。
    pub fn auth(&self) -> Auth {
        Auth(self.clone())
    }
    /// 利用者の管理（`/users`、`tenant:admin`）。
    pub fn users(&self) -> Users {
        Users(self.clone())
    }
}

/// `POST /auth/login` の結果。
#[derive(Debug, Clone, PartialEq)]
pub enum LoginOutcome {
    /// ログインした（`token` はセッションの値。[`SencaClient::with_token`] で使う）。
    Ok(Box<LoginSuccess>),
    /// 2 つ目の要素が要る（[`Auth::login_mfa`] に `mfa_token` を渡す）。
    MfaRequired(LoginMfaRequired),
}

impl LoginOutcome {
    fn from_value(v: Value) -> Result<LoginOutcome> {
        let bad = |e: serde_json::Error| Error::Decode(e.to_string());
        match v.get("status").and_then(Value::as_str) {
            Some("mfa_required") => serde_json::from_value(v).map(LoginOutcome::MfaRequired).map_err(bad),
            _ => serde_json::from_value(v).map(|s| LoginOutcome::Ok(Box::new(s))).map_err(bad),
        }
    }
}

/// ログインと本人の操作（`/auth/...`）。ログインの操作（`providers`・`login`・`login_mfa`・`exchange`）は認証なしで使える
/// （トークンなしのクライアント `SencaClient::builder(url).build()` でよい）。
#[derive(Debug, Clone)]
pub struct Auth(SencaClient);

impl Auth {
    /// ログインの方法（`getAuthProviders`、認証なし）。
    pub async fn providers(&self) -> Result<AuthProviders> {
        self.0.op_get("/auth/providers".into()).await.map(|v| v.value)
    }
    /// パスワードでログイン（`login`、認証なし）。
    pub async fn login(&self, body: &LoginRequest) -> Result<LoginOutcome> {
        let v: Versioned<Value> = self.0.op_post("/auth/login".into(), Some(body), RequestOptions::default()).await?;
        LoginOutcome::from_value(v.value)
    }
    /// 2 つ目の要素（TOTP・回復コード）でログイン（`loginMfa`、認証なし）。
    pub async fn login_mfa(&self, body: &LoginMfaRequest) -> Result<LoginSuccess> {
        self.0.op_post("/auth/login/mfa".into(), Some(body), RequestOptions::default()).await.map(|v| v.value)
    }
    /// OIDC の一度きりのコードをセッションに替える（`exchangeLoginCode`、認証なし）。ブラウザーの外から使うときは、
    /// `start`・`callback` で受けたクッキー（`senca_oidc=...`）を `cookie` に渡す（無ければ 401）。
    pub async fn exchange(&self, code: &str, cookie: Option<&str>) -> Result<LoginSuccess> {
        let mut c = Call::new(Method::Post, "/auth/exchange").json(&ExchangeRequest { code: code.to_string() })?;
        if let Some(k) = cookie {
            c.headers.push(("Cookie".into(), k.to_string()));
        }
        self.0.json(c).await.map(|v| v.value)
    }
    /// OIDC のログインを始める URL（`startOidcLogin`）。ブラウザーで開く（IdP へリダイレクトされる）。
    /// `return_to` は管理画面のパス（`/console/...`）。
    pub fn oidc_start_url(&self, provider: &str, return_to: Option<&str>) -> String {
        let mut u = format!("{}/auth/oidc/{}/start", self.0.base_url(), seg(provider));
        if let Some(r) = return_to {
            u.push_str("?return_to=");
            u.push_str(&seg(r));
        }
        u
    }
    /// OIDC のコールバックの URL（`oidcCallback`）。IdP に登録する redirect URI の既定値（ブラウザーが IdP から戻ってくる先で、
    /// SDK から呼ぶ操作ではない）。
    pub fn oidc_callback_url(&self, provider: &str) -> String {
        format!("{}/auth/oidc/{}/callback", self.0.base_url(), seg(provider))
    }
    /// 自分の情報（`getMe`）。
    pub async fn me(&self) -> Result<Me> {
        self.0.op_get("/auth/me".into()).await.map(|v| v.value)
    }
    /// ログアウト（`logout`。セッションでだけ）。
    pub async fn logout(&self) -> Result<()> {
        self.0.empty(Call::new(Method::Post, "/auth/logout")).await
    }
    /// 自分のパスワードを変える（`changePassword`）。ほかのセッションは失効する。
    pub async fn change_password(&self, body: &PasswordChange) -> Result<()> {
        self.0.empty(Call::new(Method::Post, "/auth/password").json(body)?).await
    }
    /// TOTP の登録を始める（`setupTotp`）。`secret`・`otpauth_uri` はこの応答でだけ渡る。
    pub async fn setup_totp(&self, body: &MfaSetupRequest) -> Result<MfaSetup> {
        self.0.op_post("/auth/mfa/totp/setup".into(), Some(body), RequestOptions::default()).await.map(|v| v.value)
    }
    /// 確認コードで TOTP を有効にする（`confirmTotp`）。回復コードはこの応答でだけ渡る。
    pub async fn confirm_totp(&self, body: &MfaConfirmRequest) -> Result<MfaConfirmResult> {
        self.0.op_post("/auth/mfa/totp/confirm".into(), Some(body), RequestOptions::default()).await.map(|v| v.value)
    }
    /// 自分の MFA を解除する（`disableMfa`）。
    pub async fn disable_mfa(&self, body: &MfaReauth) -> Result<()> {
        self.0.empty(Call::new(Method::Post, "/auth/mfa/disable").json(body)?).await
    }
    /// 回復コードを作り直す（`regenerateRecoveryCodes`）。
    pub async fn regenerate_recovery_codes(&self, body: &MfaReauth) -> Result<RecoveryCodes> {
        self.0.op_post("/auth/mfa/recovery-codes".into(), Some(body), RequestOptions::default()).await.map(|v| v.value)
    }
    /// 自分のセッション（`listMySessions`）。
    pub fn sessions(&self) -> Paginator<Session> {
        self.sessions_with(ListOptions::default())
    }
    pub fn sessions_with(&self, opts: ListOptions) -> Paginator<Session> {
        self.0.op_list("/auth/sessions".into(), opts)
    }
    pub async fn sessions_page(&self, opts: &ListOptions) -> Result<Page<Session>> {
        self.0.op_page("/auth/sessions".into(), opts).await
    }
    /// 自分のセッションを失効させる（`revokeMySession`）。
    pub async fn revoke_session(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/auth/sessions/{}", seg(id)), None).await
    }
}

/// 利用者の管理（`/users`）。
#[derive(Debug, Clone)]
pub struct Users(SencaClient);

impl Users {
    /// 作る（`createUser`）。
    pub async fn create(&self, body: &UserCreate) -> Result<Versioned<User>> {
        self.create_with(body, RequestOptions::default()).await
    }
    pub async fn create_with(&self, body: &UserCreate, opts: RequestOptions) -> Result<Versioned<User>> {
        self.0.op_post("/users".into(), Some(body), opts).await
    }
    /// 一覧（`listUsers`）。
    pub fn list(&self) -> Paginator<User> {
        self.list_with(ListOptions::default())
    }
    pub fn list_with(&self, opts: ListOptions) -> Paginator<User> {
        self.0.op_list("/users".into(), opts)
    }
    pub async fn list_page(&self, opts: &ListOptions) -> Result<Page<User>> {
        self.0.op_page("/users".into(), opts).await
    }
    /// 取得（`getUser`）。
    pub async fn get(&self, id: &str) -> Result<Versioned<User>> {
        self.0.op_get(format!("/users/{}", seg(id))).await
    }
    /// 変更（`updateUser`、If-Match が要る）。
    pub async fn update(&self, id: &str, patch: &UserPatch, if_match: impl Into<IfMatch>) -> Result<Versioned<User>> {
        self.0.op_patch(format!("/users/{}", seg(id)), patch, if_match.into()).await
    }
    /// 削除（`deleteUser`）。
    pub async fn delete(&self, id: &str, if_match: Option<IfMatch>) -> Result<()> {
        self.0.op_delete(format!("/users/{}", seg(id)), if_match).await
    }
    /// パスワードの再設定（`resetUserPassword`）。`password` を省くと仮のパスワードが `temporary_password` で返る。
    pub async fn reset_password(&self, id: &str, body: &UserPasswordReset) -> Result<Versioned<UserPasswordResetResult>> {
        self.0.op_post(format!("/users/{}/password", seg(id)), Some(body), RequestOptions::default()).await
    }
    /// MFA を外す（`resetUserMfa`）。
    pub async fn reset_mfa(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/users/{}/mfa", seg(id)), None).await
    }
    /// 利用者のセッション（`listUserSessions`）。
    pub fn sessions(&self, id: &str) -> Paginator<Session> {
        self.sessions_with(id, ListOptions::default())
    }
    pub fn sessions_with(&self, id: &str, opts: ListOptions) -> Paginator<Session> {
        self.0.op_list(format!("/users/{}/sessions", seg(id)), opts)
    }
    /// 利用者のセッションをすべて失効させる（`revokeUserSessions`）。
    pub async fn revoke_sessions(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/users/{}/sessions", seg(id)), None).await
    }
}
