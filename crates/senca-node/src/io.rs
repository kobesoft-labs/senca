//! 入力（pull）と出力（push）の係。制御状態の定義と実際に動いているタスクを突き合わせて起動・停止する。
//! 失敗したら指数バックオフ（揺らぎ付き）でつなぎ直す（仕様 6.6）。状態は API の `status` に出す。

use crate::app::App;
use crate::hub::HubEvent;
use senca_control::model::{Input, Output};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone, Default, Serialize)]
pub struct IoStatus {
    /// "starting"・"connecting"・"active"・"backoff"・"waiting_input"・"error"
    pub state: String,
    pub last_error: Option<String>,
    pub connects: u64,
    pub since: Option<String>,
    pub bytes: u64,
}

struct Running {
    generation: u64,
    task: tokio::task::JoinHandle<()>,
    status: Arc<Mutex<IoStatus>>,
}

#[derive(Default)]
pub struct IoManager {
    running: Mutex<HashMap<String, Running>>,
}

impl IoManager {
    pub fn status(&self, id: &str) -> Option<IoStatus> {
        self.running.lock().unwrap().get(id).map(|r| r.status.lock().unwrap().clone())
    }

    pub fn reconcile(&self, app: &Arc<App>) {
        let (inputs, outputs): (Vec<Input>, Vec<Output>) = {
            let core = app.control.lock().unwrap();
            (
                core.state.inputs.values().filter(|i| i.enabled).cloned().collect(),
                core.state.outputs.values().filter(|o| o.enabled).cloned().collect(),
            )
        };
        let mut want: HashMap<String, u64> = HashMap::new();
        for i in &inputs {
            want.insert(i.id.clone(), i.generation);
        }
        for o in &outputs {
            want.insert(o.id.clone(), o.generation);
        }
        let mut run = self.running.lock().unwrap();
        run.retain(|id, r| {
            let keep = want.get(id) == Some(&r.generation) && !r.task.is_finished();
            if !keep {
                r.task.abort();
            }
            keep
        });
        for i in inputs {
            if run.contains_key(&i.id) {
                continue;
            }
            let status = Arc::new(Mutex::new(IoStatus { state: "starting".into(), ..Default::default() }));
            let task = tokio::spawn(run_input(app.clone(), i.clone(), status.clone()));
            run.insert(i.id.clone(), Running { generation: i.generation, task, status });
        }
        for o in outputs {
            if run.contains_key(&o.id) {
                continue;
            }
            let status = Arc::new(Mutex::new(IoStatus { state: "starting".into(), ..Default::default() }));
            let task = tokio::spawn(run_output(app.clone(), o.clone(), status.clone()));
            run.insert(o.id.clone(), Running { generation: o.generation, task, status });
        }
    }
}

fn set(st: &Mutex<IoStatus>, state: &str, err: Option<String>) {
    let mut s = st.lock().unwrap();
    if s.state != state {
        s.since = Some(crate::util::rfc3339(crate::util::now_ms()));
    }
    s.state = state.into();
    if err.is_some() {
        s.last_error = err;
    }
}

fn jitter_ms(base: u64) -> u64 {
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    base / 2 + (u16::from_le_bytes(b) as u64 % (base / 2 + 1))
}

/// 指数バックオフ（1 秒〜60 秒、揺らぎ付き）。
async fn backoff(attempt: &mut u32, st: &Mutex<IoStatus>) {
    set(st, "backoff", None);
    let base = 1000u64 << (*attempt).min(6);
    *attempt += 1;
    tokio::time::sleep(Duration::from_millis(jitter_ms(base.min(60_000)))).await;
}

/// `rtmp://host[:port]/app/key` を分ける。
pub fn split_rtmp(url: &str) -> Option<(bool, String, u16, String, Option<String>)> {
    let (scheme, rest) = url.split_once("://")?;
    let tls = match scheme {
        "rtmp" => false,
        "rtmps" => true,
        _ => return None,
    };
    let (auth, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') || h.ends_with(']') => (h.trim_matches(['[', ']']).to_string(), p.parse().ok()?),
        _ => (auth.trim_matches(['[', ']']).to_string(), if tls { 443 } else { 1935 }),
    };
    let (app, key) = match path.split_once('/') {
        Some((a, k)) if !k.is_empty() => (a.to_string(), Some(k.to_string())),
        _ => (path.trim_end_matches('/').to_string(), None),
    };
    Some((tls, host, port, app, key))
}

