//! `senca-capture-worker` の本体（機器の SDK を別プロセスに隔離する、REQ-004・仕様 3.1）。
//!
//! # 起動の形
//!
//! - `senca-capture-worker list [--synthetic] [--no-sandbox]`: 機器を検出し、標準出力に JSON を 1 つ書いて終わる
//!   （`{"version":1,"backends":[...],"devices":[...]}`。ノードの `GET /devices` 用）。
//! - `senca-capture-worker serve [--synthetic] [--no-sandbox] [--no-seccomp] [--no-landlock]`:
//!   標準入出力で親とやりとりし、1 つの取り込みのセッションを動かす（終わったらプロセスも終わる）。
//!
//! # プロトコル（`senca_codec_worker::protocol` のフレーム形式とメッセージをそのまま使う）
//!
//! ```text
//! [u32 BE 本体の長さ][u8 種類][本体]   本体は MAX_BODY（64 MiB + 64 KiB）まで、上限付きの厳格な解析
//! ```
//!
//! 1. worker → `Hello`（版・pid・サンドボックスの状態・符号化の能力）。
//! 2. 親 → `SessionOpen { session_id, tenant, policy, graph }`。`graph` は [`CaptureSpec`] の JSON。
//! 3. worker → `SessionEvent { kind: Opened, body }`。本体は `{"tracks":[...],"negotiated":[...],"devices":[...]}`。
//!    失敗なら `SessionEvent { kind: Error, body: {"code","kind","message"} }` と `Closed`。
//! 4. worker → `SessionOutput { node: "capture", stream: "", kind: Media { track, generation, config, aus } }`
//!    を符号化のたびに。`config` は最初と変わったときだけ付く。
//!    worker → `SessionEvent { kind: Event, body: {"type":"stats",...} }` を 2 秒ごと。
//! 5. 親 → `SessionClose(session_id)`（または `Shutdown`、標準入力の終わり）。worker は機器を止め、符号化器を出し切って
//!    残りの `SessionOutput` と `SessionEvent { kind: Closed, body: 統計 }` を送って終わる。
//!    機器の誤りで止まったときは `Error` の後に `Closed` を送り、終了コード 1 で終わる。
//! 6. 親 → `Ping(n)` には `Pong(n)`。
//!
//! # サンドボックス
//!
//! codec worker と同じもの（`senca_codec_worker::sandbox`）を掛ける。ただし:
//!
//! - ジョブごとの CPU 時間の上限は掛けない（取り込みは長く続く）。
//! - seccomp の拒否リストは ioctl・mmap・open を拒否しないので、V4L2（`/dev/video*` の ioctl・mmap）・
//!   ALSA の `hw`/`plughw`（`/dev/snd/*`）はそのまま動く。Landlock も書き込みを `/dev` の下に限るだけ。
//! - socket は拒否する。PulseAudio・PipeWire 経由の ALSA（`default`）は使えない（`plughw` を使う）。
//!   DeckLink のドライバーが補助のデーモンと socket で話す場合は動かないので `--no-seccomp` で緩める
//!   （実機で未確認）。
//! - ライブラリ（`libDeckLinkAPI.so`・`libasound.so.2`・コーデック）は seccomp を掛ける前に読み込む。

use crate::device::{DeviceRegistry, ProbeOptions};
use crate::error::{CaptureError, ErrorKind};
use crate::session::{CaptureSession, EncodedOutput};
use crate::spec::CaptureSpec;
use crate::types::{BackendStatus, DeviceDescriptor};
use bytes::Bytes;
use senca_codec::caps::ProviderId;
use senca_codec::registry::{Policy, ProbeOptions as CodecProbeOptions, ProviderRegistry};
use senca_codec_worker::protocol::{
    self, Hello, MAX_SESSION_AUS, Message, PROTOCOL_VERSION, SessionEvent, SessionEventKind,
    SessionOpen, SessionOutput, SessionOutputKind,
};
use senca_codec_worker::sandbox::{self, SandboxConfig};
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Read, Write};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

/// `list` の出力の版。
pub const LISTING_VERSION: u32 = 1;
/// 統計を送る間隔。
const STATS_EVERY: Duration = Duration::from_secs(2);
/// `SessionOutput` の node の名前。
pub const OUTPUT_NODE: &str = "capture";

/// `list` の出力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceListing {
    pub version: u32,
    pub backends: Vec<BackendStatus>,
    pub devices: Vec<DeviceDescriptor>,
}

/// worker の設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOptions {
    pub sandbox: SandboxConfig,
    /// 試験用の機器を出す（環境変数 `SENCA_CAPTURE_SYNTHETIC=1` と同じ）。
    pub synthetic: bool,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        WorkerOptions {
            sandbox: SandboxConfig {
                // 取り込みは長く続くので、ジョブごとの CPU 時間の上限は掛けない。
                max_cpu_secs_per_job: None,
                ..SandboxConfig::default()
            },
            synthetic: false,
        }
    }
}

