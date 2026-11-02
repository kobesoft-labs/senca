//! Amazon IVS（低遅延ストリーミング）の Channel。`https://ivs.{region}.amazonaws.com` の REST-JSON
//! （どの操作も POST）。SigV4 の service は `ivs`。
//!
//! - 作成: `CreateChannel`（`tags` に `senca:owner` など）。応答の `streamKey.value` が stream key。
//! - 送り先: `rtmps://{ingestEndpoint}:443/app/` + stream key。channel に `srt` があれば SRT も
//!   （`srt://{srt.endpoint}:9000`、streamid = stream key、passphrase = `srt.passphrase`）。
//! - 取得: `GetChannel` → `ListStreamKeys` → `GetStreamKey`（stream key は channel の応答に含まれない）。
//! - 一覧: `ListChannels`（ページ送り）。各要素の `tags` で SENCA の資源を見分ける。`filterByName` は
//!   名前を変えた資源を取りこぼすので使わない。
//! - 削除: `DeleteChannel`。配信中は `ConflictException` になるので、送出を止めてから再試行する。
//!
//! options: `region`（必須）、`channel_type`（`STANDARD` 既定・`BASIC`・`ADVANCED_SD`・`ADVANCED_HD`）、
//! `latency_mode`（`LOW` 既定・`NORMAL`）、`recording_configuration_arn`、`endpoint`（試験用）。

use crate::aws::{AwsClient, aws_error, aws_json};
use crate::call::HttpReply;
use crate::error::{ErrorKind, PaasError};
use crate::provider::{
    Capabilities, CreateSpec, CreatedResource, DeleteOutcome, Ingest, IngestProtocol, Op, OneShot, PaasProvider, Playback, Ready, Step,
    TaggedResource, finished, json_str, misuse, need, owner_of, valid_id,
};
use crate::secret::Secret;
use senca_http::Method;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const NAME: &str = "aws_ivs";
/// 一覧のページ数の上限（1 ページ 100 件）。
const MAX_PAGES: usize = 100;

#[derive(Debug, Clone)]
pub struct Ivs {
    client: AwsClient,
    channel_type: String,
    latency_mode: String,
    recording_configuration_arn: Option<String>,
}

fn valid_arn(s: &str) -> bool {
    s.starts_with("arn:") && valid_id(s)
}

impl Ivs {
    pub fn from_options(credential: &Secret, options: &BTreeMap<String, String>) -> Result<Self, PaasError> {
        let client = AwsClient::from_options(credential, options, "ivs")?;
        let channel_type = options.get("channel_type").cloned().unwrap_or_else(|| "STANDARD".into());
        if !["STANDARD", "BASIC", "ADVANCED_SD", "ADVANCED_HD"].contains(&channel_type.as_str()) {
            return Err(PaasError::config("channel_type は STANDARD・BASIC・ADVANCED_SD・ADVANCED_HD"));
        }
        let latency_mode = options.get("latency_mode").cloned().unwrap_or_else(|| "LOW".into());
        if !["LOW", "NORMAL"].contains(&latency_mode.as_str()) {
            return Err(PaasError::config("latency_mode は LOW か NORMAL"));
        }
        let recording_configuration_arn = options.get("recording_configuration_arn").cloned();
        if recording_configuration_arn.as_deref().is_some_and(|a| !valid_arn(a)) {
            return Err(PaasError::config("recording_configuration_arn が不正"));
        }
        Ok(Ivs { client, channel_type, latency_mode, recording_configuration_arn })
    }

    /// `CreateChannel` の body（試験で形を確かめるため公開）。
    pub fn create_body(&self, spec: &CreateSpec) -> Value {
        let mut b = json!({
            "name": spec.safe_name(),
            "latencyMode": self.latency_mode,
            "type": self.channel_type,
            "tags": spec.all_tags(),
        });
        if let Some(a) = &self.recording_configuration_arn {
            b["recordingConfigurationArn"] = Value::String(a.clone());
        }
        b
    }

    /// channel の JSON と stream key から受け口を作る。
    fn resource(channel: &Value, stream_key: Option<&str>) -> Result<CreatedResource, PaasError> {
        let arn = json_str(channel, "arn").filter(|a| valid_arn(a)).ok_or_else(|| PaasError::malformed("channel に arn が無い"))?;
        let mut ingest = Vec::new();
        if let Some(ep) = json_str(channel, "ingestEndpoint") {
            ingest.push(Ingest {
                protocol: IngestProtocol::Rtmps,
                url: format!("rtmps://{ep}:443/app/"),
                stream_key: stream_key.map(Secret::new),
                stream_id: None,
                passphrase: None,
            });
        }
        if let Some(srt) = channel.get("srt")
            && let Some(ep) = json_str(srt, "endpoint")
        {
            ingest.push(Ingest {
                protocol: IngestProtocol::Srt,
                url: format!("srt://{ep}:9000"),
                stream_key: None,
                stream_id: stream_key.map(Secret::new),
                passphrase: json_str(srt, "passphrase").map(Secret::new),
            });
        }
        let playback = json_str(channel, "playbackUrl").map(|u| vec![Playback { kind: "hls", url: u.to_string() }]).unwrap_or_default();
        Ok(CreatedResource { resource_id: arn.to_string(), ingest, playback, owner: owner_of(channel.get("tags")).map(|(o, _)| o) })
    }
}