pub fn secret_value(app: &App, tenant: &str, r: &Option<String>) -> Result<Option<String>, String> {
    let Some(r) = r else { return Ok(None) };
    let name = r.strip_prefix("secret:").ok_or("bad credential_ref")?;
    let core = app.control.lock().unwrap();
    let s = core.state.secrets.values().find(|s| s.tenant == tenant && s.name == name).ok_or("credential_ref の秘密値がありません")?;
    let v = app.secret_box.open_sealed(&s.sealed, &format!("{tenant}/{}", s.id)).ok_or("秘密値を開けません")?;
    String::from_utf8(v).map(Some).map_err(|_| "秘密値が文字列ではありません".into())
}

async fn connect_tcp(app: &App, host: &str, port: u16) -> Result<tokio::net::TcpStream, String> {
    let addr = crate::egress::resolve(host, port, &app.cfg.egress_allow).await?;
    let s = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| format!("{host}:{port}: connect timeout"))?
        .map_err(|e| format!("{host}:{port}: {e}"))?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

pub trait IoStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> IoStream for T {}

/// TCP（rtmps なら TLS を通す）でつなぐ。
async fn connect_stream(app: &App, host: &str, port: u16, tls: bool) -> Result<Box<dyn IoStream>, String> {
    let tcp = connect_tcp(app, host, port).await?;
    if !tls {
        return Ok(Box::new(tcp));
    }
    let cfg = crate::tls_io::client_config(&[])?;
    Ok(Box::new(crate::tls_io::connect(tcp, cfg, host).await?))
}

// ---- 出力 ----

async fn run_output(app: Arc<App>, o: Output, st: Arc<Mutex<IoStatus>>) {
    let mut attempt = 0u32;
    loop {
        let res = match o.protocol.as_str() {
            "rtmp_push" => rtmp_push(&app, &o, &st).await,
            "srt_push" => crate::srt::push(&app, &o, &st).await,
            "paas" => crate::paas::run(&app, &o, &st).await,
            p => Err(format!("{p} はこのノードではまだ使えません")),
        };
        match res {
            Ok(()) => attempt = 0,
            Err(e) => {
                app.log(&format!("output {}: {e}", o.id));
                set(&st, "error", Some(e));
            }
        }
        backoff(&mut attempt, &st).await;
    }
}

async fn rtmp_push(app: &Arc<App>, o: &Output, st: &Mutex<IoStatus>) -> Result<(), String> {
    let url = o.url.clone().ok_or("url がありません")?;
    let (_, _, _, _, key_in_url) = split_rtmp(&url).ok_or("url の形が違います")?;
    let key = secret_value(app, &o.tenant, &o.credential_ref)?.or(key_in_url).ok_or("stream key がありません（url の最後か credential_ref）")?;
    rtmp_push_to(app, &o.stream, &url, &key, st).await
}

