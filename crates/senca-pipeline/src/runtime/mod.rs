//! パイプラインの実行（codec worker のプロセスの中で動く）。
//!
//! # 流れ
//!
//! ```text
//! push_input(source, AU) ─┬─▶ [AuQueue] ─▶ 復号のスレッド ─▶ board/scale/crop/snapshot（同じスレッド）
//!                         │                      │
//!                         │                      ├─▶ [Inbox] ─▶ 合成のスレッド（刻み: キャンバスの fps、帯で並列）
//!                         │                      ├─▶ [Inbox] ─▶ ミキサーのスレッド（刻み: 20 ms）
//!                         │                      └─▶ [Inbox] ─▶ 符号化のスレッド ─▶ 出力の切り替え ─▶ RuntimeOutput
//!                         └─▶（素通し）────────────────────────────────────────────▶ 出力の切り替え
//! ```
//!
//! - 入力を読む側（`push_input`）は待たない（`overflow: drop`）。溢れた映像は次のキーフレームまで捨てて知らせる。
//! - 出力の時刻はセッション時刻（[`crate::clock`]）。映像と音声は同じ時間軸。
//! - 世代: `prepare` で新しい世代を作り（検証・コーデックを開く）、`commit` で動かし始め、出力のトラックごとに
//!   新しい世代の最初のキーフレームで切り替える（[`crate::generation::OutputSwitch`]）。切り替わるまでは古い世代が
//!   出力を続け、新しい世代が失敗・時間切れなら古い世代のまま失敗を返す（仕様 5.1）。
//! - 運用の操作は次の刻み（フレーム境界）で反映する。状態とメーターは `status_interval_ms`（既定 100 ms）ごとに通知する。

mod exec;

use crate::clock::{Clock, MonotonicClock, SourceTimeMap, TickScheduler};
use crate::control::{
    ComposeStatus, Control, ControlReply, EncoderStatus, Event, MeterReading, SourceStatus,
};
use crate::convert;
use crate::error::PipelineError;
use crate::generation::{
    Decision, GenerationId, GenerationManager, GenerationState, OutputSwitch, TrackKey,
};
use crate::model::{ClockMode, Graph, GraphOptions, Node};
use crate::queue::AuQueue;
use crate::validate::{Plan, Port, PortType, Resolved, ValidationContext, resolve};
use bytes::Bytes;
use exec::*;
use senca_codec::config::DecoderConfig;
use senca_codec::frame::PixelFormat;
use senca_codec::registry::{Policy, ProviderRegistry};
use senca_compose::{AudioMixer, Compositor, MixerConfig, Slate, SourcePolicy};
use senca_core::{AccessUnit, CodecConfig, MediaKind, TrackId};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 実行の設定。
#[derive(Clone)]
pub struct RuntimeOptions {
    pub policy: Policy,
    /// コーデックの提供者（worker の中で検出したもの）。
    pub registry: Arc<ProviderRegistry>,
    pub clock: Arc<dyn Clock>,
    /// 出力のチャネルの長さ。
    pub output_capacity: usize,
}

impl RuntimeOptions {
    pub fn new(policy: Policy, registry: Arc<ProviderRegistry>) -> Self {
        RuntimeOptions {
            policy,
            registry,
            clock: Arc::new(MonotonicClock::default()),
            output_capacity: 4096,
        }
    }
}

/// 符号化済みの出力（sink のトラックごと）。
#[derive(Debug, Clone)]
pub struct MediaOutput {
    pub sink: String,
    /// 出力先の SENCA の stream id。
    pub stream: String,
    pub track: u32,
    pub generation: GenerationId,
    /// トラックの設定（最初・変わったとき・世代が切り替わったときだけ）。
    pub config: Option<CodecConfig>,
    pub aus: Vec<AccessUnit>,
}

/// 板書の変化の静止画（R6-8）。
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub node: String,
    pub generation: GenerationId,
    pub pts_us: i64,
    pub width: u32,
    pub height: u32,
    pub changed_fraction: f64,
    pub jpeg: Bytes,
}

/// 実行の出力。
#[derive(Debug, Clone)]
pub enum RuntimeOutput {
    Media(MediaOutput),
    Snapshot(Snapshot),
    Event(Event),
}

/// 入力の受け付けの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputReport {
    pub accepted: u64,
    pub dropped: u64,
}

/// 入力（source）ごとの、世代をまたぐ情報。
#[derive(Debug, Default)]
struct SourceInfo {
    aus: u64,
    dropped: u64,
    dropped_reported: u64,
    last_arrival: Option<i64>,
    ended: bool,
    /// 選んだトラック（映像・音声）。
    video_track: Option<u32>,
    audio_track: Option<u32>,
    configs: HashMap<bool, CodecConfig>,
    codec_mismatch_reported: bool,
    last_frames: u64,
    fps: f32,
}

/// 入力の行き先（世代ごと）。
#[derive(Clone)]
enum InputRoute {
    Decode(Arc<AuQueue>),
    Passthrough(SinkTarget),
}

#[derive(Default, Clone)]
struct SourceRoutes {
    stream: String,
    video: Vec<InputRoute>,
    audio: Vec<InputRoute>,
    want_video_track: Option<u32>,
    want_audio_track: Option<u32>,
    video_codec: Option<senca_core::Codec>,
    audio_codec: Option<senca_core::Codec>,
}

type Job = Box<dyn FnOnce() + Send>;

/// 1 つの世代。
struct Generation {
    id: GenerationId,
    plan: Plan,
    options: GraphOptions,
    stop: Arc<AtomicBool>,
    routes: HashMap<String, SourceRoutes>,
    /// 動かす前のスレッドの中身（commit で起動する）。
    pending: Mutex<Vec<(String, Job)>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    queues: Vec<Arc<AuQueue>>,
    compose: BTreeMap<String, Arc<Mutex<ComposeCtl>>>,
    compose_inputs: BTreeMap<String, Vec<Vec<String>>>,
    mix: BTreeMap<String, Arc<Mutex<MixCtl>>>,
    snapshot: BTreeMap<String, Arc<AtomicBool>>,
    keyframe: BTreeMap<String, Arc<AtomicBool>>,
    decode_stats: Vec<(String, MediaKind, Arc<DecodeStats>)>,
    encode_stats: BTreeMap<String, Arc<EncodeStats>>,
    sink_tracks: BTreeSet<TrackKey>,
    commit_started_us: AtomicI64,
}

impl Generation {
    fn start(&self) {
        let jobs = std::mem::take(&mut *lock(&self.pending));
        let mut th = lock(&self.threads);
        for (name, job) in jobs {
            let h = std::thread::Builder::new()
                .name(format!("senca-{name}"))
                .spawn(job)
                .expect("spawn pipeline thread");
            th.push(h);
        }
    }

