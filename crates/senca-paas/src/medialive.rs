//! AWS Elemental MediaLive の Input（`https://medialive.{region}.amazonaws.com/prod/...`、REST-JSON）。
//! SigV4 の service は `medialive`。
//!
//! **範囲**: 作るのは SENCA が送り込む Input と、それを守る Input Security Group だけ。MediaLive の
//! Channel（エンコード設定・出力先）は範囲外で、利用者が別に作ってこの Input をつなぐ。Channel に
//! つながったままの Input は消せない（`Conflict` を返す）。
//!
//! 作成の手順（途中で落ちても片付けられるように、各資源に `senca:owner` タグを付ける）:
//! 1. `POST /prod/inputSecurityGroups`（`whitelistRules` = ノードの送出元 CIDR）→ `sg=<id>` を checkpoint に。
//! 2. `POST /prod/inputs`（`type`・`destinations`・`inputSecurityGroups`・`requestId` = Idempotency-Key・`tags`）。
//!    失敗したら 1 の security group を消してから誤りを返す（消せなければ checkpoint から [`crate::reconcile`] が片付ける）。
//!
//! 資源 ID は `input=<id>;sg=<id>` の複合（既存の security group を使うときは `input=<id>` だけ）。
//! 削除は Input → security group の順。Input の削除は非同期（`DELETING`）なので、security group の
//! 削除が「使用中」で失敗したら再試行可能な誤りにする（再試行では Input は 404 で飛ばす）。
//!
//! options:
//! - `region`（必須）、`endpoint`（試験用）
//! - `input_type`: `RTMP_PUSH`（既定）か `SRT_LISTENER`（MediaLive が listener になり SENCA が caller で送る。
//!   API の形は AWS の文書で要確認。passphrase は Secrets Manager の ARN で `srt_passphrase_secret_arn` に渡す）
//! - `allow_cidr`: ノードの送出元（`,` 区切り。`input_security_group` を指定しないときは必須）
//! - `input_security_group`: 既存の security group の ID（SENCA は作らず、消さない）
//! - `pipelines`: `1`（既定、SINGLE_PIPELINE）か `2`（STANDARD。送り先が 2 つ）
//! - `rtmp_app`: RTMP の application 名（既定 `senca`）
//! - `srt_min_latency_ms`: SRT の最小遅延（既定 2000）

use crate::aws::{AwsClient, aws_error, aws_json};
use crate::call::HttpReply;
use crate::error::{ErrorKind, PaasError};
use crate::provider::{
    Capabilities, CreateSpec, CreatedResource, DeleteOutcome, Ingest, IngestProtocol, Op, PaasProvider, Ready, Step, TaggedResource,
    finished, json_str, misuse, need, owner_of,
};
use crate::secret::Secret;
use senca_http::Method;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const NAME: &str = "aws_medialive";
const MAX_PAGES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputType {
    RtmpPush,
    SrtListener,
}

#[derive(Debug, Clone)]
pub struct MediaLive {
    client: AwsClient,
    input_type: InputType,
    allow_cidrs: Vec<String>,
    existing_sg: Option<String>,
    pipelines: u8,
    rtmp_app: String,
    srt_passphrase_secret_arn: Option<String>,
    srt_min_latency_ms: u32,
}

fn valid_ml_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// `a.b.c.d/n` か IPv6 の CIDR（形だけ確かめる）。
fn valid_cidr(s: &str) -> bool {
    let Some((ip, len)) = s.split_once('/') else { return false };
    let Ok(len) = len.parse::<u8>() else { return false };
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => len <= 32,
        Ok(std::net::IpAddr::V6(_)) => len <= 128,
        Err(_) => false,
    }
}