impl PaasProvider for Ivs {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { provider: NAME, ingest: &[IngestProtocol::Rtmps, IngestProtocol::Srt], video_codecs: &["h264"], audio_codecs: &["aac"] }
    }

    fn create(&self, spec: &CreateSpec) -> Box<dyn Op<CreatedResource>> {
        if let Err(e) = spec.validate() {
            return Box::new(Ready(Some(Err(e))));
        }
        let body = self.create_body(spec);
        let client = self.client.clone();
        Box::new(OneShot::new(
            move |now| Ok(client.call(Method::Post, "/CreateChannel", &[], Some(&body), now)),
            |r| {
                let v = aws_json(r)?;
                let channel = v.get("channel").ok_or_else(|| PaasError::malformed("CreateChannel の応答に channel が無い"))?;
                let key = v.get("streamKey").and_then(|k| json_str(k, "value"));
                Ivs::resource(channel, key)
            },
        ))
    }

    fn get(&self, resource_id: &str) -> Box<dyn Op<Option<CreatedResource>>> {
        if !valid_arn(resource_id) {
            return Box::new(Ready(Some(Err(PaasError::config("channel の arn が不正")))));
        }
        Box::new(GetOp { client: self.client.clone(), arn: resource_id.to_string(), state: GetState::Start })
    }

    fn delete(&self, resource_id: &str) -> Box<dyn Op<DeleteOutcome>> {
        if !valid_arn(resource_id) {
            return Box::new(Ready(Some(Err(PaasError::config("channel の arn が不正")))));
        }
        let client = self.client.clone();
        let body = json!({ "arn": resource_id });
        Box::new(OneShot::new(
            move |now| Ok(client.call(Method::Post, "/DeleteChannel", &[], Some(&body), now)),
            |r| {
                if r.is_success() {
                    return Ok(DeleteOutcome::Deleted);
                }
                let e = aws_error(r);
                match e.kind {
                    ErrorKind::NotFound => Ok(DeleteOutcome::AlreadyGone),
                    // 配信中（送出を止めれば消せる）
                    ErrorKind::Conflict => Err(e.with_kind(ErrorKind::Retryable)),
                    _ => Err(e),
                }
            },
        ))
    }

    fn list_by_tag(&self, owner: Option<&str>) -> Box<dyn Op<Vec<TaggedResource>>> {
        Box::new(ListOp { client: self.client.clone(), owner: owner.map(str::to_string), next: None, pages: 0, out: Vec::new(), started: false })
    }
}

enum GetState {
    Start,
    Channel,
    Keys(Value),
    Key(Value),
    Done,
}

struct GetOp {
    client: AwsClient,
    arn: String,
    state: GetState,
}

impl Op<Option<CreatedResource>> for GetOp {
    fn step(&mut self, reply: Option<&HttpReply>, now: u64) -> Result<Step<Option<CreatedResource>>, PaasError> {
        let call = |path: &str, body: Value| Step::Call { call: self.client.call(Method::Post, path, &[], Some(&body), now), checkpoint: None };
        match std::mem::replace(&mut self.state, GetState::Done) {
            GetState::Start => {
                if reply.is_some() {
                    return Err(misuse());
                }
                self.state = GetState::Channel;
                Ok(call("/GetChannel", json!({ "arn": self.arn })))
            }
            GetState::Channel => {
                let r = need(reply)?;
                if !r.is_success() {
                    let e = aws_error(r);
                    return if e.kind == ErrorKind::NotFound { Ok(Step::Done(None)) } else { Err(e) };
                }
                let v = r.json()?;
                let ch = v.get("channel").cloned().ok_or_else(|| PaasError::malformed("GetChannel の応答に channel が無い"))?;
                let s = call("/ListStreamKeys", json!({ "channelArn": self.arn }));
                self.state = GetState::Keys(ch);
                Ok(s)
            }
            GetState::Keys(ch) => {
                let v = aws_json(need(reply)?)?;
                let key_arn = v.get("streamKeys").and_then(Value::as_array).and_then(|a| a.first()).and_then(|k| json_str(k, "arn"));
                match key_arn {
                    Some(a) if valid_arn(a) => {
                        let s = call("/GetStreamKey", json!({ "arn": a }));
                        self.state = GetState::Key(ch);
                        Ok(s)
                    }
                    _ => Ivs::resource(&ch, None).map(|r| Step::Done(Some(r))),
                }
            }
            GetState::Key(ch) => {
                let v = aws_json(need(reply)?)?;
                let key = v.get("streamKey").and_then(|k| json_str(k, "value"));
                Ivs::resource(&ch, key).map(|r| Step::Done(Some(r)))
            }
            GetState::Done => Err(finished()),
        }
    }
}

