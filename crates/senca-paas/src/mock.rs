//! 試験用の模擬 API（Cloudflare Stream・Amazon IVS・AWS Elemental MediaLive の最小限）。sans-io:
//! 解析済みの要求を受けて応答を返すだけで、通信は呼び出し側（`examples/paas_mock.rs`・
//! `tests/mock_flow.rs`）が行う。製品の経路では使わない。
//!
//! 認証は本物と同じく確かめる: Cloudflare は Bearer トークン、AWS は SigV4（[`crate::sigv4::verify`]。
//! service は経路ごとに `ivs`・`medialive`）。

use crate::call::HttpReply;
use crate::secret::Secret;
use crate::sigv4::{self, VerifyInput};
use bytes::Bytes;
use senca_http::{Response, Version};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// 模擬の AWS アカウント番号。
pub const ACCOUNT: &str = "123456789012";

/// 一度だけ返す誤り。
#[derive(Debug, Clone)]
struct Fault {
    method: String,
    path_prefix: String,
    status: u16,
    aws_type: Option<String>,
}

/// 模擬の連携先。状態は試験から直接見たり足したりできる。
#[derive(Debug)]
pub struct MockCloud {
    pub cf_account: String,
    pub cf_token: Secret,
    pub aws_access_key_id: String,
    pub aws_secret: Secret,
    pub aws_session_token: Option<Secret>,
    pub region: String,
    /// 一覧の 1 ページの件数の上限（ページ送りの試験用。要求の maxResults より小さければこちら）。
    pub page_size: usize,
    pub cf_inputs: BTreeMap<String, Value>,
    /// arn → (channel, stream key)
    pub ivs_channels: BTreeMap<String, (Value, String)>,
    pub ml_inputs: BTreeMap<String, Value>,
    pub ml_sgs: BTreeMap<String, Value>,
    ml_request_ids: BTreeMap<String, String>,
    /// 削除中の Input（次の要求で消える）。
    ml_deleting: Vec<String>,
    faults: Vec<Fault>,
    /// 受けた要求（`METHOD path`。秘密は含まない）。
    pub log: Vec<String>,
    next: u64,
}

impl MockCloud {
    pub fn new(cf_account: &str, cf_token: &str, aws_access_key_id: &str, aws_secret: &str, region: &str) -> Self {
        MockCloud {
            cf_account: cf_account.into(),
            cf_token: Secret::new(cf_token),
            aws_access_key_id: aws_access_key_id.into(),
            aws_secret: Secret::new(aws_secret),
            aws_session_token: None,
            region: region.into(),
            page_size: 100,
            cf_inputs: BTreeMap::new(),
            ivs_channels: BTreeMap::new(),
            ml_inputs: BTreeMap::new(),
            ml_sgs: BTreeMap::new(),
            ml_request_ids: BTreeMap::new(),
            ml_deleting: Vec::new(),
            faults: Vec::new(),
            log: Vec::new(),
            next: 0,
        }
    }

    /// 次に `method` で `path_prefix` から始まる path の要求が来たら、一度だけ `status` を返す。
    pub fn fail_next(&mut self, method: &str, path_prefix: &str, status: u16, aws_type: Option<&str>) {
        self.faults.push(Fault { method: method.into(), path_prefix: path_prefix.into(), status, aws_type: aws_type.map(Into::into) });
    }

    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// 解析済みの要求を処理する。`target` は request-target（符号化されたまま）。
    pub fn handle(&mut self, method: &str, target: &str, headers: &[(String, String)], body: &[u8], now_unix: u64) -> HttpReply {
        for id in std::mem::take(&mut self.ml_deleting) {
            self.ml_inputs.remove(&id);
        }
        let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
        let (Ok(segs), Ok(query)) = (senca_http::pct::decode_path(raw_path), senca_http::pct::parse_query(raw_query)) else {
            return HttpReply::new(400, "bad target");
        };
        let path = format!("/{}", segs.join("/"));
        self.log.push(format!("{method} {path}"));
        let segs: Vec<&str> = segs.iter().map(String::as_str).collect();

        // 認証
        let service = match segs.first() {
            Some(&"client") => None,
            Some(&"prod") => Some("medialive"),
            _ => Some("ivs"),
        };
        match service {
            None => {
                let ok = headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("authorization"))
                    .and_then(|(_, v)| v.strip_prefix("Bearer "))
                    .is_some_and(|t| self.cf_token.ct_eq(t));
                if !ok {
                    return cf_error(403, 10000, "Authentication error");
                }
            }
            Some(svc) => {
                // 署名の path は復号した形（末尾の / を保つ）
                let sign_path = if raw_path.ends_with('/') && path != "/" { format!("{path}/") } else { path.clone() };
                let req = VerifyInput { method, path: &sign_path, query: &query, headers, body };
                let lookup = |id: &str| (id == self.aws_access_key_id).then(|| (self.aws_secret.clone(), self.aws_session_token.clone()));
                match sigv4::verify(&req, lookup, svc, now_unix, 300) {
                    Ok(v) if v.region == self.region => {}
                    Ok(_) => return aws_err(403, "InvalidSignatureException", "wrong region"),
                    Err(sigv4::VerifyError::UnknownAccessKey) => return aws_err(403, "UnrecognizedClientException", "unknown access key"),
                    Err(e) => return aws_err(403, "InvalidSignatureException", &format!("{e:?}")),
                }
            }
        }

