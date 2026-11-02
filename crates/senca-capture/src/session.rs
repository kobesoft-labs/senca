//! 取り込みのセッション: 機器から読み、符号化して `AccessUnit` と `CodecConfig` にする。
//!
//! # 流れ
//!
//! 1. 方針（ADR-0004・REQ-007）を先に確かめる。H.264/H.265 の符号化はハードウェアのプロバイダーがあるときだけ
//!    （無ければ `CAPACITY_UNAVAILABLE`）。ロイヤリティフリーとして提供されているもの（VP8・VP9・AV1・Opus）は
//!    ソフトウェアでよい。AAC は扱わない。UVC カメラの H.264 の素通し（`passthrough`）は符号化しないが、
//!    ロイヤリティフリー限定の方針では拒否する。
//! 2. 機器を開始し（取り決めた形式を得る）、取り決めた解像度・フレームレートで符号化器を作る。
//! 3. 機器ごとに読み取りのスレッドを作り、上限付きの通り道でセッションに渡す（符号化が遅れると読み取りが止まり、
//!    機器の側でフレームが落ちる。メモリーは増えない）。
//! 4. [`CaptureSession::poll`] がフレームを受け取り、時刻を付け直して符号化し、トラックごとの AU を返す。
//!
//! # 時刻
//!
//! - 起点は最初に届いたフレームの `monotonic_ns`（機器をまたいで同じ時計）。起点より前の時刻のフレームは捨てる。
//! - 映像の PTS は 90 kHz で `monotonic_ns - 起点`（単調に増えるよう補正）。
//! - 音声の PTS は標本化周波数の単位で、標本数を積み上げる（連続）。時計とのずれが 100 ms を超えたら時計に合わせ直す。
//! - AU の `capture_time_ns`（UNIX 時刻）は、開始時に取った単調時刻と UNIX 時刻の組から求める。
//!
//! # 解像度の変化
//!
//! 映像の大きさが変わったら（DeckLink の形式の自動検出など）、符号化器を出し切ってから作り直す。
//! 設定の世代（`generation`）はトラックの中で増え続けるように付け直す。
//!
//! # 音声のチャンネル
//!
//! 機器のチャンネル数（DeckLink は 2/8/16）が出力より多ければ先頭から取り、少なければ最後のチャンネルを複製する。
//! 標本化周波数の変換はしない（Opus の 8/12/16/24/48 kHz 以外で取り込んだら `Unsupported`）。

use crate::clock::{self, ClockAnchor};
use crate::device::CaptureDevice;
use crate::error::{CaptureError, ErrorKind, Result};
use crate::h264::H264Passthrough;
use crate::types::{
    AudioMode, AudioRequest, CaptureConfig, CapturedFrame, Negotiated, RawFormat, VideoMode,
    VideoRequest,
};
use bytes::Bytes;
use senca_codec::caps::{Direction, ProviderId};
use senca_codec::config::{AudioEncoderConfig, EncoderConfig, VideoEncoderConfig};
use senca_codec::registry::{Policy, ProviderRegistry, check_policy};
use senca_codec::{AudioFrame, Encoder, Frame, SampleFormat};
use senca_core::{AccessUnit, Codec, CodecConfig, Rescale, Timebase, TrackId};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

/// 読み取りのスレッドからの通り道の長さ。
const QUEUE: usize = 16;
/// 音声の時計とのずれの許容（秒の分母: 1/10 秒）。
const AUDIO_RESYNC_DIV: i64 = 10;

/// 映像の出力の設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoOutput {
    pub codec: Codec,
    pub bitrate_bps: u32,
    /// キーフレームの間隔（フレーム数）。`None` なら 2 秒。
    pub gop: Option<u32>,
    /// 機器が符号化した H.264 をそのまま使う（UVC カメラ）。
    pub passthrough: bool,
    /// ソフトウェアの符号化器のスレッド数（0 = 自動）。
    pub threads: u32,
    pub track: TrackId,
}

/// 音声の出力の設定（Opus だけ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOutput {
    pub codec: Codec,
    pub bitrate_bps: u32,
    pub track: TrackId,
}