struct ListOp {
    client: AwsClient,
    owner: Option<String>,
    next: Option<String>,
    pages: usize,
    out: Vec<TaggedResource>,
    started: bool,
}

impl ListOp {
    fn request(&mut self, now: u64) -> Step<Vec<TaggedResource>> {
        let mut body = json!({ "maxResults": 100 });
        if let Some(t) = &self.next {
            body["nextToken"] = Value::String(t.clone());
        }
        self.pages += 1;
        Step::Call { call: self.client.call(Method::Post, "/ListChannels", &[], Some(&body), now), checkpoint: None }
    }
}

impl Op<Vec<TaggedResource>> for ListOp {
    fn step(&mut self, reply: Option<&HttpReply>, now: u64) -> Result<Step<Vec<TaggedResource>>, PaasError> {
        if !self.started {
            if reply.is_some() {
                return Err(misuse());
            }
            self.started = true;
            return Ok(self.request(now));
        }
        let v = aws_json(need(reply)?)?;
        let chans = v.get("channels").and_then(Value::as_array).ok_or_else(|| PaasError::malformed("ListChannels の応答に channels が無い"))?;
        for c in chans {
            let Some(arn) = json_str(c, "arn").filter(|a| valid_arn(a)) else { continue };
            let Some((o, cluster)) = owner_of(c.get("tags")) else { continue };
            if self.owner.as_deref().is_none_or(|w| w == o) {
                self.out.push(TaggedResource { resource_id: arn.to_string(), owner: o, cluster, complete: true });
            }
        }
        self.next = json_str(&v, "nextToken").map(str::to_string);
        if self.next.is_some() {
            if self.pages >= MAX_PAGES {
                return Err(PaasError::malformed("ListChannels のページが多すぎる"));
            }
            return Ok(self.request(now));
        }
        Ok(Step::Done(std::mem::take(&mut self.out)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivs() -> Ivs {
        let mut o = BTreeMap::new();
        o.insert("region".into(), "us-west-2".into());
        Ivs::from_options(&Secret::new("AKIDTEST:SECRETKEY:SESSIONTOKEN"), &o).unwrap()
    }

    fn call_of<T>(s: Step<T>) -> crate::HttpCall {
        match s {
            Step::Call { call, .. } => call,
            Step::Done(_) => panic!("expected call"),
        }
    }

    #[test]
    fn create_request_and_reply() {
        let p = ivs();
        let mut op = p.create(&CreateSpec::for_output("o1", 1, None));
        let c = call_of(op.step(None, 1_700_000_000).unwrap());
        assert_eq!(c.url(), "https://ivs.us-west-2.amazonaws.com/CreateChannel");
        assert_eq!(c.method, Method::Post);
        assert_eq!(c.header("x-amz-date"), Some("20231114T221320Z"));
        assert_eq!(c.header("x-amz-security-token"), Some("SESSIONTOKEN"));
        assert!(c.header("authorization").unwrap().starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/20231114/us-west-2/ivs/aws4_request"));
        let body: Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(
            body,
            json!({"name": "senca-o1", "latencyMode": "LOW", "type": "STANDARD", "tags": {"senca:owner": "o1", "senca:generation": "1"}})
        );
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("SESSIONTOKEN") && !dbg.contains("SECRETKEY"));

        let reply = json!({
            "channel": {"arn": "arn:aws:ivs:us-west-2:123456789012:channel/abcd1234", "ingestEndpoint": "abc.global-contribute.live-video.net",
                        "playbackUrl": "https://abc.us-west-2.playback.live-video.net/api/video/v1/x.m3u8", "tags": {"senca:owner": "o1"},
                        "srt": {"endpoint": "abc.srt.live-video.net", "passphrase": "SRTPASS"}},
            "streamKey": {"arn": "arn:aws:ivs:us-west-2:123456789012:stream-key/k1", "value": "sk_us-west-2_SECRETKEYVALUE"}});
        let Step::Done(r) = op.step(Some(&HttpReply::new(200, reply.to_string())), 0).unwrap() else { panic!() };
        assert_eq!(r.resource_id, "arn:aws:ivs:us-west-2:123456789012:channel/abcd1234");
        let t = r.ingest[0].push_target();
        assert_eq!(t.url.expose(), "rtmps://abc.global-contribute.live-video.net:443/app");
        assert_eq!(t.credential.unwrap().expose(), "sk_us-west-2_SECRETKEYVALUE");
        let s = r.ingest_for(Some(IngestProtocol::Srt)).unwrap().push_target();
        assert_eq!(s.url.expose(), "srt://abc.srt.live-video.net:9000?streamid=sk_us-west-2_SECRETKEYVALUE");
        assert_eq!(r.playback[0].kind, "hls");
        assert!(!format!("{r:?}").contains("SECRETKEYVALUE"));
    }

    #[test]
    fn get_is_three_calls() {
        let p = ivs();
        let arn = "arn:aws:ivs:us-west-2:1:channel/c";
        let mut op = p.get(arn);
        let c = call_of(op.step(None, 0).unwrap());
        assert_eq!(c.target(), "/GetChannel");
        let ch = json!({"channel": {"arn": arn, "ingestEndpoint": "e"}});
        let c = call_of(op.step(Some(&HttpReply::new(200, ch.to_string())), 0).unwrap());
        assert_eq!(c.target(), "/ListStreamKeys");
        assert_eq!(serde_json::from_slice::<Value>(&c.body).unwrap(), json!({"channelArn": arn}));
        let keys = json!({"streamKeys": [{"arn": "arn:aws:ivs:us-west-2:1:stream-key/k"}]});
        let c = call_of(op.step(Some(&HttpReply::new(200, keys.to_string())), 0).unwrap());
        assert_eq!(c.target(), "/GetStreamKey");
        let key = json!({"streamKey": {"value": "KEYV"}});
        let Step::Done(Some(r)) = op.step(Some(&HttpReply::new(200, key.to_string())), 0).unwrap() else { panic!() };
        assert_eq!(r.ingest[0].stream_key.as_ref().unwrap().expose(), "KEYV");

        let mut op = p.get(arn);
        op.step(None, 0).unwrap();
        let nf = HttpReply::new(404, r#"{"exceptionMessage":"x"}"#).with_header("x-amzn-ErrorType", "ResourceNotFoundException");
        assert!(matches!(op.step(Some(&nf), 0).unwrap(), Step::Done(None)));
    }

    #[test]
    fn delete_and_list() {
        let p = ivs();
        let arn = "arn:aws:ivs:us-west-2:1:channel/c";
        let mut op = p.delete(arn);
        let c = call_of(op.step(None, 0).unwrap());
        assert_eq!(c.target(), "/DeleteChannel");
        assert!(matches!(op.step(Some(&HttpReply::new(204, "")), 0).unwrap(), Step::Done(DeleteOutcome::Deleted)));
        let mut op = p.delete(arn);
        op.step(None, 0).unwrap();
        let conflict = HttpReply::new(409, r#"{"__type":"ConflictException","exceptionMessage":"live"}"#);
        assert!(op.step(Some(&conflict), 0).unwrap_err().is_retryable());

        let mut op = p.list_by_tag(None);
        let c = call_of(op.step(None, 0).unwrap());
        assert_eq!(serde_json::from_slice::<Value>(&c.body).unwrap(), json!({"maxResults": 100}));
        let page1 = json!({"channels": [{"arn": "arn:aws:ivs:r:1:channel/a", "tags": {"senca:owner": "o1"}},
                                         {"arn": "arn:aws:ivs:r:1:channel/u", "tags": {}}], "nextToken": "T2"});
        let c = call_of(op.step(Some(&HttpReply::new(200, page1.to_string())), 0).unwrap());
        assert_eq!(serde_json::from_slice::<Value>(&c.body).unwrap(), json!({"maxResults": 100, "nextToken": "T2"}));
        let page2 = json!({"channels": [{"arn": "arn:aws:ivs:r:1:channel/b", "tags": {"senca:owner": "o2", "senca:cluster": "k"}}]});
        let Step::Done(l) = op.step(Some(&HttpReply::new(200, page2.to_string())), 0).unwrap() else { panic!() };
        assert_eq!(l.iter().map(|t| t.resource_id.as_str()).collect::<Vec<_>>(), ["arn:aws:ivs:r:1:channel/a", "arn:aws:ivs:r:1:channel/b"]);
        assert_eq!(l[1].cluster.as_deref(), Some("k"));
    }

    #[test]
    fn options_validation() {
        let mut o = BTreeMap::new();
        o.insert("region".into(), "us-west-2".into());
        o.insert("latency_mode".into(), "ULTRA".into());
        assert!(Ivs::from_options(&Secret::new("A:B"), &o).is_err());
        assert!(ivs().delete("not-an-arn").step(None, 0).is_err());
    }
}
