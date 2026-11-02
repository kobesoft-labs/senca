//! 模擬 API（`senca_paas::mock`）を std の TCP サーバーで動かし、実際の HTTP で
//! 作成 → 取得 → 一覧 → 削除、誤り、途中で落ちたときの後始末を確かめる。クラウドには接続しない。

use senca_http::{Event, Limits, RequestParser};
use senca_paas::mock::{MockCloud, encode_reply};
use senca_paas::reconcile::{Action, Desired, reconcile};
use senca_paas::{
    CreateSpec, DeleteOutcome, ErrorKind, HttpCall, HttpReply, IngestProtocol, PaasError, PaasProvider, ReplyReader, Secret, drive,
    provider_for,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CF_ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const CF_TOKEN: &str = "cf-token-SECRET-1234567890";
const AKID: &str = "AKIAMOCK0000000001";
const AWS_SECRET: &str = "aws/secret+MOCK0000000000000000000000000";
const REGION: &str = "ap-northeast-1";

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

struct Server {
    addr: SocketAddr,
    cloud: Arc<Mutex<MockCloud>>,
}

fn serve_conn(mut s: TcpStream, cloud: &Mutex<MockCloud>) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut p = RequestParser::new(Limits::DEFAULT);
    let mut buf = [0u8; 16 * 1024];
    let mut head = None;
    let mut body = Vec::new();
    loop {
        match p.poll() {
            Ok(Event::NeedMore) => {
                let n = s.read(&mut buf)?;
                if n == 0 {
                    p.feed_eof();
                } else if p.feed(&buf[..n]).is_err() {
                    return Ok(());
                }
            }
            Ok(Event::Head(h)) => head = Some(h),
            Ok(Event::Data(d)) => body.extend_from_slice(&d),
            Ok(Event::End(_)) => break,
            Ok(Event::Closed) | Err(_) => return Ok(()),
        }
    }
    let h = head.expect("head before end");
    let headers: Vec<(String, String)> = h.headers.iter().map(|(n, v)| (n.to_string(), String::from_utf8_lossy(v).into_owned())).collect();
    let reply = cloud.lock().unwrap().handle(h.method.as_str(), &h.target, &headers, &body, now());
    s.write_all(&encode_reply(&reply))
}

fn start(cloud: MockCloud) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let cloud = Arc::new(Mutex::new(cloud));
    let c = cloud.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let _ = serve_conn(s, &c);
        }
    });
    Server { addr, cloud }
}

fn mock() -> MockCloud {
    MockCloud::new(CF_ACCOUNT, CF_TOKEN, AKID, AWS_SECRET, REGION)
}

/// 実際の TCP で 1 回送る（HTTP。本番の HTTPS はノードの TLS が担う）。
fn send(call: &HttpCall) -> Result<HttpReply, PaasError> {
    let mut s = TcpStream::connect((call.endpoint.host.as_str(), call.endpoint.port)).map_err(|e| PaasError::transport(e.to_string()))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&call.encode()?).map_err(|e| PaasError::transport(e.to_string()))?;
    let mut r = ReplyReader::new(&call.method);
    let mut buf = [0u8; 8192];
    loop {
        let n = s.read(&mut buf).map_err(|e| PaasError::transport(e.to_string()))?;
        if n == 0 {
            return r.feed_eof()?.ok_or_else(|| PaasError::transport("eof"));
        }
        if let Some(rep) = r.feed(&buf[..n])? {
            return Ok(rep);
        }
    }
}

fn run<T>(op: &mut dyn senca_paas::Op<T>) -> Result<T, PaasError> {
    drive(op, now, send, |_| Ok(()))
}

fn opts(srv: &Server, extra: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut o = BTreeMap::new();
    o.insert("endpoint".to_string(), format!("http://{}", srv.addr));
    o.insert("region".to_string(), REGION.to_string());
    for (k, v) in extra {
        o.insert(k.to_string(), v.to_string());
    }
    o
}

fn cf(srv: &Server, token: &str) -> Box<dyn PaasProvider> {
    provider_for("cloudflare_stream", &Secret::new(format!("{CF_ACCOUNT}:{token}")), &opts(srv, &[])).unwrap()
}

fn aws(srv: &Server, name: &str, cred: &str, extra: &[(&str, &str)]) -> Box<dyn PaasProvider> {
    provider_for(name, &Secret::new(cred), &opts(srv, extra)).unwrap()
}