/// セッションの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    pub video: Option<(VideoRequest, VideoOutput)>,
    pub audio: Option<(AudioRequest, AudioOutput)>,
    pub policy: Policy,
}

/// 機器の役割。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRole {
    pub video: bool,
    pub audio: bool,
}

/// トラックの出力（設定は最初と、変わったときだけ付く）。
#[derive(Debug, Clone)]
pub struct EncodedOutput {
    pub track: TrackId,
    pub config: Option<CodecConfig>,
    pub aus: Vec<AccessUnit>,
}

/// トラックの説明（開いたときの通知に使う）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrackInfo {
    pub track: u32,
    pub kind: &'static str,
    pub codec: String,
    /// 符号化したプロバイダー（素通しなら `passthrough`）。
    pub provider: String,
    pub device: String,
}

/// 統計。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionStats {
    pub video_frames: u64,
    pub audio_frames: u64,
    pub video_aus: u64,
    pub audio_aus: u64,
    pub bytes: u64,
    /// 起点より前の時刻で捨てたフレーム。
    pub early_frames: u64,
    /// 機器のフレーム番号の飛び（機器の側で落ちたフレームの数）。
    pub sequence_gaps: u64,
    /// 音声を時計に合わせ直した回数。
    pub audio_resyncs: u64,
    /// 解像度の変化で符号化器を作り直した回数。
    pub encoder_restarts: u64,
    /// H.264 の素通しで捨てた AU（SPS/PPS・IDR 待ち）。
    pub passthrough_dropped: u64,
}

enum Msg {
    Frame(CapturedFrame),
    Error(usize, CaptureError),
    Ended(usize),
}

enum VideoEnc {
    Encoder {
        enc: Box<dyn Encoder>,
        cfg: VideoEncoderConfig,
        provider: ProviderId,
    },
    Passthrough(H264Passthrough),
}

/// 設定の世代を付け直す（符号化器を作り直しても単調に増える）。
#[derive(Debug, Default)]
struct GenMap {
    base: u32,
    last_seen: Option<u32>,
    last_emitted: Option<u32>,
}

impl GenMap {
    fn map(&self, g: u32) -> u32 {
        self.base.wrapping_add(g)
    }
    /// 符号化器を作り直す（次の世代から始める）。
    fn restart(&mut self) {
        self.base = self.last_seen.map_or(self.base, |g| g.wrapping_add(1));
    }
    /// 出すべき設定（前に出したものと世代が違えば）。
    fn take_config(&mut self, c: Option<CodecConfig>) -> Option<CodecConfig> {
        let mut c = c?;
        c.generation = self.map(c.generation);
        self.last_seen = Some(c.generation);
        if self.last_emitted == Some(c.generation) {
            return None;
        }
        self.last_emitted = Some(c.generation);
        Some(c)
    }
}

struct VideoPath {
    enc: VideoEnc,
    out: VideoOutput,
    mode: VideoMode,
    gens: GenMap,
    last_pts: Option<i64>,
    last_seq: Option<u64>,
}

struct AudioPath {
    enc: Box<dyn Encoder>,
    track: TrackId,
    out_channels: u8,
    sample_rate: u32,
    gens: GenMap,
    next: Option<i64>,
}

/// 取り込みのセッション。
pub struct CaptureSession {
    registry: ProviderRegistry,
    policy: Policy,
    video: Option<VideoPath>,
    audio: Option<AudioPath>,
    rx: Option<Receiver<Msg>>,
    threads: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    ended: Vec<bool>,
    base: Option<i64>,
    anchor: ClockAnchor,
    negotiated: Vec<(String, Negotiated)>,
    tracks: Vec<TrackInfo>,
    stats: SessionStats,
    finished: bool,
}

impl std::fmt::Debug for CaptureSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureSession")
            .field("tracks", &self.tracks)
            .field("stats", &self.stats)
            .finish()
    }
}

