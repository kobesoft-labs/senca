//! worker プロセスの中身: 引数の解釈、サンドボックス、検出、ジョブの実行（同期、1 つずつ）。
//!
//! 標準入力からフレームを読み、標準出力（[`sandbox::isolate_stdout`] で隔離したもの）にだけプロトコルを書く。
//! ログは標準エラーへ。

use crate::protocol::{
    self, Hello, Job, JobKind, JobOutput, JobResult, MAX_AUS, MAX_FRAMES, Message,
    PROTOCOL_VERSION, SelfTestReport,
};
use crate::sandbox::{self, SandboxConfig, SandboxState};
use bytes::Bytes;
use senca_codec::caps::ProviderId;
use senca_codec::error::{CodecError, Result as CResult};
use senca_codec::frame::{Frame, PixelFormat, VideoFrame};
use senca_codec::provider::{Decoder, Encoder};
use senca_codec::registry::{Policy, ProbeOptions, ProviderRegistry};
use senca_codec::{DecoderConfig, EncoderConfig};
use senca_core::AccessUnit;
use std::io::{BufReader, Read, Write};
use std::time::Duration;

/// 試験用の故障（AC-17）。`--fault` を付けたときだけ有効。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// ジョブを受けたら abort する。
    Crash,
    /// ジョブを受けたら返事をせずに止まる。
    Hang,
    /// 違う id の結果を返す。
    Lie,
    /// 上限を超える長さのフレーム頭を書く。
    Oversize,
    /// 各ジョブの前に眠る（公平さの試験用）。
    Slow(Duration),
    /// セッションの入力を `--fault-after` 個受けたら abort する（セッションの異常終了の試験用）。
    SessionCrash,
}

impl Fault {
    pub fn parse(s: &str) -> Option<Fault> {
        Some(match s {
            "crash" => Fault::Crash,
            "hang" => Fault::Hang,
            "lie" => Fault::Lie,
            "oversize" => Fault::Oversize,
            "session-crash" => Fault::SessionCrash,
            _ => {
                let ms = s.strip_prefix("slow:")?.parse::<u64>().ok()?;
                Fault::Slow(Duration::from_millis(ms.min(600_000)))
            }
        })
    }
}

/// worker の設定。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkerOptions {
    pub sandbox: SandboxConfig,
    pub fault: Option<Fault>,
    /// この数のジョブを正常に処理した後で故障を起こす。
    pub fault_after: u64,
}

/// 起動の形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Serve(WorkerOptions),
    /// サンドボックスを掛けて自己点検の結果を 1 行出し、終わる（socket が拒否されれば終了コード 0）。
    SelfTestSandbox(WorkerOptions),
    Help,
}

pub const USAGE: &str = "usage: senca-codec-worker [--no-sandbox] [--no-seccomp] [--no-landlock] \
[--max-memory-mb N] [--max-cpu-secs N] [--fault crash|hang|lie|oversize|slow:MS|session-crash] [--fault-after N] \
[--self-test-sandbox]";