    /// 止める（スレッドは止める合図を見て終わる）。
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for q in &self.queues {
            q.close();
        }
        lock(&self.pending).clear();
    }

    fn end_inputs(&self) {
        for q in &self.queues {
            q.end();
        }
    }

    fn join(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let done = lock(&self.threads).iter().all(|h| h.is_finished());
            if done {
                for h in std::mem::take(&mut *lock(&self.threads)) {
                    let _ = h.join();
                }
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn request_keyframes(&self, node: Option<&str>) -> bool {
        let mut any = false;
        for (id, f) in &self.keyframe {
            if node.is_none_or(|n| n == id) {
                f.store(true, Ordering::Relaxed);
                any = true;
            }
        }
        any
    }
}

/// 世代と出力の切り替えの状態。
struct GenState {
    manager: GenerationManager,
    gens: BTreeMap<GenerationId, Arc<Generation>>,
    switch: OutputSwitch,
    /// トラックごとに最後に出した設定（世代, 設定の世代）。
    last_config: HashMap<TrackKey, (GenerationId, u32)>,
    retired: Vec<Arc<Generation>>,
    /// commit の結果（待つ側へ）。
    outcomes: HashMap<GenerationId, Result<(), PipelineError>>,
}

/// 実行の共有の状態（各スレッドから使う）。
pub(crate) struct RtShared {
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) policy: Policy,
    clock: Arc<dyn Clock>,
    start_us: i64,
    out: SyncSender<RuntimeOutput>,
    /// 合成・ミキサーが最後に出した刻みの時刻（入力時刻のモードで、新しい入力の時刻の対応付けに使う）。
    pub(crate) media_clock: AtomicI64,
    maps: Mutex<HashMap<String, SourceTimeMap>>,
    sources: Mutex<BTreeMap<String, SourceInfo>>,
    st: Mutex<GenState>,
    st_cv: Condvar,
    /// 出力の順序を保つ（判断と送信をまとめる）。
    send_lock: Mutex<()>,
    closed: AtomicBool,
    dropped_outputs: AtomicU64,
}

impl RtShared {
    pub(crate) fn session_now(&self) -> i64 {
        self.clock.now_us() - self.start_us
    }

    /// 出力を送る（一杯なら待つ: 符号化のスレッドへの背圧）。
    pub(crate) fn emit(&self, o: RuntimeOutput) {
        let _ = self.out.send(o);
    }