/// 起動の形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    List(WorkerOptions),
    Serve(WorkerOptions),
    Help,
}

pub const USAGE: &str = "usage: senca-capture-worker (list|serve) [--synthetic] [--no-sandbox] [--no-seccomp] \
[--no-landlock] [--max-memory-mb N]";

/// 引数を解釈する。
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Mode, String> {
    let mut it = args.into_iter();
    let mode = it.next().ok_or("missing mode (list or serve)")?;
    let mut o = WorkerOptions::default();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--synthetic" => o.synthetic = true,
            "--no-sandbox" => o.sandbox.enabled = false,
            "--no-seccomp" => o.sandbox.seccomp = false,
            "--no-landlock" => o.sandbox.landlock = false,
            "--max-memory-mb" => {
                let v: u64 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("--max-memory-mb needs a number")?;
                o.sandbox.max_memory_mb = (v > 0).then_some(v);
            }
            "-h" | "--help" => return Ok(Mode::Help),
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    match mode.as_str() {
        "list" => Ok(Mode::List(o)),
        "serve" => Ok(Mode::Serve(o)),
        "-h" | "--help" => Ok(Mode::Help),
        _ => Err(format!("unknown mode {mode}")),
    }
}

fn log(msg: &str) {
    eprintln!("senca-capture-worker[{}]: {msg}", std::process::id());
}

fn probe_options(o: &WorkerOptions) -> ProbeOptions {
    let mut p = ProbeOptions::from_env();
    p.synthetic |= o.synthetic;
    p
}

/// 1 つのメッセージを書く。
pub fn write_message(w: &mut impl Write, m: &Message) -> std::io::Result<()> {
    let buf = protocol::encode_message(m).map_err(|e| std::io::Error::other(e.to_string()))?;
    w.write_all(&buf)?;
    w.flush()
}

