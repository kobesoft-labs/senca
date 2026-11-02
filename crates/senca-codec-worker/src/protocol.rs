//! 親（ノード）と codec worker の間のプロトコル。I/O を持たない（sans-io）。
//!
//! # フレーム
//!
//! ```text
//! [u32 BE 本体の長さ][u8 種類][本体]
//! ```
//!
//! 本体の長さは [`MAX_BODY`] まで。整数はすべてビッグエンディアン。可変長のバイト列は `[u32 長さ][中身]`、
//! 文字列は UTF-8 のバイト列、列は `[u32 個数][要素...]`。個数・長さにはそれぞれ上限があり、
//! さらに「残りのバイト数」を超える個数は確保の前に拒否する（壊れた入力で大きな確保をしない、仕様 9.4・AC-01）。
//! 本体を読み終えた後に余りがあれば拒否する。
//!
//! # 流れ
//!
//! 1. worker は起動すると最初に [`Message::Hello`]（版・能力・サンドボックスの状態）を送る。版が違えば親は使わない。
//! 2. 親は [`Message::Job`] を 1 つ送り、worker は同じ `id` の [`Message::JobResult`] を 1 つ返す（1 つずつ順に）。
//! 3. 親は [`Message::Shutdown`] で終了を求める。
//!
//! # セッション（版 2）
//!
//! ジョブとは別に、パイプライン（`senca-pipeline`）を worker の中で動かし続ける。1 つの worker のプロセスは
//! 1 つのセッションだけを受け持つ（落ちても他のセッションに波及しない）。生のフレームはパイプを通らない
//! （符号化済みの AU と小さな通知だけ、REQ-010 R10-8）。
//!
//! 1. 親 → [`Message::SessionOpen`]（テナント・方針・グラフの JSON）。worker は検証して動かし始め、
//!    [`SessionEventKind::Opened`]（計画の JSON）か [`SessionEventKind::Error`] を返す。
//! 2. 親 → [`Message::SessionInput`]（入力ごとの AU の束、設定、終わりの印）。返事は無い。
//! 3. 親 → [`Message::SessionControl`]（操作の JSON、要求番号）。worker は同じ番号の
//!    [`SessionEventKind::ControlOk`] か [`SessionEventKind::ControlErr`] を返す（順不同）。
//! 4. worker → [`Message::SessionOutput`]（sink ごとの AU の束、または変化の静止画）と
//!    [`SessionEventKind::Event`]（状態・メーター・誤り、JSON）。
//! 5. 親 → [`Message::SessionClose`]。worker は出し切ってから [`SessionEventKind::Closed`] を返して終わる。
//!
//! JSON の本体は長さの上限を置き、ここでは UTF-8 だけを確かめる（形は受け手が `deny_unknown_fields` で検査する）。

use bytes::{Bytes, BytesMut};
use senca_codec::caps::{Capability, Direction, ProviderId, ProviderStatus};
use senca_codec::config::{
    AudioEncoderConfig, DecoderConfig, EncoderConfig, OpusApplication, RateControl,
    VideoEncoderConfig, VideoProfile,
};
use senca_codec::error::{CodecError, ErrorKind};
use senca_codec::frame::{AudioFrame, Frame, PixelFormat, SampleFormat, VideoFrame};
use senca_codec::registry::Policy;
use senca_core::media::{AudioParams, VideoParams};
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};
use std::fmt;

/// プロトコルの版。互換の無い変更で増やす。
pub const PROTOCOL_VERSION: u16 = 2;
/// フレームの頭（長さ 4 バイト + 種類 1 バイト）。
pub const HEADER_LEN: usize = 5;
/// 本体の長さの上限（64 MiB + 64 KiB）。
pub const MAX_BODY: usize = 64 * 1024 * 1024 + 64 * 1024;
/// 文字列の長さの上限。
pub const MAX_STRING: usize = 4096;
/// 1 つのジョブの AU の数の上限。
pub const MAX_AUS: usize = 10_000;
/// 1 つのジョブのフレームの数の上限。
pub const MAX_FRAMES: usize = 1_000;
/// 能力・状態の数の上限。
pub const MAX_CAPS: usize = 1024;
/// 優先プロバイダーの数の上限。
pub const MAX_PREFER: usize = 16;
/// 画素形式の数の上限。
pub const MAX_FORMATS: usize = 8;
/// セッションのグラフ・操作の JSON の上限（画像を base64 で含められる）。
pub const MAX_SESSION_JSON: usize = 16 * 1024 * 1024;
/// セッションの通知の JSON の上限。
pub const MAX_EVENT_JSON: usize = 1024 * 1024;
/// セッションの 1 つの束の AU の数の上限。
pub const MAX_SESSION_AUS: usize = 4096;
/// 部品・入力・出力の id の長さの上限。
pub const MAX_ID: usize = 256;

const T_HELLO: u8 = 1;
const T_JOB: u8 = 2;
const T_JOB_RESULT: u8 = 3;
const T_SHUTDOWN: u8 = 4;
const T_PING: u8 = 5;
const T_PONG: u8 = 6;
const T_SESSION_OPEN: u8 = 7;
const T_SESSION_INPUT: u8 = 8;
const T_SESSION_CONTROL: u8 = 9;
const T_SESSION_OUTPUT: u8 = 10;
const T_SESSION_EVENT: u8 = 11;
const T_SESSION_CLOSE: u8 = 12;

/// プロトコル違反。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(pub String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "protocol error: {}", self.0)
    }
}

impl std::error::Error for ProtocolError {}

fn perr(m: impl Into<String>) -> ProtocolError {
    ProtocolError(m.into())
}

type PResult<T> = Result<T, ProtocolError>;

/// worker の最初の通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    pub pid: u32,
    /// サンドボックスで適用したこと・できなかったことの説明。
    pub sandbox: String,
    pub capabilities: Vec<Capability>,
    pub statuses: Vec<ProviderStatus>,
}

/// ジョブ。
#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub policy: Policy,
    pub kind: JobKind,
}

/// ジョブの種類。
#[derive(Debug, Clone)]
pub enum JobKind {
    /// AU を復号して最初の映像フレームを返す（静止画・サムネイル用）。8 bit は I420 にそろえる。
    DecodeToImage {
        decoder: DecoderConfig,
        aus: Vec<AccessUnit>,
    },
    /// AU をすべて復号してフレームを返す。
    Decode {
        decoder: DecoderConfig,
        aus: Vec<AccessUnit>,
    },
    /// フレームを符号化して AU を返す。
    Encode {
        encoder: EncoderConfig,
        frames: Vec<Frame>,
    },
    /// 復号して符号化し直す。
    Transcode {
        decoder: DecoderConfig,
        aus: Vec<AccessUnit>,
        encoder: EncoderConfig,
    },
    /// サンドボックスの自己点検（入力なし）。
    SelfTest,
}

impl JobKind {
    pub fn name(&self) -> &'static str {
        match self {
            JobKind::DecodeToImage { .. } => "decode-to-image",
            JobKind::Decode { .. } => "decode",
            JobKind::Encode { .. } => "encode",
            JobKind::Transcode { .. } => "transcode",
            JobKind::SelfTest => "self-test",
        }
    }
}

