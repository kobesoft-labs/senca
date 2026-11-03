//! ノードの全体（App）。制御状態・stream タスクの登録・録画の情報・署名・能力。

use crate::api::Ctx;
use crate::api::streams::ImageFormat;
use crate::config::Config;
use crate::control::ControlCore;
use crate::hub::{self, HubDeps, HubHandle, PublishError, Publisher, Subscription};
use crate::live::LiveCache;
use crate::recorder::RecorderEvent;
use bytes::Bytes;
use hmac::{Hmac, Mac};
use senca_control::model::*;
use senca_control::schedule::Tz;
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode, secret};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// 録画の情報（索引から集計。API の表示用）。
#[derive(Debug, Clone, Default)]
pub struct RecInfo {
    pub duration_ms: u64,
    pub segments: u64,
    pub bytes: u64,
    pub gaps: Vec<Value>,
    pub has_thumbnail: bool,
}

pub struct App {
    pub cfg: Config,
    pub node_id: String,
    pub control: Mutex<ControlCore>,
    pub idem_lock: Mutex<()>,
    pub hubs: Mutex<HashMap<String, HubHandle>>,
    pub rec_info: Mutex<HashMap<String, RecInfo>>,
    pub scheduler_wake: Arc<tokio::sync::Notify>,
    pub snapshot_schedules: Mutex<HashMap<String, (String, u32)>>,
    shutting_down: AtomicBool,
    ready: AtomicBool,
    tz_cache: Mutex<HashMap<String, Option<Tz>>>,
    me: Mutex<Weak<App>>,
    /// tokio の実行環境（API のスレッドから非同期の仕事を出すため）。
    pub rt: std::sync::OnceLock<tokio::runtime::Handle>,
    pub codec: Option<Arc<crate::codec::CodecService>>,
    pub secret_box: crate::secrets::SecretBox,
    pub cluster: std::sync::OnceLock<Arc<crate::cluster::Cluster>>,
    pub io: crate::io::IoManager,
    pub pipelines: crate::pipelines::PipelineManager,
    pub captions_running: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    pub rtc: std::sync::OnceLock<Arc<crate::webrtc::Rtc>>,
    /// ノード間の中継（分散のとき）。
    pub relay: std::sync::OnceLock<Arc<crate::relay::Relay>>,
    /// グリッド（資源の申告・心拍・配置。ADR-0013）。
    pub grid: crate::grid::GridRuntime,
    /// 部屋のメディアパイプライン（ADR-0016）。
    pub mediapipes: crate::mediapipes::MediaPipelines,
    /// 部屋の出来事（SSE へ。合意ログに載せない）。
    pub room_events: tokio::sync::broadcast::Sender<crate::rooms::RoomEventOut>,
    /// 録画の保存先（S3 互換。設定したときだけ）。
    pub storage: std::sync::OnceLock<crate::storage::Storage>,
    /// 保存先へ上げる係。
    pub archiver: std::sync::OnceLock<Arc<crate::archive::Archiver>>,
    /// 削除の途中の録画（同じ録画を 2 度消しに行かない）。
    pub deleting: Mutex<std::collections::HashSet<String>>,
}