fn aws_cred() -> String {
    format!("{AKID}:{AWS_SECRET}")
}

/// 秘密が文字列に出ていないこと。
fn assert_no_secret(s: &str, secrets: &[&str]) {
    for x in secrets {
        assert!(!s.contains(x), "secret leaked into: {s}");
    }
}

#[test]
fn cloudflare_create_get_list_delete() {
    let srv = start(mock());
    let p = cf(&srv, CF_TOKEN);
    let spec = CreateSpec::for_output("out-cf", 1, None);
    let r = run(&mut *p.create(&spec)).unwrap();
    assert_eq!(r.owner.as_deref(), Some("out-cf"));
    let rtmps = r.ingest_for(Some(IngestProtocol::Rtmps)).unwrap().push_target();
    assert_eq!((rtmps.runner, rtmps.url.expose()), ("rtmp_push", "rtmps://live.cloudflare.com:443/live"));
    let key = rtmps.credential.clone().unwrap();
    assert!(key.expose().starts_with("cfkey"));
    let srt = r.ingest_for(Some(IngestProtocol::Srt)).unwrap().push_target();
    assert!(srt.url.expose().starts_with("srt://live.cloudflare.com:778?streamid="));

    let g = run(&mut *p.get(&r.resource_id)).unwrap().unwrap();
    assert_eq!(g.ingest_for(Some(IngestProtocol::Rtmps)).unwrap().stream_key.as_ref().unwrap(), &key);
    let l = run(&mut *p.list_by_tag(Some("out-cf"))).unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].resource_id, r.resource_id);
    assert!(run(&mut *p.list_by_tag(Some("other"))).unwrap().is_empty());

    assert_eq!(run(&mut *p.delete(&r.resource_id)).unwrap(), DeleteOutcome::Deleted);
    assert_eq!(run(&mut *p.delete(&r.resource_id)).unwrap(), DeleteOutcome::AlreadyGone);
    assert!(run(&mut *p.get(&r.resource_id)).unwrap().is_none());
    assert_eq!(srv.cloud.lock().unwrap().counts().0, 0);

    assert_no_secret(&format!("{r:?} {g:?} {:?}", srv.cloud.lock().unwrap().log), &[key.expose(), CF_TOKEN]);
}

#[test]
fn ivs_create_get_list_delete_with_session_token() {
    let mut m = mock();
    m.aws_session_token = Some(Secret::new("SESSION-TOKEN-xyz"));
    let srv = start(m);
    let p = aws(&srv, "aws_ivs", &format!("{}:SESSION-TOKEN-xyz", aws_cred()), &[]);
    let r = run(&mut *p.create(&CreateSpec::for_output("out-ivs", 2, None))).unwrap();
    assert!(r.resource_id.starts_with("arn:aws:ivs:ap-northeast-1:"));
    let t = r.ingest[0].push_target();
    assert!(t.url.expose().starts_with("rtmps://") && t.url.expose().ends_with(".global-contribute.live-video.net:443/app"));
    let key = t.credential.unwrap();
    assert!(key.expose().starts_with("sk_ap-northeast-1_"));
    assert_eq!(r.playback[0].kind, "hls");

    // 取得は GetChannel → ListStreamKeys → GetStreamKey の 3 回で stream key まで取れる
    let g = run(&mut *p.get(&r.resource_id)).unwrap().unwrap();
    assert_eq!(g.ingest[0].stream_key.as_ref(), Some(&key));
    assert_eq!(run(&mut *p.list_by_tag(None)).unwrap().len(), 1);
    assert_eq!(run(&mut *p.delete(&r.resource_id)).unwrap(), DeleteOutcome::Deleted);
    assert_eq!(run(&mut *p.delete(&r.resource_id)).unwrap(), DeleteOutcome::AlreadyGone);
    assert!(run(&mut *p.get(&r.resource_id)).unwrap().is_none());

    // session token が違えば拒否される
    let bad = aws(&srv, "aws_ivs", &format!("{}:OTHER-TOKEN", aws_cred()), &[]);
    let e = run(&mut *bad.list_by_tag(None)).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unauthorized);
    assert_no_secret(&format!("{e} {e:?} {r:?}"), &[AWS_SECRET, "SESSION-TOKEN-xyz", "OTHER-TOKEN", key.expose()]);
}

