//! Cloudflare Stream の Live Input（`/client/v4/accounts/{account_id}/stream/live_inputs`）。
//!
//! - 認証: API トークン（`Authorization: Bearer`）。資格情報は `account_id:api_token`。
//! - 冪等: `meta` に `senca:owner=<出力 ID>` などを入れる。一覧（GET live_inputs）で同じ owner を探す。
//! - 送り先: RTMPS（url + streamKey）、SRT（url + streamId + passphrase）、WHIP（URL そのものが鍵）。
//!
//! options:
//! - `recording`: `off`（既定）か `automatic`
//! - `customer_subdomain`: `customer-xxxx.cloudflarestream.com`（HLS・DASH の再生 URL を作るため）
//! - `endpoint`: API の接続先の上書き（試験用）

use crate::call::{Endpoint, HttpCall, HttpReply};
use crate::error::{ErrorKind, PaasError, classify_status, sanitize};
use crate::provider::{
    Capabilities, CreateSpec, CreatedResource, DeleteOutcome, Ingest, IngestProtocol, Op, OneShot, PaasProvider, Playback, Ready,
    TaggedResource, json_str, owner_of,
};
use crate::secret::Secret;
use senca_http::Method;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const NAME: &str = "cloudflare_stream";
pub const API_HOST: &str = "api.cloudflare.com";

#[derive(Clone)]
pub struct CloudflareStream {
    account_id: String,
    token: Secret,
    endpoint: Endpoint,
    recording: &'static str,
    customer_host: Option<String>,
}

impl std::fmt::Debug for CloudflareStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareStream").field("account_id", &self.account_id).field("endpoint", &self.endpoint).finish_non_exhaustive()
    }
}

impl CloudflareStream {
    pub fn new(account_id: &str, token: Secret) -> Result<Self, PaasError> {
        if account_id.is_empty() || account_id.len() > 64 || !account_id.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return Err(PaasError::config("Cloudflare の account_id が不正"));
        }
        if token.is_empty() || !token.expose().bytes().all(|c| c.is_ascii_graphic()) {
            return Err(PaasError::config("Cloudflare の API トークンが不正"));
        }
        Ok(CloudflareStream { account_id: account_id.into(), token, endpoint: Endpoint::https(API_HOST), recording: "off", customer_host: None })
    }

    /// 資格情報 `account_id:api_token` と options から作る。
    pub fn from_options(credential: &Secret, options: &BTreeMap<String, String>) -> Result<Self, PaasError> {
        let (acct, tok) =
            credential.expose().split_once(':').ok_or_else(|| PaasError::config("Cloudflare の資格情報は account_id:api_token"))?;
        let mut p = Self::new(acct, Secret::new(tok))?;
        if let Some(e) = options.get("endpoint") {
            p.endpoint = Endpoint::parse(e)?;
        }
        p.recording = match options.get("recording").map(String::as_str) {
            None | Some("off") => "off",
            Some("automatic") => "automatic",
            _ => return Err(PaasError::config("recording は off か automatic")),
        };
        if let Some(h) = options.get("customer_subdomain") {
            if h.is_empty() || !h.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-') {
                return Err(PaasError::config("customer_subdomain が不正"));
            }
            p.customer_host = Some(h.clone());
        }
        Ok(p)
    }

    pub fn with_endpoint(mut self, e: Endpoint) -> Self {
        self.endpoint = e;
        self
    }

    fn base(&self) -> String {
        format!("/client/v4/accounts/{}/stream/live_inputs", self.account_id)
    }

    fn call(&self, method: Method, path: String) -> HttpCall {
        let mut c = HttpCall::new(method, self.endpoint.clone(), path);
        c.set_header("Authorization", format!("Bearer {}", self.token.expose()));
        c.set_header("Accept", "application/json");
        c
    }

    /// 作成の要求（試験で形を確かめるため公開）。
    pub fn create_call(&self, spec: &CreateSpec) -> HttpCall {
        let mut meta = serde_json::Map::new();
        meta.insert("name".into(), Value::String(spec.safe_name()));
        for (k, v) in spec.all_tags() {
            meta.insert(k, Value::String(v));
        }
        self.call(Method::Post, self.base()).json(&json!({
            "meta": meta,
            "recording": { "mode": self.recording },
        }))
    }

    /// Live Input の JSON（`result`）を受け口にする。
    pub fn parse_input(&self, r: &Value) -> Result<CreatedResource, PaasError> {
        let uid = json_str(r, "uid").filter(|u| valid_uid(u)).ok_or_else(|| PaasError::malformed("Live Input に uid が無い"))?;
        let mut ingest = Vec::new();
        if let Some(x) = r.get("rtmps")
            && let Some(url) = json_str(x, "url")
        {
            ingest.push(Ingest {
                protocol: IngestProtocol::Rtmps,
                url: url.to_string(),
                stream_key: json_str(x, "streamKey").map(Secret::new),
                stream_id: None,
                passphrase: None,
            });
        }
        if let Some(x) = r.get("srt")
            && let Some(url) = json_str(x, "url")
        {
            ingest.push(Ingest {
                protocol: IngestProtocol::Srt,
                url: url.to_string(),
                stream_key: None,
                stream_id: json_str(x, "streamId").map(Secret::new),
                passphrase: json_str(x, "passphrase").map(Secret::new),
            });
        }
        if let Some(url) = r.get("webRTC").and_then(|x| json_str(x, "url")) {
            ingest.push(Ingest {
                protocol: IngestProtocol::Whip,
                url: origin(url),
                stream_key: Some(Secret::new(url)),
                stream_id: None,
                passphrase: None,
            });
        }
        let mut playback = Vec::new();
        if let Some(h) = &self.customer_host {
            playback.push(Playback { kind: "hls", url: format!("https://{h}/{uid}/manifest/video.m3u8") });
            playback.push(Playback { kind: "dash", url: format!("https://{h}/{uid}/manifest/video.mpd") });
        }
        if let Some(url) = r.get("webRTCPlayback").and_then(|x| json_str(x, "url")) {
            playback.push(Playback { kind: "whep", url: url.to_string() });
        }
        Ok(CreatedResource { resource_id: uid.to_string(), ingest, playback, owner: owner_of(r.get("meta")).map(|(o, _)| o) })
    }
}