    /// 通知を送る（一杯なら捨てる）。
    pub(crate) fn emit_event(&self, e: Event) {
        if let Err(TrySendError::Full(_)) = self.out.try_send(RuntimeOutput::Event(e)) {
            self.dropped_outputs.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn map_time(&self, source: &str, pts_us: i64, anchor_us: i64, disc: bool) -> i64 {
        lock(&self.maps)
            .entry(source.to_string())
            .or_default()
            .map(pts_us, anchor_us, disc)
    }

    /// 符号化した AU を出力の切り替えに通して出す。古い時刻のキーフレームだったら `true`（キーフレームを求める）。
    pub(crate) fn route(
        &self,
        gen_id: GenerationId,
        t: &SinkTarget,
        config: Option<CodecConfig>,
        aus: Vec<AccessUnit>,
    ) -> bool {
        let _order = lock(&self.send_lock);
        let key: TrackKey = (t.sink.clone(), t.track);
        let mut want_key = false;
        let mut batches: Vec<(Option<CodecConfig>, Vec<AccessUnit>)> = vec![(None, Vec::new())];
        let completed;
        {
            let mut st = lock(&self.st);
            for mut au in aus {
                au.track = TrackId(t.track);
                au.epoch = gen_id;
                match st.switch.offer(gen_id, &key, &au) {
                    Decision::Emit { switched } => {
                        let cg = config.as_ref().map_or(0, |c| c.generation);
                        let last = st.last_config.get(&key).copied();
                        if switched || last != Some((gen_id, cg)) {
                            st.last_config.insert(key.clone(), (gen_id, cg));
                            // 設定が変わる所で分ける。
                            if batches.last().is_some_and(|b| !b.1.is_empty()) {
                                batches.push((None, Vec::new()));
                            }
                            if let Some(b) = batches.last_mut() {
                                b.0 = config.clone();
                            }
                        }
                        if let Some(b) = batches.last_mut() {
                            b.1.push(au);
                        }
                    }
                    Decision::Drop => {}
                    Decision::DropRequestKeyframe => want_key = true,
                }
            }
            completed = if st.switch.pending().is_some() && st.switch.try_complete() {
                st.switch.active()
            } else {
                None
            };
        }
        for (config, aus) in batches {
            if aus.is_empty() {
                continue;
            }
            self.emit(RuntimeOutput::Media(MediaOutput {
                sink: t.sink.clone(),
                stream: t.stream.clone(),
                track: t.track,
                generation: gen_id,
                config,
                aus,
            }));
        }
        drop(_order);
        if let Some(g) = completed {
            self.finish_commit(g);
        }
        want_key
    }

    /// 切り替えが終わった: 新しい世代を active に、古い世代を止める。
    fn finish_commit(&self, g: GenerationId) {
        let old = {
            let mut st = lock(&self.st);
            let old = match st.manager.complete_commit(g) {
                Ok(o) => o,
                Err(_) => return,
            };
            st.outcomes.insert(g, Ok(()));
            let old = old.and_then(|o| st.gens.remove(&o));
            if let Some(o) = &old {
                st.retired.push(o.clone());
            }
            old
        };
        if let Some(o) = old {
            o.stop();
        }
        self.st_cv.notify_all();
        self.emit_event(Event::GenerationCommitted { generation: g });
    }

    /// 切り替え中の世代を失敗にする（古い世代を続ける）。
    fn fail_commit(&self, g: GenerationId, err: PipelineError) {
        let gen_arc = {
            let mut st = lock(&self.st);
            if st.manager.state(g) != Some(GenerationState::Committing) {
                return;
            }
            let _ = st.manager.fail_commit(g);
            if st.switch.pending() == Some(g) {
                st.switch.abort();
            }
            st.outcomes.insert(g, Err(err.clone()));
            let gen_arc = st.gens.remove(&g);
            if let Some(a) = st.manager.active().and_then(|a| st.gens.get(&a)) {
                // 戻したトラックは古い世代のキーフレームから出し直す。
                a.request_keyframes(None);
            }
            if let Some(x) = &gen_arc {
                st.retired.push(x.clone());
            }
            gen_arc
        };
        if let Some(x) = gen_arc {
            x.stop();
        }
        self.st_cv.notify_all();
        self.emit_event(Event::GenerationFailed {
            generation: g,
            code: err.api_code().to_string(),
            message: err.to_string(),
        });
    }

    fn live_gens(&self) -> Vec<Arc<Generation>> {
        lock(&self.st)
            .gens
            .values()
            .filter(|g| !g.stop.load(Ordering::Relaxed) && lock(&g.pending).is_empty())
            .cloned()
            .collect()
    }
}

/// パイプラインの実行。
pub struct Runtime {
    rt: Arc<RtShared>,
    outputs: Mutex<Option<Receiver<RuntimeOutput>>>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    stop_supervisor: Arc<AtomicBool>,
}

impl Runtime {
    /// 検証して最初の世代を動かし始める。
    pub fn start(graph: &Graph, opts: RuntimeOptions) -> Result<Runtime, PipelineError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(opts.output_capacity.max(16));
        let rt = Arc::new(RtShared {
            registry: opts.registry.clone(),
            policy: opts.policy.clone(),
            start_us: opts.clock.now_us(),
            clock: opts.clock.clone(),
            out: tx,
            media_clock: AtomicI64::new(0),
            maps: Mutex::new(HashMap::new()),
            sources: Mutex::new(BTreeMap::new()),
            st: Mutex::new(GenState {
                manager: GenerationManager::new(),
                gens: BTreeMap::new(),
                switch: OutputSwitch::default(),
                last_config: HashMap::new(),
                retired: Vec::new(),
                outcomes: HashMap::new(),
            }),
            st_cv: Condvar::new(),
            send_lock: Mutex::new(()),
            closed: AtomicBool::new(false),
            dropped_outputs: AtomicU64::new(0),
        });
        let r = Runtime {
            rt,
            outputs: Mutex::new(Some(rx)),
            supervisor: Mutex::new(None),
            stop_supervisor: Arc::new(AtomicBool::new(false)),
        };
        let (g, _) = r.prepare(graph)?;
        r.commit_inner(g, false)?;
        let rt = r.rt.clone();
        let stop = r.stop_supervisor.clone();
        let h = std::thread::Builder::new()
            .name("senca-pipeline-supervisor".into())
            .spawn(move || supervisor(rt, stop))
            .map_err(|e| PipelineError::runtime("INTERNAL_ERROR", e.to_string()))?;
        *lock(&r.supervisor) = Some(h);
        Ok(r)
    }

    /// 出力の受け口（1 回だけ取り出せる）。
    pub fn take_outputs(&self) -> Option<Receiver<RuntimeOutput>> {
        lock(&self.outputs).take()
    }

    /// 能力（検証に使う）。
    pub fn validation_context(&self) -> ValidationContext {
        ValidationContext::new(
            self.rt.policy.clone(),
            self.rt.registry.capabilities(&self.rt.policy),
        )
    }

    /// 今の active の世代の計画。
    pub fn plan(&self) -> Option<Plan> {
        let st = lock(&self.rt.st);
        st.manager
            .active()
            .and_then(|a| st.gens.get(&a))
            .map(|g| g.plan.clone())
    }

    /// 今の active の世代。
    pub fn active_generation(&self) -> Option<GenerationId> {
        lock(&self.rt.st).manager.active()
    }

    pub fn generation_state(&self, g: GenerationId) -> Option<GenerationState> {
        lock(&self.rt.st).manager.state(g)
    }

    /// セッション時刻（µs）。
    pub fn session_now(&self) -> i64 {
        self.rt.session_now()
    }

    /// 入力の AU を入れる（待たない。`overflow: block` のときだけ上限の時間まで待つ）。
    /// `config` はトラックの設定（最初と、変わったとき）。
    pub fn push_input(
        &self,
        source: &str,
        config: Option<CodecConfig>,
        aus: Vec<AccessUnit>,
    ) -> Result<InputReport, PipelineError> {
        if self.rt.closed.load(Ordering::Relaxed) {
            return Err(PipelineError::Closed);
        }
        let gens = self.rt.live_gens();
        if !gens.iter().any(|g| g.routes.contains_key(source)) {
            return Err(PipelineError::Control(format!("unknown source '{source}'")));
        }
        let now = self.rt.session_now();
        let mut rep = InputReport::default();
        // トラックの選択と設定
        let (sel_v, sel_a) = {
            let mut s = lock(&self.rt.sources);
            let info = s.entry(source.to_string()).or_default();
            info.last_arrival = Some(now);
            info.ended = false;
            if let Some(c) = &config {
                info.configs
                    .insert(c.codec.kind() == MediaKind::Video, c.clone());
            }
            let r = gens
                .iter()
                .find_map(|g| g.routes.get(source))
                .cloned()
                .unwrap_or_default();
            for au in &aus {
                match au.kind() {
                    MediaKind::Video if info.video_track.is_none() => {
                        info.video_track = Some(r.want_video_track.unwrap_or(au.track.0));
                    }
                    MediaKind::Audio if info.audio_track.is_none() => {
                        info.audio_track = Some(r.want_audio_track.unwrap_or(au.track.0));
                    }
                    _ => {}
                }
            }
            (info.video_track, info.audio_track)
        };
        let mut mismatch = false;
        for g in &gens {
            let Some(r) = g.routes.get(source) else {
                continue;
            };
            let kinds = [
                (MediaKind::Video, &r.video, sel_v, r.video_codec),
                (MediaKind::Audio, &r.audio, sel_a, r.audio_codec),
            ];
            for (kind, routes, sel, codec) in kinds {
                if routes.is_empty() {
                    continue;
                }
                if let Some(c) = config.as_ref().filter(|c| c.codec.kind() == kind) {
                    for rt in routes {
                        if let InputRoute::Decode(q) = rt {
                            q.push_config(c.clone());
                        }
                    }
                }
                let mut mine = Vec::new();
                for au in aus
                    .iter()
                    .filter(|a| a.kind() == kind && sel == Some(a.track.0))
                {
                    if codec.is_some_and(|c| c != au.codec) {
                        mismatch = true;
                        rep.dropped += 1;
                    } else {
                        mine.push(au);
                    }
                }
                if mine.is_empty() {
                    continue;
                }
                for rt in routes {
                    match rt {
                        InputRoute::Decode(q) => {
                            for au in &mine {
                                rep.dropped += q.push((*au).clone());
                            }
                        }
                        InputRoute::Passthrough(t) => {
                            let cfg = lock(&self.rt.sources)
                                .get(source)
                                .and_then(|s| s.configs.get(&(kind == MediaKind::Video)).cloned());
                            self.rt.route(
                                g.id,
                                t,
                                cfg,
                                mine.iter().map(|a| (*a).clone()).collect(),
                            );
                        }
                    }
                }
            }
        }
        rep.accepted = aus.len() as u64;
        {
            let mut s = lock(&self.rt.sources);
            let info = s.entry(source.to_string()).or_default();
            info.aus += aus.len() as u64;
            info.dropped += rep.dropped;
            if mismatch && !info.codec_mismatch_reported {
                info.codec_mismatch_reported = true;
                drop(s);
                self.rt.emit_event(Event::Error {
                    node: Some(source.to_string()),
                    code: "INVALID_INPUT".into(),
                    message: "input codec differs from the codec declared in the graph; dropped"
                        .into(),
                });
            }
        }
        Ok(rep)
    }

    /// 入力の終わり（送り手が去った、ファイルの終わり）。
    pub fn end_input(&self, source: &str) {
        if let Some(i) = lock(&self.rt.sources).get_mut(source) {
            i.ended = true;
        }
        for g in self.rt.live_gens() {
            if let Some(r) = g.routes.get(source) {
                for rt in r.video.iter().chain(&r.audio) {
                    if let InputRoute::Decode(q) = rt {
                        q.end();
                    }
                }
            }
        }
    }

    /// 新しい世代を準備する（検証し、コーデックを開いて確かめる）。まだ出力しない。
    pub fn prepare(&self, graph: &Graph) -> Result<(GenerationId, Plan), PipelineError> {
        if self.rt.closed.load(Ordering::Relaxed) {
            return Err(PipelineError::Closed);
        }
        let ctx = self.validation_context();
        let resolved = resolve(graph, &ctx)?;
        let id = lock(&self.rt.st).manager.prepare();
        match build_generation(&self.rt, id, &resolved) {
            Ok(g) => {
                let plan = g.plan.clone();
                lock(&self.rt.st).gens.insert(id, Arc::new(g));
                self.rt
                    .emit_event(Event::GenerationPrepared { generation: id });
                Ok((id, plan))
            }
            Err(e) => {
                let _ = lock(&self.rt.st).manager.fail_commit(id);
                Err(e)
            }
        }
    }

    /// 準備した世代に切り替える。切り替わるか失敗するまで待つ。失敗しても古い世代は続く。
    pub fn commit(&self, g: GenerationId) -> Result<(), PipelineError> {
        self.commit_inner(g, true)
    }

    fn commit_inner(&self, g: GenerationId, wait: bool) -> Result<(), PipelineError> {
        let (gen_arc, first) = {
            let mut st = lock(&self.rt.st);
            st.manager.begin_commit(g)?;
            let gen_arc = st.gens.get(&g).cloned().ok_or_else(|| {
                PipelineError::Generation(format!("generation {g} has no runtime"))
            })?;
            let first = st.switch.active().is_none();
            st.switch.begin(g, gen_arc.sink_tracks.clone());
            (gen_arc, first)
        };
        gen_arc
            .commit_started_us
            .store(self.rt.session_now(), Ordering::Relaxed);
        // 既に届いている設定を新しい世代へ
        {
            let s = lock(&self.rt.sources);
            for (src, r) in &gen_arc.routes {
                if let Some(info) = s.get(src) {
                    for (is_video, c) in &info.configs {
                        let routes = if *is_video { &r.video } else { &r.audio };
                        for rt in routes {
                            if let InputRoute::Decode(q) = rt {
                                q.push_config(c.clone());
                            }
                        }
                    }
                }
            }
        }
        // 合成・ミキサーの刻みは今のメディア時刻の続きから
        gen_arc.start();
        if first {
            let mut st = lock(&self.rt.st);
            st.manager.complete_commit(g)?;
            st.outcomes.insert(g, Ok(()));
            drop(st);
            self.rt
                .emit_event(Event::GenerationCommitted { generation: g });
            return Ok(());
        }
        if gen_arc.sink_tracks.is_empty() {
            // 出力が無い世代（静止画だけなど）はすぐ切り替える。
            let ok = {
                let mut st = lock(&self.rt.st);
                st.switch.try_complete()
            };
            if ok {
                self.rt.finish_commit(g);
            }
        }
        if !wait {
            return Ok(());
        }
        let timeout = Duration::from_millis(gen_arc.options.commit_timeout_ms as u64 + 2000);
        let deadline = std::time::Instant::now() + timeout;
        let mut st = lock(&self.rt.st);
        loop {
            if let Some(r) = st.outcomes.remove(&g) {
                return r;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                drop(st);
                self.rt.fail_commit(
                    g,
                    PipelineError::runtime("CAPACITY_UNAVAILABLE", "commit timed out"),
                );
                return Err(PipelineError::runtime(
                    "CAPACITY_UNAVAILABLE",
                    "commit timed out",
                ));
            }
            st = self
                .rt
                .st_cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// 切り替わっていない世代を捨てる。
    pub fn rollback(&self, g: GenerationId) -> Result<(), PipelineError> {
        let gen_arc = {
            let mut st = lock(&self.rt.st);
            st.manager.rollback(g)?;
            if st.switch.pending() == Some(g) {
                st.switch.abort();
                if let Some(a) = st.manager.active().and_then(|a| st.gens.get(&a)) {
                    a.request_keyframes(None);
                }
            }
            st.outcomes.insert(
                g,
                Err(PipelineError::Generation(format!(
                    "generation {g} rolled back"
                ))),
            );
            st.gens.remove(&g)
        };
        if let Some(x) = gen_arc {
            x.stop();
            lock(&self.rt.st).retired.push(x);
        }
        self.rt.st_cv.notify_all();
        self.rt
            .emit_event(Event::GenerationRolledBack { generation: g });
        Ok(())
    }

    /// 運用の操作。合成・ミキサーへの操作は次の刻みで反映する。
    pub fn control(&self, c: Control) -> Result<ControlReply, PipelineError> {
        if self.rt.closed.load(Ordering::Relaxed) {
            return Err(PipelineError::Closed);
        }
        match c {
            Control::Prepare { graph } => {
                let (g, plan) = self.prepare(&graph)?;
                return Ok(ControlReply {
                    generation: Some(g),
                    plan: Some(Box::new(plan)),
                });
            }
            Control::Commit { generation } => {
                self.commit(generation)?;
                return Ok(ControlReply {
                    generation: Some(generation),
                    plan: None,
                });
            }
            Control::Rollback { generation } => {
                self.rollback(generation)?;
                return Ok(ControlReply {
                    generation: Some(generation),
                    plan: None,
                });
            }
            _ => {}
        }
        let gen_arc = {
            let st = lock(&self.rt.st);
            st.manager
                .active()
                .and_then(|a| st.gens.get(&a).cloned())
                .ok_or(PipelineError::Closed)?
        };
        let unknown =
            |n: &str, what: &str| PipelineError::Control(format!("unknown {what} node '{n}'"));
        match c {
            Control::SwitchProgram {
                node,
                scene,
                transition,
            } => {
                let ctl = gen_arc
                    .compose
                    .get(&node)
                    .ok_or_else(|| unknown(&node, "compose"))?;
                let mut c = lock(ctl);
                let s =
                    c.scenes.get(&scene).cloned().ok_or_else(|| {
                        PipelineError::Control(format!("unknown scene '{scene}'"))
                    })?;
                let tr = transition.unwrap_or_default();
                check_transition(&tr)?;
                c.program = scene;
                c.pending.push(ComposeCmd::Program(s, tr.to_compose()));
            }
            Control::SetPreview { node, scene } => {
                let ctl = gen_arc
                    .compose
                    .get(&node)
                    .ok_or_else(|| unknown(&node, "compose"))?;
                let mut c = lock(ctl);
                if !c.scenes.contains_key(&scene) {
                    return Err(PipelineError::Control(format!("unknown scene '{scene}'")));
                }
                c.preview = Some(scene);
            }
            Control::Take { node, transition } => {
                let ctl = gen_arc
                    .compose
                    .get(&node)
                    .ok_or_else(|| unknown(&node, "compose"))?;
                let mut c = lock(ctl);
                let Some(p) = c.preview.clone() else {
                    return Err(PipelineError::Control("no preview scene is set".into()));
                };
                let tr = transition.unwrap_or(c.default_transition);
                check_transition(&tr)?;
                let s = c
                    .scenes
                    .get(&p)
                    .cloned()
                    .ok_or_else(|| PipelineError::Control("preview scene vanished".into()))?;
                let old = std::mem::replace(&mut c.program, p);
                c.preview = Some(old);
                c.pending.push(ComposeCmd::Program(s, tr.to_compose()));
            }
            Control::UpdateLayer {
                node,
                scene,
                layer,
                patch,
            } => {
                let ctl = gen_arc
                    .compose
                    .get(&node)
                    .ok_or_else(|| unknown(&node, "compose"))?;
                let mut c = lock(ctl);
                let mut spec =
                    c.specs.get(&scene).cloned().ok_or_else(|| {
                        PipelineError::Control(format!("unknown scene '{scene}'"))
                    })?;
                let l = spec.layers.get_mut(layer).ok_or_else(|| {
                    PipelineError::Control(format!("scene '{scene}' has no layer {layer}"))
                })?;
                if let Some(v) = patch.x {
                    l.x = v;
                }
                if let Some(v) = patch.y {
                    l.y = v;
                }
                if let Some(v) = patch.width {
                    l.width = v;
                }
                if let Some(v) = patch.height {
                    l.height = v;
                }
                if let Some(v) = patch.z {
                    l.z = v;
                }
                if let Some(v) = patch.opacity {
                    l.opacity = v;
                }
                if let Some(v) = patch.visible {
                    l.visible = v;
                }
                if let Some(v) = patch.source {
                    l.source = Some(v);
                    l.image = None;
                    l.color = None;
                }
                set_scene(&mut c, scene, spec)?;
            }
            Control::SetScene {
                node,
                scene,
                layers,
            } => {
                let ctl = gen_arc
                    .compose
                    .get(&node)
                    .ok_or_else(|| unknown(&node, "compose"))?;
                let mut c = lock(ctl);
                if !c.scenes.contains_key(&scene) && c.scenes.len() >= 64 {
                    return Err(PipelineError::Control("too many scenes".into()));
                }
                if scene.is_empty() || scene.len() > 64 {
                    return Err(PipelineError::Control("bad scene name".into()));
                }
                set_scene(&mut c, scene, crate::model::SceneSpec { layers })?;
            }
            Control::SetAudio {
                node,
                input,
                gain_db,
                mute,
                solo,
                pan,
                delay_ms,
            } => {
                let ctl = gen_arc
                    .mix
                    .get(&node)
                    .ok_or_else(|| unknown(&node, "audio_mix"))?;
                let mut c = lock(ctl);
                let i = c.names.iter().position(|n| *n == input).ok_or_else(|| {
                    PipelineError::Control(format!("unknown mixer input '{input}'"))
                })?;
                let mut cfg = c.configs[i];
                if let Some(v) = gain_db {
                    cfg.gain_db = v;
                }
                if let Some(v) = mute {
                    cfg.mute = v;
                }
                if let Some(v) = solo {
                    cfg.solo = v;
                }
                if let Some(v) = pan {
                    cfg.pan = v;
                }
                if let Some(v) = delay_ms {
                    cfg.delay_ms = v;
                }
                let spec = crate::model::MixInputSpec {
                    from: String::new(),
                    gain_db: cfg.gain_db,
                    mute: cfg.mute,
                    solo: cfg.solo,
                    pan: cfg.pan,
                    delay_ms: cfg.delay_ms,
                };
                convert::check_mix_input(&spec).map_err(PipelineError::Control)?;
                c.configs[i] = cfg;
                c.dirty = true;
            }
            Control::SetMaster { node, gain_db } => {
                let ctl = gen_arc
                    .mix
                    .get(&node)
                    .ok_or_else(|| unknown(&node, "audio_mix"))?;
                if !(gain_db.is_finite() && (-120.0..=24.0).contains(&gain_db)) {
                    return Err(PipelineError::Control("gain_db must be -120..=24".into()));
                }
                let mut c = lock(ctl);
                c.master_gain_db = gain_db;
                c.dirty = true;
            }
            Control::Snapshot { node, enabled } => {
                let f = gen_arc
                    .snapshot
                    .get(&node)
                    .ok_or_else(|| unknown(&node, "snapshot_on_change"))?;
                f.store(enabled, Ordering::Relaxed);
            }
            Control::RequestKeyframe { node } => {
                if !gen_arc.request_keyframes(node.as_deref()) {
                    return Err(unknown(node.as_deref().unwrap_or("*"), "video encode"));
                }
            }
            Control::Prepare { .. } | Control::Commit { .. } | Control::Rollback { .. } => {
                unreachable!("handled above")
            }
        }
        Ok(ControlReply::default())
    }

    /// 状態の通知を今作る（試験・UI 用）。
    pub fn status(&self) -> Vec<Event> {
        status_events(&self.rt)
    }

    /// 入力を終わりにし、残りを出し切って止める（`timeout` まで待つ）。出力のチャネルは閉じる。
    pub fn close(&self, timeout: Duration) {
        if self.rt.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let (active, others) = {
            let st = lock(&self.rt.st);
            let a = st.manager.active();
            let active = a.and_then(|a| st.gens.get(&a).cloned());
            let others: Vec<_> = st
                .gens
                .iter()
                .filter(|(k, _)| Some(**k) != a)
                .map(|(_, v)| v.clone())
                .collect();
            (active, others)
        };
        for o in &others {
            o.stop();
        }
        if let Some(a) = &active {
            a.end_inputs();
            if !a.join(timeout) {
                a.stop();
                a.join(Duration::from_secs(5));
            }
        }
        self.stop_supervisor.store(true, Ordering::SeqCst);
        if let Some(h) = lock(&self.supervisor).take() {
            let _ = h.join();
        }
        for o in &others {
            o.join(Duration::from_secs(5));
        }
        let retired = std::mem::take(&mut lock(&self.rt.st).retired);
        for r in retired {
            r.stop();
            r.join(Duration::from_secs(5));
        }
        // 最後の状態
        for e in status_events(&self.rt) {
            self.rt.emit_event(e);
        }
        lock(&self.rt.st).gens.clear();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if !self.rt.closed.load(Ordering::SeqCst) {
            self.rt.closed.store(true, Ordering::SeqCst);
            let gens: Vec<_> = lock(&self.rt.st).gens.values().cloned().collect();
            for g in gens {
                g.stop();
            }
            self.stop_supervisor.store(true, Ordering::SeqCst);
        }
    }
}

fn check_transition(t: &crate::model::TransitionSpec) -> Result<(), PipelineError> {
    if t.duration_ms > 60_000 {
        return Err(PipelineError::Control("transition longer than 60 s".into()));
    }
    Ok(())
}

fn set_scene(
    c: &mut ComposeCtl,
    name: String,
    spec: crate::model::SceneSpec,
) -> Result<(), PipelineError> {
    let scene = c
        .names
        .scene(&spec)
        .map_err(|e| PipelineError::Control(format!("scene '{name}': {e}")))?;
    if c.program == name {
        c.pending.push(ComposeCmd::ProgramEdited(scene.clone()));
    }
    c.scenes.insert(name.clone(), scene);
    c.specs.insert(name, spec);
    Ok(())
}

// ---------------------------------------------------------------------------
// 世代を組み立てる

/// 部品をどのスレッドで動かすか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    None,
    Exec(usize),
}

fn build_generation(
    rt: &Arc<RtShared>,
    id: GenerationId,
    r: &Resolved,
) -> Result<Generation, PipelineError> {
    let g = &r.graph;
    let o = &g.options;
    let n = g.nodes.len();
    let stop = Arc::new(AtomicBool::new(false));
    let ctx = Ctx {
        rt: rt.clone(),
        generation: id,
        stop: stop.clone(),
        clock_mode: o.clock,
        latency_us: if o.clock == ClockMode::Realtime {
            o.latency_ms as i64 * 1000
        } else {
            0
        },
        missing_timeout_us: o.missing_timeout_ms as i64 * 1000,
    };
    let block = Duration::from_millis(o.block_timeout_ms as u64);
    // 持ち主のスレッド
    let mut owner = vec![Owner::None; n];
    let mut exec_of: Vec<usize> = Vec::new(); // exec 番号 → 根の部品
    for &i in &r.order {
        owner[i] = match &g.nodes[i] {
            Node::Decode(_) | Node::Compose(_) | Node::AudioMix(_) | Node::Encode(_) => {
                exec_of.push(i);
                Owner::Exec(exec_of.len() - 1)
            }
            Node::Board(_) | Node::Scale(_) | Node::Crop(_) | Node::SnapshotOnChange(_) => {
                owner[r.inputs[i][0].0]
            }
            Node::Source(_) | Node::Sink(_) => Owner::None,
        };
    }
    // 受け口（別スレッドの入力）
    let mut inboxes: HashMap<usize, Arc<exec::InboxT>> = HashMap::new();
    for (i, node) in g.nodes.iter().enumerate() {
        let k = match node {
            Node::Compose(_) | Node::AudioMix(_) | Node::Encode(_) => r.inputs[i].len(),
            _ => continue,
        };
        inboxes.insert(
            i,
            Arc::new(crate::queue::Inbox::new(
                k,
                o.frame_queue as usize,
                o.overflow,
                block,
            )),
        );
    }
    // 部品の連なり（exec ごと）
    let mut chain_index: HashMap<usize, (usize, usize)> = HashMap::new(); // 部品 → (exec, 連なりの番号)
    let mut chains: Vec<Vec<ChainNode>> = (0..exec_of.len()).map(|_| Vec::new()).collect();
    let mut snapshot_flags = BTreeMap::new();
    let mut preview_roots: HashMap<usize, usize> = HashMap::new();
    for &i in &r.order {
        let Owner::Exec(e) = owner[i] else { continue };
        let node = &g.nodes[i];
        let proc = match node {
            Node::Board(b) => {
                Proc::Board(Box::new(BoardProc::new(b.params.clone()).map_err(|m| {
                    PipelineError::runtime("INVALID_CONFIG", format!("{}: {m}", b.id))
                })?))
            }
            Node::Scale(s) => Proc::Scale {
                width: s.width as usize,
                height: s.height as usize,
                filter: s.filter.to_image(),
            },
            Node::Crop(c) => Proc::Crop {
                rect: senca_image::Rect::new(
                    c.x as usize,
                    c.y as usize,
                    c.width as usize & !1,
                    c.height as usize & !1,
                ),
            },
            Node::SnapshotOnChange(s) => {
                let p = SnapshotProc::new(s.params.clone());
                snapshot_flags.insert(s.id.clone(), p.enabled.clone());
                Proc::Snapshot(Box::new(p))
            }
            _ => Proc::Root,
        };
        chain_index.insert(i, (e, chains[e].len()));
        chains[e].push(ChainNode {
            id: node.id().to_string(),
            proc,
            outs: Vec::new(),
        });
        if let Node::Compose(_) = node
            && r.consumers.contains_key(&(i, Port::Preview))
        {
            preview_roots.insert(i, chains[e].len());
            chains[e].push(ChainNode {
                id: format!("{}.preview", node.id()),
                proc: Proc::Root,
                outs: Vec::new(),
            });
        }
    }
    // 出力先
    for &i in &r.order {
        let Owner::Exec(e) = owner[i] else { continue };
        let (_, ci) = chain_index[&i];
        for port in [Port::Out, Port::Program, Port::Preview] {
            let Some(cons) = r.consumers.get(&(i, port)) else {
                continue;
            };
            let from = if port == Port::Preview {
                preview_roots[&i]
            } else {
                ci
            };
            for &(c, k) in cons {
                let t = match (&g.nodes[c], owner[c]) {
                    (
                        Node::Board(_) | Node::Scale(_) | Node::Crop(_) | Node::SnapshotOnChange(_),
                        Owner::Exec(ce),
                    ) if ce == e => Target::Local(chain_index[&c].1),
                    (Node::Compose(_) | Node::AudioMix(_) | Node::Encode(_), _) => Target::Remote {
                        inbox: inboxes[&c].clone(),
                        idx: k,
                    },
                    (Node::Sink(_), _) => continue, // 符号化の出力は exec が直接出す
                    (other, _) => {
                        return Err(PipelineError::runtime(
                            "INVALID_CONFIG",
                            format!(
                                "{} cannot consume {} at runtime",
                                other.type_name(),
                                g.nodes[i].id()
                            ),
                        ));
                    }
                };
                chains[e][from].outs.push(t);
            }
        }
    }
    let mut chains: Vec<Option<Chain>> = chains
        .into_iter()
        .map(|nodes| Some(Chain { nodes }))
        .collect();
    // sink のトラック
    let mut sink_targets: HashMap<(usize, Port), Vec<SinkTarget>> = HashMap::new();
    let mut sink_tracks = BTreeSet::new();
    for (i, node) in g.nodes.iter().enumerate() {
        if let Node::Sink(s) = node {
            for (k, &(j, p)) in r.inputs[i].iter().enumerate() {
                sink_targets.entry((j, p)).or_default().push(SinkTarget {
                    sink: s.id.clone(),
                    stream: s.stream.clone(),
                    track: k as u32,
                });
                sink_tracks.insert((s.id.clone(), k as u32));
            }
        }
    }
    // 入力の行き先とスレッド
    let mut routes: HashMap<String, SourceRoutes> = HashMap::new();
    let mut queues = Vec::new();
    let mut jobs: Vec<(String, Job)> = Vec::new();
    let mut compose = BTreeMap::new();
    let mut compose_inputs = BTreeMap::new();
    let mut mix = BTreeMap::new();
    let mut keyframe = BTreeMap::new();
    let mut decode_stats = Vec::new();
    let mut encode_stats = BTreeMap::new();
    for (i, node) in g.nodes.iter().enumerate() {
        if let Node::Source(s) = node {
            let e = routes.entry(s.id.clone()).or_default();
            e.stream = s.stream.clone();
            e.want_video_track = s.video.as_ref().and_then(|t| t.track);
            e.want_audio_track = s.audio.as_ref().and_then(|t| t.track);
            e.video_codec = s
                .video
                .as_ref()
                .and_then(|t| senca_core::Codec::parse(&t.codec));
            e.audio_codec = s
                .audio
                .as_ref()
                .and_then(|t| senca_core::Codec::parse(&t.codec));
            for (port, video) in [(Port::Video, true), (Port::Audio, false)] {
                for t in sink_targets.get(&(i, port)).into_iter().flatten() {
                    let rr = if video { &mut e.video } else { &mut e.audio };
                    rr.push(InputRoute::Passthrough(t.clone()));
                }
            }
        }
    }
    for (ei, &i) in exec_of.iter().enumerate() {
        let node = &g.nodes[i];
        let chain = chains[ei].take().expect("each chain used once");
        let c2 = ctx.clone();
        match node {
            Node::Decode(d) => {
                let (src, port) = r.inputs[i][0];
                let Node::Source(s) = &g.nodes[src] else {
                    return Err(PipelineError::runtime(
                        "INVALID_CONFIG",
                        format!("{}: decode input must be a source", d.id),
                    ));
                };
                let video = port == Port::Video;
                let codec = match r.port_types.get(&(src, port)) {
                    Some(PortType::Encoded { codec, .. }) => *codec,
                    _ => {
                        return Err(PipelineError::runtime(
                            "INVALID_CONFIG",
                            "decode input type",
                        ));
                    }
                };
                let mut cfg = DecoderConfig::new(codec);
                cfg.max_width = d.max_width.unwrap_or(3840);
                cfg.max_height = d.max_height.unwrap_or(2160);
                cfg.threads = d.threads;
                cfg.output_format = Some(PixelFormat::I420);
                cfg.sample_rate = 48000;
                // 確保の確認（開けなければ準備で失敗させる）
                rt.registry
                    .create_decoder(&cfg, &rt.policy)
                    .map_err(PipelineError::from)?;
                let q = Arc::new(AuQueue::new(
                    if video {
                        o.input_queue_aus as usize
                    } else {
                        o.input_queue_aus as usize * 4
                    },
                    video,
                    o.overflow,
                    block,
                ));
                queues.push(q.clone());
                let e = routes.entry(s.id.clone()).or_default();
                if video {
                    e.video.push(InputRoute::Decode(q.clone()));
                } else {
                    e.audio.push(InputRoute::Decode(q.clone()));
                }
                let stats = Arc::new(DecodeStats::default());
                decode_stats.push((
                    s.id.clone(),
                    if video {
                        MediaKind::Video
                    } else {
                        MediaKind::Audio
                    },
                    stats.clone(),
                ));
                let ex = DecodeExec {
                    node: d.id.clone(),
                    source: s.id.clone(),
                    kind: if video {
                        MediaKind::Video
                    } else {
                        MediaKind::Audio
                    },
                    queue: q,
                    config: cfg,
                    chain,
                    stats,
                };
                jobs.push((d.id.clone(), Box::new(move || ex.run(c2))));
            }
            Node::Compose(c) => {
                let names = convert::ComposeNames::new(c);
                let spec = names.spec;
                let mut comp = Compositor::new(
                    senca_compose::Canvas {
                        width: c.canvas.width as usize,
                        height: c.canvas.height as usize,
                        fps_num: c.canvas.fps_num,
                        fps_den: c.canvas.fps_den,
                        background: convert::color(&c.canvas.background, spec),
                        spec,
                    },
                    c.threads as usize,
                )?;
                for (k, (name, img)) in c.images.iter().enumerate() {
                    let im = convert::decode_image(img, spec).map_err(|e| {
                        PipelineError::runtime(
                            "INVALID_CONFIG",
                            format!("{}: image '{name}': {e}", c.id),
                        )
                    })?;
                    comp.set_image(senca_compose::ImageId(k as u32), im);
                }
                comp.set_policy(SourcePolicy {
                    hold_timeout_us: c.missing.hold_ms as i64 * 1000,
                    slate: match c
                        .missing
                        .slate_image
                        .as_ref()
                        .and_then(|s| names.image_id(s))
                    {
                        Some(im) => Slate::Image(im),
                        None => Slate::Color(convert::color(&c.missing.slate_color, spec)),
                    },
                });
                let mut scenes = BTreeMap::new();
                for (k, s) in &c.scenes {
                    scenes.insert(
                        k.clone(),
                        names
                            .scene(s)
                            .map_err(|e| PipelineError::runtime("INVALID_CONFIG", e))?,
                    );
                }
                comp.set_program(scenes[&c.program].clone())?;
                // 入力の名前 → 元の source（タリー用）
                let ins: Vec<Vec<String>> = r.inputs[i]
                    .iter()
                    .map(|(j, _)| upstream_sources(r, *j))
                    .collect();
                compose_inputs.insert(c.id.clone(), ins);
                let ctl = Arc::new(Mutex::new(ComposeCtl {
                    names,
                    specs: c.scenes.clone(),
                    scenes,
                    program: c.program.clone(),
                    preview: c.preview.clone(),
                    default_transition: c.transition,
                    pending: Vec::new(),
                    in_transition: false,
                    frames: 0,
                    skipped: 0,
                    missing: Vec::new(),
                }));
                compose.insert(c.id.clone(), ctl.clone());
                let mut sched = TickScheduler::video(
                    o.clock,
                    c.canvas.fps_num,
                    c.canvas.fps_den,
                    ctx.latency_us,
                    ctx.missing_timeout_us,
                );
                let origin = next_tick_origin(rt, &sched);
                sched.start_at(origin);
                let ex = ComposeExec {
                    node: c.id.clone(),
                    inbox: inboxes[&i].clone(),
                    compositor: comp,
                    ctl,
                    sched,
                    chain,
                    preview_root: preview_roots.get(&i).copied(),
                };
                jobs.push((c.id.clone(), Box::new(move || ex.run(c2))));
            }
            Node::AudioMix(m) => {
                let mut mcfg = MixerConfig {
                    master_gain_db: m.master_gain_db,
                    ..MixerConfig::default()
                };
                mcfg.limiter.enabled = m.limiter;
                let names: Vec<String> = m.inputs.keys().cloned().collect();
                let configs = m.inputs.values().map(convert::mix_input).collect();
                let ctl = Arc::new(Mutex::new(MixCtl {
                    names,
                    configs,
                    master_gain_db: m.master_gain_db,
                    dirty: false,
                    meters: None,
                    frames: 0,
                }));
                mix.insert(m.id.clone(), ctl.clone());
                let mut sched = TickScheduler::audio(
                    o.clock,
                    MIX_FRAMES,
                    ctx.latency_us,
                    ctx.missing_timeout_us,
                );
                let origin = next_tick_origin(rt, &sched);
                sched.start_at(origin);
                let ex = MixExec {
                    node: m.id.clone(),
                    inbox: inboxes[&i].clone(),
                    mixer: AudioMixer::new(mcfg),
                    ctl,
                    sched,
                    chain,
                    added: vec![false; m.inputs.len()],
                };
                jobs.push((m.id.clone(), Box::new(move || ex.run(c2))));
            }
            Node::Encode(e) => {
                let codec = senca_core::Codec::parse(&e.codec)
                    .ok_or_else(|| PipelineError::runtime("UNSUPPORTED_CODEC", "codec"))?;
                let in_t = r.port_types.get(&r.inputs[i][0]).copied();
                let (size, fps, audio) = match in_t {
                    Some(PortType::RawVideo { width, height, fps }) => {
                        (width.zip(height), fps, None)
                    }
                    Some(PortType::RawAudio {
                        sample_rate,
                        channels,
                    }) => (None, None, Some((sample_rate, channels.unwrap_or(2)))),
                    _ => (None, None, None),
                };
                // 大きさが分かっていれば準備で開く（開けなければ失敗）。分からなければ最初のフレームで開く。
                let encoder = match exec::encoder_config(e, codec, fps, size, audio) {
                    Some(cfg)
                        if codec.kind() == MediaKind::Video
                            || audio.is_some_and(|(_, c)| c > 0) =>
                    {
                        let (_, enc) = rt
                            .registry
                            .create_encoder(&cfg, &rt.policy)
                            .map_err(PipelineError::from)?;
                        Some((cfg, enc))
                    }
                    _ => {
                        if codec.kind() == MediaKind::Video {
                            // 開けるかだけ確かめる（仮の大きさ）
                            let probe = exec::encoder_config(e, codec, fps, Some((320, 240)), None)
                                .expect("video config with size");
                            rt.registry
                                .create_encoder(&probe, &rt.policy)
                                .map_err(PipelineError::from)?;
                        }
                        None
                    }
                };
                if r.consumers.get(&(i, Port::Out)).is_some_and(|cs| {
                    cs.iter()
                        .any(|(c, _)| !matches!(g.nodes[*c], Node::Sink(_)))
                }) {
                    return Err(PipelineError::runtime(
                        "INVALID_CONFIG",
                        format!("{}: encoded output can only go to sinks", e.id),
                    ));
                }
                let kf = Arc::new(AtomicBool::new(false));
                if codec.kind() == MediaKind::Video {
                    keyframe.insert(e.id.clone(), kf.clone());
                }
                let stats = Arc::new(EncodeStats::default());
                encode_stats.insert(e.id.clone(), stats.clone());
                let ex = EncodeExec {
                    node: e.id.clone(),
                    spec: e.clone(),
                    codec,
                    fps,
                    inbox: inboxes[&i].clone(),
                    keyframe: kf,
                    targets: sink_targets
                        .get(&(i, Port::Out))
                        .cloned()
                        .unwrap_or_default(),
                    stats,
                    encoder,
                };
                jobs.push((e.id.clone(), Box::new(move || ex.run(c2))));
            }
            _ => {}
        }
    }
    Ok(Generation {
        id,
        plan: r.plan.clone(),
        options: o.clone(),
        stop,
        routes,
        pending: Mutex::new(jobs),
        threads: Mutex::new(Vec::new()),
        queues,
        compose,
        compose_inputs,
        mix,
        snapshot: snapshot_flags,
        keyframe,
        decode_stats,
        encode_stats,
        sink_tracks,
        commit_started_us: AtomicI64::new(0),
    })
}

/// 新しい世代の刻みの始まり（前の世代の続き）。
fn next_tick_origin(rt: &RtShared, s: &TickScheduler) -> i64 {
    let has_active = lock(&rt.st).manager.active().is_some();
    if !has_active {
        return 0;
    }
    rt.media_clock.load(Ordering::Relaxed) + s.period_us()
}

/// 部品の上流をたどって source の id を集める。
fn upstream_sources(r: &Resolved, j: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![j];
    let mut seen = BTreeSet::new();
    while let Some(k) = stack.pop() {
        if !seen.insert(k) {
            continue;
        }
        if let Node::Source(s) = &r.graph.nodes[k] {
            out.push(s.id.clone());
        }
        stack.extend(r.inputs[k].iter().map(|(x, _)| *x));
    }
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// 監督（状態の通知・commit の時間切れ・終わった世代の片付け）

fn supervisor(rt: Arc<RtShared>, stop: Arc<AtomicBool>) {
    let mut last_status = rt.session_now();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
        let now = rt.session_now();
        // commit の時間切れ
        let committing = {
            let st = lock(&rt.st);
            st.manager
                .committing()
                .and_then(|g| st.gens.get(&g).cloned())
        };
        if let Some(g) = committing {
            let started = g.commit_started_us.load(Ordering::Relaxed);
            if now - started > g.options.commit_timeout_ms as i64 * 1000 {
                rt.fail_commit(
                    g.id,
                    PipelineError::runtime(
                        "CAPACITY_UNAVAILABLE",
                        "new generation did not reach a keyframe boundary in time",
                    ),
                );
            }
        }
        // 終わった世代を片付ける
        let retired = std::mem::take(&mut lock(&rt.st).retired);
        let mut keep = Vec::new();
        for r in retired {
            if !r.join(Duration::ZERO) {
                keep.push(r);
            }
        }
        lock(&rt.st).retired.extend(keep);
        // 状態の通知
        let interval = {
            let st = lock(&rt.st);
            st.manager
                .active()
                .and_then(|a| st.gens.get(&a))
                .map_or(100, |g| g.options.status_interval_ms)
        };
        if interval > 0 && now - last_status >= interval as i64 * 1000 {
            last_status = now;
            for e in status_events(&rt) {
                rt.emit_event(e);
            }
            // 取りこぼしの知らせ
            let drops: Vec<(String, u64)> = {
                let mut s = lock(&rt.sources);
                s.iter_mut()
                    .filter(|(_, i)| i.dropped > i.dropped_reported)
                    .map(|(k, i)| {
                        let d = i.dropped - i.dropped_reported;
                        i.dropped_reported = i.dropped;
                        (k.clone(), d)
                    })
                    .collect()
            };
            for (source, count) in drops {
                rt.emit_event(Event::InputDropped {
                    source,
                    count,
                    reason: "input queue overflow or waiting for a keyframe".into(),
                });
            }
        }
    }
}

/// 今の状態の通知（状態とメーター）。
fn status_events(rt: &RtShared) -> Vec<Event> {
    let gen_arc = {
        let st = lock(&rt.st);
        st.manager.active().and_then(|a| st.gens.get(&a).cloned())
    };
    let Some(g) = gen_arc else { return vec![] };
    let now = rt.session_now();
    let mut out = Vec::new();
    let mut compose = Vec::new();
    let mut on_program = BTreeSet::new();
    let mut on_preview = BTreeSet::new();
    for (id, ctl) in &g.compose {
        let c = lock(ctl);
        let ins = g.compose_inputs.get(id);
        let names_of = |scene: &str| -> Vec<String> { c.visible_inputs(scene) };
        let prog = names_of(&c.program);
        let prev = c.preview.as_deref().map(names_of).unwrap_or_default();
        for (set, list) in [(&mut on_program, &prog), (&mut on_preview, &prev)] {
            for n in list {
                if let (Some(k), Some(ins)) = (c.names.inputs.iter().position(|x| x == n), ins) {
                    set.extend(ins[k].iter().cloned());
                }
            }
        }
        compose.push(ComposeStatus {
            node: id.clone(),
            program: c.program.clone(),
            preview: c.preview.clone(),
            in_transition: c.in_transition,
            frames: c.frames,
            skipped_ticks: c.skipped,
            missing: c.missing.clone(),
            program_inputs: prog,
            preview_inputs: prev,
        });
    }
    let mut sources = Vec::new();
    {
        let mut s = lock(&rt.sources);
        for src in g.routes.keys() {
            let info = s.entry(src.clone()).or_default();
            let (mut frames, mut errors, mut last) = (0u64, 0u64, None);
            let mut vframes = 0u64;
            for (sid, kind, st) in &g.decode_stats {
                if sid == src {
                    let f = st.frames.load(Ordering::Relaxed);
                    frames += f;
                    errors += st.errors.load(Ordering::Relaxed);
                    if *kind == MediaKind::Video {
                        vframes += f;
                        last = Some(st.last_us.load(Ordering::Relaxed));
                    }
                }
            }
            let interval = g.options.status_interval_ms.max(1) as f32 / 1000.0;
            let df = vframes.saturating_sub(info.last_frames);
            info.last_frames = vframes;
            info.fps = info.fps * 0.7 + (df as f32 / interval) * 0.3;
            let state = if info.ended {
                "ended"
            } else {
                match info.last_arrival {
                    None => "waiting",
                    Some(t) if now - t > g.options.missing_timeout_ms as i64 * 1000 => "stalled",
                    Some(_) => "live",
                }
            };
            let stream = g
                .routes
                .get(src)
                .map(|r| r.stream.clone())
                .unwrap_or_default();
            sources.push(SourceStatus {
                id: src.clone(),
                stream,
                state: state.into(),
                aus: info.aus,
                dropped_aus: info.dropped,
                decoded_frames: frames,
                decode_errors: errors,
                fps: (info.fps * 10.0).round() / 10.0,
                last_us: last,
                on_program: on_program.contains(src),
                on_preview: on_preview.contains(src),
            });
        }
    }
    let encoders = g
        .encode_stats
        .iter()
        .map(|(id, s)| EncoderStatus {
            node: id.clone(),
            frames_in: s.frames_in.load(Ordering::Relaxed),
            aus_out: s.aus_out.load(Ordering::Relaxed),
            dropped_frames: 0,
            errors: s.errors.load(Ordering::Relaxed),
        })
        .collect();
    out.push(Event::Status {
        generation: g.id,
        session_us: now,
        sources,
        compose,
        encoders,
    });
    for (id, ctl) in &g.mix {
        let c = lock(ctl);
        let Some(m) = &c.meters else { continue };
        let inputs = c
            .names
            .iter()
            .enumerate()
            .map(|(k, name)| {
                let meter = m
                    .inputs
                    .iter()
                    .find(|(i, _)| *i == k as u32)
                    .map(|(_, x)| *x)
                    .unwrap_or_default();
                MeterReading {
                    name: name.clone(),
                    peak_db: meter.peak_db,
                    rms_db: meter.rms_db,
                    mute: c.configs[k].mute,
                    solo: c.configs[k].solo,
                    gain_db: c.configs[k].gain_db,
                }
            })
            .collect();
        out.push(Event::Meters {
            node: id.clone(),
            inputs,
            master: MeterReading {
                name: "master".into(),
                peak_db: m.master.peak_db,
                rms_db: m.master.rms_db,
                mute: false,
                solo: false,
                gain_db: c.master_gain_db,
            },
            limiter_reduction_db: m.limiter_reduction_db,
        });
    }
    out
}