#[test]
fn medialive_create_get_list_delete() {
    let srv = start(mock());
    let p = aws(&srv, "aws_medialive", &aws_cred(), &[("allow_cidr", "203.0.113.7/32"), ("pipelines", "2")]);
    let mut checkpoints = Vec::new();
    let r = drive(&mut *p.create(&CreateSpec::for_output("out-ml", 1, None)), now, send, |c| {
        checkpoints.push(c.to_string());
        Ok(())
    })
    .unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert!(checkpoints[0].starts_with("sg="));
    assert!(p.is_partial(&checkpoints[0]));
    assert!(r.resource_id.starts_with("input=") && r.resource_id.ends_with(&checkpoints[0]));
    assert!(!p.is_partial(&r.resource_id));
    assert_eq!(r.ingest.len(), 2);
    let t = r.ingest[0].push_target();
    assert!(t.url.expose().starts_with("rtmp://198.51.100.") && t.url.expose().ends_with(":1935/senca"));
    assert_eq!(t.credential.unwrap().expose(), "senca-out-ml-a");
    {
        let c = srv.cloud.lock().unwrap();
        let sg = c.ml_sgs.values().next().unwrap();
        assert_eq!(sg["whitelistRules"][0]["cidr"], "203.0.113.7/32");
        assert_eq!(sg["tags"]["senca:owner"], "out-ml");
    }

    let g = run(&mut *p.get(&r.resource_id)).unwrap().unwrap();
    assert_eq!(g.resource_id, r.resource_id);
    let l = run(&mut *p.list_by_tag(None)).unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].resource_id, r.resource_id);

    // 同じ Idempotency-Key（requestId）での再作成は同じ Input を返す
    let again = run(&mut *p.create(&CreateSpec::for_output("out-ml", 1, None))).unwrap();
    assert_eq!(again.resource_id.split(';').next(), r.resource_id.split(';').next());
    // 2 度目の作成で作られた security group は孤児（Input に使われていない）
    let l = run(&mut *p.list_by_tag(Some("out-ml"))).unwrap();
    assert_eq!(l.iter().filter(|t| !t.complete).count(), 1);
    for t in l.iter().filter(|t| !t.complete) {
        run(&mut *p.delete(&t.resource_id)).unwrap();
    }

    assert_eq!(run(&mut *p.delete(&r.resource_id)).unwrap(), DeleteOutcome::Deleted);
    assert!(run(&mut *p.get(&r.resource_id)).unwrap().is_none());
    assert_eq!(run(&mut *p.delete(&r.resource_id)).unwrap(), DeleteOutcome::AlreadyGone);
    let (_, _, inputs, sgs) = srv.cloud.lock().unwrap().counts();
    assert_eq!((inputs, sgs), (0, 0));
}

#[test]
fn auth_and_retryable_errors() {
    let srv = start(mock());
    let e = run(&mut *cf(&srv, "wrong-token").list_by_tag(None)).unwrap_err();
    assert_eq!((e.kind, e.status, e.code.as_deref()), (ErrorKind::Unauthorized, Some(403), Some("10000")));
    assert!(!e.is_retryable());
    assert_no_secret(&format!("{e} {e:?}"), &["wrong-token"]);

    let e = run(&mut *aws(&srv, "aws_ivs", &format!("{AKID}:not-the-secret"), &[]).list_by_tag(None)).unwrap_err();
    assert_eq!((e.kind, e.code.as_deref()), (ErrorKind::Unauthorized, Some("InvalidSignatureException")));
    let e = run(&mut *aws(&srv, "aws_medialive", "AKIAUNKNOWN:x", &[("allow_cidr", "10.0.0.0/8")]).list_by_tag(None)).unwrap_err();
    assert_eq!(e.code.as_deref(), Some("UnrecognizedClientException"));

    // 一時的な誤りは再試行できる。再試行で二重に作らない（作成は失敗しているので 1 つだけ）
    srv.cloud.lock().unwrap().fail_next("POST", "/CreateChannel", 503, Some("ServiceUnavailableException"));
    let p = aws(&srv, "aws_ivs", &aws_cred(), &[]);
    let spec = CreateSpec::for_output("out-retry", 1, None);
    let e = run(&mut *p.create(&spec)).unwrap_err();
    assert!(e.is_retryable());
    run(&mut *p.create(&spec)).unwrap();
    assert_eq!(srv.cloud.lock().unwrap().counts().1, 1);
    srv.cloud.lock().unwrap().fail_next("POST", "/ListChannels", 429, Some("ThrottlingException"));
    assert!(run(&mut *p.list_by_tag(None)).unwrap_err().is_retryable());
    srv.cloud.lock().unwrap().fail_next("GET", "/client", 500, None);
    assert!(run(&mut *cf(&srv, CF_TOKEN).list_by_tag(None)).unwrap_err().is_retryable());

    // 接続できない（通信の誤り）は再試行できる
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let mut o = BTreeMap::new();
    o.insert("endpoint".to_string(), format!("http://{dead}"));
    let p = provider_for("cloudflare_stream", &Secret::new(format!("{CF_ACCOUNT}:{CF_TOKEN}")), &o).unwrap();
    assert!(run(&mut *p.list_by_tag(None)).unwrap_err().is_retryable());
}