/// 引数を解釈する。
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Mode, String> {
    let mut o = WorkerOptions::default();
    let mut self_test = false;
    let mut it = args.into_iter();
    let num = |v: Option<String>, name: &str| -> Result<u64, String> {
        v.and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("{name} needs a number"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-sandbox" => o.sandbox.enabled = false,
            "--no-seccomp" => o.sandbox.seccomp = false,
            "--no-landlock" => o.sandbox.landlock = false,
            "--max-memory-mb" => {
                let v = num(it.next(), &a)?;
                o.sandbox.max_memory_mb = (v > 0).then_some(v);
            }
            "--max-cpu-secs" => {
                let v = num(it.next(), &a)?;
                o.sandbox.max_cpu_secs_per_job = (v > 0).then_some(v);
            }
            "--fault" => {
                let v = it.next().ok_or("--fault needs a value")?;
                o.fault = Some(Fault::parse(&v).ok_or_else(|| format!("unknown fault {v}"))?);
            }
            "--fault-after" => o.fault_after = num(it.next(), &a)?,
            "--self-test-sandbox" => self_test = true,
            "-h" | "--help" => return Ok(Mode::Help),
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    Ok(if self_test {
        Mode::SelfTestSandbox(o)
    } else {
        Mode::Serve(o)
    })
}

pub(crate) fn log(msg: &str) {
    eprintln!("senca-codec-worker[{}]: {msg}", std::process::id());
}

fn self_test(st: &SandboxState) -> SelfTestReport {
    SelfTestReport {
        sandbox: st.summary(),
        no_new_privs: st.no_new_privs,
        seccomp: st.seccomp,
        socket_denied: sandbox::socket_denied(),
        fs_write_denied: sandbox::fs_write_denied(&std::env::temp_dir()),
    }
}

/// `--self-test-sandbox` の本体。
pub fn run_self_test(o: &WorkerOptions) -> i32 {
    let mut st = sandbox::apply_early(&o.sandbox);
    sandbox::apply_late(&o.sandbox, &mut st, false);
    let r = self_test(&st);
    println!(
        "no_new_privs={} seccomp={} socket_denied={} fs_write_denied={} sandbox=\"{}\"",
        r.no_new_privs as u8,
        r.seccomp as u8,
        r.socket_denied as u8,
        r.fs_write_denied as u8,
        r.sandbox
    );
    if r.socket_denied { 0 } else { 1 }
}

/// 標準入力から 1 フレームを読む。入力の終わりなら `Ok(None)`。
pub(crate) fn read_message(r: &mut impl Read) -> Result<Option<Message>, String> {
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

fn write_message(w: &mut impl Write, m: &Message) -> std::io::Result<()> {
    let buf = match protocol::encode_message(m) {
        Ok(b) => b,
        Err(e) => {
            // 結果が大きすぎるなど。ジョブの結果ならエラーに置き換える。
            let Message::JobResult(r) = m else {
                return Err(std::io::Error::other(e.to_string()));
            };
            protocol::encode_message(&Message::JobResult(JobResult {
                id: r.id,
                result: Err(CodecError::invalid_input(format!(
                    "job output exceeds protocol limit: {e}"
                ))),
            }))
            .map_err(|e| std::io::Error::other(e.to_string()))?
        }
    };
    w.write_all(&buf)?;
    w.flush()
}

/// worker の本体。終了コードを返す。
pub fn serve(o: &WorkerOptions) -> i32 {
    let mut out = match sandbox::isolate_stdout() {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            log(&format!("cannot isolate stdout: {e}"));
            return 2;
        }
    };
    let mut st = sandbox::apply_early(&o.sandbox);
    let registry = ProviderRegistry::probe(&ProbeOptions::from_env());
    let gpu_runtime = registry
        .statuses()
        .iter()
        .any(|s| s.provider == ProviderId::Nvidia && s.available);
    sandbox::apply_late(&o.sandbox, &mut st, gpu_runtime);
    log(&format!("sandbox: {}", st.summary()));
    let hello = Hello {
        version: PROTOCOL_VERSION,
        pid: std::process::id(),
        sandbox: st.summary(),
        capabilities: registry.capabilities(&Policy::default()),
        statuses: registry.statuses(),
    };
    if let Err(e) = write_message(&mut out, &Message::Hello(hello)) {
        log(&format!("cannot send hello: {e}"));
        return 2;
    }
    let mut input = BufReader::with_capacity(1 << 16, std::io::stdin().lock());
    let mut done: u64 = 0;
    loop {
        let m = match read_message(&mut input) {
            Ok(Some(m)) => m,
            Ok(None) => return 0,
            Err(e) => {
                log(&format!("bad input from parent: {e}"));
                return 3;
            }
        };
        match m {
            Message::Job(job) => {
                if let Some(f) = o.fault.filter(|_| done >= o.fault_after) {
                    match f {
                        Fault::Crash => {
                            log("fault injection: crash");
                            std::process::abort();
                        }
                        Fault::Hang => {
                            log("fault injection: hang");
                            loop {
                                std::thread::sleep(Duration::from_secs(3600));
                            }
                        }
                        Fault::Lie => {
                            log("fault injection: lie");
                            let fake = Message::JobResult(JobResult {
                                id: job.id.wrapping_add(1),
                                result: Err(CodecError::internal("lie")),
                            });
                            let _ = write_message(&mut out, &fake);
                            done += 1;
                            continue;
                        }
                        Fault::Oversize => {
                            log("fault injection: oversize");
                            let _ = out.write_all(&[0xff, 0xff, 0xff, 0xff, 3]);
                            let _ = out.flush();
                            done += 1;
                            continue;
                        }
                        Fault::Slow(d) => std::thread::sleep(d),
                        Fault::SessionCrash => {}
                    }
                }
                if let Some(s) = o.sandbox.max_cpu_secs_per_job.filter(|_| o.sandbox.enabled) {
                    sandbox::arm_cpu_limit(s);
                }
                let id = job.id;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute(&registry, &st, job)
                }))
                .unwrap_or_else(|_| {
                    Err(CodecError::internal(
                        "worker panicked while processing the job",
                    ))
                });
                if let Err(e) =
                    write_message(&mut out, &Message::JobResult(JobResult { id, result }))
                {
                    log(&format!("cannot send result: {e}"));
                    return 2;
                }
                done += 1;
            }
            Message::Ping(n) => {
                if write_message(&mut out, &Message::Pong(n)).is_err() {
                    return 2;
                }
            }
            Message::Shutdown => return 0,
            Message::SessionOpen(open) => {
                // このプロセスはセッション専用になる（終わったら終了する）。
                return crate::session::serve_session(o, registry, open, input, out);
            }
            other => {
                log(&format!("unexpected message from parent: {other:?}"));
                return 3;
            }
        }
    }
}