/// 1 つのメッセージを読む（終わりなら `None`）。
pub fn read_message(r: &mut impl Read) -> Result<Option<Message>, String> {
    let mut h = [0u8; protocol::HEADER_LEN];
    let mut got = 0;
    while got < h.len() {
        match r.read(&mut h[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err("truncated header".into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let (len, ty) = protocol::parse_header(&h).map_err(|e| e.to_string())?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).map_err(|e| e.to_string())?;
    protocol::decode_body(ty, Bytes::from(body))
        .map(Some)
        .map_err(|e| e.to_string())
}

/// `list` を実行する。
pub fn run_list(o: &WorkerOptions) -> i32 {
    let mut out = match sandbox::isolate_stdout() {
        Ok(f) => f,
        Err(e) => {
            log(&format!("cannot isolate stdout: {e}"));
            return 2;
        }
    };
    let mut st = sandbox::apply_early(&o.sandbox);
    let reg = DeviceRegistry::probe(&probe_options(o));
    sandbox::apply_late(&o.sandbox, &mut st, false);
    let listing = DeviceListing {
        version: LISTING_VERSION,
        backends: reg.statuses(),
        devices: reg.devices(),
    };
    let json = serde_json::to_string(&listing).expect("serializable");
    match writeln!(out, "{json}").and_then(|()| out.flush()) {
        Ok(()) => 0,
        Err(e) => {
            log(&format!("cannot write listing: {e}"));
            2
        }
    }
}

fn error_body(e: &CaptureError) -> String {
    serde_json::json!({
        "code": e.kind.api_code(),
        "kind": e.kind.as_str(),
        "message": e.message,
    })
    .to_string()
}

fn event(session_id: u64, kind: SessionEventKind, body: String) -> Message {
    Message::SessionEvent(SessionEvent {
        session_id,
        kind,
        request_id: 0,
        body,
    })
}

/// 出力を `SessionOutput` にする（AU が多ければ分ける）。
pub fn output_messages(session_id: u64, o: EncodedOutput) -> Vec<Message> {
    let generation = o
        .config
        .as_ref()
        .map(|c| c.generation as u64)
        .or_else(|| o.aus.first().map(|a| a.config_generation as u64))
        .unwrap_or(0);
    let mut msgs = Vec::new();
    let mut config = o.config;
    let mut aus = o.aus;
    loop {
        let rest = if aus.len() > MAX_SESSION_AUS {
            aus.split_off(MAX_SESSION_AUS)
        } else {
            Vec::new()
        };
        msgs.push(Message::SessionOutput(SessionOutput {
            session_id,
            node: OUTPUT_NODE.into(),
            stream: String::new(),
            kind: SessionOutputKind::Media {
                track: o.track.0,
                generation,
                config: config.take(),
                aus,
            },
        }));
        if rest.is_empty() {
            break;
        }
        aus = rest;
    }
    msgs
}

enum Input {
    Msg(Message),
    End,
    Bad(String),
}

fn spawn_input_reader(mut input: impl Read + Send + 'static) -> Receiver<Input> {
    let (tx, rx) = sync_channel(64);
    std::thread::Builder::new()
        .name("capture-input".into())
        .spawn(move || {
            loop {
                let m = match read_message(&mut input) {
                    Ok(Some(m)) => Input::Msg(m),
                    Ok(None) => Input::End,
                    Err(e) => Input::Bad(e),
                };
                let stop = !matches!(m, Input::Msg(_));
                if tx.send(m).is_err() || stop {
                    break;
                }
            }
        })
        .expect("spawn input reader");
    rx
}

/// `serve` を実行する。
pub fn serve(o: &WorkerOptions) -> i32 {
    let mut out = match sandbox::isolate_stdout() {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            log(&format!("cannot isolate stdout: {e}"));
            return 2;
        }
    };
    let mut st = sandbox::apply_early(&o.sandbox);
    let codecs = ProviderRegistry::probe(&CodecProbeOptions::from_env());
    let devices = DeviceRegistry::probe(&probe_options(o));
    let gpu_runtime = codecs
        .statuses()
        .iter()
        .any(|s| s.provider == ProviderId::Nvidia && s.available);
    sandbox::apply_late(&o.sandbox, &mut st, gpu_runtime);
    log(&format!("sandbox: {}", st.summary()));
    let hello = Hello {
        version: PROTOCOL_VERSION,
        pid: std::process::id(),
        sandbox: st.summary(),
        capabilities: codecs.capabilities(&Policy::default()),
        statuses: codecs.statuses(),
    };
    if let Err(e) = write_message(&mut out, &Message::Hello(hello)) {
        log(&format!("cannot send hello: {e}"));
        return 2;
    }
    let input = spawn_input_reader(BufReader::with_capacity(1 << 16, std::io::stdin()));
    loop {
        match input.recv() {
            Ok(Input::Msg(Message::SessionOpen(open))) => {
                return run_session(open, &codecs, &devices, &input, &mut out);
            }
            Ok(Input::Msg(Message::Ping(n))) => {
                if write_message(&mut out, &Message::Pong(n)).is_err() {
                    return 2;
                }
            }
            Ok(Input::Msg(Message::Shutdown)) | Ok(Input::End) | Err(_) => return 0,
            Ok(Input::Msg(other)) => {
                log(&format!("unexpected message before SessionOpen: {other:?}"));
                return 3;
            }
            Ok(Input::Bad(e)) => {
                log(&format!("bad input from parent: {e}"));
                return 3;
            }
        }
    }
}

fn open_session(
    open: &SessionOpen,
    codecs: &ProviderRegistry,
    devices: &DeviceRegistry,
) -> Result<CaptureSession, CaptureError> {
    let spec = CaptureSpec::from_json(&open.graph)?;
    let devices = match &spec.synthetic {
        Some(s) => devices.with_synthetic(s.clone()),
        None => devices.clone(),
    };
    let cfg = spec.session_config(open.policy.clone())?;
    crate::session::check_config(&cfg, codecs)?;
    let devs = spec.open_devices(&devices)?;
    CaptureSession::start(devs, &cfg, codecs)
}

fn run_session(
    open: SessionOpen,
    codecs: &ProviderRegistry,
    devices: &DeviceRegistry,
    input: &Receiver<Input>,
    out: &mut impl Write,
) -> i32 {
    let sid = open.session_id;
    let mut session = match open_session(&open, codecs, devices) {
        Ok(s) => s,
        Err(e) => {
            log(&format!(
                "session {sid} (tenant {}) rejected: {e}",
                open.tenant
            ));
            let _ = write_message(out, &event(sid, SessionEventKind::Error, error_body(&e)));
            let _ = write_message(out, &event(sid, SessionEventKind::Closed, "{}".into()));
            return 0;
        }
    };
    let opened = serde_json::json!({
        "tracks": session.tracks(),
        "negotiated": session.negotiated().iter().map(|(id, n)| serde_json::json!({
            "device": id, "video": n.video, "audio": n.audio,
        })).collect::<Vec<_>>(),
    });
    if write_message(
        out,
        &event(sid, SessionEventKind::Opened, opened.to_string()),
    )
    .is_err()
    {
        return 2;
    }
    let mut last_stats = Instant::now();
    let mut failure: Option<CaptureError> = None;
    let mut closing = false;
    loop {
        // 親からの指示（ブロックしない）
        loop {
            match input.try_recv() {
                Ok(Input::Msg(Message::SessionClose(id))) if id == sid => closing = true,
                Ok(Input::Msg(Message::Shutdown)) | Ok(Input::End) => closing = true,
                Ok(Input::Msg(Message::Ping(n))) => {
                    if write_message(out, &Message::Pong(n)).is_err() {
                        return 2;
                    }
                }
                Ok(Input::Msg(other)) => log(&format!("ignoring message: {other:?}")),
                Ok(Input::Bad(e)) => {
                    log(&format!("bad input from parent: {e}"));
                    closing = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closing = true;
                    break;
                }
            }
        }
        if closing {
            break;
        }
        match session.poll(Duration::from_millis(50)) {
            Ok(outs) => {
                for o in outs {
                    for m in output_messages(sid, o) {
                        if write_message(out, &m).is_err() {
                            return 2;
                        }
                    }
                }
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
        if session.is_finished() {
            break;
        }
        if last_stats.elapsed() >= STATS_EVERY {
            last_stats = Instant::now();
            let mut body = serde_json::to_value(session.stats()).unwrap_or_default();
            body["type"] = "stats".into();
            if write_message(out, &event(sid, SessionEventKind::Event, body.to_string())).is_err() {
                return 2;
            }
        }
    }
    match session.finish() {
        Ok(outs) => {
            for o in outs {
                for m in output_messages(sid, o) {
                    if write_message(out, &m).is_err() {
                        return 2;
                    }
                }
            }
        }
        Err(e) => {
            failure.get_or_insert(e);
        }
    }
    let code = match &failure {
        Some(e) => {
            log(&format!("session {sid} failed: {e}"));
            let _ = write_message(out, &event(sid, SessionEventKind::Error, error_body(e)));
            1
        }
        None => 0,
    };
    let stats = serde_json::to_string(session.stats()).unwrap_or_default();
    let _ = write_message(out, &event(sid, SessionEventKind::Closed, stats));
    code
}

/// 子プロセスの誤り（`Error` の本体）を読む。
pub fn parse_error_body(body: &str) -> CaptureError {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let kind = v["kind"]
        .as_str()
        .and_then(ErrorKind::parse)
        .unwrap_or(ErrorKind::Internal);
    CaptureError::new(
        kind,
        v["message"].as_str().unwrap_or("capture worker error"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_core::{AccessUnit, Codec, Timebase, TrackId};

    #[test]
    fn args() {
        let a = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
        assert!(matches!(a(&["list", "--synthetic"]).unwrap(), Mode::List(o) if o.synthetic));
        assert!(
            matches!(a(&["serve", "--no-seccomp"]).unwrap(), Mode::Serve(o) if !o.sandbox.seccomp)
        );
        assert_eq!(a(&["--help"]).unwrap(), Mode::Help);
        assert!(a(&[]).is_err());
        assert!(a(&["serve", "--bogus"]).is_err());
        assert!(a(&["dance"]).is_err());
        let Mode::Serve(o) = a(&["serve"]).unwrap() else {
            panic!()
        };
        assert_eq!(o.sandbox.max_cpu_secs_per_job, None);
        assert!(o.sandbox.seccomp && o.sandbox.enabled);
    }

    #[test]
    fn outputs_are_split_and_roundtrip() {
        let au = |i: u64| AccessUnit {
            track: TrackId(0),
            codec: Codec::Vp9,
            epoch: 0,
            sequence: i,
            config_generation: 3,
            pts: i as i64,
            dts: i as i64,
            duration: 1,
            timebase: Timebase::MPEG,
            keyframe: i == 0,
            discontinuity: false,
            capture_time_ns: Some(5),
            payload: Bytes::from_static(b"x"),
        };
        let o = EncodedOutput {
            track: TrackId(0),
            config: None,
            aus: (0..(MAX_SESSION_AUS as u64 + 3)).map(au).collect(),
        };
        let msgs = output_messages(9, o);
        assert_eq!(msgs.len(), 2);
        let mut total = 0;
        for m in &msgs {
            let buf = protocol::encode_message(m).unwrap();
            let back = read_message(&mut &buf[..]).unwrap().unwrap();
            let Message::SessionOutput(so) = back else {
                panic!()
            };
            assert_eq!(so.node, OUTPUT_NODE);
            let SessionOutputKind::Media {
                generation, aus, ..
            } = so.kind
            else {
                panic!()
            };
            assert_eq!(generation, 3);
            total += aus.len();
        }
        assert_eq!(total, MAX_SESSION_AUS + 3);
        assert!(read_message(&mut &[0u8, 0][..]).is_err());
        assert!(read_message(&mut &[][..]).unwrap().is_none());
    }

    #[test]
    fn error_body_roundtrip() {
        let e = CaptureError::unavailable("no hw");
        let back = parse_error_body(&error_body(&e));
        assert_eq!(back, e);
    }
}