#[test]
fn pagination() {
    let mut m = mock();
    m.page_size = 2;
    let srv = start(m);
    let ivs = aws(&srv, "aws_ivs", &aws_cred(), &[]);
    let ml = aws(&srv, "aws_medialive", &aws_cred(), &[("allow_cidr", "10.0.0.0/8")]);
    for i in 0..5 {
        run(&mut *ivs.create(&CreateSpec::for_output(&format!("o{i}"), 1, None))).unwrap();
        run(&mut *ml.create(&CreateSpec::for_output(&format!("o{i}"), 1, None))).unwrap();
    }
    assert_eq!(run(&mut *ivs.list_by_tag(None)).unwrap().len(), 5);
    let l = run(&mut *ml.list_by_tag(None)).unwrap();
    assert_eq!(l.len(), 5);
    assert!(l.iter().all(|t| t.complete && t.resource_id.contains(";sg=")));
    assert_eq!(run(&mut *ml.list_by_tag(Some("o3"))).unwrap().len(), 1);
}

/// 同期で reconcile の結果を実行する（ノードが行う処理の縮図）。記録は `records` に持つ。
fn execute(p: &dyn PaasProvider, actions: Vec<Action>, records: &mut BTreeMap<String, Option<String>>) {
    for a in actions {
        match a {
            Action::Keep { .. } => {}
            Action::Adopt { output_id, resource_id } => {
                records.insert(output_id, Some(resource_id));
            }
            Action::Create { output_id } => {
                let spec = CreateSpec::for_output(&output_id, 1, None);
                let mut cp = None;
                let r = drive(&mut *p.create(&spec), now, send, |c| {
                    cp = Some(c.to_string());
                    Ok(())
                });
                match r {
                    Ok(r) => records.insert(output_id, Some(r.resource_id)),
                    Err(_) => records.insert(output_id, cp),
                };
            }
            Action::VerifyMissing { output_id, resource_id } => {
                if run(&mut *p.get(&resource_id)).unwrap().is_none() {
                    records.insert(output_id, None);
                }
            }
            Action::Delete { resource_id, clear_record_of, .. } => {
                run(&mut *p.delete(&resource_id)).unwrap();
                if let Some(o) = clear_record_of {
                    records.insert(o, None);
                }
            }
        }
    }
}

fn desired(p: &dyn PaasProvider, records: &BTreeMap<String, Option<String>>) -> Vec<Desired> {
    records
        .iter()
        .map(|(id, rec)| Desired {
            output_id: id.clone(),
            enabled: true,
            provider_resource: rec.clone(),
            partial: rec.as_deref().is_some_and(|r| p.is_partial(r)),
        })
        .collect()
}