fn valid_uid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// `https://host/…` の `https://host`。
fn origin(url: &str) -> String {
    match url.split_once("://") {
        Some((s, rest)) => format!("{s}://{}", rest.split('/').next().unwrap_or("")),
        None => String::new(),
    }
}

/// Cloudflare の応答を確かめ、`result` を返す。誤りの封筒（`success:false, errors[]`）を誤りにする。
pub fn check_envelope(reply: &HttpReply) -> Result<Value, PaasError> {
    let v = reply.json().unwrap_or(Value::Null);
    let success = v.get("success").and_then(Value::as_bool);
    if reply.is_success() && success != Some(false) {
        if success.is_none() && !v.is_null() {
            return Err(PaasError::malformed("Cloudflare の応答に success が無い"));
        }
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
    let first = v.get("errors").and_then(Value::as_array).and_then(|a| a.first());
    let code = first.and_then(|e| e.get("code")).and_then(Value::as_i64);
    let msg = first.and_then(|e| json_str(e, "message")).unwrap_or("");
    let mut kind = if reply.is_success() { ErrorKind::Invalid } else { classify_status(reply.status) };
    // 10000: Authentication error
    if code == Some(10000) {
        kind = ErrorKind::Unauthorized;
    }
    Err(PaasError { kind, status: Some(reply.status), code: code.map(|c| c.to_string()), message: sanitize(msg) })
}

impl PaasProvider for CloudflareStream {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            provider: NAME,
            ingest: &[IngestProtocol::Rtmps, IngestProtocol::Srt, IngestProtocol::Whip],
            video_codecs: &["h264"],
            audio_codecs: &["aac", "opus"],
        }
    }

    fn create(&self, spec: &CreateSpec) -> Box<dyn Op<CreatedResource>> {
        if let Err(e) = spec.validate() {
            return Box::new(Ready(Some(Err(e))));
        }
        let call = self.create_call(spec);
        let me = self.clone();
        Box::new(OneShot::new(move |_| Ok(call), move |r| me.parse_input(&check_envelope(r)?)))
    }

    fn get(&self, resource_id: &str) -> Box<dyn Op<Option<CreatedResource>>> {
        if !valid_uid(resource_id) {
            return Box::new(Ready(Some(Err(PaasError::config("Live Input の uid が不正")))));
        }
        let call = self.call(Method::Get, format!("{}/{resource_id}", self.base()));
        let me = self.clone();
        Box::new(OneShot::new(
            move |_| Ok(call),
            move |r| match check_envelope(r) {
                Ok(v) => me.parse_input(&v).map(Some),
                Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            },
        ))
    }

    fn delete(&self, resource_id: &str) -> Box<dyn Op<DeleteOutcome>> {
        if !valid_uid(resource_id) {
            return Box::new(Ready(Some(Err(PaasError::config("Live Input の uid が不正")))));
        }
        let call = self.call(Method::Delete, format!("{}/{resource_id}", self.base()));
        Box::new(OneShot::new(
            move |_| Ok(call),
            |r| match check_envelope(r) {
                Ok(_) => Ok(DeleteOutcome::Deleted),
                Err(e) if e.kind == ErrorKind::NotFound => Ok(DeleteOutcome::AlreadyGone),
                Err(e) => Err(e),
            },
        ))
    }

    fn list_by_tag(&self, owner: Option<&str>) -> Box<dyn Op<Vec<TaggedResource>>> {
        let call = self.call(Method::Get, self.base());
        let owner = owner.map(str::to_string);
        Box::new(OneShot::new(
            move |_| Ok(call),
            move |r| {
                let v = check_envelope(r)?;
                // result は配列か `{ "liveInputs": [...] }`
                let items = v.as_array().or_else(|| v.get("liveInputs").and_then(Value::as_array));
                let items = items.ok_or_else(|| PaasError::malformed("Live Input の一覧が無い"))?;
                Ok(items
                    .iter()
                    .filter_map(|i| {
                        let uid = json_str(i, "uid").filter(|u| valid_uid(u))?;
                        let (o, cluster) = owner_of(i.get("meta"))?;
                        (owner.is_none() || owner.as_deref() == Some(o.as_str())).then(|| TaggedResource {
                            resource_id: uid.to_string(),
                            owner: o,
                            cluster,
                            complete: true,
                        })
                    })
                    .collect())
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Step;

    fn cf() -> CloudflareStream {
        let mut o = BTreeMap::new();
        o.insert("customer_subdomain".into(), "customer-abc.cloudflarestream.com".into());
        CloudflareStream::from_options(&Secret::new("0123abcd:TOKEN-xyz"), &o).unwrap()
    }

    fn first_call<T>(op: &mut dyn Op<T>) -> HttpCall {
        match op.step(None, 0).unwrap() {
            Step::Call { call, .. } => call,
            Step::Done(_) => panic!("expected call"),
        }
    }

    #[test]
    fn create_request_shape() {
        let p = cf();
        let spec = CreateSpec::for_output("out1", 2, Some("clu"));
        let c = first_call(&mut *p.create(&spec));
        assert_eq!(c.method, Method::Post);
        assert_eq!(c.url(), "https://api.cloudflare.com/client/v4/accounts/0123abcd/stream/live_inputs");
        assert_eq!(c.header("authorization"), Some("Bearer TOKEN-xyz"));
        assert_eq!(c.header("content-type"), Some("application/json"));
        let body: Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(
            body,
            json!({"meta": {"name": "senca-out1", "senca:owner": "out1", "senca:cluster": "clu", "senca:generation": "2"},
                   "recording": {"mode": "off"}})
        );
        assert!(!format!("{c:?}").contains("TOKEN-xyz"));
        assert!(!format!("{p:?}").contains("TOKEN-xyz"));
    }

    #[test]
    fn parse_create_reply() {
        let p = cf();
        let mut op = p.create(&CreateSpec::for_output("out1", 1, None));
        first_call(&mut *op);
        let body = json!({"success": true, "errors": [], "messages": [], "result": {
            "uid": "1a2b3c", "meta": {"senca:owner": "out1"},
            "rtmps": {"url": "rtmps://live.cloudflare.com:443/live/", "streamKey": "SKEY"},
            "srt": {"url": "srt://live.cloudflare.com:778", "streamId": "1a2b3c", "passphrase": "SPASS"},
            "webRTC": {"url": "https://customer-abc.cloudflarestream.com/WKEY/webRTC/publish"},
            "webRTCPlayback": {"url": "https://customer-abc.cloudflarestream.com/1a2b3c/webRTC/play"}}});
        let Step::Done(r) = op.step(Some(&HttpReply::new(200, body.to_string())), 0).unwrap() else { panic!() };
        assert_eq!(r.resource_id, "1a2b3c");
        assert_eq!(r.owner.as_deref(), Some("out1"));
        assert_eq!(r.ingest.len(), 3);
        let rtmps = r.ingest_for(Some(IngestProtocol::Rtmps)).unwrap();
        assert_eq!(rtmps.stream_key.as_ref().unwrap().expose(), "SKEY");
        let srt = r.ingest_for(Some(IngestProtocol::Srt)).unwrap().push_target();
        assert_eq!(srt.url.expose(), "srt://live.cloudflare.com:778?streamid=1a2b3c");
        assert_eq!(srt.credential.unwrap().expose(), "SPASS");
        let whip = r.ingest_for(Some(IngestProtocol::Whip)).unwrap();
        assert_eq!(whip.url, "https://customer-abc.cloudflarestream.com");
        assert!(whip.push_target().url.expose().ends_with("/WKEY/webRTC/publish"));
        assert_eq!(r.playback[0].url, "https://customer-abc.cloudflarestream.com/1a2b3c/manifest/video.m3u8");
        assert_eq!(r.playback.iter().find(|p| p.kind == "whep").unwrap().url, "https://customer-abc.cloudflarestream.com/1a2b3c/webRTC/play");
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("SKEY") && !dbg.contains("SPASS") && !dbg.contains("WKEY"));
    }

    #[test]
    fn error_envelope() {
        let body = json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}], "messages": [], "result": null});
        let e = check_envelope(&HttpReply::new(403, body.to_string())).unwrap_err();
        assert_eq!((e.kind, e.status, e.code.as_deref()), (ErrorKind::Unauthorized, Some(403), Some("10000")));
        assert!(!e.is_retryable());
        let e = check_envelope(&HttpReply::new(503, "<html>oops</html>")).unwrap_err();
        assert!(e.is_retryable());
        let e = check_envelope(&HttpReply::new(429, json!({"success": false, "errors": [{"code": 971, "message": "slow down"}]}).to_string()))
            .unwrap_err();
        assert!(e.is_retryable());
        assert_eq!(e.message, "slow down");
        // 2xx でも success:false は誤り
        let e = check_envelope(&HttpReply::new(200, json!({"success": false, "errors": []}).to_string())).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Invalid);
    }

    #[test]
    fn delete_get_list() {
        let p = cf();
        let mut op = p.delete("1a2b3c");
        let c = first_call(&mut *op);
        assert_eq!((c.method.as_str(), c.target().as_str()), ("DELETE", "/client/v4/accounts/0123abcd/stream/live_inputs/1a2b3c"));
        let nf = json!({"success": false, "errors": [{"code": 10003, "message": "Not found"}]}).to_string();
        assert!(matches!(op.step(Some(&HttpReply::new(404, nf.clone())), 0).unwrap(), Step::Done(DeleteOutcome::AlreadyGone)));
        let mut op = p.get("1a2b3c");
        assert_eq!(first_call(&mut *op).method, Method::Get);
        assert!(matches!(op.step(Some(&HttpReply::new(404, nf)), 0).unwrap(), Step::Done(None)));
        assert!(p.delete("../x").step(None, 0).is_err());

        let mut op = p.list_by_tag(Some("out1"));
        let c = first_call(&mut *op);
        assert_eq!(c.target(), "/client/v4/accounts/0123abcd/stream/live_inputs");
        let body = json!({"success": true, "result": {"liveInputs": [
            {"uid": "aa", "meta": {"senca:owner": "out1", "senca:cluster": "c"}},
            {"uid": "bb", "meta": {"senca:owner": "out2"}},
            {"uid": "cc", "meta": {"name": "someone else's"}}]}});
        let Step::Done(l) = op.step(Some(&HttpReply::new(200, body.to_string())), 0).unwrap() else { panic!() };
        assert_eq!(l, vec![TaggedResource { resource_id: "aa".into(), owner: "out1".into(), cluster: Some("c".into()), complete: true }]);
    }

    #[test]
    fn credential_errors_hide_values() {
        for bad in ["SEKRIT", ":SEKRIT", "acct:", "ac/ct:SEKRIT", "acct:SEK RIT"] {
            let e = CloudflareStream::from_options(&Secret::new(bad), &BTreeMap::new()).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Config);
            assert!(!e.to_string().contains("SEK"));
        }
    }
}