/// 資源 ID `input=<id>;sg=<id>` を分ける。
pub fn parse_resource_id(rid: &str) -> Result<(Option<String>, Option<String>), PaasError> {
    let (mut input, mut sg) = (None, None);
    for part in rid.split(';') {
        let (k, v) = part.split_once('=').ok_or_else(|| PaasError::config("MediaLive の資源 ID の形が不正"))?;
        if !valid_ml_id(v) {
            return Err(PaasError::config("MediaLive の資源 ID の形が不正"));
        }
        let slot = match k {
            "input" => &mut input,
            "sg" => &mut sg,
            _ => return Err(PaasError::config("MediaLive の資源 ID の形が不正")),
        };
        if slot.replace(v.to_string()).is_some() {
            return Err(PaasError::config("MediaLive の資源 ID の形が不正"));
        }
    }
    if input.is_none() && sg.is_none() {
        return Err(PaasError::config("MediaLive の資源 ID が空"));
    }
    Ok((input, sg))
}

/// 資源 ID を組み立てる。
pub fn resource_id(input: Option<&str>, sg: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(i) = input {
        parts.push(format!("input={i}"));
    }
    if let Some(s) = sg {
        parts.push(format!("sg={s}"));
    }
    parts.join(";")
}

impl MediaLive {
    pub fn from_options(credential: &Secret, options: &BTreeMap<String, String>) -> Result<Self, PaasError> {
        let client = AwsClient::from_options(credential, options, "medialive")?;
        let input_type = match options.get("input_type").map(String::as_str) {
            None | Some("RTMP_PUSH") => InputType::RtmpPush,
            Some("SRT_LISTENER") => InputType::SrtListener,
            _ => return Err(PaasError::config("input_type は RTMP_PUSH か SRT_LISTENER")),
        };
        let allow_cidrs: Vec<String> =
            options.get("allow_cidr").map(|s| s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()).unwrap_or_default();
        if allow_cidrs.len() > 10 || allow_cidrs.iter().any(|c| !valid_cidr(c)) {
            return Err(PaasError::config("allow_cidr は CIDR を , で区切って 10 個まで"));
        }
        let existing_sg = options.get("input_security_group").cloned();
        if existing_sg.as_deref().is_some_and(|s| !valid_ml_id(s)) {
            return Err(PaasError::config("input_security_group が不正"));
        }
        if existing_sg.is_none() && allow_cidrs.is_empty() {
            return Err(PaasError::config("allow_cidr（ノードの送出元）か input_security_group が要る"));
        }
        let pipelines = match options.get("pipelines").map(String::as_str) {
            None | Some("1") => 1,
            Some("2") => 2,
            _ => return Err(PaasError::config("pipelines は 1 か 2")),
        };
        let rtmp_app = options.get("rtmp_app").cloned().unwrap_or_else(|| "senca".into());
        if rtmp_app.is_empty() || rtmp_app.len() > 64 || !rtmp_app.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            return Err(PaasError::config("rtmp_app は英数字・-・_"));
        }
        let srt_passphrase_secret_arn = options.get("srt_passphrase_secret_arn").cloned();
        if srt_passphrase_secret_arn.as_deref().is_some_and(|a| !a.starts_with("arn:") || !crate::provider::valid_id(a)) {
            return Err(PaasError::config("srt_passphrase_secret_arn が不正"));
        }
        let srt_min_latency_ms = match options.get("srt_min_latency_ms") {
            None => 2000,
            Some(v) => v.parse().ok().filter(|ms| (120..=15_000).contains(ms)).ok_or_else(|| PaasError::config("srt_min_latency_ms は 120〜15000"))?,
        };
        Ok(MediaLive { client, input_type, allow_cidrs, existing_sg, pipelines, rtmp_app, srt_passphrase_secret_arn, srt_min_latency_ms })
    }

    /// `CreateInputSecurityGroup` の body。
    pub fn security_group_body(&self, spec: &CreateSpec) -> Value {
        let rules: Vec<Value> = self.allow_cidrs.iter().map(|c| json!({ "cidr": c })).collect();
        json!({ "whitelistRules": rules, "tags": spec.all_tags() })
    }

    /// `CreateInput` の body。
    pub fn input_body(&self, spec: &CreateSpec, sg: &str) -> Value {
        let name = spec.safe_name();
        let mut b = json!({
            "name": name,
            "inputSecurityGroups": [sg],
            "requestId": spec.idempotency_key,
            "tags": spec.all_tags(),
        });
        match self.input_type {
            InputType::RtmpPush => {
                b["type"] = json!("RTMP_PUSH");
                let dests: Vec<Value> =
                    ["a", "b"].iter().take(self.pipelines as usize).map(|s| json!({ "streamName": format!("{}/{name}-{s}", self.rtmp_app) })).collect();
                b["destinations"] = Value::Array(dests);
            }
            InputType::SrtListener => {
                b["type"] = json!("SRT_LISTENER");
                let mut s = json!({ "minimumLatency": self.srt_min_latency_ms, "streamId": name });
                if let Some(arn) = &self.srt_passphrase_secret_arn {
                    s["decryption"] = json!({ "algorithm": "AES256", "passphraseSecretArn": arn });
                }
                b["srtListenerSettings"] = s;
            }
        }
        b
    }

    /// Input の JSON を受け口にする。
    fn resource(&self, input: &Value, sg: Option<&str>) -> Result<CreatedResource, PaasError> {
        let id = json_str(input, "id").filter(|i| valid_ml_id(i)).ok_or_else(|| PaasError::malformed("Input に id が無い"))?;
        let mut ingest = Vec::new();
        for d in input.get("destinations").and_then(Value::as_array).into_iter().flatten() {
            let Some(url) = json_str(d, "url") else { continue };
            if let Some(rest) = url.strip_prefix("rtmp://") {
                // rtmp://ip:1935/app/name → (rtmp://ip:1935/app, name)
                let (base, key) = match rest.rsplit_once('/') {
                    Some((b, k)) if b.contains('/') => (format!("rtmp://{b}"), Some(Secret::new(k))),
                    _ => (url.to_string(), None),
                };
                ingest.push(Ingest { protocol: IngestProtocol::Rtmp, url: base, stream_key: key, stream_id: None, passphrase: None });
            } else if url.starts_with("srt://") {
                let stream_id = input.get("srtListenerSettings").and_then(|s| json_str(s, "streamId")).map(Secret::new);
                ingest.push(Ingest { protocol: IngestProtocol::Srt, url: url.to_string(), stream_key: None, stream_id, passphrase: None });
            }
        }
        Ok(CreatedResource { resource_id: resource_id(Some(id), sg), ingest, playback: Vec::new(), owner: owner_of(input.get("tags")).map(|(o, _)| o) })
    }

    fn call(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>, now: u64) -> Step<CreatedResource> {
        Step::Call { call: self.client.call(method, path, query, body, now), checkpoint: None }
    }
}

