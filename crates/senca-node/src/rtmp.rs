//! RTMP の接続ごとのタスク。publish は hub へ、play は hub の購読者として送る。

use crate::app::App;
use crate::hub::{HubEvent, PublishError, Publisher};
use bytes::Bytes;
use senca_flv::ingest::{FlvIngest, IngestEvent, IngestOptions};
use senca_rtmp::{MediaType, ServerConfig, ServerEvent, ServerSession};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;

const READ_BUF: usize = 64 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// play の送信待ちがこれを超えたら遅い購読者として次のキーフレームまで落とす。
const MAX_PENDING_OUT: usize = 8 << 20;
/// `latency_profile: interactive` の stream では小さく保つ（遅れをためずに次のキーフレームへ飛ぶ）。
const MAX_PENDING_OUT_INTERACTIVE: usize = 1 << 20;

pub async fn serve<S>(app: Arc<App>, mut sock: S, peer: String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut sess = ServerSession::new(ServerConfig::default());
    let mut buf = vec![0u8; READ_BUF];
    let mut publisher: Option<(u32, Publisher, FlvIngest)> = None;
    let mut player: Option<(u32, crate::hub::Subscription)> = None;
    // connect の tcUrl（クエリに署名が入ることがある）
    let mut tc_query = String::new();
    let res: Result<(), String> = async {
        loop {
            // 送信
            let out = sess.take_output();
            if !out.is_empty() {
                sock.write_all(&out).await.map_err(|e| e.to_string())?;
            }
            if let Some((sid, sub)) = player.as_mut() {
                // play 中: 受信と配信を同時に待つ
                tokio::select! {
                    r = tokio::time::timeout(IDLE_TIMEOUT * 10, sock.read(&mut buf)) => {
                        let n = r.map_err(|_| "idle timeout".to_string())?.map_err(|e| e.to_string())?;
                        if n == 0 { return Ok(()); }
                        sess.feed(&buf[..n]).map_err(|e| e.to_string())?;
                    }
                    ev = sub.recv() => {
                        match ev {
                            Ok(crate::hub::HubEvent::End) => { let _ = sess.end_play(*sid); let out = sess.take_output(); let _ = sock.write_all(&out).await; return Ok(()); }
                            Ok(ev) => send_play(&mut sess, *sid, &ev, sub)?,
                            Err(RecvError::Lagged(n)) => { sub.lagged(n); continue; }
                            Err(RecvError::Closed) => { let _ = sess.end_play(*sid); let out = sess.take_output(); let _ = sock.write_all(&out).await; return Ok(()); }
                        }
                        let limit = if sub.interactive { MAX_PENDING_OUT_INTERACTIVE } else { MAX_PENDING_OUT };
                        if sess.pending_output() > limit { sub.lagged(1); }
                        continue;
                    }
                }
            } else {
                let n = tokio::time::timeout(IDLE_TIMEOUT, sock.read(&mut buf))
                    .await
                    .map_err(|_| "idle timeout".to_string())?
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    return Ok(());
                }
                sess.feed(&buf[..n]).map_err(|e| e.to_string())?;
            }
            while let Some(ev) = sess.poll_event() {
                match ev {
                    ServerEvent::Connect(info) => {
                        if let Some(t) = info.tc_url.as_ref().map(|t| t.expose().to_string())
                            && let Some((_, q)) = t.split_once('?')
                        {
                            tc_query = q.to_string();
                        }
                        if let Some(q) = info.app_query.as_ref() {
                            tc_query = q.expose().to_string();
                        }
                        if info.app == "live" || info.app.starts_with("live") || info.app == "play" {
                            sess.accept_connect().map_err(|e| e.to_string())?;
                        } else {
                            sess.reject_connect("unknown app").map_err(|e| e.to_string())?;
                        }
                    }
                    ServerEvent::Publish { stream_id, stream_key, .. } => {
                        if publisher.is_some() {
                            sess.reject_publish(stream_id, "NetStream.Publish.BadName", "already publishing").map_err(|e| e.to_string())?;
                            continue;
                        }
                        let key = stream_key.expose().split('?').next().unwrap_or("");
                        match app.attach_publisher(key, "rtmp", &peer).await {
                            Ok(p) => {
                                sess.accept_publish(stream_id).map_err(|e| e.to_string())?;
                                let ing = FlvIngest::new(IngestOptions { epoch: p.epoch(), ..Default::default() });
                                app.log(&format!("rtmp publish started stream={} peer={peer}", p.stream_id()));
                                publisher = Some((stream_id, p, ing));
                            }
                            Err(e) => {
                                let (code, desc) = match e {
                                    PublishError::BadKey => ("NetStream.Publish.BadName", "bad stream key"),
                                    PublishError::Busy => ("NetStream.Publish.BadName", "stream is already being published"),
                                    PublishError::Stopped => ("NetStream.Publish.BadName", "stream is stopped"),
                                    PublishError::Draining => ("NetStream.Publish.Rejected", "this node is draining; publish to another node"),
                                    PublishError::Rejected(ref why) if why.contains("ingress bitrate") => {
                                        ("NetStream.Publish.Rejected", "ingress bitrate limit exceeded; retry later")
                                    }
                                    PublishError::Rejected(_) => ("NetStream.Publish.Rejected", "publish rejected"),
                                };
                                app.log(&format!("rtmp publish rejected peer={peer}: {e:?}"));
                                sess.reject_publish(stream_id, code, desc).map_err(|e| e.to_string())?;
                            }
                        }
                    }
                    ServerEvent::Media { stream_id, message } => {
                        let Some((sid, p, ing)) = publisher.as_mut() else { continue };
                        if *sid != stream_id {
                            continue;
                        }
                        let tag_type = match message.media_type {
                            MediaType::Audio => senca_flv::tag::TagType::Audio,
                            MediaType::Video => senca_flv::tag::TagType::Video,
                        };
                        let mut evs = Vec::new();
                        if let Err(e) = ing.push(tag_type, message.timestamp, &message.payload, &mut evs) {
                            return Err(format!("flv: {e}"));
                        }
                        for e in evs {
                            match e {
                                IngestEvent::Config { track, mut config } => { crate::media::enrich(&mut config); p.config(track, config).await }.map_err(|e| format!("{e:?}"))?,
                                IngestEvent::AccessUnit(au) => p.access_unit(au).await.map_err(|e| format!("{e:?}"))?,
                                IngestEvent::EndOfSequence { .. } => {}
                            }
                        }
                    }
                    ServerEvent::Metadata { .. } | ServerEvent::Data { .. } | ServerEvent::PingResponse { .. } => {}
                    ServerEvent::Play { stream_id, stream_key, .. } => {
                        let mut key = stream_key.expose().to_string();
                        if !key.contains('?') && !tc_query.is_empty() {
                            key = format!("{key}?{tc_query}");
                        }
                        match app.subscribe_play(&key, "rtmp") {
                            Ok(mut sub) => {
                                sess.accept_play(stream_id).map_err(|e| e.to_string())?;
                                for ev in sub.initial() {
                                    send_play(&mut sess, stream_id, &ev, &mut sub)?;
                                }
                                player = Some((stream_id, sub));
                            }
                            Err(crate::app::PlayError::TooManyViewers) => {
                                app.log(&format!("rtmp play rejected peer={peer}: too many viewers"));
                                sess.reject_play(stream_id, "NetStream.Play.Failed", "too many viewers").map_err(|e| e.to_string())?;
                            }
                            Err(_) => {
                                sess.reject_play(stream_id, "NetStream.Play.StreamNotFound", "not found").map_err(|e| e.to_string())?;
                            }
                        }
                    }
                    ServerEvent::StreamClosed { stream_id, .. } => {
                        if publisher.as_ref().is_some_and(|(s, _, _)| *s == stream_id) {
                            let (_, p, _) = publisher.take().unwrap();
                            p.detach("unpublish").await;
                        }
                        if player.as_ref().is_some_and(|(s, _)| *s == stream_id) {
                            player = None;
                        }
                    }
                }
            }
        }
    }
    .await;
    if let Some((_, p, _)) = publisher.take() {
        p.detach(if res.is_ok() { "disconnect" } else { "error" }).await;
    }
    if let Err(e) = res {
        app.log(&format!("rtmp connection {peer} closed: {e}"));
    }
}

/// 購読者向けの FLV 化。payload はコピーせず部品で渡す。
fn send_play(sess: &mut ServerSession, sid: u32, ev: &HubEvent, sub: &mut crate::hub::Subscription) -> Result<(), String> {
    let mut tags = Vec::new();
    sub.flv(ev, &mut tags);
    for t in tags {
        let mt = if t.tag_type == senca_flv::tag::TagType::Video { MediaType::Video } else { MediaType::Audio };
        let parts = t.body.parts();
        sess.send_media(sid, mt, t.timestamp, &parts).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[allow(dead_code)]
fn _assert_bytes(_: Bytes) {}