/// ジョブを 1 つ実行する。
pub fn execute(registry: &ProviderRegistry, st: &SandboxState, job: Job) -> CResult<JobOutput> {
    let policy = &job.policy;
    match job.kind {
        JobKind::SelfTest => Ok(JobOutput::SelfTest(self_test(st))),
        JobKind::DecodeToImage { decoder, aus } => {
            let mut dec = open_decoder(registry, &decoder, policy)?;
            let mut first: Option<VideoFrame> = None;
            fn take(first: &mut Option<VideoFrame>, f: Frame) {
                if first.is_none()
                    && let Frame::Video(v) = f
                {
                    *first = Some(v);
                }
            }
            for au in &aus {
                dec.send(au)?;
                while let Some(f) = dec.receive()? {
                    take(&mut first, f);
                }
                if first.is_some() {
                    break;
                }
            }
            if first.is_none() {
                dec.flush()?;
                while let Some(f) = dec.receive()? {
                    take(&mut first, f);
                }
            }
            let f =
                first.ok_or_else(|| CodecError::invalid_input("no picture could be decoded"))?;
            let f = if f.format == PixelFormat::Nv12 {
                f.convert(PixelFormat::I420)?
            } else {
                f
            };
            Ok(JobOutput::Image(f))
        }
        JobKind::Decode { decoder, aus } => {
            let mut dec = open_decoder(registry, &decoder, policy)?;
            let mut out = Vec::new();
            for au in &aus {
                dec.send(au)?;
                drain_decoder(dec.as_mut(), &mut out)?;
            }
            dec.flush()?;
            drain_decoder(dec.as_mut(), &mut out)?;
            Ok(JobOutput::Frames(out))
        }
        JobKind::Encode { encoder, frames } => {
            let mut enc = open_encoder(registry, &encoder, policy)?;
            let mut aus = Vec::new();
            for f in &frames {
                let f = adapt_frame(f, &encoder)?;
                enc.send(&f)?;
                drain_encoder(enc.as_mut(), &mut aus)?;
            }
            enc.flush()?;
            drain_encoder(enc.as_mut(), &mut aus)?;
            Ok(JobOutput::Encoded {
                config: enc.codec_config(),
                aus,
            })
        }
        JobKind::Transcode {
            decoder,
            aus,
            encoder,
        } => {
            let mut dec = open_decoder(registry, &decoder, policy)?;
            let mut enc = open_encoder(registry, &encoder, policy)?;
            let mut out = Vec::new();
            let pass = |dec: &mut dyn Decoder,
                        enc: &mut dyn Encoder,
                        out: &mut Vec<AccessUnit>|
             -> CResult<()> {
                while let Some(f) = dec.receive()? {
                    let f = adapt_frame(&f, &encoder)?;
                    enc.send(&f)?;
                    drain_encoder(enc, out)?;
                }
                Ok(())
            };
            for au in &aus {
                dec.send(au)?;
                pass(dec.as_mut(), enc.as_mut(), &mut out)?;
            }
            dec.flush()?;
            pass(dec.as_mut(), enc.as_mut(), &mut out)?;
            enc.flush()?;
            drain_encoder(enc.as_mut(), &mut out)?;
            Ok(JobOutput::Encoded {
                config: enc.codec_config(),
                aus: out,
            })
        }
    }
}