impl PaasProvider for MediaLive {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        let ingest: &'static [IngestProtocol] = match self.input_type {
            InputType::RtmpPush => &[IngestProtocol::Rtmp],
            InputType::SrtListener => &[IngestProtocol::Srt],
        };
        Capabilities { provider: NAME, ingest, video_codecs: &["h264", "h265"], audio_codecs: &["aac"] }
    }

    fn create(&self, spec: &CreateSpec) -> Box<dyn Op<CreatedResource>> {
        if let Err(e) = spec.validate() {
            return Box::new(Ready(Some(Err(e))));
        }
        Box::new(CreateOp { p: self.clone(), spec: spec.clone(), state: CreateState::Start })
    }

    fn get(&self, rid: &str) -> Box<dyn Op<Option<CreatedResource>>> {
        match parse_resource_id(rid) {
            Err(e) => Box::new(Ready(Some(Err(e)))),
            // security group だけ（作りかけ）は受け口として使えない。
            Ok((None, _)) => Box::new(Ready(Some(Ok(None)))),
            Ok((Some(input), sg)) => {
                let p = self.clone();
                Box::new(crate::provider::OneShot::new(
                    {
                        let p = p.clone();
                        let input = input.clone();
                        move |now| Ok(p.client.call(Method::Get, &format!("/prod/inputs/{input}"), &[], None, now))
                    },
                    move |r| {
                        if !r.is_success() {
                            let e = aws_error(r);
                            return if e.kind == ErrorKind::NotFound { Ok(None) } else { Err(e) };
                        }
                        let v = r.json()?;
                        let i = v.get("input").unwrap_or(&v);
                        // 削除中のものは無いとみなす
                        if matches!(json_str(i, "state"), Some("DELETING" | "DELETED")) {
                            return Ok(None);
                        }
                        p.resource(i, sg.as_deref()).map(Some)
                    },
                ))
            }
        }
    }

    fn delete(&self, rid: &str) -> Box<dyn Op<DeleteOutcome>> {
        match parse_resource_id(rid) {
            Err(e) => Box::new(Ready(Some(Err(e)))),
            Ok((input, sg)) => Box::new(DeleteOp { client: self.client.clone(), input, sg, state: DelState::Start, deleted: false }),
        }
    }

    fn is_partial(&self, rid: &str) -> bool {
        matches!(parse_resource_id(rid), Ok((None, Some(_))))
    }

    fn list_by_tag(&self, owner: Option<&str>) -> Box<dyn Op<Vec<TaggedResource>>> {
        Box::new(ListOp {
            client: self.client.clone(),
            owner: owner.map(str::to_string),
            phase: ListPhase::Start,
            next: None,
            pages: 0,
            inputs: Vec::new(),
            sgs: Vec::new(),
        })
    }
}