#[test]
fn crash_recovery_adopts_and_cleans_orphans_but_never_untagged() {
    let srv = start(mock());
    let p = cf(&srv, CF_TOKEN);
    // 1) 作成の応答を記録する前に落ちた（out-a）
    let lost = run(&mut *p.create(&CreateSpec::for_output("out-a", 1, None))).unwrap();
    // 2) 消された出力の残り（out-gone）
    run(&mut *p.create(&CreateSpec::for_output("out-gone", 1, None))).unwrap();
    // 3) 利用者が手で作った、タグの無い Live Input
    let manual = {
        let mut c = srv.cloud.lock().unwrap();
        c.cf_inputs.insert("feedfacefeedfacefeedfacefeedface".into(), serde_json::json!({"uid": "feedfacefeedfacefeedfacefeedface", "meta": {"name": "manual"}}));
        "feedfacefeedfacefeedfacefeedface".to_string()
    };
    let mut records: BTreeMap<String, Option<String>> = BTreeMap::new();
    records.insert("out-a".into(), None);
    records.insert("out-b".into(), None);

    let observed = run(&mut *p.list_by_tag(None)).unwrap();
    let actions = reconcile(&desired(&*p, &records), &observed, None);
    assert!(actions.contains(&Action::Adopt { output_id: "out-a".into(), resource_id: lost.resource_id.clone() }));
    assert!(actions.contains(&Action::Create { output_id: "out-b".into() }));
    assert!(actions.iter().any(|a| matches!(a, Action::Delete { owner: Some(o), .. } if o == "out-gone")));
    execute(&*p, actions, &mut records);

    let c = srv.cloud.lock().unwrap();
    assert_eq!(c.cf_inputs.len(), 3, "out-a・out-b・手で作ったもの");
    assert!(c.cf_inputs.contains_key(&manual));
    assert_eq!(records["out-a"].as_deref(), Some(lost.resource_id.as_str()));
    drop(c);

    // 2 回目は何もしない（収束）
    let observed = run(&mut *p.list_by_tag(None)).unwrap();
    let actions = reconcile(&desired(&*p, &records), &observed, None);
    assert!(actions.iter().all(|a| matches!(a, Action::Keep { .. })), "{actions:?}");

    // 外で消された資源は get で確かめてから作り直す
    let b = records["out-b"].clone().unwrap();
    srv.cloud.lock().unwrap().cf_inputs.remove(&b);
    let observed = run(&mut *p.list_by_tag(None)).unwrap();
    let actions = reconcile(&desired(&*p, &records), &observed, None);
    assert_eq!(actions[1], Action::VerifyMissing { output_id: "out-b".into(), resource_id: b });
    execute(&*p, actions, &mut records);
    assert_eq!(records["out-b"], None);
    let observed = run(&mut *p.list_by_tag(None)).unwrap();
    execute(&*p, reconcile(&desired(&*p, &records), &observed, None), &mut records);
    assert!(records["out-b"].is_some());
    assert_eq!(srv.cloud.lock().unwrap().cf_inputs.len(), 3);
}

#[test]
fn medialive_partial_create_is_cleaned_up() {
    let srv = start(mock());
    let p = aws(&srv, "aws_medialive", &aws_cred(), &[("allow_cidr", "10.0.0.0/8")]);
    // CreateInput が失敗 → security group を巻き戻す（AC-P03）
    srv.cloud.lock().unwrap().fail_next("POST", "/prod/inputs", 400, Some("BadRequestException"));
    let e = run(&mut *p.create(&CreateSpec::for_output("out-p", 1, None))).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Invalid);
    assert_eq!(srv.cloud.lock().unwrap().counts(), (0, 0, 0, 0));

    // 巻き戻しも失敗 → checkpoint が残り、reconcile が部品を消して作り直す
    {
        let mut c = srv.cloud.lock().unwrap();
        c.fail_next("POST", "/prod/inputs", 500, None);
        c.fail_next("DELETE", "/prod/inputSecurityGroups", 500, None);
    }
    let mut records = BTreeMap::new();
    records.insert("out-p".to_string(), None);
    execute(&*p, vec![Action::Create { output_id: "out-p".into() }], &mut records);
    let rec = records["out-p"].clone().unwrap();
    assert!(rec.starts_with("sg=") && p.is_partial(&rec));
    assert_eq!(srv.cloud.lock().unwrap().counts(), (0, 0, 0, 1));

    let observed = run(&mut *p.list_by_tag(None)).unwrap();
    let actions = reconcile(&desired(&*p, &records), &observed, None);
    assert!(matches!(&actions[0], Action::Delete { resource_id, .. } if *resource_id == rec));
    assert_eq!(actions[1], Action::Create { output_id: "out-p".into() });
    execute(&*p, actions, &mut records);
    let rec = records["out-p"].clone().unwrap();
    assert!(rec.starts_with("input=") && !p.is_partial(&rec));
    assert_eq!(srv.cloud.lock().unwrap().counts(), (0, 0, 1, 1));

    // 出力を消す → 孤児として Input と security group が消える
    records.clear();
    let observed = run(&mut *p.list_by_tag(None)).unwrap();
    execute(&*p, reconcile(&[], &observed, None), &mut records);
    assert_eq!(srv.cloud.lock().unwrap().counts(), (0, 0, 0, 0));
}