/// 方針を先に確かめる（機器を開始する前に、無理な要求を断る）。
pub fn check_config(cfg: &SessionConfig, registry: &ProviderRegistry) -> Result<()> {
    if cfg.video.is_none() && cfg.audio.is_none() {
        return Err(CaptureError::invalid_config("nothing to capture"));
    }
    if let Some((req, out)) = &cfg.video {
        crate::negotiate::validate_video_request(req)?;
        if out.passthrough {
            if out.codec != Codec::H264 {
                return Err(CaptureError::invalid_config(
                    "passthrough is only available for H.264 from UVC cameras",
                ));
            }
            check_policy(Codec::H264, &cfg.policy)?;
            if !req.formats.is_empty() && req.formats != [RawFormat::H264] {
                return Err(CaptureError::invalid_config(
                    "passthrough needs formats = [\"h264\"]",
                ));
            }
        } else {
            if req.formats.contains(&RawFormat::H264) {
                return Err(CaptureError::invalid_config(
                    "h264 device format requires passthrough (device H.264 is not decoded)",
                ));
            }
            registry.candidates(
                out.codec,
                Direction::Encode,
                Some((req.width, req.height)),
                &cfg.policy,
            )?;
        }
    }
    if let Some((req, out)) = &cfg.audio {
        crate::negotiate::validate_audio_request(req)?;
        if out.codec != Codec::Opus {
            return Err(CaptureError::unsupported(format!(
                "audio codec {} is not supported for capture (use opus)",
                out.codec
            )));
        }
        if !(1..=2).contains(&req.channels) {
            return Err(CaptureError::invalid_config(
                "opus output supports 1 or 2 channels",
            ));
        }
        registry.candidates(Codec::Opus, Direction::Encode, None, &cfg.policy)?;
    }
    Ok(())
}

fn opus_rate(r: u32) -> bool {
    matches!(r, 8000 | 12000 | 16000 | 24000 | 48000)
}

fn video_encoder_config(out: &VideoOutput, mode: &VideoMode) -> VideoEncoderConfig {
    let fps_int = (mode.fps.as_f64().round() as u32).max(1);
    let mut c =
        VideoEncoderConfig::new(out.codec, mode.width, mode.height, fps_int, out.bitrate_bps);
    c.fps_num = mode.fps.num;
    c.fps_den = mode.fps.den;
    c.gop = out.gop.unwrap_or(fps_int.saturating_mul(2));
    c.threads = out.threads;
    c.track = out.track;
    c.timebase = Timebase::MPEG;
    c
}