// ---- 作成 ----

enum CreateState {
    Start,
    /// security group の作成を待つ。
    Sg,
    /// Input の作成を待つ。`owned_sg` は SENCA が作った security group。
    Input { sg: String, owned: bool },
    /// Input の作成に失敗し、security group を消している。
    Rollback(PaasError),
    Done,
}

struct CreateOp {
    p: MediaLive,
    spec: CreateSpec,
    state: CreateState,
}

impl CreateOp {
    fn create_input(&mut self, sg: String, owned: bool, now: u64) -> Step<CreatedResource> {
        let body = self.p.input_body(&self.spec, &sg);
        let checkpoint = owned.then(|| resource_id(None, Some(&sg)));
        self.state = CreateState::Input { sg, owned };
        Step::Call { call: self.p.client.call(Method::Post, "/prod/inputs", &[], Some(&body), now), checkpoint }
    }
}

impl Op<CreatedResource> for CreateOp {
    fn step(&mut self, reply: Option<&HttpReply>, now: u64) -> Result<Step<CreatedResource>, PaasError> {
        match std::mem::replace(&mut self.state, CreateState::Done) {
            CreateState::Start => {
                if reply.is_some() {
                    return Err(misuse());
                }
                if let Some(sg) = self.p.existing_sg.clone() {
                    return Ok(self.create_input(sg, false, now));
                }
                self.state = CreateState::Sg;
                let body = self.p.security_group_body(&self.spec);
                Ok(self.p.call(Method::Post, "/prod/inputSecurityGroups", &[], Some(&body), now))
            }
            CreateState::Sg => {
                let v = aws_json(need(reply)?)?;
                let g = v.get("securityGroup").unwrap_or(&v);
                let sg = json_str(g, "id").filter(|i| valid_ml_id(i)).ok_or_else(|| PaasError::malformed("security group に id が無い"))?;
                Ok(self.create_input(sg.to_string(), true, now))
            }
            CreateState::Input { sg, owned } => {
                let r = need(reply)?;
                if r.is_success() {
                    let v = r.json()?;
                    let i = v.get("input").unwrap_or(&v);
                    return self.p.resource(i, owned.then_some(sg.as_str())).map(Step::Done);
                }
                let e = aws_error(r);
                if !owned {
                    return Err(e);
                }
                // 作りかけを残さない（AC-P03）
                self.state = CreateState::Rollback(e);
                Ok(Step::Call {
                    call: self.p.client.call(Method::Delete, &format!("/prod/inputSecurityGroups/{sg}"), &[], None, now),
                    checkpoint: None,
                })
            }
            // 片付けの成否にかかわらず、元の誤りを返す（片付けに失敗したら checkpoint が残る）。
            CreateState::Rollback(e) => {
                need(reply)?;
                Err(e)
            }
            CreateState::Done => Err(finished()),
        }
    }
}

// ---- 削除 ----