        if let Some(i) = self.faults.iter().position(|f| f.method == method && path.starts_with(&f.path_prefix)) {
            let f = self.faults.remove(i);
            return match (service, f.aws_type) {
                (None, _) => cf_error(f.status, 10001, "injected failure"),
                (Some(_), t) => aws_err(f.status, t.as_deref().unwrap_or("InternalServerException"), "injected failure"),
            };
        }

        let body: Value = if body.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpReply::new(400, r#"{"message":"bad json"}"#),
            }
        };
        let page = |q: &[(String, String)], max: usize| -> (usize, usize) {
            let start = q.iter().find(|(k, _)| k == "nextToken").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
            let mr = q.iter().find(|(k, _)| k == "maxResults").and_then(|(_, v)| v.parse().ok()).unwrap_or(max);
            (start, mr.min(max))
        };
        let page_size = self.page_size;

        match (method, segs.as_slice()) {
            // ---- Cloudflare Stream ----
            ("POST", ["client", "v4", "accounts", acct, "stream", "live_inputs"]) if *acct == self.cf_account => {
                let n = self.id();
                let uid = format!("{n:032x}");
                let r = json!({
                    "uid": uid, "meta": body.get("meta").cloned().unwrap_or(json!({})),
                    "recording": body.get("recording").cloned().unwrap_or(json!({"mode": "off"})),
                    "rtmps": {"url": "rtmps://live.cloudflare.com:443/live/", "streamKey": format!("cfkey{n:027x}")},
                    "srt": {"url": "srt://live.cloudflare.com:778", "streamId": uid, "passphrase": format!("cfpass{n:026x}")},
                    "webRTC": {"url": format!("https://customer-mock.cloudflarestream.com/cfwhip{n:026x}/webRTC/publish")},
                    "webRTCPlayback": {"url": format!("https://customer-mock.cloudflarestream.com/{uid}/webRTC/play")},
                    "status": null,
                });
                self.cf_inputs.insert(uid, r.clone());
                cf_ok(201, r)
            }
            ("GET", ["client", "v4", "accounts", acct, "stream", "live_inputs"]) if *acct == self.cf_account => {
                let list: Vec<Value> = self.cf_inputs.values().map(|v| json!({"uid": v["uid"], "meta": v["meta"]})).collect();
                let total = list.len();
                cf_ok(200, json!({"liveInputs": list, "range": 1000, "total": total}))
            }
            ("GET", ["client", "v4", "accounts", acct, "stream", "live_inputs", uid]) if *acct == self.cf_account => {
                match self.cf_inputs.get(*uid) {
                    Some(v) => cf_ok(200, v.clone()),
                    None => cf_error(404, 10003, "Live input not found"),
                }
            }
            ("DELETE", ["client", "v4", "accounts", acct, "stream", "live_inputs", uid]) if *acct == self.cf_account => {
                match self.cf_inputs.remove(*uid) {
                    Some(_) => HttpReply::new(200, json!({"success": true, "errors": [], "messages": [], "result": null}).to_string()),
                    None => cf_error(404, 10003, "Live input not found"),
                }
            }
            (_, ["client", ..]) => cf_error(404, 7003, "No route"),

            // ---- Amazon IVS ----
            ("POST", ["CreateChannel"]) => {
                let n = self.id();
                let arn = format!("arn:aws:ivs:{}:{ACCOUNT}:channel/{n:012x}", self.region);
                let key = format!("sk_{}_{n:024x}", self.region);
                let ch = json!({
                    "arn": arn, "name": body.get("name").cloned().unwrap_or(json!("")),
                    "latencyMode": body.get("latencyMode").cloned().unwrap_or(json!("LOW")),
                    "type": body.get("type").cloned().unwrap_or(json!("STANDARD")),
                    "ingestEndpoint": format!("{n:012x}.global-contribute.live-video.net"),
                    "playbackUrl": format!("https://{n:012x}.{}.playback.live-video.net/api/video/v1/{}.channel.m3u8", self.region, ACCOUNT),
                    "tags": body.get("tags").cloned().unwrap_or(json!({})),
                });
                self.ivs_channels.insert(arn.clone(), (ch.clone(), key.clone()));
                let sk = json!({"arn": format!("arn:aws:ivs:{}:{ACCOUNT}:stream-key/{n:012x}", self.region), "channelArn": arn, "value": key});
                HttpReply::new(200, json!({"channel": ch, "streamKey": sk}).to_string())
            }
            ("POST", ["GetChannel"]) => match body.get("arn").and_then(Value::as_str).and_then(|a| self.ivs_channels.get(a)) {
                Some((ch, _)) => HttpReply::new(200, json!({"channel": ch}).to_string()),
                None => aws_err(404, "ResourceNotFoundException", "channel not found"),
            },
            ("POST", ["ListStreamKeys"]) => match body.get("channelArn").and_then(Value::as_str).and_then(|a| self.ivs_channels.get_key_value(a)) {
                Some((arn, _)) => {
                    let id = arn.rsplit('/').next().unwrap_or("");
                    let k = format!("arn:aws:ivs:{}:{ACCOUNT}:stream-key/{id}", self.region);
                    HttpReply::new(200, json!({"streamKeys": [{"arn": k, "channelArn": arn}]}).to_string())
                }
                None => aws_err(404, "ResourceNotFoundException", "channel not found"),
            },
            ("POST", ["GetStreamKey"]) => {
                let id = body.get("arn").and_then(Value::as_str).and_then(|a| a.rsplit('/').next()).unwrap_or("");
                let found = self.ivs_channels.iter().find(|(arn, _)| arn.ends_with(&format!("/{id}")));
                match found {
                    Some((arn, (_, key))) => HttpReply::new(200, json!({"streamKey": {"arn": body["arn"], "channelArn": arn, "value": key}}).to_string()),
                    None => aws_err(404, "ResourceNotFoundException", "stream key not found"),
                }
            }
            ("POST", ["DeleteChannel"]) => match body.get("arn").and_then(Value::as_str).and_then(|a| self.ivs_channels.remove(a)) {
                Some(_) => HttpReply::new(204, ""),
                None => aws_err(404, "ResourceNotFoundException", "channel not found"),
            },
            ("POST", ["ListChannels"]) => {
                let start = body.get("nextToken").and_then(Value::as_str).and_then(|t| t.parse().ok()).unwrap_or(0usize);
                let max = body.get("maxResults").and_then(Value::as_u64).map_or(page_size, |m| (m as usize).min(page_size));
                let all: Vec<Value> = self
                    .ivs_channels
                    .values()
                    .map(|(c, _)| json!({"arn": c["arn"], "name": c["name"], "latencyMode": c["latencyMode"], "tags": c["tags"]}))
                    .collect();
                let mut r = json!({"channels": all.iter().skip(start).take(max).cloned().collect::<Vec<_>>()});
                if start + max < all.len() {
                    r["nextToken"] = json!((start + max).to_string());
                }
                HttpReply::new(200, r.to_string())
            }

            // ---- AWS Elemental MediaLive ----
            ("POST", ["prod", "inputSecurityGroups"]) => {
                let id = (self.id() + 1000).to_string();
                let g = json!({
                    "id": id, "arn": format!("arn:aws:medialive:{}:{ACCOUNT}:inputSecurityGroup:{id}", self.region),
                    "whitelistRules": body.get("whitelistRules").cloned().unwrap_or(json!([])),
                    "tags": body.get("tags").cloned().unwrap_or(json!({})), "inputs": [], "state": "IDLE",
                });
                self.ml_sgs.insert(id, g.clone());
                HttpReply::new(200, json!({"securityGroup": g}).to_string())
            }
            ("GET", ["prod", "inputSecurityGroups"]) => {
                let (start, max) = page(&query, page_size);
                let all: Vec<Value> = self.ml_sgs.values().cloned().collect();
                let mut r = json!({"inputSecurityGroups": all.iter().skip(start).take(max).cloned().collect::<Vec<_>>()});
                if start + max < all.len() {
                    r["nextToken"] = json!((start + max).to_string());
                }
                HttpReply::new(200, r.to_string())
            }
            ("DELETE", ["prod", "inputSecurityGroups", id]) => {
                if !self.ml_sgs.contains_key(*id) {
                    return aws_err(404, "NotFoundException", "security group not found");
                }
                let in_use = self.ml_inputs.values().any(|i| i["securityGroups"].as_array().is_some_and(|a| a.iter().any(|s| s == *id)));
                if in_use {
                    return aws_err(409, "ConflictException", "security group is in use");
                }
                self.ml_sgs.remove(*id);
                HttpReply::new(200, "{}")
            }
            ("POST", ["prod", "inputs"]) => {
                if let Some(existing) = body.get("requestId").and_then(Value::as_str).and_then(|r| self.ml_request_ids.get(r))
                    && let Some(i) = self.ml_inputs.get(existing)
                {
                    return HttpReply::new(201, json!({"input": i}).to_string());
                }
                let sgs: Vec<String> =
                    body.get("inputSecurityGroups").and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect();
                if sgs.iter().any(|s| !self.ml_sgs.contains_key(s)) {
                    return aws_err(400, "BadRequestException", "unknown security group");
                }
                let id = (self.id() + 2000).to_string();
                let n = self.next;
                let dests: Vec<Value> = body
                    .get("destinations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(k, d)| {
                        let ip = format!("198.51.100.{}", (n as usize * 2 + k) % 250 + 1);
                        json!({"ip": ip, "port": "1935", "url": format!("rtmp://{ip}:1935/{}", d["streamName"].as_str().unwrap_or(""))})
                    })
                    .collect();
                let i = json!({
                    "id": id, "arn": format!("arn:aws:medialive:{}:{ACCOUNT}:input:{id}", self.region),
                    "name": body.get("name").cloned().unwrap_or(json!("")), "type": body.get("type").cloned().unwrap_or(json!("RTMP_PUSH")),
                    "state": "DETACHED", "destinations": dests, "securityGroups": sgs,
                    "tags": body.get("tags").cloned().unwrap_or(json!({})), "inputClass": "SINGLE_PIPELINE",
                });
                for s in &sgs {
                    if let Some(g) = self.ml_sgs.get_mut(s)
                        && let Some(a) = g["inputs"].as_array_mut()
                    {
                        a.push(json!(id));
                    }
                }
                if let Some(r) = body.get("requestId").and_then(Value::as_str) {
                    self.ml_request_ids.insert(r.to_string(), id.clone());
                }
                self.ml_inputs.insert(id, i.clone());
                HttpReply::new(201, json!({"input": i}).to_string())
            }
            ("GET", ["prod", "inputs"]) => {
                let (start, max) = page(&query, page_size);
                let all: Vec<Value> = self.ml_inputs.values().cloned().collect();
                let mut r = json!({"inputs": all.iter().skip(start).take(max).cloned().collect::<Vec<_>>()});
                if start + max < all.len() {
                    r["nextToken"] = json!((start + max).to_string());
                }
                HttpReply::new(200, r.to_string())
            }
            ("GET", ["prod", "inputs", id]) => match self.ml_inputs.get(*id) {
                Some(i) => HttpReply::new(200, i.to_string()),
                None => aws_err(404, "NotFoundException", "input not found"),
            },
            ("DELETE", ["prod", "inputs", id]) => {
                let id = id.to_string();
                let Some(i) = self.ml_inputs.get_mut(&id) else { return aws_err(404, "NotFoundException", "input not found") };
                if i["state"] == "DELETING" {
                    return aws_err(404, "NotFoundException", "input not found");
                }
                i["state"] = json!("DELETING");
                for g in self.ml_sgs.values_mut() {
                    if let Some(a) = g["inputs"].as_array_mut() {
                        a.retain(|x| *x != json!(id));
                    }
                }
                self.ml_deleting.push(id);
                HttpReply::new(200, "{}")
            }
            _ => aws_err(404, "NotFoundException", "no such operation"),
        }
    }

    /// 消えずに残っている資源の数（Cloudflare・IVS・MediaLive の Input・security group）。
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let live_inputs = self.ml_inputs.values().filter(|i| i["state"] != "DELETING").count();
        (self.cf_inputs.len(), self.ivs_channels.len(), live_inputs, self.ml_sgs.len())
    }
}