impl CaptureSession {
    /// 機器を開始してセッションを作る。`devices` は開いた（まだ開始していない）機器と役割。
    pub fn start(
        devices: Vec<(Box<dyn CaptureDevice>, DeviceRole)>,
        cfg: &SessionConfig,
        registry: &ProviderRegistry,
    ) -> Result<Self> {
        check_config(cfg, registry)?;
        if cfg.video.is_some() != devices.iter().any(|(_, r)| r.video)
            || cfg.audio.is_some() != devices.iter().any(|(_, r)| r.audio)
            || devices.iter().filter(|(_, r)| r.video).count() > 1
            || devices.iter().filter(|(_, r)| r.audio).count() > 1
        {
            return Err(CaptureError::invalid_config(
                "exactly one device per requested media kind is required",
            ));
        }
        let mut devices = devices;
        let mut negotiated = Vec::new();
        let mut video_mode: Option<(VideoMode, String)> = None;
        let mut audio_mode: Option<(AudioMode, String)> = None;
        for (dev, role) in devices.iter_mut() {
            let c = CaptureConfig {
                video: role
                    .video
                    .then(|| cfg.video.as_ref().map(|v| v.0.clone()))
                    .flatten(),
                audio: role
                    .audio
                    .then(|| cfg.audio.as_ref().map(|a| a.0))
                    .flatten(),
            };
            let id = dev.descriptor().id.clone();
            let n = match dev.start(&c) {
                Ok(n) => n,
                Err(e) => {
                    for (d, _) in devices.iter_mut() {
                        let _ = d.stop();
                    }
                    return Err(e);
                }
            };
            if let Some(v) = &n.video {
                video_mode = Some((v.clone(), id.clone()));
            }
            if let Some(a) = n.audio {
                audio_mode = Some((a, id.clone()));
            }
            negotiated.push((id, n));
        }
        let stop_all = |devices: &mut Vec<(Box<dyn CaptureDevice>, DeviceRole)>| {
            for (d, _) in devices.iter_mut() {
                let _ = d.stop();
            }
        };
        let mut tracks = Vec::new();
        let video = match (&cfg.video, video_mode) {
            (Some((_, out)), Some((mode, dev))) => {
                let enc = if out.passthrough {
                    if mode.format != RawFormat::H264 {
                        stop_all(&mut devices);
                        return Err(CaptureError::unsupported("device did not provide H.264"));
                    }
                    VideoEnc::Passthrough(H264Passthrough::new(out.track, Timebase::MPEG))
                } else {
                    let vc = video_encoder_config(out, &mode);
                    match registry.create_encoder(&EncoderConfig::Video(vc.clone()), &cfg.policy) {
                        Ok((provider, enc)) => VideoEnc::Encoder {
                            enc,
                            cfg: vc,
                            provider,
                        },
                        Err(e) => {
                            stop_all(&mut devices);
                            return Err(e.into());
                        }
                    }
                };
                tracks.push(TrackInfo {
                    track: out.track.0,
                    kind: "video",
                    codec: out.codec.to_string(),
                    provider: match &enc {
                        VideoEnc::Encoder { provider, .. } => provider.as_str().to_string(),
                        VideoEnc::Passthrough(_) => "passthrough".into(),
                    },
                    device: dev,
                });
                Some(VideoPath {
                    enc,
                    out: out.clone(),
                    mode,
                    gens: GenMap::default(),
                    last_pts: None,
                    last_seq: None,
                })
            }
            (None, _) => None,
            (Some(_), None) => {
                stop_all(&mut devices);
                return Err(CaptureError::internal(
                    "video device did not negotiate video",
                ));
            }
        };
        let audio = match (&cfg.audio, audio_mode) {
            (Some((req, out)), Some((mode, dev))) => {
                if !opus_rate(mode.sample_rate) {
                    stop_all(&mut devices);
                    return Err(CaptureError::unsupported(format!(
                        "device captures at {} Hz; opus needs 8/12/16/24/48 kHz (resampling is not implemented)",
                        mode.sample_rate
                    )));
                }
                let mut ac =
                    AudioEncoderConfig::opus(mode.sample_rate, req.channels, out.bitrate_bps);
                ac.input_format = SampleFormat::I16;
                ac.track = out.track;
                let (provider, enc) =
                    match registry.create_encoder(&EncoderConfig::Audio(ac), &cfg.policy) {
                        Ok(v) => v,
                        Err(e) => {
                            stop_all(&mut devices);
                            return Err(e.into());
                        }
                    };
                tracks.push(TrackInfo {
                    track: out.track.0,
                    kind: "audio",
                    codec: out.codec.to_string(),
                    provider: provider.as_str().into(),
                    device: dev,
                });
                Some(AudioPath {
                    enc,
                    track: out.track,
                    out_channels: req.channels,
                    sample_rate: mode.sample_rate,
                    gens: GenMap::default(),
                    next: None,
                })
            }
            (None, _) => None,
            (Some(_), None) => {
                stop_all(&mut devices);
                return Err(CaptureError::internal(
                    "audio device did not negotiate audio",
                ));
            }
        };
        let (tx, rx) = sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let n = devices.len();
        let mut threads = Vec::new();
        for (i, (dev, _)) in devices.into_iter().enumerate() {
            let tx = tx.clone();
            let stop = stop.clone();
            let h = std::thread::Builder::new()
                .name(format!("capture-{i}"))
                .spawn(move || reader(i, dev, tx, stop))
                .map_err(|e| CaptureError::internal(format!("spawn reader: {e}")))?;
            threads.push(h);
        }
        Ok(CaptureSession {
            registry: registry.clone(),
            policy: cfg.policy.clone(),
            video,
            audio,
            rx: Some(rx),
            threads,
            stop,
            ended: vec![false; n],
            base: None,
            anchor: ClockAnchor::now(),
            negotiated,
            tracks,
            stats: SessionStats::default(),
            finished: false,
        })
    }