fn open_decoder(r: &ProviderRegistry, c: &DecoderConfig, p: &Policy) -> CResult<Box<dyn Decoder>> {
    r.create_decoder(c, p).map(|(_, d)| d)
}

fn open_encoder(r: &ProviderRegistry, c: &EncoderConfig, p: &Policy) -> CResult<Box<dyn Encoder>> {
    r.create_encoder(c, p).map(|(_, e)| e)
}

/// 映像フレームをエンコーダーの入力形式にそろえる。
fn adapt_frame(f: &Frame, enc: &EncoderConfig) -> CResult<Frame> {
    match (f, enc) {
        (Frame::Video(v), EncoderConfig::Video(c)) if v.format != c.input_format => {
            Ok(Frame::Video(v.convert(c.input_format)?))
        }
        _ => Ok(f.clone()),
    }
}

fn drain_decoder(dec: &mut dyn Decoder, out: &mut Vec<Frame>) -> CResult<()> {
    while let Some(f) = dec.receive()? {
        if out.len() >= MAX_FRAMES {
            return Err(CodecError::invalid_input(
                "too many decoded frames for one job",
            ));
        }
        out.push(f);
    }
    Ok(())
}

fn drain_encoder(enc: &mut dyn Encoder, out: &mut Vec<AccessUnit>) -> CResult<()> {
    while let Some(au) = enc.receive()? {
        if out.len() >= MAX_AUS {
            return Err(CodecError::invalid_input(
                "too many encoded units for one job",
            ));
        }
        out.push(au);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args() {
        let m = parse_args([
            "--fault".into(),
            "slow:20".into(),
            "--fault-after".into(),
            "2".into(),
        ])
        .unwrap();
        let Mode::Serve(o) = m else { panic!() };
        assert_eq!(o.fault, Some(Fault::Slow(Duration::from_millis(20))));
        assert_eq!(o.fault_after, 2);
        assert!(parse_args(["--fault".into(), "x".into()]).is_err());
        assert!(parse_args(["--bogus".into()]).is_err());
        assert!(matches!(
            parse_args(["--self-test-sandbox".into()]).unwrap(),
            Mode::SelfTestSandbox(_)
        ));
        let Mode::Serve(o) = parse_args([
            "--max-memory-mb".into(),
            "512".into(),
            "--no-sandbox".into(),
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(o.sandbox.max_memory_mb, Some(512));
        assert!(!o.sandbox.enabled);
    }

    #[test]
    fn read_rejects_truncated() {
        let mut r: &[u8] = &[0, 0];
        assert!(read_message(&mut r).is_err());
        let mut r: &[u8] = &[];
        assert!(read_message(&mut r).unwrap().is_none());
        let mut r: &[u8] = &[0xff, 0xff, 0xff, 0xff, 2];
        assert!(read_message(&mut r).is_err());
    }
}