impl App {
    pub fn new(
        cfg: Config,
        control: ControlCore,
        node_id: String,
        codec: Option<Arc<crate::codec::CodecService>>,
        secret_box: crate::secrets::SecretBox,
    ) -> Arc<App> {
        let app = Arc::new(App {
            cfg,
            node_id,
            control: Mutex::new(control),
            idem_lock: Mutex::new(()),
            hubs: Mutex::new(HashMap::new()),
            rec_info: Mutex::new(HashMap::new()),
            scheduler_wake: Arc::new(tokio::sync::Notify::new()),
            snapshot_schedules: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            tz_cache: Mutex::new(HashMap::new()),
            me: Mutex::new(Weak::new()),
            rt: std::sync::OnceLock::new(),
            codec,
            secret_box,
            cluster: std::sync::OnceLock::new(),
            relay: std::sync::OnceLock::new(),
            grid: Default::default(),
            room_events: tokio::sync::broadcast::channel(4096).0,
            mediapipes: Default::default(),
            io: crate::io::IoManager::default(),
            pipelines: crate::pipelines::PipelineManager::default(),
            captions_running: Mutex::new(HashMap::new()),
            rtc: std::sync::OnceLock::new(),
            storage: std::sync::OnceLock::new(),
            archiver: std::sync::OnceLock::new(),
            deleting: Mutex::new(Default::default()),
        });
        *app.me.lock().unwrap() = Arc::downgrade(&app);
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            let _ = app.rt.set(h);
        }
        app
    }

    fn arc(&self) -> Arc<App> {
        self.me.lock().unwrap().upgrade().expect("app alive")
    }

    /// 制御状態を変える唯一の入口。単一ノードではその場で、分散では合意を取ってから適用する。
    /// ブロックするので、tokio のワーカーからは spawn_blocking の中で呼ぶ。
    pub fn propose(&self, cmd: senca_control::state::Command) -> Result<(), Problem> {
        match self.cluster.get() {
            None => self.control.lock().unwrap().propose(cmd),
            Some(c) => {
                // 早めの 4xx: 手元の写しで試す（確定の結果は合意後の適用で決まる）
                {
                    let mut probe = self.control.lock().unwrap().state.clone();
                    probe.apply(cmd.clone())?;
                }
                c.propose(cmd)
            }
        }
    }

    pub fn log(&self, msg: &str) {
        if self.cfg.log_json {
            println!("{}", json!({ "ts": crate::util::rfc3339(crate::util::now_ms()), "node": self.node_id, "msg": msg }));
        } else {
            eprintln!("{} {msg}", crate::util::rfc3339(crate::util::now_ms()));
        }
    }

    /// 監査ログ（仕様 9.5）。秘密・本文は書かない。
    pub fn audit(&self, ctx: &Ctx, action: &str, resource: &str) {
        let line = json!({
            "ts": crate::util::rfc3339(ctx.now_ms),
            "tenant": ctx.principal.tenant,
            "subject": ctx.principal.subject,
            "action": action,
            "resource": resource,
            "request_id": ctx.req.request_id,
        });
        let path = self.cfg.data_dir.join("audit.log");
        let r = std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{line}")
        });
        if let Err(e) = r {
            self.log(&format!("audit log write failed: {e}"));
        }
    }

    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed) && !self.shutting_down()
    }

    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }

    pub fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    pub fn wake_scheduler(&self) {
        self.scheduler_wake.notify_one();
    }

    pub fn timezone(&self, name: &str) -> Option<Tz> {
        if name == "UTC" || name == "Etc/UTC" {
            return Some((Tz::UTC)());
        }
        if name.is_empty() || name.len() > 64 || name.split('/').any(|s| s.is_empty() || s == "." || s == ".." || s.starts_with('.')) {
            return None;
        }
        let mut c = self.tz_cache.lock().unwrap();
        c.entry(name.to_string())
            .or_insert_with(|| std::fs::read(self.cfg.zoneinfo_dir.join(name)).ok().and_then(|b| Tz::parse_tzif(&b)))
            .clone()
    }

    // ---- stream タスク ----

    fn hub_deps(&self) -> HubDeps {
        let me = self.arc();
        let me2 = self.arc();
        let me3 = self.arc();
        HubDeps {
            data_dir: self.cfg.data_dir.clone(),
            record_queue: (self.cfg.record_queue / 64).clamp(16, 4096),
            reconnect_grace: Duration::from_millis(self.cfg.reconnect_grace_ms),
            on_recorder_event: Arc::new(move |e| me.on_recorder_event(e)),
            on_idle: Arc::new(move |id| me2.on_hub_idle(id)),
            log: Arc::new(move |m| me3.log(m)),
        }
    }

    pub fn hub(&self, s: &Stream) -> HubHandle {
        let mut hubs = self.hubs.lock().unwrap();
        if let Some(h) = hubs.get(&s.id) {
            return h.clone();
        }
        let h = hub::spawn(s.clone(), self.hub_deps());
        hubs.insert(s.id.clone(), h.clone());
        h
    }

    fn on_hub_idle(&self, _id: &str) {}

    pub async fn attach_publisher(&self, key: &str, transport: &'static str, peer: &str) -> Result<Publisher, PublishError> {
        if !key.starts_with(crate::api::streams::KEY_PREFIX) || key.len() > 128 {
            return Err(PublishError::BadKey);
        }
        let h = secret::hash(key);
        let stream = {
            let core = self.control.lock().unwrap();
            core.state.streams.values().find(|s| s.key_hash == h).cloned()
        };
        let Some(stream) = stream else { return Err(PublishError::BadKey) };
        if !self.publish_allowed(&stream) {
            return Err(PublishError::Rejected("outside of the live schedule".into()));
        }
        self.attach_stream(&stream, transport, peer).await
    }

    /// stream の送り手としてつなぐ（publish 鍵の経路と、入力の pull・キャプチャの経路に共通）。
    /// 写しを止め、字幕・所有権の主張・自動録画を始める。
    pub async fn attach_stream(&self, stream: &Stream, transport: &'static str, peer: &str) -> Result<Publisher, PublishError> {
        let stream = stream.clone();
        // 写しがあれば止めて、本物の送り手に譲る
        if let Some(r) = self.relay.get()
            && r.is_mirror(&stream.id)
        {
            r.stop_mirror(self, &stream.id).await;
        }
        let hub = self.hub(&stream);
        let p = hub.attach(transport, peer).await?;
        // 字幕（設定があるときだけ。外部の送り先は利用者が設定したものだけ）
        if stream.captions.is_some() {
            let me = self.arc();
            let s2 = stream.clone();
            let key = s2.id.clone();
            let mut running = self.captions_running.lock().unwrap();
            if running.get(&key).is_none_or(|t: &tokio::task::JoinHandle<()>| t.is_finished()) {
                running.insert(key, tokio::spawn(async move { crate::captions::run(me, s2).await }));
            }
        }
        // 分散: 所有権を取る（古い所有者は送り手を切る）
        if self.cluster.get().is_some() {
            let me = self.arc();
            let sid = stream.id.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = me.propose(Command::ClaimStream { stream: sid.clone(), node: me.node_id.clone(), now_ms: crate::util::now_ms() }) {
                    me.log(&format!("cluster: claim stream {sid} failed: {e}"));
                }
            });
        }
        // 録画方針: publish で自動録画
        if stream.recording.enabled && !self.has_active_auto_recording(&stream.id) {
            let me = self.arc();
            let s2 = stream.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = me.start_recording(&s2, None, s2.recording.retention_days, None, None) {
                    me.log(&format!("auto recording for {} failed: {e}", s2.id));
                }
            });
        }
        Ok(p)
    }

    /// 予約に「live」があれば、その窓の間だけ publish を許す。
    fn publish_allowed(&self, s: &Stream) -> bool {
        let core = self.control.lock().unwrap();
        let lives: Vec<&Schedule> = core
            .state
            .schedules
            .values()
            .filter(|x| x.stream == s.id && x.action == ScheduleAction::Live && !matches!(x.state, ScheduleState::Canceled))
            .collect();
        if lives.is_empty() {
            return true;
        }
        lives.iter().any(|x| x.state == ScheduleState::Running)
    }

    fn has_active_auto_recording(&self, stream: &str) -> bool {
        let core = self.control.lock().unwrap();
        core.state
            .recordings
            .values()
            .any(|r| r.stream == stream && r.state == RecordingState::Recording && r.schedule.is_none())
    }

    /// RTMP play（SRT・RTSP の play、WHEP も）: キーは署名付きの `{stream_id}?exp=..&sig=..`（publish 鍵では見られない）。
    pub fn subscribe_rtmp_play(&self, key: &str) -> Option<Subscription> {
        let (id, q) = key.split_once('?').unwrap_or((key, ""));
        let mut exp = None;
        let mut sig = None;
        for kv in q.split('&') {
            if let Some(v) = kv.strip_prefix("exp=") {
                exp = v.parse::<i64>().ok();
            } else if let Some(v) = kv.strip_prefix("sig=") {
                sig = Some(v);
            }
        }
        let (exp, sig) = (exp?, sig?);
        if crate::util::now_ms() / 1000 > exp || !self.verify_signature("live", id, exp, sig) {
            return None;
        }
        let hub = self.source_hub(id)?;
        Some(hub.subscribe())
    }

    /// stream の hub。このノードに無く、別のノードが持っていれば中継の写しを作る（ADR-0014）。
    pub fn source_hub(&self, id: &str) -> Option<HubHandle> {
        let local = self.hubs.lock().unwrap().get(id).cloned();
        if let Some(r) = self.relay.get() {
            if local.as_ref().is_some_and(|h| h.is_live()) && !r.is_mirror(id) {
                return local;
            }
            if let Some(h) = r.ensure_mirror(&self.arc(), id) {
                return Some(h);
            }
        }
        local
    }

    pub fn live_info(&self, id: &str) -> Option<Value> {
        let hub = self.hubs.lock().unwrap().get(id).cloned()?;
        let l = hub.shared.live.lock().unwrap().clone()?;
        serde_json::to_value(l).ok()
    }

    pub fn live_cache(&self, id: &str) -> Option<Arc<LiveCache>> {
        self.hubs.lock().unwrap().get(id).map(|h| h.shared.cache.clone())
    }

    pub fn on_stream_changed(&self, s: &Stream) {
        if let Some(h) = self.hubs.lock().unwrap().get(&s.id) {
            h.update(s);
        }
    }

    pub fn on_stream_deleted(&self, id: &str) {
        self.io_reconcile();
        let h = self.hubs.lock().unwrap().remove(id);
        if let Some(h) = h {
            tokio::spawn(async move { h.shutdown().await });
        }
        let _ = std::fs::remove_dir_all(self.cfg.data_dir.join("snapshots").join(id));
    }

    // ---- 録画 ----

    pub fn start_recording(
        &self,
        s: &Stream,
        name: Option<String>,
        retention_days: Option<u32>,
        schedule: Option<String>,
        not_before_ms: Option<i64>,
    ) -> Result<Recording, Problem> {
        if s.desired_state != "running" {
            return Err(Problem::new(ProblemCode::Conflict, "stream が stopped です"));
        }
        let active = {
            let core = self.control.lock().unwrap();
            core.state.recordings.values().filter(|r| r.stream == s.id && r.state == RecordingState::Recording).count()
        };
        if active >= 8 {
            return Err(Problem::new(ProblemCode::QuotaExceeded, "同じ stream の同時の録画が上限（8）です"));
        }
        let now = crate::util::now_ms();
        let rec = Recording {
            id: crate::api::new_id()?,
            tenant: s.tenant.clone(),
            stream: s.id.clone(),
            generation: 1,
            state: RecordingState::Recording,
            started_at_ms: now,
            ended_at_ms: None,
            retention_until_ms: retention_days.map(|d| now + d as i64 * 86_400_000),
            legal_hold: false,
            schedule,
            name,
            node: Some(self.node_id.clone()),
            storage: None,
        };
        // 保存してから書き手に頼む
        self.propose(Command::PutRecording { recording: rec.clone() })?;
        let hub = self.hub(s);
        let id = rec.id.clone();
        let res = tokio::runtime::Handle::current().block_on(hub.start_recording(&id, not_before_ms));
        if let Err(e) = res {
            self.log(&format!("recording {id} could not start: {e}"));
            let mut r = rec.clone();
            r.state = RecordingState::Interrupted;
            r.ended_at_ms = Some(crate::util::now_ms());
            r.generation += 1;
            let _ = self.propose(Command::PutRecording { recording: r });
            return Err(Problem::internal());
        }
        self.rec_info.lock().unwrap().insert(rec.id.clone(), RecInfo::default());
        Ok(rec)
    }

    pub fn stop_recording(&self, id: &str, end_at_ms: Option<i64>) {
        let r = self.control.lock().unwrap().state.recordings.get(id).cloned();
        let Some(r) = r else { return };
        let hub = self.hubs.lock().unwrap().get(&r.stream).cloned();
        match hub {
            Some(h) => h.stop_recording(id, end_at_ms),
            None => self.finish_recording(id, crate::util::now_ms(), RecordingState::Finalized),
        }
    }

    fn finish_recording(&self, id: &str, ended: i64, state: RecordingState) {
        let r = self.control.lock().unwrap().state.recordings.get(id).cloned();
        if let Some(mut r) = r
            && r.state == RecordingState::Recording
        {
            r.state = state;
            r.ended_at_ms = Some(ended);
            r.generation += 1;
            let _ = self.propose(Command::PutRecording { recording: r });
        }
    }

    fn on_recorder_event(&self, e: RecorderEvent) {
        match e {
            RecorderEvent::Written { rel } => {
                if let Some(a) = self.archiver.get() {
                    a.object(rel);
                }
            }
            RecorderEvent::Appended { recording, segment } => {
                let mut m = self.rec_info.lock().unwrap();
                let i = m.entry(recording).or_default();
                i.duration_ms += segment.duration_ms as u64;
                i.segments += 1;
                i.bytes += segment.bytes;
            }
            RecorderEvent::Stopped { recording, ended_at_ms, state } => {
                let st = if state == "finalized" { RecordingState::Finalized } else { RecordingState::Interrupted };
                self.finish_recording(&recording, ended_at_ms, st);
                let me = self.arc();
                // サムネイルを作ってから保存先へ上げる（サムネイルも一緒に上げる）
                std::thread::spawn(move || {
                    crate::thumbnail::generate(&me, &recording);
                    if let Some(a) = me.archiver.get() {
                        a.finalize(&recording);
                    }
                });
            }
            RecorderEvent::Failed { recording, error } => {
                self.log(&format!("recording {recording} failed: {error}"));
                self.finish_recording(&recording, crate::util::now_ms(), RecordingState::Interrupted);
            }
        }
    }

    pub fn recording_info(&self, id: &str) -> Option<RecInfo> {
        if let Some(i) = self.rec_info.lock().unwrap().get(id) {
            return Some(i.clone());
        }
        let idx = self.read_index(id)?;
        let info = RecInfo {
            duration_ms: idx.duration_ms(),
            segments: idx.segments.len() as u64,
            bytes: idx.bytes(),
            gaps: idx.gaps.iter().map(|(at, r, d)| json!({ "at": crate::util::rfc3339(*at), "reason": r, "dropped": d })).collect(),
            has_thumbnail: self.cfg.data_dir.join("recordings").join(id).join("thumbnail.jpg").exists()
                || self.control.lock().unwrap().state.recordings.get(id).and_then(|r| r.storage.as_ref()).is_some_and(|s| s.thumbnail),
        };
        self.rec_info.lock().unwrap().insert(id.to_string(), info.clone());
        Some(info)
    }

    pub fn delete_recording(&self, r: &Recording) -> Result<(), Problem> {
        if r.state == RecordingState::Recording {
            self.stop_recording(&r.id, None);
        }
        let mut d = r.clone();
        d.state = RecordingState::Deleting;
        d.generation += 1;
        self.propose(Command::PutRecording { recording: d })?;
        // 消すのは手元のファイルを持つノード（分散では、持ち主が適用されたコマンドを見て消す）
        if self.cluster.get().is_none() || r.node.as_deref().is_none_or(|n| n == self.node_id) {
            let me = self.arc();
            let id = r.id.clone();
            std::thread::spawn(move || crate::recovery::delete_recording(&me, &id));
        }
        Ok(())
    }

    pub fn recording_thumbnail(&self, id: &str) -> Option<Bytes> {
        if id.parse::<senca_core::Id128>().is_err() {
            return None;
        }
        self.rec_object_blocking(id, &format!("recordings/{id}/thumbnail.jpg"), None, 16 << 20)
    }

    pub fn export_mp4(&self, id: &str) -> Result<Bytes, Problem> {
        // 保存先にあれば、前に作った書き出しを使う
        if let Some((st, loc)) = self.remote_of(id)
            && let Some(rt) = self.rt.get()
        {
            let key = loc.key(&format!("recordings/{id}/export.mp4"));
            match crate::util::block_on(rt, st.client.get_object(&loc.bucket, &key, None, crate::storage::MAX_EXPORT)) {
                Ok(Some(g)) => return Ok(g.body),
                Ok(None) => {}
                Err(e) => self.log(&format!("storage: export of {id}: {e}")),
            }
            let b = crate::export::mp4(self, id)?;
            crate::archive::store_export(&self.arc(), id, b.clone());
            return Ok(b);
        }
        crate::export::mp4(self, id)
    }

    // ---- 再生 ----

    fn playback_key(&self) -> Vec<u8> {
        let k = self.control.lock().unwrap().state.playback_key.clone().unwrap_or_default();
        (0..k.len() / 2).filter_map(|i| u8::from_str_radix(&k[2 * i..2 * i + 2], 16).ok()).collect()
    }

    fn sign(&self, kind: &str, id: &str, exp: i64) -> String {
        use base64::Engine;
        let mut m = Hmac::<Sha256>::new_from_slice(&self.playback_key()).expect("hmac key");
        m.update(format!("{kind}/{id}/{exp}").as_bytes());
        let tag = m.finalize().into_bytes();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&tag[..16])
    }

    pub fn verify_signature(&self, kind: &str, id: &str, exp: i64, sig: &str) -> bool {
        use subtle::ConstantTimeEq;
        let want = self.sign(kind, id, exp);
        want.len() == sig.len() && bool::from(want.as_bytes().ct_eq(sig.as_bytes()))
    }

    pub fn signed_urls(&self, kind: &str, id: &str, exp: i64) -> Result<Value, Problem> {
        let sig = self.sign(kind, id, exp);
        let http = self.cfg.public.http.clone().unwrap_or_default();
        let mut v = json!({ "hls": format!("{http}/s/{exp}/{sig}/{kind}/{id}/index.m3u8") });
        if kind == "live" {
            v["rtmp"] = json!(format!("{}/play/{id}?exp={exp}&sig={sig}", self.cfg.public.rtmp));
            // WHEP は WebRTC（--webrtc-udp）を開いたノードだけ
            v["whep"] = if self.cfg.webrtc_udp.is_some() { json!(format!("{http}/whep/{id}?exp={exp}&sig={sig}")) } else { Value::Null };
            if let Some(r) = &self.cfg.public.rtsp {
                v["rtsp"] = json!(format!("{r}/live/{id}?exp={exp}&sig={sig}"));
            }
        } else {
            v["mp4"] = Value::Null;
        }
        Ok(v)
    }

    pub async fn vod_playlist(&self, id: &str) -> Option<String> {
        let idx = self.index_async(id).await?;
        {
            let core = self.control.lock().unwrap();
            let r = core.state.recordings.get(id)?;
            if r.state == RecordingState::Deleting {
                return None;
            }
        }
        let init = idx.init.as_ref().map(|_| "init.mp4".to_string());
        let mut p = idx.vod_playlist(|s| format!("{}-{}.{}", s.epoch, s.sequence, if idx.container == "fmp4" { "m4s" } else { "ts" }), init);
        if idx.end.is_none() {
            // 録画中は EVENT（ENDLIST なし）
            p = p.replace("#EXT-X-PLAYLIST-TYPE:VOD", "#EXT-X-PLAYLIST-TYPE:EVENT").replace("#EXT-X-ENDLIST\n", "");
        }
        Some(p)
    }

    /// VOD のファイル名 →（Content-Type, 相対パス, 保存先のキー）。
    pub async fn vod_object_ref(&self, id: &str, file: &str) -> Option<(&'static str, String, Option<String>)> {
        let idx = self.index_async(id).await?;
        if file == "init.mp4" {
            let (p, _) = idx.init.as_ref()?;
            return Some(("video/mp4", p.clone(), idx.init_key.clone()));
        }
        let (stem, ext) = file.rsplit_once('.')?;
        let (e, s) = stem.split_once('-')?;
        let (e, s): (u64, u64) = (e.parse().ok()?, s.parse().ok()?);
        let seg = idx.segments.iter().find(|x| x.epoch == e && x.sequence == s)?;
        let ct = match ext {
            "ts" => "video/mp2t",
            "m4s" => "video/iso.segment",
            _ => return None,
        };
        Some((ct, seg.path.clone(), seg.key.clone()))
    }

    // ---- 静止画 ----

    pub fn snapshot_now(&self, s: &Stream, fmt: ImageFormat) -> Result<Bytes, Problem> {
        let hub = self.hubs.lock().unwrap().get(&s.id).cloned();
        let Some(hub) = hub.filter(|h| h.is_live()) else {
            return Err(Problem::new(ProblemCode::Conflict, "配信中ではありません"));
        };
        let gop: Vec<_> = hub.shared.gop.lock().unwrap().clone();
        let configs = hub.shared.configs.lock().unwrap().clone();
        crate::snapshot::render(self, s, &configs, &gop, fmt)
    }

    pub fn snapshot_store_list(&self, stream: &str) -> Vec<Value> {
        crate::snapshot::list(self, stream)
    }

    pub fn snapshot_store_get(&self, stream: &str, id: &str) -> Option<(&'static str, Bytes)> {
        crate::snapshot::get(self, stream, id)
    }

    pub fn snapshot_store_delete(&self, stream: &str, id: &str) -> bool {
        crate::snapshot::delete(self, stream, id)
    }

    pub fn set_schedule_snapshots(&self, stream: &str, schedule: &str, interval: Option<u32>) {
        let mut m = self.snapshot_schedules.lock().unwrap();
        match interval {
            Some(i) => {
                m.insert(schedule.to_string(), (stream.to_string(), i));
            }
            None => {
                m.remove(schedule);
            }
        }
    }

    // ---- 計画・能力・ノード ----

    pub fn plan(&self, s: &Stream) -> Value {
        crate::plan::plan(self, s)
    }

    pub fn capabilities(&self) -> Value {
        let codec = self.codec.as_ref().map(|c| c.capabilities()).unwrap_or(json!([]));
        json!({
            "api_version": "v1",
            "node": self.node_id,
            "ingest": {
                "rtmp": self.cfg.rtmp_listen.is_some(),
                "rtmps": self.cfg.rtmps_listen.is_some(),
                "srt": self.cfg.srt_listen.is_some(),
                "rtsp_pull": true,
                "whip": self.cfg.webrtc_udp.is_some(),
            },
            "output": {
                "hls": { "containers": ["ts", "fmp4"], "ll_hls": true },
                "rtmp_play": true,
                "whep": self.cfg.webrtc_udp.is_some(),
            },
            "codecs": {
                "passthrough": ["h264", "h265", "av1", "vp8", "vp9", "aac", "opus", "pcma", "pcmu"],
                "transcode": codec,
            },
            "codec_profiles": ["any", "royalty_free"],
            "recordings": { "export": ["mp4"], "max_concurrent_per_stream": 8 },
            "schedules": { "actions": ["record", "live", "snapshot"], "recurrence": ["daily", "weekly"] },
            "inputs": ["rtmp_pull", "rtsp_pull", "srt_pull", "hls_pull", "device"],
            "outputs": ["rtmp_push", "srt_push", "paas"],
            "paas_providers": crate::paas::PROVIDERS,
            "pipelines": self.codec.is_some(),
            "captions": { "asr": ["openai", "whisper_cpp"], "translate": ["anthropic", "deepl", "openai"] },
            "rtsp_play": self.cfg.rtsp_listen.is_some(),
            "cluster": self.cluster.get().is_some(),
        })
    }

    pub fn render_node(&self, n: &NodeInfo) -> Value {
        let mine = n.id == self.node_id;
        let streams = if mine { self.hubs.lock().unwrap().values().filter(|h| h.is_live()).count() } else { 0 };
        json!({
            "id": n.id,
            "name": n.name,
            "draining": n.draining,
            "registered_at": crate::util::rfc3339(n.registered_at_ms),
            "labels": n.labels,
            "live_streams": streams,
            "self": mine,
            "relay": if mine { self.relay.get().map(|r| r.status()) } else { None },
        })
    }

    pub fn devices(&self) -> Value {
        let devs = self.codec.as_ref().map(|c| c.devices()).unwrap_or(json!({ "providers": [], "sandbox": null }));
        let capture = crate::capture::listing(self);
        json!({ "nodes": [{ "node": self.node_id, "devices": devs, "capture": capture }] })
    }

    /// 適用されたコマンドを見て、所有権を失った stream の送り手を切る（fencing）。
    pub fn watch_ownership(self: &Arc<Self>) {
        let mut rx = self.control.lock().unwrap().applied_tx.subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(a) => {
                        // 写し（中継で受けている hub）は切らない: 写しは所有者の移動に中継の側で付いていく
                        // 削除中になった録画で、手元のファイルを持つのが自分なら消す
                        if let Command::PutRecording { recording: r } = a.cmd.as_ref()
                            && r.state == RecordingState::Deleting
                            && r.node.as_deref() == Some(me.node_id.as_str())
                        {
                            let (m2, id) = (me.clone(), r.id.clone());
                            std::thread::spawn(move || crate::recovery::delete_recording(&m2, &id));
                        }
                        if let Command::ClaimStream { stream, node, .. } = a.cmd.as_ref()
                            && *node != me.node_id
                            && !me.relay.get().is_some_and(|r| r.is_mirror(stream))
                            && let Some(h) = me.hubs.lock().unwrap().get(stream)
                        {
                            h.evict();
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        });
    }

    /// この stream の配信・録画を持つ別のノードの公開 URL（分散のとき）。
    pub fn owner_base(&self, stream: &str) -> Option<String> {
        self.cluster.get()?;
        let core = self.control.lock().unwrap();
        let o = core.state.owners.get(stream)?;
        if o.node == self.node_id {
            return None;
        }
        core.state.nodes.get(&o.node)?.media_addr.clone()
    }

    pub fn recording_base(&self, rec: &str) -> Option<String> {
        self.cluster.get()?;
        let core = self.control.lock().unwrap();
        let n = core.state.recordings.get(rec)?.node.clone()?;
        if n == self.node_id {
            return None;
        }
        core.state.nodes.get(&n)?.media_addr.clone()
    }

    pub fn io_status(&self, id: &str) -> Option<crate::io::IoStatus> {
        self.io.status(id)
    }

    pub fn io_reconcile(&self) {
        let me = self.arc();
        // API のスレッド（tokio の外）からも呼ばれる
        if let Some(rt) = self.rt.get() {
            rt.spawn(async move { me.io.reconcile(&me) });
        }
    }

    pub fn pipelines_reconcile(&self) {
        let me = self.arc();
        if let Some(rt) = self.rt.get() {
            rt.spawn(async move { me.pipelines.reconcile(&me) });
        }
    }

    pub fn paas_cleanup(&self, o: &Output) {
        crate::paas::cleanup(self, o);
    }

    pub async fn webrtc_http(&self, h: &senca_http::RequestHead, body: Bytes, peer: &str) -> crate::http::Reply {
        match self.rtc.get() {
            Some(r) => crate::webrtc::http(&self.arc(), r, h, body, peer).await,
            None => crate::http::Reply { status: 404, headers: vec![], body: Bytes::new() },
        }
    }

    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        if let Some(r) = self.relay.get() {
            r.endpoint.go_away("node shutting down");
        }
        let hubs: Vec<HubHandle> = self.hubs.lock().unwrap().values().cloned().collect();
        for h in hubs {
            h.shutdown().await;
        }
        if let Some(c) = &self.codec {
            c.shutdown().await;
        }
    }
}

#[allow(dead_code)]
fn _unused(_: PathBuf) {}