/// サンドボックスの自己点検の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfTestReport {
    pub sandbox: String,
    pub no_new_privs: bool,
    pub seccomp: bool,
    /// `socket()` が拒否された。
    pub socket_denied: bool,
    /// 一時ディレクトリへのファイル作成が拒否された（Landlock）。
    pub fs_write_denied: bool,
}

/// ジョブの出力。
#[derive(Debug, Clone)]
pub enum JobOutput {
    Image(VideoFrame),
    Frames(Vec<Frame>),
    Encoded {
        config: Option<CodecConfig>,
        aus: Vec<AccessUnit>,
    },
    SelfTest(SelfTestReport),
}

/// ジョブの結果。
#[derive(Debug, Clone)]
pub struct JobResult {
    pub id: u64,
    pub result: Result<JobOutput, CodecError>,
}

/// セッションを開く。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionOpen {
    pub session_id: u64,
    pub tenant: String,
    pub policy: Policy,
    /// パイプラインの定義（JSON、`senca_pipeline::Graph`）。
    pub graph: String,
}

/// セッションの入力（1 つの入力の AU の束）。
#[derive(Debug, Clone)]
pub struct SessionInput {
    pub session_id: u64,
    /// グラフの `source` の id。
    pub source: String,
    /// トラックの設定（最初と、変わったとき）。
    pub config: Option<CodecConfig>,
    pub aus: Vec<AccessUnit>,
    /// この入力の終わり。
    pub end: bool,
}

/// セッションの操作。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionControl {
    pub session_id: u64,
    pub request_id: u64,
    /// 操作（JSON、`senca_pipeline::Control`）。
    pub control: String,
}

/// セッションの出力の中身。
#[derive(Debug, Clone)]
pub enum SessionOutputKind {
    /// 符号化済みの AU（sink のトラックごと）。
    Media {
        track: u32,
        generation: u64,
        config: Option<CodecConfig>,
        aus: Vec<AccessUnit>,
    },
    /// 板書の変化の静止画（JPEG）。
    Snapshot {
        generation: u64,
        pts_us: i64,
        width: u32,
        height: u32,
        changed_fraction: f64,
        jpeg: Bytes,
    },
}

/// セッションの出力。
#[derive(Debug, Clone)]
pub struct SessionOutput {
    pub session_id: u64,
    /// sink の id（静止画なら `snapshot_on_change` の id）。
    pub node: String,
    /// 出力先の stream id（静止画では空）。
    pub stream: String,
    pub kind: SessionOutputKind,
}

/// セッションの通知の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEventKind {
    /// 開いた（本体は計画の JSON）。
    Opened,
    /// パイプラインの通知（本体は `senca_pipeline::Event` の JSON）。
    Event,
    /// 操作が成功（本体は `ControlReply` の JSON）。
    ControlOk,
    /// 操作が失敗（本体は `{"code","message","issues"}` の JSON）。
    ControlErr,
    /// セッションの誤り（開けなかった等。本体は `{"code","message","issues"}`）。
    Error,
    /// 閉じた（出し切った）。
    Closed,
}

impl SessionEventKind {
    fn to_u8(self) -> u8 {
        match self {
            SessionEventKind::Opened => 0,
            SessionEventKind::Event => 1,
            SessionEventKind::ControlOk => 2,
            SessionEventKind::ControlErr => 3,
            SessionEventKind::Error => 4,
            SessionEventKind::Closed => 5,
        }
    }
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => SessionEventKind::Opened,
            1 => SessionEventKind::Event,
            2 => SessionEventKind::ControlOk,
            3 => SessionEventKind::ControlErr,
            4 => SessionEventKind::Error,
            5 => SessionEventKind::Closed,
            _ => return None,
        })
    }
}

/// セッションの通知。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEvent {
    pub session_id: u64,
    pub kind: SessionEventKind,
    /// 操作への返事なら、その要求番号（それ以外は 0）。
    pub request_id: u64,
    /// JSON。
    pub body: String,
}

/// メッセージ。
#[derive(Debug, Clone)]
pub enum Message {
    Hello(Hello),
    Job(Job),
    JobResult(JobResult),
    Shutdown,
    Ping(u64),
    Pong(u64),
    SessionOpen(SessionOpen),
    SessionInput(SessionInput),
    SessionControl(SessionControl),
    SessionOutput(SessionOutput),
    SessionEvent(SessionEvent),
    SessionClose(u64),
}

// ---------------------------------------------------------------------------
// 書き込み

struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.0.push(v as u8);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn count(&mut self, n: usize, max: usize) -> PResult<()> {
        if n > max {
            return Err(perr(format!("too many elements ({n} > {max})")));
        }
        self.u32(n as u32);
        Ok(())
    }
    fn bytes(&mut self, b: &[u8]) -> PResult<()> {
        if b.len() > MAX_BODY {
            return Err(perr("byte string too long"));
        }
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        Ok(())
    }
    fn str(&mut self, s: &str) -> PResult<()> {
        // 長すぎる文字列は UTF-8 の境界で切り詰める（エラーメッセージなど）。
        let mut end = s.len().min(MAX_STRING);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes(&s.as_bytes()[..end])
    }
    /// 長い文字列（切り詰めない。上限を超えれば `Err`）。
    fn long_str(&mut self, s: &str, max: usize) -> PResult<()> {
        if s.len() > max {
            return Err(perr("string too long"));
        }
        self.bytes(s.as_bytes())
    }
    fn id(&mut self, s: &str) -> PResult<()> {
        if s.is_empty() || s.len() > MAX_ID {
            return Err(perr("bad id"));
        }
        self.bytes(s.as_bytes())
    }
    fn opt_str(&mut self, s: &Option<String>) -> PResult<()> {
        match s {
            None => self.u8(0),
            Some(s) => {
                self.u8(1);
                self.str(s)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 読み取り

struct Dec {
    buf: Bytes,
    pos: usize,
}

impl Dec {
    fn new(buf: Bytes) -> Self {
        Dec { buf, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn take(&mut self, n: usize) -> PResult<&[u8]> {
        if self.remaining() < n {
            return Err(perr("truncated"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> PResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> PResult<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(perr("bad boolean")),
        }
    }
    fn u16(&mut self) -> PResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> PResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> PResult<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }
    fn i64(&mut self) -> PResult<i64> {
        Ok(self.u64()? as i64)
    }
    /// 列の個数。各要素は少なくとも `min_elem` バイトなので、残りから見て多すぎる個数は確保前に拒否する。
    fn count(&mut self, max: usize, min_elem: usize) -> PResult<usize> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(perr(format!("too many elements ({n} > {max})")));
        }
        if n.saturating_mul(min_elem.max(1)) > self.remaining() {
            return Err(perr("element count exceeds remaining data"));
        }
        Ok(n)
    }
    fn bytes(&mut self, max: usize) -> PResult<Bytes> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(perr("byte string too long"));
        }
        if n > self.remaining() {
            return Err(perr("truncated byte string"));
        }
        let b = self.buf.slice(self.pos..self.pos + n);
        self.pos += n;
        Ok(b)
    }
    fn str(&mut self) -> PResult<String> {
        let b = self.bytes(MAX_STRING)?;
        String::from_utf8(b.to_vec()).map_err(|_| perr("invalid UTF-8"))
    }
    /// 長い UTF-8 の文字列（JSON）。
    fn long_str(&mut self, max: usize) -> PResult<String> {
        let b = self.bytes(max)?;
        String::from_utf8(b.to_vec()).map_err(|_| perr("invalid UTF-8"))
    }
    /// id（空でなく、上限内）。
    fn id(&mut self) -> PResult<String> {
        let s = self.str()?;
        if s.is_empty() || s.len() > MAX_ID {
            return Err(perr("bad id"));
        }
        Ok(s)
    }
    fn opt_str(&mut self) -> PResult<Option<String>> {
        Ok(if self.bool()? {
            Some(self.str()?)
        } else {
            None
        })
    }
    fn finish(&self) -> PResult<()> {
        if self.remaining() != 0 {
            return Err(perr("trailing bytes"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 個々の型

const CODECS: [Codec; 10] = [
    Codec::H264,
    Codec::H265,
    Codec::Av1,
    Codec::Vp8,
    Codec::Vp9,
    Codec::Aac,
    Codec::Opus,
    Codec::Pcma,
    Codec::Pcmu,
    Codec::WebVtt,
];

fn put_codec(e: &mut Enc, c: Codec) {
    e.u8(CODECS
        .iter()
        .position(|x| *x == c)
        .expect("all codecs listed") as u8);
}

fn get_codec(d: &mut Dec) -> PResult<Codec> {
    CODECS
        .get(d.u8()? as usize)
        .copied()
        .ok_or_else(|| perr("unknown codec"))
}

fn put_timebase(e: &mut Enc, t: Timebase) {
    e.u32(t.num);
    e.u32(t.den);
}

fn get_timebase(d: &mut Dec) -> PResult<Timebase> {
    let num = d.u32()?;
    let den = d.u32()?;
    if num == 0 || den == 0 {
        return Err(perr("zero timebase"));
    }
    Ok(Timebase::new(num, den))
}

fn put_pixfmt(e: &mut Enc, f: PixelFormat) {
    e.u8(f.to_u8());
}

fn get_pixfmt(d: &mut Dec) -> PResult<PixelFormat> {
    PixelFormat::from_u8(d.u8()?).ok_or_else(|| perr("unknown pixel format"))
}

fn get_samplefmt(d: &mut Dec) -> PResult<SampleFormat> {
    SampleFormat::from_u8(d.u8()?).ok_or_else(|| perr("unknown sample format"))
}

fn get_provider(d: &mut Dec) -> PResult<ProviderId> {
    ProviderId::from_u8(d.u8()?).ok_or_else(|| perr("unknown provider"))
}

fn put_codec_config(e: &mut Enc, c: &CodecConfig) -> PResult<()> {
    put_codec(e, c.codec);
    e.u32(c.generation);
    e.bytes(&c.record)?;
    match &c.video {
        None => e.u8(0),
        Some(v) => {
            e.u8(1);
            e.u32(v.width);
            e.u32(v.height);
            e.u8(v.profile);
            e.u8(v.compat);
            e.u8(v.level);
        }
    }
    match &c.audio {
        None => e.u8(0),
        Some(a) => {
            e.u8(1);
            e.u32(a.sample_rate);
            e.u8(a.channels);
            e.u8(a.object_type);
        }
    }
    Ok(())
}

/// 設定レコード（avcC など）の長さの上限。
const MAX_RECORD: usize = 1024 * 1024;

fn get_codec_config(d: &mut Dec) -> PResult<CodecConfig> {
    let codec = get_codec(d)?;
    let generation = d.u32()?;
    let record = d.bytes(MAX_RECORD)?;
    let video = if d.bool()? {
        Some(VideoParams {
            width: d.u32()?,
            height: d.u32()?,
            profile: d.u8()?,
            compat: d.u8()?,
            level: d.u8()?,
        })
    } else {
        None
    };
    let audio = if d.bool()? {
        Some(AudioParams {
            sample_rate: d.u32()?,
            channels: d.u8()?,
            object_type: d.u8()?,
        })
    } else {
        None
    };
    Ok(CodecConfig {
        codec,
        generation,
        record,
        video,
        audio,
    })
}

fn put_opt_codec_config(e: &mut Enc, c: &Option<CodecConfig>) -> PResult<()> {
    match c {
        None => e.u8(0),
        Some(c) => {
            e.u8(1);
            put_codec_config(e, c)?;
        }
    }
    Ok(())
}

fn get_opt_codec_config(d: &mut Dec) -> PResult<Option<CodecConfig>> {
    Ok(if d.bool()? {
        Some(get_codec_config(d)?)
    } else {
        None
    })
}

fn put_au(e: &mut Enc, a: &AccessUnit) -> PResult<()> {
    e.u32(a.track.0);
    put_codec(e, a.codec);
    e.u64(a.epoch);
    e.u64(a.sequence);
    e.u32(a.config_generation);
    e.i64(a.pts);
    e.i64(a.dts);
    e.u64(a.duration);
    put_timebase(e, a.timebase);
    let flags = (a.keyframe as u8)
        | ((a.discontinuity as u8) << 1)
        | ((a.capture_time_ns.is_some() as u8) << 2);
    e.u8(flags);
    if let Some(t) = a.capture_time_ns {
        e.i64(t);
    }
    e.bytes(&a.payload)
}

/// AU 1 つの最小のバイト数。
const AU_MIN: usize = 4 + 1 + 8 + 8 + 4 + 8 + 8 + 8 + 8 + 1 + 4;

fn get_au(d: &mut Dec) -> PResult<AccessUnit> {
    let track = TrackId(d.u32()?);
    let codec = get_codec(d)?;
    let epoch = d.u64()?;
    let sequence = d.u64()?;
    let config_generation = d.u32()?;
    let pts = d.i64()?;
    let dts = d.i64()?;
    let duration = d.u64()?;
    let timebase = get_timebase(d)?;
    let flags = d.u8()?;
    if flags & !7 != 0 {
        return Err(perr("unknown AU flags"));
    }
    let capture_time_ns = if flags & 4 != 0 { Some(d.i64()?) } else { None };
    let payload = d.bytes(MAX_BODY)?;
    Ok(AccessUnit {
        track,
        codec,
        epoch,
        sequence,
        config_generation,
        pts,
        dts,
        duration,
        timebase,
        keyframe: flags & 1 != 0,
        discontinuity: flags & 2 != 0,
        capture_time_ns,
        payload,
    })
}

fn put_aus(e: &mut Enc, aus: &[AccessUnit]) -> PResult<()> {
    e.count(aus.len(), MAX_AUS)?;
    aus.iter().try_for_each(|a| put_au(e, a))
}

fn get_aus(d: &mut Dec) -> PResult<Vec<AccessUnit>> {
    let n = d.count(MAX_AUS, AU_MIN)?;
    (0..n).map(|_| get_au(d)).collect()
}

fn put_video_frame(e: &mut Enc, f: &VideoFrame) -> PResult<()> {
    put_pixfmt(e, f.format);
    e.u32(f.width);
    e.u32(f.height);
    e.i64(f.pts);
    put_timebase(e, f.timebase);
    e.bool(f.keyframe);
    // 面は stride を詰めて送る（受け手で `from_packed` が大きさを検査する）。
    let packed = f
        .to_packed()
        .map_err(|err| perr(format!("bad frame: {err}")))?;
    e.bytes(&packed)
}

fn get_video_frame(d: &mut Dec) -> PResult<VideoFrame> {
    let format = get_pixfmt(d)?;
    let width = d.u32()?;
    let height = d.u32()?;
    let pts = d.i64()?;
    let timebase = get_timebase(d)?;
    let keyframe = d.bool()?;
    let data = d.bytes(MAX_BODY)?;
    let mut f = VideoFrame::from_packed(format, width, height, data, pts, timebase)
        .map_err(|err| perr(format!("bad frame: {err}")))?;
    f.keyframe = keyframe;
    Ok(f)
}

fn put_audio_frame(e: &mut Enc, f: &AudioFrame) -> PResult<()> {
    e.u8(f.format.to_u8());
    e.u32(f.sample_rate);
    e.u8(f.channels);
    e.i64(f.pts);
    put_timebase(e, f.timebase);
    e.bytes(&f.data)
}

fn get_audio_frame(d: &mut Dec) -> PResult<AudioFrame> {
    let f = AudioFrame {
        format: get_samplefmt(d)?,
        sample_rate: d.u32()?,
        channels: d.u8()?,
        pts: d.i64()?,
        timebase: get_timebase(d)?,
        data: d.bytes(MAX_BODY)?,
    };
    f.validate()
        .map_err(|err| perr(format!("bad audio frame: {err}")))?;
    Ok(f)
}

fn put_frame(e: &mut Enc, f: &Frame) -> PResult<()> {
    match f {
        Frame::Video(v) => {
            e.u8(0);
            put_video_frame(e, v)
        }
        Frame::Audio(a) => {
            e.u8(1);
            put_audio_frame(e, a)
        }
    }
}

fn get_frame(d: &mut Dec) -> PResult<Frame> {
    match d.u8()? {
        0 => Ok(Frame::Video(get_video_frame(d)?)),
        1 => Ok(Frame::Audio(get_audio_frame(d)?)),
        _ => Err(perr("unknown frame type")),
    }
}

/// フレーム 1 つの最小のバイト数（音声の場合）。
const FRAME_MIN: usize = 1 + 1 + 4 + 1 + 8 + 8 + 4;

fn put_frames(e: &mut Enc, frames: &[Frame]) -> PResult<()> {
    e.count(frames.len(), MAX_FRAMES)?;
    frames.iter().try_for_each(|f| put_frame(e, f))
}

fn get_frames(d: &mut Dec) -> PResult<Vec<Frame>> {
    let n = d.count(MAX_FRAMES, FRAME_MIN)?;
    (0..n).map(|_| get_frame(d)).collect()
}

fn put_encoder(e: &mut Enc, c: &EncoderConfig) -> PResult<()> {
    match c {
        EncoderConfig::Video(v) => {
            e.u8(0);
            put_codec(e, v.codec);
            e.u32(v.width);
            e.u32(v.height);
            e.u32(v.fps_num);
            e.u32(v.fps_den);
            e.u32(v.bitrate_bps);
            match v.max_bitrate_bps {
                None => e.u8(0),
                Some(m) => {
                    e.u8(1);
                    e.u32(m);
                }
            }
            e.u32(v.gop);
            match v.rate_control {
                RateControl::Cbr => e.u8(0),
                RateControl::Vbr => e.u8(1),
                RateControl::ConstQp(q) => {
                    e.u8(2);
                    e.u8(q);
                }
            }
            match v.profile {
                None => e.u8(0xff),
                Some(p) => e.u8(p.to_u8()),
            }
            e.bool(v.low_latency);
            e.u32(v.b_frames);
            put_pixfmt(e, v.input_format);
            e.u32(v.threads);
            e.u32(v.track.0);
            put_timebase(e, v.timebase);
        }
        EncoderConfig::Audio(a) => {
            e.u8(1);
            put_codec(e, a.codec);
            e.u32(a.sample_rate);
            e.u8(a.channels);
            e.u32(a.bitrate_bps);
            e.u32(a.frame_duration_us);
            e.u8(match a.application {
                OpusApplication::Voip => 0,
                OpusApplication::Audio => 1,
                OpusApplication::LowDelay => 2,
            });
            e.u8(a.input_format.to_u8());
            e.u32(a.track.0);
        }
    }
    Ok(())
}

fn get_encoder(d: &mut Dec) -> PResult<EncoderConfig> {
    match d.u8()? {
        0 => {
            let codec = get_codec(d)?;
            let width = d.u32()?;
            let height = d.u32()?;
            let fps_num = d.u32()?;
            let fps_den = d.u32()?;
            let bitrate_bps = d.u32()?;
            let max_bitrate_bps = if d.bool()? { Some(d.u32()?) } else { None };
            let gop = d.u32()?;
            let rate_control = match d.u8()? {
                0 => RateControl::Cbr,
                1 => RateControl::Vbr,
                2 => RateControl::ConstQp(d.u8()?),
                _ => return Err(perr("unknown rate control")),
            };
            let profile = match d.u8()? {
                0xff => None,
                v => Some(VideoProfile::from_u8(v).ok_or_else(|| perr("unknown profile"))?),
            };
            let low_latency = d.bool()?;
            let b_frames = d.u32()?;
            let input_format = get_pixfmt(d)?;
            let threads = d.u32()?;
            let track = TrackId(d.u32()?);
            let timebase = get_timebase(d)?;
            Ok(EncoderConfig::Video(VideoEncoderConfig {
                codec,
                width,
                height,
                fps_num,
                fps_den,
                bitrate_bps,
                max_bitrate_bps,
                gop,
                rate_control,
                profile,
                low_latency,
                b_frames,
                input_format,
                threads,
                track,
                timebase,
            }))
        }
        1 => {
            let codec = get_codec(d)?;
            let sample_rate = d.u32()?;
            let channels = d.u8()?;
            let bitrate_bps = d.u32()?;
            let frame_duration_us = d.u32()?;
            let application = match d.u8()? {
                0 => OpusApplication::Voip,
                1 => OpusApplication::Audio,
                2 => OpusApplication::LowDelay,
                _ => return Err(perr("unknown opus application")),
            };
            let input_format = get_samplefmt(d)?;
            let track = TrackId(d.u32()?);
            Ok(EncoderConfig::Audio(AudioEncoderConfig {
                codec,
                sample_rate,
                channels,
                bitrate_bps,
                frame_duration_us,
                application,
                input_format,
                track,
            }))
        }
        _ => Err(perr("unknown encoder config type")),
    }
}

fn put_decoder(e: &mut Enc, c: &DecoderConfig) -> PResult<()> {
    put_codec(e, c.codec);
    put_opt_codec_config(e, &c.config)?;
    match c.output_format {
        None => e.u8(0xff),
        Some(f) => put_pixfmt(e, f),
    }
    e.u32(c.max_width);
    e.u32(c.max_height);
    e.u32(c.threads);
    e.u8(c.sample_format.to_u8());
    e.u32(c.sample_rate);
    e.u8(c.channels);
    Ok(())
}

fn get_decoder(d: &mut Dec) -> PResult<DecoderConfig> {
    let codec = get_codec(d)?;
    let config = get_opt_codec_config(d)?;
    let output_format = match d.u8()? {
        0xff => None,
        v => Some(PixelFormat::from_u8(v).ok_or_else(|| perr("unknown pixel format"))?),
    };
    Ok(DecoderConfig {
        codec,
        config,
        output_format,
        max_width: d.u32()?,
        max_height: d.u32()?,
        threads: d.u32()?,
        sample_format: get_samplefmt(d)?,
        sample_rate: d.u32()?,
        channels: d.u8()?,
    })
}

fn put_policy(e: &mut Enc, p: &Policy) -> PResult<()> {
    e.bool(p.royalty_free_only);
    e.bool(p.allow_software);
    e.count(p.prefer.len(), MAX_PREFER)?;
    for id in &p.prefer {
        e.u8(id.to_u8());
    }
    Ok(())
}

fn get_policy(d: &mut Dec) -> PResult<Policy> {
    let royalty_free_only = d.bool()?;
    let allow_software = d.bool()?;
    let n = d.count(MAX_PREFER, 1)?;
    let prefer = (0..n)
        .map(|_| get_provider(d))
        .collect::<PResult<Vec<_>>>()?;
    Ok(Policy {
        royalty_free_only,
        allow_software,
        prefer,
    })
}

fn put_capability(e: &mut Enc, c: &Capability) -> PResult<()> {
    e.u8(c.provider.to_u8());
    put_codec(e, c.codec);
    e.u8(match c.direction {
        Direction::Encode => 0,
        Direction::Decode => 1,
    });
    e.bool(c.hardware);
    e.u32(c.max_width);
    e.u32(c.max_height);
    match c.max_sessions {
        None => e.u8(0),
        Some(s) => {
            e.u8(1);
            e.u32(s);
        }
    }
    e.count(c.formats.len(), MAX_FORMATS)?;
    for f in &c.formats {
        put_pixfmt(e, *f);
    }
    e.u32(c.device_index);
    e.str(&c.device_name)?;
    e.opt_str(&c.driver_version)
}

fn get_capability(d: &mut Dec) -> PResult<Capability> {
    let provider = get_provider(d)?;
    let codec = get_codec(d)?;
    let direction = match d.u8()? {
        0 => Direction::Encode,
        1 => Direction::Decode,
        _ => return Err(perr("unknown direction")),
    };
    let hardware = d.bool()?;
    let max_width = d.u32()?;
    let max_height = d.u32()?;
    let max_sessions = if d.bool()? { Some(d.u32()?) } else { None };
    let n = d.count(MAX_FORMATS, 1)?;
    let formats = (0..n).map(|_| get_pixfmt(d)).collect::<PResult<Vec<_>>>()?;
    Ok(Capability {
        provider,
        codec,
        direction,
        hardware,
        max_width,
        max_height,
        max_sessions,
        formats,
        device_index: d.u32()?,
        device_name: d.str()?,
        driver_version: d.opt_str()?,
    })
}

fn put_error(e: &mut Enc, err: &CodecError) -> PResult<()> {
    e.u8(err.kind.to_u8());
    e.str(&err.message)
}

fn get_error(d: &mut Dec) -> PResult<CodecError> {
    let kind = ErrorKind::from_u8(d.u8()?).ok_or_else(|| perr("unknown error kind"))?;
    Ok(CodecError::new(kind, d.str()?))
}

fn put_job_kind(e: &mut Enc, k: &JobKind) -> PResult<()> {
    match k {
        JobKind::DecodeToImage { decoder, aus } => {
            e.u8(0);
            put_decoder(e, decoder)?;
            put_aus(e, aus)
        }
        JobKind::Decode { decoder, aus } => {
            e.u8(1);
            put_decoder(e, decoder)?;
            put_aus(e, aus)
        }
        JobKind::Encode { encoder, frames } => {
            e.u8(2);
            put_encoder(e, encoder)?;
            put_frames(e, frames)
        }
        JobKind::Transcode {
            decoder,
            aus,
            encoder,
        } => {
            e.u8(3);
            put_decoder(e, decoder)?;
            put_aus(e, aus)?;
            put_encoder(e, encoder)
        }
        JobKind::SelfTest => {
            e.u8(4);
            Ok(())
        }
    }
}

fn get_job_kind(d: &mut Dec) -> PResult<JobKind> {
    Ok(match d.u8()? {
        0 => JobKind::DecodeToImage {
            decoder: get_decoder(d)?,
            aus: get_aus(d)?,
        },
        1 => JobKind::Decode {
            decoder: get_decoder(d)?,
            aus: get_aus(d)?,
        },
        2 => JobKind::Encode {
            encoder: get_encoder(d)?,
            frames: get_frames(d)?,
        },
        3 => JobKind::Transcode {
            decoder: get_decoder(d)?,
            aus: get_aus(d)?,
            encoder: get_encoder(d)?,
        },
        4 => JobKind::SelfTest,
        _ => return Err(perr("unknown job kind")),
    })
}

fn put_output(e: &mut Enc, o: &JobOutput) -> PResult<()> {
    match o {
        JobOutput::Image(f) => {
            e.u8(0);
            put_video_frame(e, f)
        }
        JobOutput::Frames(fs) => {
            e.u8(1);
            put_frames(e, fs)
        }
        JobOutput::Encoded { config, aus } => {
            e.u8(2);
            put_opt_codec_config(e, config)?;
            put_aus(e, aus)
        }
        JobOutput::SelfTest(r) => {
            e.u8(3);
            e.str(&r.sandbox)?;
            e.bool(r.no_new_privs);
            e.bool(r.seccomp);
            e.bool(r.socket_denied);
            e.bool(r.fs_write_denied);
            Ok(())
        }
    }
}

fn get_output(d: &mut Dec) -> PResult<JobOutput> {
    Ok(match d.u8()? {
        0 => JobOutput::Image(get_video_frame(d)?),
        1 => JobOutput::Frames(get_frames(d)?),
        2 => JobOutput::Encoded {
            config: get_opt_codec_config(d)?,
            aus: get_aus(d)?,
        },
        3 => JobOutput::SelfTest(SelfTestReport {
            sandbox: d.str()?,
            no_new_privs: d.bool()?,
            seccomp: d.bool()?,
            socket_denied: d.bool()?,
            fs_write_denied: d.bool()?,
        }),
        _ => return Err(perr("unknown job output")),
    })
}

// ---------------------------------------------------------------------------
// メッセージ

/// メッセージをフレーム（頭付き）にする。本体が [`MAX_BODY`] を超えれば `Err`。
pub fn encode_message(m: &Message) -> PResult<Vec<u8>> {
    let mut e = Enc(vec![0; HEADER_LEN]);
    let ty = match m {
        Message::Hello(h) => {
            e.u16(h.version);
            e.u32(h.pid);
            e.str(&h.sandbox)?;
            e.count(h.capabilities.len(), MAX_CAPS)?;
            for c in &h.capabilities {
                put_capability(&mut e, c)?;
            }
            e.count(h.statuses.len(), MAX_CAPS)?;
            for s in &h.statuses {
                e.u8(s.provider.to_u8());
                e.bool(s.available);
                e.str(&s.detail)?;
            }
            T_HELLO
        }
        Message::Job(j) => {
            e.u64(j.id);
            put_policy(&mut e, &j.policy)?;
            put_job_kind(&mut e, &j.kind)?;
            T_JOB
        }
        Message::JobResult(r) => {
            e.u64(r.id);
            match &r.result {
                Ok(o) => {
                    e.u8(0);
                    put_output(&mut e, o)?;
                }
                Err(err) => {
                    e.u8(1);
                    put_error(&mut e, err)?;
                }
            }
            T_JOB_RESULT
        }
        Message::Shutdown => T_SHUTDOWN,
        Message::Ping(n) => {
            e.u64(*n);
            T_PING
        }
        Message::Pong(n) => {
            e.u64(*n);
            T_PONG
        }
        Message::SessionOpen(o) => {
            e.u64(o.session_id);
            e.id(&o.tenant)?;
            put_policy(&mut e, &o.policy)?;
            e.long_str(&o.graph, MAX_SESSION_JSON)?;
            T_SESSION_OPEN
        }
        Message::SessionInput(i) => {
            e.u64(i.session_id);
            e.id(&i.source)?;
            put_opt_codec_config(&mut e, &i.config)?;
            e.count(i.aus.len(), MAX_SESSION_AUS)?;
            for a in &i.aus {
                put_au(&mut e, a)?;
            }
            e.bool(i.end);
            T_SESSION_INPUT
        }
        Message::SessionControl(c) => {
            e.u64(c.session_id);
            e.u64(c.request_id);
            e.long_str(&c.control, MAX_SESSION_JSON)?;
            T_SESSION_CONTROL
        }
        Message::SessionOutput(o) => {
            e.u64(o.session_id);
            e.id(&o.node)?;
            e.str(&o.stream)?;
            match &o.kind {
                SessionOutputKind::Media {
                    track,
                    generation,
                    config,
                    aus,
                } => {
                    e.u8(0);
                    e.u32(*track);
                    e.u64(*generation);
                    put_opt_codec_config(&mut e, config)?;
                    e.count(aus.len(), MAX_SESSION_AUS)?;
                    for a in aus {
                        put_au(&mut e, a)?;
                    }
                }
                SessionOutputKind::Snapshot {
                    generation,
                    pts_us,
                    width,
                    height,
                    changed_fraction,
                    jpeg,
                } => {
                    e.u8(1);
                    e.u64(*generation);
                    e.i64(*pts_us);
                    e.u32(*width);
                    e.u32(*height);
                    e.u64(changed_fraction.to_bits());
                    e.bytes(jpeg)?;
                }
            }
            T_SESSION_OUTPUT
        }
        Message::SessionEvent(ev) => {
            e.u64(ev.session_id);
            e.u8(ev.kind.to_u8());
            e.u64(ev.request_id);
            e.long_str(&ev.body, MAX_EVENT_JSON)?;
            T_SESSION_EVENT
        }
        Message::SessionClose(id) => {
            e.u64(*id);
            T_SESSION_CLOSE
        }
    };
    let body = e.0.len() - HEADER_LEN;
    if body > MAX_BODY {
        return Err(perr(format!("message too large ({body} bytes)")));
    }
    e.0[..4].copy_from_slice(&(body as u32).to_be_bytes());
    e.0[4] = ty;
    Ok(e.0)
}

/// フレームの頭を読む。`(本体の長さ, 種類)`。長さが上限を超えれば `Err`。
pub fn parse_header(h: &[u8; HEADER_LEN]) -> PResult<(usize, u8)> {
    let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
    if len > MAX_BODY {
        return Err(perr(format!("frame too large ({len} bytes)")));
    }
    if !(T_HELLO..=T_SESSION_CLOSE).contains(&h[4]) {
        return Err(perr("unknown message type"));
    }
    Ok((len, h[4]))
}

/// 本体を読む。
pub fn decode_body(ty: u8, body: Bytes) -> PResult<Message> {
    let mut d = Dec::new(body);
    let m = match ty {
        T_HELLO => {
            let version = d.u16()?;
            let pid = d.u32()?;
            let sandbox = d.str()?;
            let n = d.count(MAX_CAPS, 20)?;
            let capabilities = (0..n)
                .map(|_| get_capability(&mut d))
                .collect::<PResult<Vec<_>>>()?;
            let n = d.count(MAX_CAPS, 6)?;
            let statuses = (0..n)
                .map(|_| {
                    Ok(ProviderStatus {
                        provider: get_provider(&mut d)?,
                        available: d.bool()?,
                        detail: d.str()?,
                    })
                })
                .collect::<PResult<Vec<_>>>()?;
            Message::Hello(Hello {
                version,
                pid,
                sandbox,
                capabilities,
                statuses,
            })
        }
        T_JOB => {
            let id = d.u64()?;
            let policy = get_policy(&mut d)?;
            let kind = get_job_kind(&mut d)?;
            Message::Job(Job { id, policy, kind })
        }
        T_JOB_RESULT => {
            let id = d.u64()?;
            let result = match d.u8()? {
                0 => Ok(get_output(&mut d)?),
                1 => Err(get_error(&mut d)?),
                _ => return Err(perr("bad result tag")),
            };
            Message::JobResult(JobResult { id, result })
        }
        T_SHUTDOWN => Message::Shutdown,
        T_PING => Message::Ping(d.u64()?),
        T_PONG => Message::Pong(d.u64()?),
        T_SESSION_OPEN => Message::SessionOpen(SessionOpen {
            session_id: d.u64()?,
            tenant: d.id()?,
            policy: get_policy(&mut d)?,
            graph: d.long_str(MAX_SESSION_JSON)?,
        }),
        T_SESSION_INPUT => {
            let session_id = d.u64()?;
            let source = d.id()?;
            let config = get_opt_codec_config(&mut d)?;
            let n = d.count(MAX_SESSION_AUS, AU_MIN)?;
            let aus = (0..n)
                .map(|_| get_au(&mut d))
                .collect::<PResult<Vec<_>>>()?;
            Message::SessionInput(SessionInput {
                session_id,
                source,
                config,
                aus,
                end: d.bool()?,
            })
        }
        T_SESSION_CONTROL => Message::SessionControl(SessionControl {
            session_id: d.u64()?,
            request_id: d.u64()?,
            control: d.long_str(MAX_SESSION_JSON)?,
        }),
        T_SESSION_OUTPUT => {
            let session_id = d.u64()?;
            let node = d.id()?;
            let stream = d.str()?;
            let kind = match d.u8()? {
                0 => {
                    let track = d.u32()?;
                    let generation = d.u64()?;
                    let config = get_opt_codec_config(&mut d)?;
                    let n = d.count(MAX_SESSION_AUS, AU_MIN)?;
                    let aus = (0..n)
                        .map(|_| get_au(&mut d))
                        .collect::<PResult<Vec<_>>>()?;
                    SessionOutputKind::Media {
                        track,
                        generation,
                        config,
                        aus,
                    }
                }
                1 => {
                    let generation = d.u64()?;
                    let pts_us = d.i64()?;
                    let width = d.u32()?;
                    let height = d.u32()?;
                    let changed_fraction = f64::from_bits(d.u64()?);
                    if !changed_fraction.is_finite() {
                        return Err(perr("bad changed fraction"));
                    }
                    let jpeg = d.bytes(MAX_BODY)?;
                    SessionOutputKind::Snapshot {
                        generation,
                        pts_us,
                        width,
                        height,
                        changed_fraction,
                        jpeg,
                    }
                }
                _ => return Err(perr("unknown session output kind")),
            };
            Message::SessionOutput(SessionOutput {
                session_id,
                node,
                stream,
                kind,
            })
        }
        T_SESSION_EVENT => {
            let session_id = d.u64()?;
            let kind = SessionEventKind::from_u8(d.u8()?)
                .ok_or_else(|| perr("unknown session event kind"))?;
            Message::SessionEvent(SessionEvent {
                session_id,
                kind,
                request_id: d.u64()?,
                body: d.long_str(MAX_EVENT_JSON)?,
            })
        }
        T_SESSION_CLOSE => Message::SessionClose(d.u64()?),
        _ => return Err(perr("unknown message type")),
    };
    d.finish()?;
    Ok(m)
}

/// バッファの先頭から 1 フレームを取り出す（足りなければ `Ok(None)`）。
pub fn try_decode_frame(buf: &mut BytesMut) -> PResult<Option<Message>> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let mut h = [0u8; HEADER_LEN];
    h.copy_from_slice(&buf[..HEADER_LEN]);
    let (len, ty) = parse_header(&h)?;
    if buf.len() < HEADER_LEN + len {
        return Ok(None);
    }
    let mut frame = buf.split_to(HEADER_LEN + len);
    let body = frame.split_off(HEADER_LEN).freeze();
    decode_body(ty, body).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(payload: &'static [u8]) -> AccessUnit {
        AccessUnit {
            track: TrackId(2),
            codec: Codec::Vp8,
            epoch: 3,
            sequence: 4,
            config_generation: 5,
            pts: -6,
            dts: -7,
            duration: 8,
            timebase: Timebase::MPEG,
            keyframe: true,
            discontinuity: false,
            capture_time_ns: Some(99),
            payload: Bytes::from_static(payload),
        }
    }

    fn roundtrip(m: &Message) -> Message {
        let enc = encode_message(m).unwrap();
        let mut buf = BytesMut::from(&enc[..]);
        let out = try_decode_frame(&mut buf).unwrap().unwrap();
        assert!(buf.is_empty());
        // 再符号化して同じバイト列になる
        assert_eq!(encode_message(&out).unwrap(), enc);
        out
    }

    fn sample_job() -> Message {
        let mut dec = DecoderConfig::new(Codec::Vp8);
        dec.config = Some(CodecConfig {
            codec: Codec::Vp8,
            generation: 1,
            record: Bytes::from_static(b"rec"),
            video: Some(VideoParams {
                width: 4,
                height: 2,
                profile: 0,
                compat: 0,
                level: 0,
            }),
            audio: Some(AudioParams {
                sample_rate: 1,
                channels: 1,
                object_type: 0,
            }),
        });
        let mut venc = VideoEncoderConfig::new(Codec::Vp9, 4, 2, 30, 1000);
        venc.rate_control = RateControl::ConstQp(30);
        venc.profile = Some(VideoProfile::Vp9Profile0);
        venc.max_bitrate_bps = Some(5);
        Message::Job(Job {
            id: 42,
            policy: Policy {
                royalty_free_only: true,
                allow_software: false,
                prefer: vec![ProviderId::Libvpx],
            },
            kind: JobKind::Transcode {
                decoder: dec,
                aus: vec![au(b"abc"), au(b"")],
                encoder: EncoderConfig::Video(venc),
            },
        })
    }

    #[test]
    fn job_roundtrip() {
        let m = sample_job();
        let Message::Job(j) = roundtrip(&m) else {
            panic!()
        };
        assert_eq!(j.id, 42);
        assert!(j.policy.royalty_free_only);
        let JobKind::Transcode {
            decoder,
            aus,
            encoder,
        } = j.kind
        else {
            panic!()
        };
        assert_eq!(aus.len(), 2);
        assert_eq!(aus[0].payload.as_ref(), b"abc");
        assert_eq!(aus[0].capture_time_ns, Some(99));
        assert_eq!(decoder.config.unwrap().record.as_ref(), b"rec");
        let EncoderConfig::Video(v) = encoder else {
            panic!()
        };
        assert_eq!(v.rate_control, RateControl::ConstQp(30));
    }

    #[test]
    fn encode_job_and_results_roundtrip() {
        let frame = VideoFrame::from_packed(
            PixelFormat::Nv12,
            3,
            3,
            Bytes::from(vec![7u8; 17]),
            10,
            Timebase::MPEG,
        )
        .unwrap();
        let audio = AudioFrame {
            format: SampleFormat::F32,
            sample_rate: 48000,
            channels: 1,
            data: Bytes::from(vec![0u8; 8]),
            pts: 0,
            timebase: Timebase::new(1, 48000),
        };
        let m = Message::Job(Job {
            id: 1,
            policy: Policy::default(),
            kind: JobKind::Encode {
                encoder: EncoderConfig::Audio(AudioEncoderConfig::opus(48000, 1, 32000)),
                frames: vec![Frame::Video(frame.clone()), Frame::Audio(audio)],
            },
        });
        let Message::Job(Job {
            kind: JobKind::Encode { frames, .. },
            ..
        }) = roundtrip(&m)
        else {
            panic!()
        };
        assert_eq!(frames[0], Frame::Video(frame.clone()));

        for r in [
            Ok(JobOutput::Image(frame)),
            Ok(JobOutput::Encoded {
                config: None,
                aus: vec![au(b"x")],
            }),
            Ok(JobOutput::SelfTest(SelfTestReport {
                sandbox: "s".into(),
                no_new_privs: true,
                seccomp: true,
                socket_denied: true,
                fs_write_denied: false,
            })),
            Err(CodecError::unavailable("no hw")),
        ] {
            let Message::JobResult(back) = roundtrip(&Message::JobResult(JobResult {
                id: 9,
                result: r.clone(),
            })) else {
                panic!()
            };
            assert_eq!(back.id, 9);
            assert_eq!(back.result.is_ok(), r.is_ok());
            if let Err(e) = back.result {
                assert_eq!(e.kind, ErrorKind::Unavailable);
            }
        }
        roundtrip(&Message::Shutdown);
        roundtrip(&Message::Ping(5));
        roundtrip(&Message::Job(Job {
            id: 2,
            policy: Policy::default(),
            kind: JobKind::SelfTest,
        }));
    }

    #[test]
    fn hello_roundtrip() {
        let h = Hello {
            version: PROTOCOL_VERSION,
            pid: 123,
            sandbox: "seccomp=on".into(),
            capabilities: vec![Capability {
                provider: ProviderId::Dav1d,
                codec: Codec::Av1,
                direction: Direction::Decode,
                hardware: false,
                max_width: 8192,
                max_height: 8192,
                max_sessions: Some(3),
                formats: vec![PixelFormat::I420, PixelFormat::P010],
                device_index: 0,
                device_name: "dav1d".into(),
                driver_version: Some("1.5.3".into()),
            }],
            statuses: vec![ProviderStatus {
                provider: ProviderId::Nvidia,
                available: false,
                detail: "none".into(),
            }],
        };
        let Message::Hello(b) = roundtrip(&Message::Hello(h.clone())) else {
            panic!()
        };
        assert_eq!(b, h);
    }

    #[test]
    fn partial_frames_wait() {
        let enc = encode_message(&sample_job()).unwrap();
        for cut in 0..enc.len() {
            let mut buf = BytesMut::from(&enc[..cut]);
            assert!(try_decode_frame(&mut buf).unwrap().is_none(), "cut {cut}");
        }
    }

    #[test]
    fn rejects_oversize_and_unknown() {
        let mut buf = BytesMut::from(&[0xff, 0xff, 0xff, 0xff, T_JOB][..]);
        assert!(try_decode_frame(&mut buf).is_err());
        let mut buf = BytesMut::from(&[0, 0, 0, 0, 77][..]);
        assert!(try_decode_frame(&mut buf).is_err());
        // 余りのバイト
        let mut enc = encode_message(&Message::Ping(1)).unwrap();
        enc.push(0);
        enc[3] += 1;
        assert!(try_decode_frame(&mut BytesMut::from(&enc[..])).is_err());
        // 個数が残りを超える（確保前に拒否）
        let mut body = vec![0u8; 8];
        body.extend_from_slice(&[0, 1, 0]); // policy
        body.extend_from_slice(&[0, 0, 0, 0]); // prefer = 0
        body.push(1); // Decode
        assert!(decode_body(T_JOB, Bytes::from(body)).is_err());
        let mut body = 7u64.to_be_bytes().to_vec();
        body.extend_from_slice(&[0, 1, 0, 0, 0, 0, 2]); // policy + Encode
        body.extend_from_slice(&[0u8; 3]);
        assert!(decode_body(T_JOB, Bytes::from(body)).is_err());
    }

    #[test]
    fn corrupted_bytes_never_panic() {
        // 正しいフレームの各バイトを書き換えても、パニックせず Ok か Err を返す。
        let enc = encode_message(&sample_job()).unwrap();
        for i in 0..enc.len() {
            for v in [0u8, 1, 0x7f, 0xff] {
                let mut c = enc.clone();
                c[i] = v;
                let _ = try_decode_frame(&mut BytesMut::from(&c[..]));
            }
        }
        // 疑似乱数のごみ
        let mut x = 0x1234_5678_9abc_def0u64;
        for _ in 0..2000 {
            let mut g = Vec::new();
            for _ in 0..64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                g.push(x as u8);
            }
            g[0] = 0;
            g[1] = 0;
            g[4] = (g[4] % 12) + 1;
            let _ = try_decode_frame(&mut BytesMut::from(&g[..]));
        }
    }

    #[test]
    fn session_messages_roundtrip() {
        let cfg = CodecConfig {
            codec: Codec::Vp8,
            generation: 2,
            record: Bytes::from_static(b"r"),
            video: None,
            audio: None,
        };
        let msgs = vec![
            Message::SessionOpen(SessionOpen {
                session_id: 7,
                tenant: "t1".into(),
                policy: Policy::royalty_free(),
                graph: "{\"version\":1,\"nodes\":[]}".into(),
            }),
            Message::SessionInput(SessionInput {
                session_id: 7,
                source: "cam".into(),
                config: Some(cfg.clone()),
                aus: vec![au(b"abc"), au(b"")],
                end: true,
            }),
            Message::SessionControl(SessionControl {
                session_id: 7,
                request_id: 9,
                control: "{\"op\":\"request_keyframe\"}".into(),
            }),
            Message::SessionOutput(SessionOutput {
                session_id: 7,
                node: "out".into(),
                stream: "program".into(),
                kind: SessionOutputKind::Media {
                    track: 1,
                    generation: 3,
                    config: Some(cfg),
                    aus: vec![au(b"x")],
                },
            }),
            Message::SessionOutput(SessionOutput {
                session_id: 7,
                node: "snap".into(),
                stream: String::new(),
                kind: SessionOutputKind::Snapshot {
                    generation: 1,
                    pts_us: -5,
                    width: 64,
                    height: 32,
                    changed_fraction: 0.25,
                    jpeg: Bytes::from_static(&[0xff, 0xd8]),
                },
            }),
            Message::SessionEvent(SessionEvent {
                session_id: 7,
                kind: SessionEventKind::ControlErr,
                request_id: 9,
                body: "{}".into(),
            }),
            Message::SessionClose(7),
        ];
        for m in msgs {
            let back = roundtrip(&m);
            assert_eq!(format!("{back:?}"), format!("{m:?}"));
        }
    }

    #[test]
    fn session_messages_enforce_bounds() {
        // 空の id
        let m = Message::SessionInput(SessionInput {
            session_id: 1,
            source: String::new(),
            config: None,
            aus: vec![],
            end: false,
        });
        assert!(encode_message(&m).is_err());
        // AU の数の上限
        let m = Message::SessionInput(SessionInput {
            session_id: 1,
            source: "a".into(),
            config: None,
            aus: vec![au(b""); MAX_SESSION_AUS + 1],
            end: false,
        });
        assert!(encode_message(&m).is_err());
        // 通知の JSON の上限
        let m = Message::SessionEvent(SessionEvent {
            session_id: 1,
            kind: SessionEventKind::Event,
            request_id: 0,
            body: "x".repeat(MAX_EVENT_JSON + 1),
        });
        assert!(encode_message(&m).is_err());
        // 知らない通知の種類・出力の種類
        let mut body = 1u64.to_be_bytes().to_vec();
        body.push(9);
        body.extend_from_slice(&0u64.to_be_bytes());
        body.extend_from_slice(&[0, 0, 0, 0]);
        assert!(decode_body(T_SESSION_EVENT, Bytes::from(body)).is_err());
        let mut body = 1u64.to_be_bytes().to_vec();
        body.extend_from_slice(&[0, 0, 0, 1, b'o', 0, 0, 0, 0, 7]);
        assert!(decode_body(T_SESSION_OUTPUT, Bytes::from(body)).is_err());
        // 壊れた UTF-8 の JSON
        let mut body = 1u64.to_be_bytes().to_vec();
        body.extend_from_slice(&2u64.to_be_bytes());
        body.extend_from_slice(&[0, 0, 0, 2, 0xff, 0xfe]);
        assert!(decode_body(T_SESSION_CONTROL, Bytes::from(body)).is_err());
        // 個数が残りを超える（確保前に拒否）
        let mut body = 1u64.to_be_bytes().to_vec();
        body.extend_from_slice(&[0, 0, 0, 1, b's', 0, 0, 0, 0x0f, 0xff]);
        assert!(decode_body(T_SESSION_INPUT, Bytes::from(body)).is_err());
        // 壊したバイト列でもパニックしない
        let enc = encode_message(&Message::SessionOutput(SessionOutput {
            session_id: 7,
            node: "out".into(),
            stream: "p".into(),
            kind: SessionOutputKind::Media {
                track: 0,
                generation: 1,
                config: None,
                aus: vec![au(b"abc")],
            },
        }))
        .unwrap();
        for i in 0..enc.len() {
            for v in [0u8, 1, 0x7f, 0xff] {
                let mut c = enc.clone();
                c[i] = v;
                let _ = try_decode_frame(&mut BytesMut::from(&c[..]));
            }
        }
    }

    #[test]
    fn long_strings_truncated_on_utf8_boundary() {
        let msg = "あ".repeat(3000);
        let m = Message::JobResult(JobResult {
            id: 1,
            result: Err(CodecError::backend(msg)),
        });
        let Message::JobResult(r) = roundtrip(&m) else {
            panic!()
        };
        let e = r.result.err().unwrap();
        assert!(e.message.len() <= MAX_STRING);
    }
}
