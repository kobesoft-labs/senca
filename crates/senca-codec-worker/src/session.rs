//! worker の中のセッション: パイプライン（`senca-pipeline` の [`Runtime`]）を動かし続ける。
//!
//! # スレッド
//!
//! - 主スレッド: 標準入力を読む（入力の AU は `push_input` へ。待たない）。操作は操作のスレッドへ渡す。
//! - 操作のスレッド: 操作を順に実行して返事を送る（`commit` は切り替わるまで待つので、入力の読み込みを止めない）。
//! - 出力のスレッド: 実行の出力（AU・静止画・通知）をメッセージにする。
//! - 書き手のスレッド: メッセージを標準出力（隔離したもの）へ書く。
//!
//! 生のフレームは worker の外に出さない（REQ-010 R10-8）。1 つのプロセスは 1 つのセッションだけを持ち、
//! 閉じたら終わる。

use crate::protocol::{
    self, MAX_EVENT_JSON, MAX_SESSION_AUS, Message, SessionEvent, SessionEventKind, SessionOpen,
    SessionOutput, SessionOutputKind,
};
use crate::worker::{Fault, WorkerOptions, log, read_message};
use senca_codec::registry::ProviderRegistry;
use senca_pipeline::runtime::RuntimeOutput;
use senca_pipeline::{Control, Graph, PipelineError, Runtime, RuntimeOptions};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

/// 出し切るのを待つ上限。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

fn error_body(e: &PipelineError) -> String {
    serde_json::json!({
        "code": e.api_code(),
        "message": e.to_string(),
        "issues": e.issues(),
    })
    .to_string()
}

fn event(sid: u64, kind: SessionEventKind, request_id: u64, body: String) -> Message {
    // 大きすぎる通知は切り詰めずに誤りに置き換える（JSON を壊さない）。
    let body = if body.len() > MAX_EVENT_JSON {
        serde_json::json!({"code": "INTERNAL_ERROR", "message": "event too large"}).to_string()
    } else {
        body
    };
    Message::SessionEvent(SessionEvent {
        session_id: sid,
        kind,
        request_id,
        body,
    })
}

/// 書き手のスレッド。書けなくなったら終わる。
fn writer(mut out: impl Write, rx: std::sync::mpsc::Receiver<Message>) {
    for m in rx {
        let buf = match protocol::encode_message(&m) {
            Ok(b) => b,
            Err(e) => {
                log(&format!("dropping an oversized session message: {e}"));
                continue;
            }
        };
        if out.write_all(&buf).and_then(|_| out.flush()).is_err() {
            return;
        }
    }
}

/// 実行の出力をメッセージにする。
fn pump(sid: u64, outputs: std::sync::mpsc::Receiver<RuntimeOutput>, tx: SyncSender<Message>) {
    for o in outputs {
        let msgs: Vec<Message> = match o {
            RuntimeOutput::Media(m) => {
                let mut config = m.config;
                m.aus
                    .chunks(MAX_SESSION_AUS)
                    .map(|chunk| {
                        Message::SessionOutput(SessionOutput {
                            session_id: sid,
                            node: m.sink.clone(),
                            stream: m.stream.clone(),
                            kind: SessionOutputKind::Media {
                                track: m.track,
                                generation: m.generation,
                                config: config.take(),
                                aus: chunk.to_vec(),
                            },
                        })
                    })
                    .collect()
            }
            RuntimeOutput::Snapshot(s) => vec![Message::SessionOutput(SessionOutput {
                session_id: sid,
                node: s.node,
                stream: String::new(),
                kind: SessionOutputKind::Snapshot {
                    generation: s.generation,
                    pts_us: s.pts_us,
                    width: s.width,
                    height: s.height,
                    changed_fraction: s.changed_fraction,
                    jpeg: s.jpeg,
                },
            })],
            RuntimeOutput::Event(e) => {
                let body = serde_json::to_string(&e).unwrap_or_default();
                vec![event(sid, SessionEventKind::Event, 0, body)]
            }
        };
        for m in msgs {
            if tx.send(m).is_err() {
                return;
            }
        }
    }
}