enum DelState {
    Start,
    Input,
    Sg,
    Done,
}

struct DeleteOp {
    client: AwsClient,
    input: Option<String>,
    sg: Option<String>,
    state: DelState,
    deleted: bool,
}

impl DeleteOp {
    fn next_sg(&mut self, now: u64) -> Step<DeleteOutcome> {
        match self.sg.clone() {
            Some(sg) => {
                self.state = DelState::Sg;
                Step::Call { call: self.client.call(Method::Delete, &format!("/prod/inputSecurityGroups/{sg}"), &[], None, now), checkpoint: None }
            }
            None => Step::Done(self.outcome()),
        }
    }

    fn outcome(&self) -> DeleteOutcome {
        if self.deleted { DeleteOutcome::Deleted } else { DeleteOutcome::AlreadyGone }
    }
}

impl Op<DeleteOutcome> for DeleteOp {
    fn step(&mut self, reply: Option<&HttpReply>, now: u64) -> Result<Step<DeleteOutcome>, PaasError> {
        match std::mem::replace(&mut self.state, DelState::Done) {
            DelState::Start => {
                if reply.is_some() {
                    return Err(misuse());
                }
                match self.input.clone() {
                    Some(i) => {
                        self.state = DelState::Input;
                        Ok(Step::Call { call: self.client.call(Method::Delete, &format!("/prod/inputs/{i}"), &[], None, now), checkpoint: None })
                    }
                    None => Ok(self.next_sg(now)),
                }
            }
            DelState::Input => {
                let r = need(reply)?;
                if r.is_success() {
                    self.deleted = true;
                } else {
                    let e = aws_error(r);
                    match e.kind {
                        ErrorKind::NotFound => {}
                        // Channel につながっている（利用者が外すまで消せない）
                        ErrorKind::Conflict | ErrorKind::Invalid => return Err(e.with_kind(ErrorKind::Conflict)),
                        _ => return Err(e),
                    }
                }
                Ok(self.next_sg(now))
            }
            DelState::Sg => {
                let r = need(reply)?;
                if r.is_success() {
                    return Ok(Step::Done(DeleteOutcome::Deleted));
                }
                let e = aws_error(r);
                match e.kind {
                    ErrorKind::NotFound => Ok(Step::Done(self.outcome())),
                    // Input の削除が終わるまで使用中
                    ErrorKind::Conflict | ErrorKind::Invalid => Err(e.with_kind(ErrorKind::Retryable)),
                    _ => Err(e),
                }
            }
            DelState::Done => Err(finished()),
        }
    }
}

// ---- 一覧 ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListPhase {
    Start,
    Inputs,
    Sgs,
}

struct ListOp {
    client: AwsClient,
    owner: Option<String>,
    phase: ListPhase,
    next: Option<String>,
    pages: usize,
    inputs: Vec<Value>,
    sgs: Vec<Value>,
}

impl ListOp {
    fn request(&mut self, now: u64) -> Result<Step<Vec<TaggedResource>>, PaasError> {
        self.pages += 1;
        if self.pages > MAX_PAGES {
            return Err(PaasError::malformed("一覧のページが多すぎる"));
        }
        let mut q = vec![("maxResults", "100".to_string())];
        if let Some(t) = &self.next {
            q.push(("nextToken", t.clone()));
        }
        let path = if self.phase == ListPhase::Inputs { "/prod/inputs" } else { "/prod/inputSecurityGroups" };
        Ok(Step::Call { call: self.client.call(Method::Get, path, &q, None, now), checkpoint: None })
    }

