//! 字幕と翻訳の係（R6-4、ADR-0011）。音声は復号しない（ADR-0004）。
//!
//! 配信中の stream で `captions` が設定されていれば、音声の AU を切り出して音声認識のサービスへ送り、
//! 結果を言語ごとの WebVTT の HLS 字幕にする（`sub_{lang}.m3u8`・`sub_{lang}_{msn}.vtt`、`master.m3u8`）。
//! 翻訳の設定があれば、確定した字幕をまとめて翻訳し、言語ごとに同じ区切りで出す。
//! 字幕は遅れて出てよい方式（映像は遅らせない）。認識・翻訳の失敗は字幕だけが欠け、配信は止めない。

use crate::app::App;
use crate::hub::HubEvent;
use bytes::Bytes;
use senca_caption::asr::{AsrAdapter, OpenAiTranscription, WhisperCppServer};
use senca_caption::chunk::{AudioChunker, ChunkerConfig};
use senca_caption::hls::{SubtitleSegmenter, SubtitleTrackConfig};
use senca_caption::http::{Endpoint, HttpCall, HttpReply, Scheme};
use senca_caption::merge::{CueMerger, MergeConfig};
use senca_caption::translate::{AnthropicTranslator, BatcherConfig, DeepLTranslator, OpenAiChatTranslator, TranslationBatcher, Translator};
use senca_caption::Secret;
use senca_control::model::{CaptionSettings, Stream};
use senca_core::MediaKind;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 1 回の要求を送る（https なら TLS、宛先の制限つき）。
async fn exec(app: &App, call: &HttpCall) -> Result<HttpReply, String> {
    let ep = &call.endpoint;
    let addr = crate::egress::resolve(&ep.host, ep.port, &app.cfg.egress_allow).await?;
    let fut = async {
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let mut s: Box<dyn crate::io::IoStream> = match ep.scheme {
            Scheme::Https => Box::new(crate::tls_io::connect(tcp, crate::httpc::client_tls()?, &ep.host).await?),
            Scheme::Http => Box::new(tcp),
        };
        s.write_all(&call.encode().map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
        let mut r = call.reader();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = s.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return r.feed_eof().map_err(|e| e.to_string());
            }
            if let Some(reply) = r.feed(&buf[..n]).map_err(|e| e.to_string())? {
                return Ok(reply);
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(60), fut).await.map_err(|_| "timeout".to_string())?
}

fn secret(app: &App, tenant: &str, r: &Option<String>) -> Result<Option<Secret>, String> {
    Ok(crate::io::secret_value(app, tenant, r)?.map(Secret::new))
}

fn asr(app: &App, s: &Stream, c: &CaptionSettings) -> Result<Box<dyn AsrAdapter + Send>, String> {
    let ep = Endpoint::parse(&c.asr_endpoint).map_err(|e| format!("{e:?}"))?;
    Ok(match c.asr_provider.as_str() {
        "whisper_cpp" => {
            let mut a = WhisperCppServer::new(ep);
            a.language = Some(c.language.clone());
            Box::new(a)
        }
        _ => {
            let mut a = OpenAiTranscription::new(ep, c.asr_model.clone().unwrap_or_else(|| "whisper-1".into()));
            a.api_key = secret(app, &s.tenant, &c.asr_credential_ref)?;
            a.language = Some(c.language.clone());
            Box::new(a)
        }
    })
}

fn translator(app: &App, s: &Stream, c: &CaptionSettings) -> Result<Option<Box<dyn Translator + Send>>, String> {
    let Some(p) = &c.translate_provider else { return Ok(None) };
    let ep = Endpoint::parse(c.translate_endpoint.as_deref().unwrap_or("")).map_err(|e| format!("{e:?}"))?;
    let key = secret(app, &s.tenant, &c.translate_credential_ref)?;
    Ok(Some(match p.as_str() {
        "anthropic" => {
            let mut t = AnthropicTranslator::new(ep, key.ok_or("翻訳の資格情報がありません")?);
            if let Some(m) = &c.translate_model {
                t.model = m.clone();
            }
            Box::new(t)
        }
        "deepl" => Box::new(DeepLTranslator::new(ep, key.ok_or("翻訳の資格情報がありません")?)),
        _ => {
            let mut t = OpenAiChatTranslator::new(ep, c.translate_model.clone().unwrap_or_else(|| "gpt-4o-mini".into()));
            t.api_key = key;
            Box::new(t)
        }
    }))
}

/// 配信の間だけ動く字幕の係。stream の hub を購読する。
pub async fn run(app: Arc<App>, stream: Stream) {
    let Some(c) = stream.captions.clone() else { return };
    let (asr, tr) = match (asr(&app, &stream, &c), translator(&app, &stream, &c)) {
        (Ok(a), Ok(t)) => (a, t),
        (Err(e), _) | (_, Err(e)) => {
            app.log(&format!("captions {}: {e}", stream.id));
            return;
        }
    };
    let hub = loop {
        let h = app.hubs.lock().unwrap().get(&stream.id).cloned();
        match h {
            Some(h) if h.is_live() => break h,
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
    let cache = hub.shared.cache.clone();
    let mut sub = hub.subscribe();
    let Ok(mut chunker) = AudioChunker::new(ChunkerConfig::default()) else { return };
    let Ok(mut merger) = CueMerger::new(MergeConfig::default()) else { return };
    let langs: Vec<String> = std::iter::once(c.language.clone()).chain(c.targets.iter().cloned()).collect();
    let mut segs: BTreeMap<String, SubtitleSegmenter> = BTreeMap::new();
    for l in &langs {
        let mut cfg = SubtitleTrackConfig::new(l, l);
        cfg.default = *l == c.language;
        cfg.segment_prefix = format!("sub_{l}_");
        cfg.playlist_uri = format!("sub_{l}.m3u8");
        if let Ok(s) = SubtitleSegmenter::new(cfg) {
            segs.insert(l.clone(), s);
        }
    }
    let targets: Vec<&str> = c.targets.iter().map(String::as_str).collect();
    let mut batcher = TranslationBatcher::new(BatcherConfig::default(), &c.language, &targets).ok();
    // 認識・翻訳は 1 本ずつ、配信の受信を止めないよう別のタスクで
    let (asr_tx, mut asr_rx) = tokio::sync::mpsc::channel::<senca_caption::chunk::AudioChunk>(4);
    let (res_tx, mut res_rx) = tokio::sync::mpsc::channel::<(senca_caption::chunk::ChunkInfo, Result<senca_caption::asr::Transcript, String>)>(8);
    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut prompt: Option<String> = None;
            while let Some(ch) = asr_rx.recv().await {
                let info = ch.info;
                let r = match asr.build_request(&ch, prompt.as_deref()) {
                    Ok(call) => match exec(&app, &call).await {
                        Ok(reply) => asr.parse_response(&info, &reply).map_err(|e| senca_caption::translate::describe_error(&e)),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(format!("{e:?}")),
                };
                if let Ok(t) = &r {
                    prompt = Some(t.text().chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect());
                }
                if res_tx.send((info, r)).await.is_err() {
                    return;
                }
            }
        });
    }
    let (tr_tx, mut tr_rx) = tokio::sync::mpsc::channel::<senca_caption::translate::TranslationBatch>(8);
    let (trres_tx, mut trres_rx) = tokio::sync::mpsc::channel::<(senca_caption::translate::TranslationBatch, Vec<Option<String>>)>(8);
    if let Some(tr) = tr {
        let app = app.clone();
        tokio::spawn(async move {
            while let Some(b) = tr_rx.recv().await {
                let r = match tr.build_request(&b) {
                    Ok(call) => match exec(&app, &call).await {
                        Ok(reply) => tr.parse_response(&b, &reply).map_err(|e| senca_caption::translate::describe_error(&e)),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(format!("{e:?}")),
                };
                match r {
                    Ok(v) => {
                        if trres_tx.send((b, v)).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => app.log(&format!("captions: translation failed: {e}")),
                }
            }
        });
    }
    let t0 = std::time::Instant::now();
    let mut publish_master = true;
    // 翻訳の結果を元の字幕に戻すため、最近の字幕を覚える
    let mut recent: BTreeMap<u64, senca_caption::merge::Cue> = BTreeMap::new();
    // 最後に閉じた映像のセグメントの終わり（媒体時刻）。遅れて届いた字幕はここへずらす（字幕は遅れて出てよい方式）
    let mut closed_until: i64 = i64::MIN;
    let lag = |mut cue: senca_caption::merge::Cue, closed: i64| {
        if cue.start_us < closed {
            let len = (cue.end_us - cue.start_us).max(500_000);
            cue.start_us = closed;
            cue.end_us = closed + len;
        }
        cue
    };
    loop {
        tokio::select! {
            ev = sub.recv() => match ev {
                Ok(HubEvent::Config(_, cfg)) if cfg.codec.kind() == MediaKind::Audio => { let _ = chunker.set_config(&cfg); }
                Ok(HubEvent::Au(a)) if a.codec.kind() == MediaKind::Audio => {
                    let _ = chunker.push(&a);
                    while let Ok(Some(ch)) = chunker.take_chunk() {
                        if asr_tx.try_send(ch).is_err() {
                            // 認識が遅れている: 字幕は欠けてよい（ライブ優先）
                        }
                    }
                }
                Ok(HubEvent::Segment { seq, start_us, duration_us, discontinuity }) => {
                    closed_until = closed_until.max(start_us + duration_us as i64);
                    for (l, s) in segs.iter_mut() {
                        if let Ok(v) = s.close_segment(seq, start_us, duration_us, discontinuity) {
                            cache.put_extra(&v.uri, Bytes::from(v.body), "text/vtt");
                            if let Ok(pl) = s.playlist() {
                                cache.put_extra(&format!("sub_{l}.m3u8"), Bytes::from(pl), "application/vnd.apple.mpegurl");
                            }
                        }
                    }
                    if publish_master {
                        put_master(&cache, &segs);
                        publish_master = false;
                    }
                }
                Ok(HubEvent::End) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => sub.lagged(n),
                _ => {}
            },
            Some((info, r)) = res_rx.recv() => match r {
                Ok(t) => {
                    if let Ok(cues) = merger.push(&info, &t) {
                        for cue in cues {
                            recent.insert(cue.id, cue.clone());
                            while recent.len() > 512 { let k = *recent.keys().next().unwrap(); recent.remove(&k); }
                            if let Some(s) = segs.get_mut(&c.language) { s.push_cue(lag(cue.clone(), closed_until)); }
                            if let Some(b) = batcher.as_mut() { b.push(cue, t0.elapsed().as_micros() as u64); }
                        }
                    }
                }
                Err(e) => app.log(&format!("captions {}: asr failed: {e}", stream.id)),
            },
            Some((b, v)) = trres_rx.recv() => {
                let cues: Vec<senca_caption::merge::Cue> = b.items.iter().filter_map(|i| recent.get(&i.cue_id).cloned()).collect();
                for cue in senca_caption::translate::translated_cues(&cues, &b, &v, 32) {
                    if let Some(s) = segs.get_mut(&b.target_lang) { s.push_cue(lag(cue, closed_until)); }
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        if let Some(b) = batcher.as_mut() {
            for batch in b.poll(t0.elapsed().as_micros() as u64, false) {
                let _ = tr_tx.try_send(batch);
            }
        }
    }
    for s in segs.values_mut() {
        s.end();
    }
}

/// `master.m3u8`（映像の index と字幕の EXT-X-MEDIA）。
fn put_master(cache: &crate::live::LiveCache, segs: &BTreeMap<String, SubtitleSegmenter>) {
    let rends: Vec<_> = segs.values().map(|s| s.rendition()).collect();
    let Ok(lines) = senca_caption::hls::media_lines(&rends) else { return };
    let mut m = String::from("#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    for l in lines {
        m.push_str(&l);
        m.push('\n');
    }
    let group = segs.values().next().map(|s| s.config().group_id.clone()).unwrap_or_else(|| "subs".into());
    m.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH=3000000,SUBTITLES=\"{group}\"\nindex.m3u8\n"));
    cache.put_extra("master.m3u8", Bytes::from(m), "application/vnd.apple.mpegurl");
}