/// セッションを受け持つ（このプロセスが終わるまで）。終了コードを返す。
pub fn serve_session(
    o: &WorkerOptions,
    registry: ProviderRegistry,
    open: SessionOpen,
    mut input: impl Read,
    out: impl Write + Send + 'static,
) -> i32 {
    let sid = open.session_id;
    let (wtx, wrx) = sync_channel::<Message>(1024);
    let writer_thread = std::thread::Builder::new()
        .name("session-writer".into())
        .spawn(move || writer(out, wrx))
        .expect("spawn writer");
    let finish =
        |wtx: SyncSender<Message>, code: i32, writer_thread: std::thread::JoinHandle<()>| {
            let _ = wtx.send(event(sid, SessionEventKind::Closed, 0, "{}".into()));
            drop(wtx);
            let _ = writer_thread.join();
            code
        };
    let started = Graph::from_json(&open.graph).and_then(|g| {
        let opts = RuntimeOptions::new(open.policy.clone(), Arc::new(registry));
        Runtime::start(&g, opts)
    });
    let rt = match started {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            log(&format!(
                "session {sid} (tenant {}) rejected: {e}",
                open.tenant
            ));
            let _ = wtx.send(event(sid, SessionEventKind::Error, 0, error_body(&e)));
            return finish(wtx, 0, writer_thread);
        }
    };
    let plan = rt
        .plan()
        .map(|p| serde_json::to_string(&p).unwrap_or_default())
        .unwrap_or_default();
    let _ = wtx.send(event(sid, SessionEventKind::Opened, 0, plan));
    let outputs = rt.take_outputs().expect("outputs taken once");
    let pump_thread = {
        let tx = wtx.clone();
        std::thread::Builder::new()
            .name("session-pump".into())
            .spawn(move || pump(sid, outputs, tx))
            .expect("spawn pump")
    };
    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<(u64, String)>();
    let ctl_thread = {
        let rt = rt.clone();
        let tx = wtx.clone();
        std::thread::Builder::new()
            .name("session-control".into())
            .spawn(move || {
                for (rid, js) in ctl_rx {
                    let r = Control::from_json(&js).and_then(|c| rt.control(c));
                    let m = match r {
                        Ok(reply) => event(
                            sid,
                            SessionEventKind::ControlOk,
                            rid,
                            serde_json::to_string(&reply).unwrap_or_default(),
                        ),
                        Err(e) => event(sid, SessionEventKind::ControlErr, rid, error_body(&e)),
                    };
                    if tx.send(m).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn control")
    };
    let mut inputs: u64 = 0;
    let mut graceful = false;
    let code = loop {
        let m = match read_message(&mut input) {
            Ok(Some(m)) => m,
            Ok(None) => break 0,
            Err(e) => {
                log(&format!("bad input from parent: {e}"));
                break 3;
            }
        };
        match m {
            Message::SessionInput(i) if i.session_id == sid => {
                if o.fault == Some(Fault::SessionCrash) && inputs >= o.fault_after {
                    log("fault injection: session crash");
                    std::process::abort();
                }
                inputs += 1;
                if (!i.aus.is_empty() || i.config.is_some())
                    && let Err(e) = rt.push_input(&i.source, i.config, i.aus)
                {
                    let _ = wtx.try_send(event(
                        sid,
                        SessionEventKind::Event,
                        0,
                        serde_json::to_string(&senca_pipeline::Event::Error {
                            node: Some(i.source.clone()),
                            code: e.api_code().to_string(),
                            message: e.to_string(),
                        })
                        .unwrap_or_default(),
                    ));
                }
                if i.end {
                    rt.end_input(&i.source);
                }
            }
            Message::SessionControl(c) if c.session_id == sid => {
                let _ = ctl_tx.send((c.request_id, c.control));
            }
            Message::SessionClose(id) if id == sid => {
                graceful = true;
                break 0;
            }
            Message::Ping(n) => {
                let _ = wtx.send(Message::Pong(n));
            }
            Message::Shutdown => break 0,
            other => {
                log(&format!("unexpected message in session {sid}: {other:?}"));
                break 3;
            }
        }
    };
    drop(ctl_tx);
    let _ = ctl_thread.join();
    rt.close(if graceful {
        CLOSE_TIMEOUT
    } else {
        Duration::from_secs(2)
    });
    drop(rt);
    let _ = pump_thread.join();
    finish(wtx, code, writer_thread)
}