/// stream を外部の RTMP(S) へ送る（PaaS の受け口にも使う）。
pub async fn rtmp_push_to(app: &Arc<App>, stream: &str, url: &str, key: &str, st: &Mutex<IoStatus>) -> Result<(), String> {
    use senca_rtmp::{ClientConfig, ClientEvent, ClientMode, ClientSession, MediaType, Secret};
    let key = key.to_string();
    // 配信が始まるまで待つ
    let hub = loop {
        let h = app.hubs.lock().unwrap().get(stream).cloned();
        match h {
            Some(h) if h.is_live() => break h,
            _ => {
                set(st, "waiting_input", None);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    let (tls, host, port, rtmp_app, _) = split_rtmp(url).ok_or("url の形が違います")?;
    set(st, "connecting", None);
    let mut sock = connect_stream(app, &host, port, tls).await?;
    let tc = format!("rtmp://{host}:{port}/{rtmp_app}");
    let mut c = ClientSession::new(ClientConfig::new(&rtmp_app, &tc, ClientMode::Publish { stream_key: Secret::new(key), publish_type: "live".into() }));
    let mut buf = vec![0u8; 64 * 1024];
    sock.write_all(&c.take_output()).await.map_err(|e| e.to_string())?;
    // publish が始まるまで
    loop {
        let n = tokio::time::timeout(Duration::from_secs(15), sock.read(&mut buf)).await.map_err(|_| "handshake timeout")?.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("相手が切りました".into());
        }
        c.feed(&buf[..n]).map_err(|e| e.to_string())?;
        let out = c.take_output();
        if !out.is_empty() {
            sock.write_all(&out).await.map_err(|e| e.to_string())?;
        }
        let mut started = false;
        while let Some(ev) = c.poll_event() {
            match ev {
                ClientEvent::PublishStarted { .. } => started = true,
                ClientEvent::Rejected { code, .. } => return Err(format!("拒否されました: {}", code.unwrap_or_default())),
                _ => {}
            }
        }
        if started {
            break;
        }
    }
    {
        let mut s = st.lock().unwrap();
        s.connects += 1;
    }
    set(st, "active", None);
    let mut sub = hub.subscribe();
    let (mut rd, mut wr) = tokio::io::split(sock);
    let mut tags = Vec::new();
    let mut pending: Vec<HubEvent> = sub.initial();
    loop {
        for ev in pending.drain(..) {
            tags.clear();
            sub.flv(&ev, &mut tags);
            for t in &tags {
                let mt = if t.tag_type == senca_flv::tag::TagType::Video { MediaType::Video } else { MediaType::Audio };
                c.send_media(mt, t.timestamp, &t.body.parts()).map_err(|e| e.to_string())?;
            }
        }
        let out = c.take_output();
        if !out.is_empty() {
            st.lock().unwrap().bytes += out.len() as u64;
            tokio::time::timeout(Duration::from_secs(10), wr.write_all(&out)).await.map_err(|_| "write timeout")?.map_err(|e| e.to_string())?;
        }
        tokio::select! {
            ev = sub.recv() => match ev {
                Ok(HubEvent::End) => return Ok(()),
                Ok(e) => pending.push(e),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => sub.lagged(n),
                Err(_) => return Ok(()),
            },
            r = rd.read(&mut buf) => {
                let n = r.map_err(|e| e.to_string())?;
                if n == 0 { return Err("相手が切りました".into()); }
                c.feed(&buf[..n]).map_err(|e| e.to_string())?;
                while c.poll_event().is_some() {}
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                if !hub.is_live() { return Ok(()); }
            }
        }
    }
}

// ---- 入力 ----

async fn run_input(app: Arc<App>, i: Input, st: Arc<Mutex<IoStatus>>) {
    let mut attempt = 0u32;
    loop {
        let res = match i.protocol.as_str() {
            "rtmp_pull" => rtmp_pull(&app, &i, &st).await,
            "hls_pull" => crate::hls_pull::run(&app, &i, &st).await,
            "srt_pull" => crate::srt::pull(&app, &i, &st).await,
            "rtsp_pull" => crate::rtsp_pull::run(&app, &i, &st).await,
            "device" => crate::capture::run(&app, &i, &st).await,
            p => Err(format!("{p} はこのノードではまだ使えません")),
        };
        match res {
            // VOD（ENDLIST）の HLS は読み終えたら終わり（繰り返さない）
            Ok(()) if i.protocol == "hls_pull" => {
                set(&st, "ended", None);
                return;
            }
            Ok(()) => attempt = 0,
            Err(e) => {
                app.log(&format!("input {}: {e}", i.id));
                set(&st, "error", Some(e));
            }
        }
        backoff(&mut attempt, &st).await;
    }
}

/// 入力の stream に送り手としてつなぐ（publish 鍵を使わない内部の経路）。
pub async fn attach(app: &Arc<App>, stream: &str, transport: &'static str, peer: &str) -> Result<crate::hub::Publisher, String> {
    let s = app.control.lock().unwrap().state.streams.get(stream).cloned().ok_or("stream がありません")?;
    let hub = app.hub(&s);
    hub.attach(transport, peer).await.map_err(|e| format!("{e:?}"))
}

async fn rtmp_pull(app: &Arc<App>, i: &Input, st: &Mutex<IoStatus>) -> Result<(), String> {
    use senca_flv::ingest::{FlvIngest, IngestEvent, IngestOptions};
    use senca_rtmp::{ClientConfig, ClientEvent, ClientMode, ClientSession, MediaType, Secret};
    let (tls, host, port, rtmp_app, key_in_url) = split_rtmp(&i.url).ok_or("url の形が違います")?;
    let key = secret_value(app, &i.tenant, &i.credential_ref)?.or(key_in_url).ok_or("stream 名がありません")?;
    set(st, "connecting", None);
    let mut sock = connect_stream(app, &host, port, tls).await?;
    let tc = format!("rtmp://{host}:{port}/{rtmp_app}");
    let mut c = ClientSession::new(ClientConfig::new(&rtmp_app, &tc, ClientMode::Play { stream_key: Secret::new(key), start: -2.0 }));
    sock.write_all(&c.take_output()).await.map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut publisher: Option<crate::hub::Publisher> = None;
    let mut ing: Option<FlvIngest> = None;
    loop {
        let n = tokio::time::timeout(Duration::from_secs(20), sock.read(&mut buf)).await.map_err(|_| "受信が止まりました")?.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        st.lock().unwrap().bytes += n as u64;
        c.feed(&buf[..n]).map_err(|e| e.to_string())?;
        let out = c.take_output();
        if !out.is_empty() {
            sock.write_all(&out).await.map_err(|e| e.to_string())?;
        }
        while let Some(ev) = c.poll_event() {
            match ev {
                ClientEvent::PlayStarted { .. } => {
                    let p = attach(app, &i.stream, "rtmp_pull", &format!("{host}:{port}")).await?;
                    ing = Some(FlvIngest::new(IngestOptions { epoch: p.epoch(), ..Default::default() }));
                    publisher = Some(p);
                    st.lock().unwrap().connects += 1;
                    set(st, "active", None);
                }
                ClientEvent::Rejected { code, .. } => return Err(format!("拒否されました: {}", code.unwrap_or_default())),
                ClientEvent::Media { message, .. } => {
                    let (Some(p), Some(g)) = (publisher.as_ref(), ing.as_mut()) else { continue };
                    let tt = if message.media_type == MediaType::Video { senca_flv::tag::TagType::Video } else { senca_flv::tag::TagType::Audio };
                    let mut evs = Vec::new();
                    g.push(tt, message.timestamp, &message.payload, &mut evs).map_err(|e| format!("flv: {e}"))?;
                    for e in evs {
                        match e {
                            IngestEvent::Config { track, mut config } => {
                                crate::media::enrich(&mut config);
                                p.config(track, config).await.map_err(|e| e.to_string())?;
                            }
                            IngestEvent::AccessUnit(au) => p.access_unit(au).await.map_err(|e| e.to_string())?,
                            IngestEvent::EndOfSequence { .. } => {}
                        }
                    }
                }
                ClientEvent::StreamEnded { .. } => {
                    if let Some(p) = publisher.take() {
                        p.detach("pull ended").await;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }
    if let Some(p) = publisher.take() {
        p.detach("pull disconnected").await;
    }
    Err("相手が切りました".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtmp_urls() {
        assert_eq!(split_rtmp("rtmp://a.b/live/key"), Some((false, "a.b".into(), 1935, "live".into(), Some("key".into()))));
        assert_eq!(split_rtmp("rtmps://a.b:8443/app"), Some((true, "a.b".into(), 8443, "app".into(), None)));
        assert_eq!(split_rtmp("rtmp://[::1]:1936/live/k/x"), Some((false, "::1".into(), 1936, "live".into(), Some("k/x".into()))));
    }
}