    /// 機器ごとの取り決めた形式。
    pub fn negotiated(&self) -> &[(String, Negotiated)] {
        &self.negotiated
    }

    pub fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }

    /// すべての機器が終わり、符号化器を出し切った。
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// フレームを待って処理する。`timeout` の間に何も無ければ空を返す。
    /// 機器の誤りは `Err`（その後は [`CaptureSession::finish`] で後始末する）。
    pub fn poll(&mut self, timeout: Duration) -> Result<Vec<EncodedOutput>> {
        let mut outs = Vec::new();
        if self.finished {
            return Ok(outs);
        }
        let Some(rx) = self.rx.as_ref() else {
            return Ok(outs);
        };
        let mut msgs = Vec::new();
        match rx.recv_timeout(timeout) {
            Ok(m) => msgs.push(m),
            Err(RecvTimeoutError::Timeout) => return Ok(outs),
            Err(RecvTimeoutError::Disconnected) => self.ended.iter_mut().for_each(|e| *e = true),
        }
        while msgs.len() < QUEUE {
            match rx.try_recv() {
                Ok(m) => msgs.push(m),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.ended.iter_mut().for_each(|e| *e = true);
                    break;
                }
            }
        }
        for m in msgs {
            match m {
                Msg::Frame(f) => self.process(f, &mut outs)?,
                Msg::Ended(i) => self.ended[i] = true,
                Msg::Error(i, e) if e.kind == ErrorKind::EndOfStream => self.ended[i] = true,
                Msg::Error(_, e) => return Err(e),
            }
        }
        if self.ended.iter().all(|e| *e) {
            self.flush_encoders(&mut outs)?;
            self.finished = true;
        }
        Ok(merge(outs))
    }

    /// 機器を止め、符号化器を出し切る。
    pub fn finish(&mut self) -> Result<Vec<EncodedOutput>> {
        self.stop.store(true, Ordering::Release);
        // 受け手を捨てると、送りかけのスレッドも抜ける。
        self.rx = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let mut outs = Vec::new();
        if !self.finished {
            self.flush_encoders(&mut outs)?;
            self.finished = true;
        }
        Ok(merge(outs))
    }

    fn process(&mut self, f: CapturedFrame, outs: &mut Vec<EncodedOutput>) -> Result<()> {
        let ts = *f.timestamps();
        let base = *self.base.get_or_insert(ts.monotonic_ns);
        if ts.monotonic_ns < base {
            self.stats.early_frames += 1;
            return Ok(());
        }
        let rel_ns = ts.monotonic_ns - base;
        match f {
            CapturedFrame::Video { frame, .. } => {
                self.stats.video_frames += 1;
                let Some(v) = self.video.as_mut() else {
                    return Ok(());
                };
                if let Some(prev) = v.last_seq
                    && ts.sequence > prev + 1
                {
                    self.stats.sequence_gaps += ts.sequence - prev - 1;
                }
                v.last_seq = Some(ts.sequence);
                let mut pts = rel_ns.rescale(Timebase::NANOS, Timebase::MPEG).unwrap_or(0);
                if let Some(l) = v.last_pts
                    && pts <= l
                {
                    pts = l + 1;
                }
                v.last_pts = Some(pts);
                let restart = matches!(&v.enc, VideoEnc::Encoder { cfg, .. }
                    if cfg.width != frame.width || cfg.height != frame.height);
                if restart {
                    self.restart_video(frame.width, frame.height, outs)?;
                }
                let v = self.video.as_mut().expect("video path");
                let VideoEnc::Encoder { enc, .. } = &mut v.enc else {
                    return Ok(()); // 素通しの設定で非圧縮が来ることは無い（機器の形式で決まる）
                };
                let mut frame = frame;
                frame.pts = pts;
                frame.timebase = Timebase::MPEG;
                enc.send(&Frame::Video(frame))?;
                let aus = drain(enc.as_mut())?;
                let config = v.gens.take_config(enc.codec_config());
                self.emit_video(config, aus, outs);
            }
            CapturedFrame::H264 { annexb, .. } => {
                self.stats.video_frames += 1;
                let Some(v) = self.video.as_mut() else {
                    return Ok(());
                };
                let pts = rel_ns.rescale(Timebase::NANOS, Timebase::MPEG).unwrap_or(0);
                let pts = v.last_pts.map_or(pts, |l| pts.max(l + 1));
                v.last_pts = Some(pts);
                let dur = v
                    .mode
                    .fps
                    .frame_duration_ns()
                    .rescale(Timebase::NANOS, Timebase::MPEG)
                    .unwrap_or(0)
                    .max(0) as u64;
                let VideoEnc::Passthrough(p) = &mut v.enc else {
                    return Ok(());
                };
                let before = p.dropped;
                let au = p.push(&annexb, pts, dur)?;
                self.stats.passthrough_dropped += p.dropped - before;
                let config = v.gens.take_config(p.codec_config());
                self.emit_video(config, au.into_iter().collect(), outs);
            }
            CapturedFrame::Audio { frame, .. } => {
                self.stats.audio_frames += 1;
                let Some(a) = self.audio.as_mut() else {
                    return Ok(());
                };
                if frame.sample_rate != a.sample_rate {
                    return Err(CaptureError::invalid_input("audio sample rate changed"));
                }
                let clock_pts = (rel_ns as i128 * a.sample_rate as i128 / 1_000_000_000) as i64;
                let tolerance = a.sample_rate as i64 / AUDIO_RESYNC_DIV;
                let pts = match a.next {
                    Some(n) if (clock_pts - n).abs() <= tolerance => n,
                    Some(_) => {
                        self.stats.audio_resyncs += 1;
                        clock_pts
                    }
                    None => clock_pts,
                };
                let data = remap_channels(&frame, a.out_channels)?;
                let samples = frame.samples_per_channel() as i64;
                a.next = Some(pts + samples);
                let f = AudioFrame {
                    format: SampleFormat::I16,
                    sample_rate: a.sample_rate,
                    channels: a.out_channels,
                    data,
                    pts,
                    timebase: Timebase::new(1, a.sample_rate),
                };
                a.enc.send(&Frame::Audio(f))?;
                let aus = drain(a.enc.as_mut())?;
                let config = a.gens.take_config(a.enc.codec_config());
                self.emit_audio(config, aus, outs);
            }
        }
        Ok(())
    }

    fn restart_video(
        &mut self,
        width: u32,
        height: u32,
        outs: &mut Vec<EncodedOutput>,
    ) -> Result<()> {
        let v = self.video.as_mut().expect("video path");
        let VideoEnc::Encoder { enc, .. } = &mut v.enc else {
            return Ok(());
        };
        enc.flush()?;
        let aus = drain(enc.as_mut())?;
        let config = v.gens.take_config(enc.codec_config());
        v.mode.width = width;
        v.mode.height = height;
        let vc = video_encoder_config(&v.out, &v.mode);
        let (provider, enc) = self
            .registry
            .create_encoder(&EncoderConfig::Video(vc.clone()), &self.policy)?;
        v.enc = VideoEnc::Encoder {
            enc,
            cfg: vc,
            provider,
        };
        v.gens.restart();
        self.stats.encoder_restarts += 1;
        self.emit_video(config, aus, outs);
        Ok(())
    }

    fn flush_encoders(&mut self, outs: &mut Vec<EncodedOutput>) -> Result<()> {
        if let Some(v) = self.video.as_mut()
            && let VideoEnc::Encoder { enc, .. } = &mut v.enc
        {
            enc.flush()?;
            let aus = drain(enc.as_mut())?;
            let config = v.gens.take_config(enc.codec_config());
            self.emit_video(config, aus, outs);
        }
        if let Some(a) = self.audio.as_mut() {
            a.enc.flush()?;
            let aus = drain(a.enc.as_mut())?;
            let config = a.gens.take_config(a.enc.codec_config());
            self.emit_audio(config, aus, outs);
        }
        Ok(())
    }

    fn stamp(&self, aus: &mut [AccessUnit], gens: &GenMap) {
        let base = self.base.unwrap_or(0);
        for au in aus.iter_mut() {
            au.config_generation = gens.map(au.config_generation);
            if let Some(ns) = au.pts.rescale(au.timebase, Timebase::NANOS) {
                au.capture_time_ns = Some(self.anchor.to_realtime(base + ns));
            }
        }
    }

    fn emit_video(
        &mut self,
        config: Option<CodecConfig>,
        mut aus: Vec<AccessUnit>,
        outs: &mut Vec<EncodedOutput>,
    ) {
        let Some(v) = self.video.as_ref() else { return };
        let track = v.out.track;
        self.stamp(&mut aus, &v.gens);
        self.stats.video_aus += aus.len() as u64;
        self.stats.bytes += aus.iter().map(|a| a.payload.len() as u64).sum::<u64>();
        if config.is_some() || !aus.is_empty() {
            outs.push(EncodedOutput { track, config, aus });
        }
    }

    fn emit_audio(
        &mut self,
        config: Option<CodecConfig>,
        mut aus: Vec<AccessUnit>,
        outs: &mut Vec<EncodedOutput>,
    ) {
        let Some(a) = self.audio.as_ref() else { return };
        let track = a.track;
        self.stamp(&mut aus, &a.gens);
        self.stats.audio_aus += aus.len() as u64;
        self.stats.bytes += aus.iter().map(|a| a.payload.len() as u64).sum::<u64>();
        if config.is_some() || !aus.is_empty() {
            outs.push(EncodedOutput { track, config, aus });
        }
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.rx = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn drain(enc: &mut dyn Encoder) -> Result<Vec<AccessUnit>> {
    let mut v = Vec::new();
    while let Some(au) = enc.receive()? {
        v.push(au);
    }
    Ok(v)
}

/// 同じトラックの続く出力をまとめる（設定が付いたものは前とまとめない）。
fn merge(outs: Vec<EncodedOutput>) -> Vec<EncodedOutput> {
    let mut r: Vec<EncodedOutput> = Vec::new();
    for o in outs {
        if o.config.is_none()
            && let Some(last) = r.iter_mut().rev().find(|x| x.track == o.track)
        {
            last.aus.extend(o.aus);
            continue;
        }
        r.push(o);
    }
    r
}

/// チャンネル数を合わせる（多ければ先頭から、少なければ最後を複製）。i16 だけ。
pub fn remap_channels(f: &AudioFrame, out: u8) -> Result<Bytes> {
    f.validate()?;
    if f.format != SampleFormat::I16 {
        return Err(CaptureError::internal("capture audio must be i16"));
    }
    if f.channels == out {
        return Ok(f.data.clone());
    }
    let (inc, outc) = (f.channels as usize, out as usize);
    let n = f.samples_per_channel();
    let mut v = Vec::with_capacity(n * outc * 2);
    for s in 0..n {
        for c in 0..outc {
            let src = c.min(inc - 1);
            let o = (s * inc + src) * 2;
            v.extend_from_slice(&f.data[o..o + 2]);
        }
    }
    Ok(Bytes::from(v))
}

/// 読み取りのスレッド。
fn reader(i: usize, mut dev: Box<dyn CaptureDevice>, tx: SyncSender<Msg>, stop: Arc<AtomicBool>) {
    let poll = Duration::from_millis(100);
    while !stop.load(Ordering::Acquire) {
        match dev.read(poll) {
            Ok(Some(f)) => {
                if tx.send(Msg::Frame(f)).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                let _ = tx.send(Msg::Error(i, e));
                let _ = dev.stop();
                return;
            }
        }
    }
    let _ = dev.stop();
    let _ = tx.send(Msg::Ended(i));
}

/// いまの単調時刻（試験用に公開）。
pub fn now_ns() -> i64 {
    clock::monotonic_ns()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Fps;

    #[test]
    fn channel_remap() {
        let mk = |ch: u8, vals: &[i16]| AudioFrame {
            format: SampleFormat::I16,
            sample_rate: 48000,
            channels: ch,
            data: Bytes::from(
                vals.iter()
                    .flat_map(|v| v.to_ne_bytes())
                    .collect::<Vec<u8>>(),
            ),
            pts: 0,
            timebase: Timebase::new(1, 48000),
        };
        let to = |b: Bytes| -> Vec<i16> {
            b.chunks(2)
                .map(|c| i16::from_ne_bytes([c[0], c[1]]))
                .collect()
        };
        // 8 ch → 2 ch は先頭の 2 つ
        let f = mk(8, &[1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 16, 17, 18]);
        assert_eq!(to(remap_channels(&f, 2).unwrap()), vec![1, 2, 11, 12]);
        // 1 ch → 2 ch は複製
        let f = mk(1, &[5, 6]);
        assert_eq!(to(remap_channels(&f, 2).unwrap()), vec![5, 5, 6, 6]);
        let f = mk(2, &[5, 6]);
        assert_eq!(to(remap_channels(&f, 2).unwrap()), vec![5, 6]);
    }

    #[test]
    fn generation_mapping() {
        let mut g = GenMap::default();
        let c = |g| CodecConfig {
            codec: Codec::Vp9,
            generation: g,
            record: Bytes::new(),
            video: None,
            audio: None,
        };
        assert_eq!(g.take_config(Some(c(0))).unwrap().generation, 0);
        assert!(g.take_config(Some(c(0))).is_none());
        assert_eq!(g.take_config(Some(c(1))).unwrap().generation, 1);
        g.restart();
        // 新しい符号化器は 0 から数えるが、2 に付け直す
        assert_eq!(g.take_config(Some(c(0))).unwrap().generation, 2);
        assert_eq!(g.map(0), 2);
        assert!(g.take_config(None).is_none());
    }

    #[test]
    fn h264_without_hardware_is_capacity_unavailable() {
        let registry = ProviderRegistry::with_providers(vec![]);
        let cfg = SessionConfig {
            video: Some((
                VideoRequest {
                    width: 640,
                    height: 480,
                    fps: Fps::integer(30),
                    formats: vec![],
                },
                VideoOutput {
                    codec: Codec::H264,
                    bitrate_bps: 1_000_000,
                    gop: None,
                    passthrough: false,
                    threads: 0,
                    track: TrackId(0),
                },
            )),
            audio: None,
            policy: Policy::default(),
        };
        let e = check_config(&cfg, &registry).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unavailable);
        assert_eq!(e.kind.api_code(), "CAPACITY_UNAVAILABLE");
        // 素通しは符号化しないので通る（方針がロイヤリティフリー限定なら拒否）
        let mut p = cfg.clone();
        p.video.as_mut().unwrap().1.passthrough = true;
        p.video.as_mut().unwrap().0.formats = vec![RawFormat::H264];
        assert!(check_config(&p, &registry).is_ok());
        p.policy = Policy::royalty_free();
        assert_eq!(
            check_config(&p, &registry).unwrap_err().kind,
            ErrorKind::PolicyRejected
        );
        // 素通しでないのに機器の H.264 を求めるのは誤り
        let mut q = cfg.clone();
        q.video.as_mut().unwrap().0.formats = vec![RawFormat::H264];
        assert_eq!(
            check_config(&q, &registry).unwrap_err().kind,
            ErrorKind::InvalidConfig
        );
        // VP9 のプロバイダーが無ければ Unavailable、AAC は Unsupported
        let mut r = cfg.clone();
        r.video.as_mut().unwrap().1.codec = Codec::Vp9;
        assert_eq!(
            check_config(&r, &registry).unwrap_err().kind,
            ErrorKind::Unavailable
        );
        let a = SessionConfig {
            video: None,
            audio: Some((
                AudioRequest {
                    sample_rate: 48000,
                    channels: 2,
                },
                AudioOutput {
                    codec: Codec::Aac,
                    bitrate_bps: 128000,
                    track: TrackId(1),
                },
            )),
            policy: Policy::default(),
        };
        assert_eq!(
            check_config(&a, &registry).unwrap_err().kind,
            ErrorKind::Unsupported
        );
    }
}
