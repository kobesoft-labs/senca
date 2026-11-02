//! クラウドのメディア PaaS との連携（REQ-001、ADR-0010）。要求の組み立てと応答の解析は senca-paas（sans-io）、
//! 通信はここ（外向きの HTTPS、宛先の制限つき）。
//!
//! - 出力（protocol = "paas"）が有効になったら、外部の受け口（Live Input・Channel・Input）を作る。
//!   作る前・途中の経過（checkpoint）を `provider_resource` に保存してから要求を送る（落ちても二重に作らない）。
//! - 受け口の秘密（stream key・パスフレーズ）は保存しない。送る前に `get` で取り直す。
//! - 送るのは既存の rtmp_push（RTMPS）・srt_push。
//! - 出力を消すと外部の資源も消す。SENCA のタグの無い資源は消さない。

use crate::app::App;
use crate::io::IoStatus;
use senca_control::model::Output;
use senca_control::state::Command;
use senca_paas::{ErrorKind, HttpCall, HttpReply, Op, PaasError, ReplyReader, Scheme, Secret, Step};

fn retryable(m: String) -> PaasError {
    PaasError::new(ErrorKind::Retryable, m)
}
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const PROVIDERS: &[&str] = &["cloudflare_stream", "aws_ivs", "aws_medialive"];

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 1 回の要求を送る（https なら TLS、宛先の制限つき）。
async fn exec(app: &App, call: &HttpCall) -> Result<HttpReply, PaasError> {
    let err = retryable;
    let addr = crate::egress::resolve(&call.endpoint.host, call.endpoint.port, &app.cfg.egress_allow).await.map_err(err)?;
    let fut = async {
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let mut s: Box<dyn crate::io::IoStream> = match call.endpoint.scheme {
            Scheme::Https => Box::new(crate::tls_io::connect(tcp, crate::httpc::client_tls()?, &call.endpoint.host).await?),
            Scheme::Http => Box::new(tcp),
        };
        let req = call.encode().map_err(|e| e.to_string())?;
        s.write_all(&req).await.map_err(|e| e.to_string())?;
        let mut r = ReplyReader::new(&call.method);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = s.read(&mut buf).await.map_err(|e| e.to_string())?;
            let got = if n == 0 { r.feed_eof() } else { r.feed(&buf[..n]) }.map_err(|e| e.to_string())?;
            if let Some(reply) = got {
                return Ok::<_, String>(reply);
            }
            if n == 0 {
                return Err("応答の途中で切れました".into());
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), fut).await.map_err(|_| err("timeout".into()))?.map_err(err)
}

/// 操作を最後まで進める。checkpoint は保存してから次の要求を送る。
async fn drive<T>(app: &Arc<App>, op: &mut dyn Op<T>, o: &Output) -> Result<T, PaasError> {
    let mut reply: Option<HttpReply> = None;
    for _ in 0..32 {
        match op.step(reply.as_ref(), unix_now())? {
            Step::Done(t) => return Ok(t),
            Step::Call { call, checkpoint } => {
                if let Some(cp) = checkpoint {
                    save_resource(app, o, Some(cp)).await.map_err(retryable)?;
                }
                reply = Some(exec(app, &call).await?);
            }
        }
    }
    Err(retryable("操作の手順が多すぎます".into()))
}

/// `provider_resource` を保存する（世代は変えない: 動いている係を作り直さないため）。
async fn save_resource(app: &Arc<App>, o: &Output, rid: Option<String>) -> Result<(), String> {
    let app2 = app.clone();
    let id = o.id.clone();
    tokio::task::spawn_blocking(move || {
        let cur = app2.control.lock().unwrap().state.outputs.get(&id).cloned();
        let Some(mut cur) = cur else { return Err("出力が消されました".to_string()) };
        cur.provider_resource = rid;
        app2.propose(Command::PutOutput { output: cur }).map_err(|e| e.title)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn provider(app: &App, o: &Output) -> Result<Box<dyn senca_paas::PaasProvider>, String> {
    let name = o.provider.as_deref().ok_or("provider がありません")?;
    let cred = crate::io::secret_value(app, &o.tenant, &o.credential_ref)?.ok_or("credential_ref がありません")?;
    senca_paas::provider_for(name, &Secret::new(cred), &o.options).map_err(|e| e.to_string())
}

fn cluster_tag(app: &App) -> String {
    app.cluster.get().map(|_| "senca-cluster".to_string()).unwrap_or_else(|| app.node_id.clone())
}

/// 出力の係: 外部の受け口を用意し、そこへ送る。
pub async fn run(app: &Arc<App>, o: &Output, st: &Mutex<IoStatus>) -> Result<(), String> {
    let p = provider(app, o)?;
    st.lock().unwrap().state = "provisioning".into();
    let rid = {
        let cur = app.control.lock().unwrap().state.outputs.get(&o.id).and_then(|x| x.provider_resource.clone());
        match cur {
            Some(r) if !p.is_partial(&r) => r,
            _ => {
                let spec = senca_paas::CreateSpec::for_output(&o.id, o.generation, Some(&cluster_tag(app)));
                let mut op = p.create(&spec);
                let res = drive(app, op.as_mut(), o).await.map_err(|e| format!("作成に失敗: {e}"))?;
                save_resource(app, o, Some(res.resource_id.clone())).await?;
                res.resource_id
            }
        }
    };
    // 秘密は保存しないので毎回取り直す
    let mut op = p.get(&rid);
    let res = drive(app, op.as_mut(), o).await.map_err(|e| format!("取得に失敗: {e}"))?.ok_or("外部の資源が見つかりません（消された可能性）")?;
    let pref = match o.options.get("ingest").map(String::as_str) {
        Some("srt") => Some(senca_paas::IngestProtocol::Srt),
        Some("rtmps") => Some(senca_paas::IngestProtocol::Rtmps),
        _ => None,
    };
    let ing = res.ingest_for(pref).or_else(|| res.ingest.first()).ok_or("受け口がありません")?;
    let t = ing.push_target();
    match t.runner {
        "rtmp_push" => {
            let key = t.credential.as_ref().map(|s| s.expose().to_string()).ok_or("stream key がありません")?;
            crate::io::rtmp_push_to(app, &o.stream, t.url.expose(), &key, st).await
        }
        "srt_push" => crate::srt::push_to(app, &o.stream, t.url.expose(), t.credential.as_ref().map(|s| s.expose().to_string()), st).await,
        r => Err(format!("{r} はまだ使えません")),
    }
}

/// 出力を消すときの後片付け（API のスレッドから呼ばれる）。
pub fn cleanup(app: &App, o: &Output) {
    let Some(rid) = o.provider_resource.clone() else { return };
    let Some(rt) = app.rt.get() else { return };
    let p = match provider(app, o) {
        Ok(p) => p,
        Err(e) => {
            app.log(&format!("paas: output {}: 後片付けできません: {e}", o.id));
            return;
        }
    };
    let mut op = p.delete(&rid);
    let r: Result<(), PaasError> = rt.block_on(async {
        let mut reply: Option<HttpReply> = None;
        for _ in 0..32 {
            match op.step(reply.as_ref(), unix_now())? {
                Step::Done(_) => return Ok(()),
                Step::Call { call, .. } => reply = Some(exec(app, &call).await?),
            }
        }
        Ok(())
    });
    if let Err(e) = r {
        app.log(&format!("paas: output {}: 外部の資源 {rid} を消せませんでした: {e}", o.id));
    }
}
