//! 部屋のメディアパイプラインの REST（`/media-pipelines`、ADR-0016、AWS Chime SDK Media Pipelines 相当）。
//! 部屋の参加者を stream に書き出し、録画（`recording`）・合成（`composite`）・送り出し（`egress`）・文字起こし（`captions`）を
//! 1 つの資源で指定する。型は openapi.yaml からの生成物（[`crate::models`] の `MediaPipeline`・`MediaPipelineCreate`・`MediaPipelinePatch` など）。

use crate::client::{IfMatch, RequestOptions, SencaClient, Versioned, encode_segment as seg};
use crate::error::Result;
use crate::models::{MediaPipeline, MediaPipelineCreate, MediaPipelinePatch, MediaPipelineState};
use crate::pagination::{ListOptions, Page, Paginator};

/// メディアパイプライン（`/media-pipelines`）。
#[derive(Debug, Clone)]
pub struct MediaPipelines(pub(crate) SencaClient);

impl MediaPipelines {
    /// 作る（`createMediaPipeline`。`desired_state` の既定は running）。
    pub async fn create(&self, body: &MediaPipelineCreate) -> Result<Versioned<MediaPipeline>> {
        self.create_with(body, RequestOptions::default()).await
    }
    pub async fn create_with(&self, body: &MediaPipelineCreate, opts: RequestOptions) -> Result<Versioned<MediaPipeline>> {
        self.0.op_post("/media-pipelines".into(), Some(body), opts).await
    }
    /// 一覧（`listMediaPipelines`。`ListOptions::room` で部屋を絞る。今のノードは `room` を 400 にする不備がある）。
    pub fn list(&self) -> Paginator<MediaPipeline> {
        self.list_with(ListOptions::default())
    }
    pub fn list_with(&self, opts: ListOptions) -> Paginator<MediaPipeline> {
        self.0.op_list("/media-pipelines".into(), opts)
    }
    pub async fn list_page(&self, opts: &ListOptions) -> Result<Page<MediaPipeline>> {
        self.0.op_page("/media-pipelines".into(), opts).await
    }
    /// 取得（`getMediaPipeline`、ETag つき。子の資源は `artifacts`、このノードでの様子は `status`）。
    pub async fn get(&self, id: &str) -> Result<Versioned<MediaPipeline>> {
        self.0.op_get(format!("/media-pipelines/{}", seg(id))).await
    }
    /// 更新（`patchMediaPipeline`、If-Match 必須）。
    pub async fn update(&self, id: &str, patch: &MediaPipelinePatch, if_match: impl Into<IfMatch>) -> Result<Versioned<MediaPipeline>> {
        self.0.op_patch(format!("/media-pipelines/{}", seg(id)), patch, if_match.into()).await
    }
    /// 動かす・止める（`desired_state` だけの `patchMediaPipeline`）。止めると合成・送り出しを片付ける（書き出した stream・録画は残る）。
    pub async fn set_state(&self, id: &str, state: MediaPipelineState, if_match: impl Into<IfMatch>) -> Result<Versioned<MediaPipeline>> {
        let patch = MediaPipelinePatch { desired_state: Some(Some(state)), ..Default::default() };
        self.update(id, &patch, if_match).await
    }
    /// 消す（`deleteMediaPipeline`。合成のパイプライン・送り出しの出力も消し、書き出した stream・録画は残る）。
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.0.op_delete(format!("/media-pipelines/{}", seg(id)), None).await
    }
}