fn cf_ok(status: u16, result: Value) -> HttpReply {
    HttpReply::new(status, json!({"success": true, "errors": [], "messages": [], "result": result}).to_string())
}

fn cf_error(status: u16, code: u32, message: &str) -> HttpReply {
    HttpReply::new(status, json!({"success": false, "errors": [{"code": code, "message": message}], "messages": [], "result": null}).to_string())
}

fn aws_err(status: u16, ty: &str, message: &str) -> HttpReply {
    HttpReply::new(status, json!({"message": message}).to_string()).with_header("x-amzn-ErrorType", &format!("{ty}:http://internal.amazon.com/coral/"))
}

/// 応答を HTTP/1.1 のバイト列にする（接続は閉じる）。
pub fn encode_reply(r: &HttpReply) -> Bytes {
    let mut resp = Response::new(r.status);
    for (n, v) in &r.headers {
        resp = resp.header(n, v.clone()).unwrap_or_else(|_| Response::new(500));
    }
    if r.status != 204 {
        resp = resp.header("Content-Type", "application/json").unwrap_or_else(|_| Response::new(500));
    }
    let body = if r.status == 204 { Bytes::new() } else { r.body.clone() };
    match resp.encode(body, Version::Http11, false, false) {
        Ok(e) => {
            let mut out = e.head.to_vec();
            out.extend_from_slice(&e.body);
            Bytes::from(out)
        }
        Err(_) => Bytes::from_static(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    }
}