    fn finish(&mut self) -> Vec<TaggedResource> {
        let tagged_sgs: BTreeMap<String, (String, Option<String>)> = self
            .sgs
            .iter()
            .filter_map(|g| Some((json_str(g, "id").filter(|i| valid_ml_id(i))?.to_string(), owner_of(g.get("tags"))?)))
            .collect();
        let mut referenced = BTreeSet::new();
        let mut out = Vec::new();
        for i in &self.inputs {
            let Some(id) = json_str(i, "id").filter(|i| valid_ml_id(i)) else { continue };
            let sgs: Vec<&str> = i.get("securityGroups").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            referenced.extend(sgs.iter().map(|s| s.to_string()));
            if matches!(json_str(i, "state"), Some("DELETED")) {
                continue;
            }
            let Some((owner, cluster)) = owner_of(i.get("tags")) else { continue };
            let ours = sgs.iter().find(|s| tagged_sgs.contains_key(**s)).copied();
            out.push(TaggedResource { resource_id: resource_id(Some(id), ours), owner, cluster, complete: true });
        }
        for g in &self.sgs {
            let Some(id) = json_str(g, "id").filter(|i| valid_ml_id(i)) else { continue };
            let attached = g.get("inputs").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
            if referenced.contains(id) || attached || matches!(json_str(g, "state"), Some("DELETED")) {
                continue;
            }
            if let Some((owner, cluster)) = tagged_sgs.get(id) {
                out.push(TaggedResource { resource_id: resource_id(None, Some(id)), owner: owner.clone(), cluster: cluster.clone(), complete: false });
            }
        }
        out.retain(|t| self.owner.as_deref().is_none_or(|o| o == t.owner));
        out
    }
}

