//! キャプチャ機器の入力（REQ-004、protocol = "device"）。機器の SDK はノードに読み込まず、
//! 別プロセスの senca-capture-worker で取り込み・符号化し、符号化済みの AU だけを受け取る。
//!
//! 入力の定義: `url` = `device:{video_device}`（例: `device:v4l2:/dev/video0`・`device:decklink:0`・`device:synthetic:0`）、
//! `options`: `codec`（既定 vp9）・`width`・`height`・`fps`・`bitrate_bps`・`audio_device`・`audio_codec`（既定 opus）。

use crate::app::App;
use crate::io::IoStatus;
use senca_capture::client::{CaptureWorker, WorkerCommand, WorkerEvent};
use senca_capture::spec::CaptureSpec;
use senca_control::model::Input;
use senca_core::TrackId;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn worker_cmd(app: &App) -> Option<WorkerCommand> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("senca-capture-worker");
    if !p.exists() {
        return None;
    }
    let mut c = WorkerCommand::new(p);
    if let Some(d) = &app.cfg.codec_lib_dir {
        c.env.push(("SENCA_CODEC_LIB_DIR".into(), d.display().to_string()));
    }
    if app.cfg.capture_synthetic {
        c.args.push("--synthetic".into());
    }
    Some(c)
}

/// `GET /devices` に出すキャプチャ機器。
pub fn listing(app: &App) -> Value {
    let Some(cmd) = worker_cmd(app) else { return json!({ "available": false, "reason": "senca-capture-worker がありません" }) };
    let Some(rt) = app.rt.get() else { return Value::Null };
    match rt.block_on(senca_capture::client::list_devices(&cmd, Duration::from_secs(5))) {
        Ok(l) => serde_json::to_value(&l).unwrap_or(Value::Null),
        Err(e) => json!({ "available": false, "reason": e.to_string() }),
    }
}

fn spec_of(i: &Input) -> Result<CaptureSpec, String> {
    let dev = i.url.strip_prefix("device:").ok_or("url は device:{機器 ID}")?;
    let o = &i.options;
    let num = |k: &str, d: u32| o.get(k).map(|v| v.parse::<u32>().map_err(|_| format!("{k} は数"))).transpose().map(|x| x.unwrap_or(d));
    let mut v = json!({
        "video_device": dev,
        "video": {
            "width": num("width", 1280)?, "height": num("height", 720)?, "fps": num("fps", 30)?,
            "codec": o.get("codec").cloned().unwrap_or_else(|| "vp9".into()),
            "bitrate_bps": num("bitrate_bps", 2_500_000)?,
        },
    });
    if let Some(a) = o.get("audio_device") {
        v["audio_device"] = json!(a);
        v["audio"] = json!({ "codec": o.get("audio_codec").cloned().unwrap_or_else(|| "opus".into()) });
    }
    CaptureSpec::from_json(&v.to_string()).map_err(|e| e.to_string())
}

pub async fn run(app: &Arc<App>, i: &Input, st: &Mutex<IoStatus>) -> Result<(), String> {
    let cmd = worker_cmd(app).ok_or("senca-capture-worker がありません")?;
    let spec = spec_of(i)?;
    let rf = app.control.lock().unwrap().state.streams.get(&i.stream).is_some_and(|s| s.codec_profile == senca_control::model::CodecProfile::RoyaltyFree);
    let policy = senca_codec::Policy { royalty_free_only: rf, ..Default::default() };
    st.lock().unwrap().state = "connecting".into();
    let sid = u64::from_str_radix(&i.id[..16.min(i.id.len())], 16).unwrap_or(1);
    let (mut w, mut rx) = CaptureWorker::spawn(&cmd, sid, &i.tenant, policy, &spec, Duration::from_secs(15)).await.map_err(|e| e.to_string())?;
    let p = crate::io::attach(app, &i.stream, "device", &i.url).await?;
    st.lock().unwrap().connects += 1;
    st.lock().unwrap().state = "active".into();
    let mut gens: std::collections::HashMap<u32, u32> = Default::default();
    let r = loop {
        match rx.recv().await {
            Some(WorkerEvent::Media { track, config, aus }) => {
                if let Some(mut c) = config {
                    let g = gens.get(&track).map(|g| g + 1).unwrap_or(1);
                    c.generation = g;
                    gens.insert(track, g);
                    if p.config(TrackId(track), c).await.is_err() {
                        break Err("stream が閉じました".to_string());
                    }
                }
                for mut a in aus {
                    a.track = TrackId(track);
                    a.config_generation = *gens.get(&track).unwrap_or(&1);
                    st.lock().unwrap().bytes += a.payload.len() as u64;
                    if p.access_unit(a).await.is_err() {
                        break;
                    }
                }
            }
            Some(WorkerEvent::Error(e)) => break Err(e.to_string()),
            Some(WorkerEvent::Exited(c)) => break Err(format!("取り込みの worker が終わりました（{c:?}）")),
            Some(WorkerEvent::Closed(_)) | None => break Ok(()),
            Some(WorkerEvent::Event(_)) => {}
        }
    };
    let _ = w.close().await;
    let _ = w.wait(Duration::from_secs(3)).await;
    p.detach("device input stopped").await;
    r
}