impl Op<Vec<TaggedResource>> for ListOp {
    fn step(&mut self, reply: Option<&HttpReply>, now: u64) -> Result<Step<Vec<TaggedResource>>, PaasError> {
        if self.phase == ListPhase::Start {
            if reply.is_some() {
                return Err(misuse());
            }
            self.phase = ListPhase::Inputs;
            return self.request(now);
        }
        let v = aws_json(need(reply)?)?;
        let key = if self.phase == ListPhase::Inputs { "inputs" } else { "inputSecurityGroups" };
        let items = v.get(key).and_then(Value::as_array).ok_or_else(|| PaasError::malformed("一覧の応答の形が不正"))?;
        let dst = if self.phase == ListPhase::Inputs { &mut self.inputs } else { &mut self.sgs };
        dst.extend(items.iter().cloned());
        self.next = json_str(&v, "nextToken").map(str::to_string);
        if self.next.is_some() {
            return self.request(now);
        }
        if self.phase == ListPhase::Inputs {
            self.phase = ListPhase::Sgs;
            return self.request(now);
        }
        Ok(Step::Done(self.finish()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ml(extra: &[(&str, &str)]) -> MediaLive {
        let mut o = BTreeMap::new();
        o.insert("region".to_string(), "ap-northeast-1".to_string());
        o.insert("allow_cidr".to_string(), "203.0.113.10/32, 198.51.100.0/24".to_string());
        for (k, v) in extra {
            o.insert(k.to_string(), v.to_string());
        }
        MediaLive::from_options(&Secret::new("AKID1:SECRET1"), &o).unwrap()
    }

    fn call_of<T>(s: Step<T>) -> (crate::HttpCall, Option<String>) {
        match s {
            Step::Call { call, checkpoint } => (call, checkpoint),
            Step::Done(_) => panic!("expected call"),
        }
    }

    fn body(c: &crate::HttpCall) -> Value {
        serde_json::from_slice(&c.body).unwrap()
    }

    #[test]
    fn resource_id_round_trip() {
        assert_eq!(parse_resource_id("input=123;sg=45").unwrap(), (Some("123".into()), Some("45".into())));
        assert_eq!(parse_resource_id("sg=45").unwrap(), (None, Some("45".into())));
        for bad in ["", "input=", "x=1", "input=1;input=2", "input=../1", "input=1;"] {
            assert!(parse_resource_id(bad).is_err(), "{bad}");
        }
        assert_eq!(resource_id(Some("1"), Some("2")), "input=1;sg=2");
    }

    #[test]
    fn create_flow_shapes() {
        let p = ml(&[("pipelines", "2")]);
        let spec = CreateSpec::for_output("o9", 4, None);
        let mut op = p.create(&spec);
        let (c, cp) = call_of(op.step(None, 0).unwrap());
        assert_eq!(cp, None);
        assert_eq!(c.url(), "https://medialive.ap-northeast-1.amazonaws.com/prod/inputSecurityGroups");
        assert!(c.header("authorization").unwrap().contains("/ap-northeast-1/medialive/aws4_request"));
        assert_eq!(
            body(&c),
            json!({"whitelistRules": [{"cidr": "203.0.113.10/32"}, {"cidr": "198.51.100.0/24"}],
                   "tags": {"senca:owner": "o9", "senca:generation": "4"}})
        );
        let (c, cp) = call_of(op.step(Some(&HttpReply::new(200, r#"{"securityGroup":{"id":"5551","state":"IDLE"}}"#)), 0).unwrap());
        assert_eq!(cp.as_deref(), Some("sg=5551"));
        assert_eq!(c.target(), "/prod/inputs");
        assert_eq!(
            body(&c),
            json!({"name": "senca-o9", "type": "RTMP_PUSH", "inputSecurityGroups": ["5551"], "requestId": "o9-4",
                   "destinations": [{"streamName": "senca/senca-o9-a"}, {"streamName": "senca/senca-o9-b"}],
                   "tags": {"senca:owner": "o9", "senca:generation": "4"}})
        );
        let reply = json!({"input": {"id": "777", "type": "RTMP_PUSH", "state": "DETACHED", "securityGroups": ["5551"],
            "destinations": [{"url": "rtmp://198.51.100.1:1935/senca/senca-o9-a", "ip": "198.51.100.1", "port": "1935"},
                             {"url": "rtmp://198.51.100.2:1935/senca/senca-o9-b"}], "tags": {"senca:owner": "o9"}}});
        let Step::Done(r) = op.step(Some(&HttpReply::new(201, reply.to_string())), 0).unwrap() else { panic!() };
        assert_eq!(r.resource_id, "input=777;sg=5551");
        assert_eq!(r.ingest.len(), 2);
        let t = r.ingest[0].push_target();
        assert_eq!((t.runner, t.url.expose(), t.credential.unwrap().expose()), ("rtmp_push", "rtmp://198.51.100.1:1935/senca", "senca-o9-a"));
    }

    #[test]
    fn create_rolls_back_security_group() {
        let p = ml(&[]);
        let mut op = p.create(&CreateSpec::for_output("o9", 1, None));
        op.step(None, 0).unwrap();
        op.step(Some(&HttpReply::new(200, r#"{"securityGroup":{"id":"5551"}}"#)), 0).unwrap();
        let bad = HttpReply::new(422, r#"{"message":"Input name too long"}"#).with_header("x-amzn-ErrorType", "UnprocessableEntityException");
        let (c, _) = call_of(op.step(Some(&bad), 0).unwrap());
        assert_eq!((c.method.as_str(), c.target().as_str()), ("DELETE", "/prod/inputSecurityGroups/5551"));
        let e = op.step(Some(&HttpReply::new(200, "{}")), 0).unwrap_err();
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::Invalid, "Input name too long"));
    }

    #[test]
    fn existing_security_group_is_not_owned() {
        let p = ml(&[("input_security_group", "42")]);
        let mut op = p.create(&CreateSpec::for_output("o1", 1, None));
        let (c, cp) = call_of(op.step(None, 0).unwrap());
        assert_eq!((c.target().as_str(), cp), ("/prod/inputs", None));
        let Step::Done(r) = op.step(Some(&HttpReply::new(201, r#"{"input":{"id":"8","destinations":[]}}"#)), 0).unwrap() else { panic!() };
        assert_eq!(r.resource_id, "input=8");
    }

    #[test]
    fn srt_listener_body() {
        let p = ml(&[("input_type", "SRT_LISTENER"), ("srt_passphrase_secret_arn", "arn:aws:secretsmanager:ap-northeast-1:1:secret:x")]);
        let b = p.input_body(&CreateSpec::for_output("o1", 1, None), "9");
        assert_eq!(b["type"], "SRT_LISTENER");
        assert_eq!(b["srtListenerSettings"]["decryption"]["algorithm"], "AES256");
        assert!(b.get("destinations").is_none());
    }

    #[test]
    fn delete_input_then_sg() {
        let p = ml(&[]);
        let mut op = p.delete("input=777;sg=5551");
        let (c, _) = call_of(op.step(None, 0).unwrap());
        assert_eq!((c.method.as_str(), c.target().as_str()), ("DELETE", "/prod/inputs/777"));
        let (c, _) = call_of(op.step(Some(&HttpReply::new(200, "{}")), 0).unwrap());
        assert_eq!(c.target(), "/prod/inputSecurityGroups/5551");
        let in_use = HttpReply::new(409, r#"{"message":"in use"}"#).with_header("x-amzn-ErrorType", "ConflictException");
        assert!(op.step(Some(&in_use), 0).unwrap_err().is_retryable());
        // 再試行: Input は既に無い
        let mut op = p.delete("input=777;sg=5551");
        op.step(None, 0).unwrap();
        let nf = HttpReply::new(404, r#"{"message":"nope"}"#).with_header("x-amzn-ErrorType", "NotFoundException");
        op.step(Some(&nf), 0).unwrap();
        assert!(matches!(op.step(Some(&HttpReply::new(200, "{}")), 0).unwrap(), Step::Done(DeleteOutcome::Deleted)));
        // Channel につながった Input
        let mut op = p.delete("input=777");
        op.step(None, 0).unwrap();
        let e = op.step(Some(&HttpReply::new(409, r#"{"__type":"ConflictException","message":"attached"}"#)), 0).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Conflict);
        assert!(!e.is_retryable());
    }

    #[test]
    fn list_joins_inputs_and_groups() {
        let p = ml(&[]);
        let mut op = p.list_by_tag(None);
        let (c, _) = call_of(op.step(None, 0).unwrap());
        assert_eq!(c.target(), "/prod/inputs?maxResults=100");
        let inputs = json!({"inputs": [
            {"id": "1", "securityGroups": ["10"], "tags": {"senca:owner": "a"}},
            {"id": "2", "securityGroups": ["99"], "tags": {"senca:owner": "b", "senca:cluster": "k"}},
            {"id": "3", "securityGroups": ["30"], "tags": {}}], "nextToken": "N"});
        let (c, _) = call_of(op.step(Some(&HttpReply::new(200, inputs.to_string())), 0).unwrap());
        assert_eq!(c.target(), "/prod/inputs?maxResults=100&nextToken=N");
        let (c, _) = call_of(op.step(Some(&HttpReply::new(200, r#"{"inputs":[]}"#)), 0).unwrap());
        assert_eq!(c.target(), "/prod/inputSecurityGroups?maxResults=100");
        let sgs = json!({"inputSecurityGroups": [
            {"id": "10", "inputs": ["1"], "tags": {"senca:owner": "a"}},
            {"id": "20", "inputs": [], "tags": {"senca:owner": "c"}},
            {"id": "30", "inputs": ["3"], "tags": {"senca:owner": "d"}},
            {"id": "40", "tags": {}}]});
        let Step::Done(l) = op.step(Some(&HttpReply::new(200, sgs.to_string())), 0).unwrap() else { panic!() };
        let got: Vec<(&str, &str, bool)> = l.iter().map(|t| (t.resource_id.as_str(), t.owner.as_str(), t.complete)).collect();
        assert_eq!(got, [("input=1;sg=10", "a", true), ("input=2", "b", true), ("sg=20", "c", false)]);
    }

    #[test]
    fn options_validation() {
        let mut o = BTreeMap::new();
        o.insert("region".to_string(), "us-east-1".to_string());
        let cred = Secret::new("A:B");
        assert!(MediaLive::from_options(&cred, &o).is_err(), "allow_cidr か input_security_group が要る");
        o.insert("allow_cidr".to_string(), "10.0.0.1".to_string());
        assert!(MediaLive::from_options(&cred, &o).is_err());
        o.insert("allow_cidr".to_string(), "10.0.0.0/8".to_string());
        assert!(MediaLive::from_options(&cred, &o).is_ok());
        o.insert("input_type".to_string(), "UDP_PUSH".to_string());
        assert!(MediaLive::from_options(&cred, &o).is_err());
    }
}
