//! WorkerPool と worker プロセスを実際に起動する試験（AC-17、REQ-010 R10-2）。

use bytes::Bytes;
use senca_codec::caps::Direction;
use senca_codec::config::{AudioEncoderConfig, EncoderConfig, VideoEncoderConfig};
use senca_codec::error::ErrorKind;
use senca_codec::frame::{AudioFrame, Frame, SampleFormat};
use senca_codec::registry::Policy;
use senca_codec_worker::{FailureCause, JobKind, JobOutput, PoolConfig, WorkerPool};
use senca_core::{Codec, Timebase};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const LIB_DIR: &str = "/home/cc/tools/codeclibs/usr/lib/x86_64-linux-gnu";

fn config(size: usize, args: &[&str]) -> PoolConfig {
    let mut c = PoolConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_senca-codec-worker")));
    c.size = size;
    c.args = args.iter().map(|s| s.into()).collect();
    c.job_timeout = Duration::from_secs(20);
    c.restart_backoff_min = Duration::from_millis(20);
    c.restart_backoff_max = Duration::from_millis(200);
    let dir = std::env::var_os("SENCA_CODEC_LIB_DIR").unwrap_or_else(|| LIB_DIR.into());
    c.env.retain(|(k, _)| k != "SENCA_CODEC_LIB_DIR");
    c.env.push(("SENCA_CODEC_LIB_DIR".into(), dir));
    c
}

fn h264_encode() -> JobKind {
    JobKind::Encode {
        encoder: EncoderConfig::Video(VideoEncoderConfig::new(Codec::H264, 320, 240, 30, 500_000)),
        frames: vec![],
    }
}

#[tokio::test]
async fn self_test_reports_sandbox() {
    let pool = WorkerPool::start(config(1, &[])).await.unwrap();
    let info = pool.worker_info().expect("hello received");
    assert!(info.sandbox.contains("no_new_privs"), "{}", info.sandbox);
    let JobOutput::SelfTest(r) = pool
        .submit("t", Policy::default(), JobKind::SelfTest)
        .await
        .unwrap()
    else {
        panic!("wrong output")
    };
    assert!(r.no_new_privs);
    assert!(r.seccomp, "{}", r.sandbox);
    assert!(r.socket_denied, "socket() must be denied inside the worker");
    pool.shutdown().await;

    let pool = WorkerPool::start(config(1, &["--no-sandbox"]))
        .await
        .unwrap();
    let JobOutput::SelfTest(r) = pool
        .submit("t", Policy::default(), JobKind::SelfTest)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(!r.socket_denied);
    pool.shutdown().await;
}

#[tokio::test]
async fn policy_errors_come_back_cleanly() {
    let pool = WorkerPool::start(config(1, &[])).await.unwrap();
    // GPU の無い機械: H.264 は 503 相当。
    let e = pool
        .submit("t", Policy::default(), h264_encode())
        .await
        .unwrap_err();
    assert_eq!(e.cause, FailureCause::Codec);
    assert_eq!(e.kind, ErrorKind::Unavailable);
    assert_eq!(e.api_code(), "CAPACITY_UNAVAILABLE");
    assert_eq!(e.http_status(), 503);
    // ロイヤリティフリー限定
    let e = pool
        .submit("t", Policy::royalty_free(), h264_encode())
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PolicyRejected);
    // AAC
    let mut a = AudioEncoderConfig::opus(48000, 2, 64000);
    a.codec = Codec::Aac;
    let e = pool
        .submit(
            "t",
            Policy::default(),
            JobKind::Encode {
                encoder: EncoderConfig::Audio(a),
                frames: vec![],
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    let s = pool.stats();
    assert_eq!((s.restarts, s.crashes, s.alive), (0, 0, 1));
    pool.shutdown().await;
}

#[tokio::test]
async fn opus_encode_when_available() {
    let pool = WorkerPool::start(config(1, &[])).await.unwrap();
    let has_opus = pool
        .capabilities()
        .iter()
        .any(|c| c.codec == Codec::Opus && c.direction == Direction::Encode);
    if !has_opus {
        eprintln!("libopus not available to the worker; skipping");
        pool.shutdown().await;
        return;
    }
    let tb = Timebase::new(1, 48000);
    let frames = (0..10)
        .map(|i| {
            let mut d = Vec::with_capacity(960 * 4);
            for n in 0..960 {
                let t = (i * 960 + n) as f32 / 48000.0;
                d.extend_from_slice(
                    &((t * 440.0 * std::f32::consts::TAU).sin() * 0.3).to_ne_bytes(),
                );
            }
            Frame::Audio(AudioFrame {
                format: SampleFormat::F32,
                sample_rate: 48000,
                channels: 1,
                data: Bytes::from(d),
                pts: i * 960,
                timebase: tb,
            })
        })
        .collect();
    let out = pool
        .submit(
            "t",
            Policy::royalty_free(),
            JobKind::Encode {
                encoder: EncoderConfig::Audio(AudioEncoderConfig::opus(48000, 1, 32000)),
                frames,
            },
        )
        .await
        .unwrap();
    let JobOutput::Encoded { config, aus } = out else {
        panic!()
    };
    assert!(!aus.is_empty());
    assert!(aus.iter().all(|a| a.codec == Codec::Opus));
    assert!(config.unwrap().record.starts_with(b"OpusHead"));
    pool.shutdown().await;
}

async fn ok_job(pool: &WorkerPool) {
    match pool.submit("t", Policy::default(), JobKind::SelfTest).await {
        Ok(JobOutput::SelfTest(_)) => {}
        other => panic!("expected a successful job, got {other:?}"),
    }
}

#[tokio::test]
async fn crash_is_isolated_and_worker_restarted() {
    let pool = WorkerPool::start(config(1, &["--fault", "crash", "--fault-after", "1"]))
        .await
        .unwrap();
    ok_job(&pool).await;
    let e = pool
        .submit("t", Policy::default(), JobKind::SelfTest)
        .await
        .unwrap_err();
    assert_eq!(e.cause, FailureCause::WorkerCrashed);
    ok_job(&pool).await;
    let s = pool.stats();
    assert_eq!((s.restarts, s.crashes), (1, 1));
    assert_eq!(s.alive, 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn hang_times_out_and_worker_replaced() {
    let mut c = config(1, &["--fault", "hang", "--fault-after", "1"]);
    c.job_timeout = Duration::from_millis(800);
    let pool = WorkerPool::start(c).await.unwrap();
    ok_job(&pool).await;
    let t0 = std::time::Instant::now();
    let e = pool
        .submit("t", Policy::default(), JobKind::SelfTest)
        .await
        .unwrap_err();
    assert_eq!(e.cause, FailureCause::Timeout);
    assert_eq!(e.kind, ErrorKind::Unavailable);
    assert!(t0.elapsed() < Duration::from_secs(5));
    ok_job(&pool).await;
    assert_eq!(pool.stats().timeouts, 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn lying_worker_is_replaced() {
    for fault in ["lie", "oversize"] {
        let pool = WorkerPool::start(config(1, &["--fault", fault, "--fault-after", "1"]))
            .await
            .unwrap();
        ok_job(&pool).await;
        let e = pool
            .submit("t", Policy::default(), JobKind::SelfTest)
            .await
            .unwrap_err();
        assert_eq!(e.cause, FailureCause::ProtocolViolation, "{fault}");
        ok_job(&pool).await;
        let s = pool.stats();
        assert_eq!((s.violations, s.restarts), (1, 1), "{fault}");
        pool.shutdown().await;
    }
}

#[tokio::test]
async fn missing_executable_is_unavailable() {
    let mut c = config(2, &[]);
    c.program = "/nonexistent/senca-codec-worker".into();
    let pool = WorkerPool::start(c).await.unwrap();
    let e = pool
        .submit("t", Policy::default(), JobKind::SelfTest)
        .await
        .unwrap_err();
    assert_eq!(e.cause, FailureCause::NoWorkers);
    assert_eq!(e.http_status(), 503);
    assert!(pool.stats().start_failures >= 2);
    pool.shutdown().await;
}

#[tokio::test]
async fn protocol_version_mismatch_is_rejected() {
    // 版 2 の Hello（本体 18 バイト）を出して止まる偽の worker。
    let script = r"printf '\000\000\000\022\001\000\002\000\000\000\001\000\000\000\000\000\000\000\000\000\000\000\000'; read x";
    let mut c = config(1, &[]);
    c.program = "/bin/sh".into();
    c.args = vec!["-c".into(), script.into()];
    c.hello_timeout = Duration::from_secs(3);
    let pool = WorkerPool::start(c).await.unwrap();
    let e = pool
        .submit("t", Policy::default(), JobKind::SelfTest)
        .await
        .unwrap_err();
    assert_eq!(e.cause, FailureCause::NoWorkers);
    assert!(pool.worker_info().is_none());
    pool.shutdown().await;
}

#[tokio::test]
async fn tenants_are_served_round_robin() {
    let pool = Arc::new(
        WorkerPool::start(config(1, &["--fault", "slow:80"]))
            .await
            .unwrap(),
    );
    let order = Arc::new(AtomicUsize::new(0));
    let mut a_jobs = Vec::new();
    for _ in 0..8 {
        let (p, o) = (pool.clone(), order.clone());
        a_jobs.push(tokio::spawn(async move {
            p.submit("tenant-a", Policy::default(), JobKind::SelfTest)
                .await
                .unwrap();
            o.fetch_add(1, Ordering::SeqCst)
        }));
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    let (p, o) = (pool.clone(), order.clone());
    let b = tokio::spawn(async move {
        p.submit("tenant-b", Policy::default(), JobKind::SelfTest)
            .await
            .unwrap();
        o.fetch_add(1, Ordering::SeqCst)
    });
    let b_pos = b.await.unwrap();
    for j in a_jobs {
        j.await.unwrap();
    }
    // A の 1 つ目（処理中）と A の 2 つ目の後には B の番が来る。
    assert!(b_pos <= 2, "tenant B finished at position {b_pos}");
    pool.shutdown().await;
}

#[tokio::test]
async fn bounded_queue_and_cancellation() {
    let mut c = config(1, &["--fault", "slow:200"]);
    c.max_queue_per_tenant = 2;
    let pool = Arc::new(WorkerPool::start(c).await.unwrap());
    let mut hs = Vec::new();
    for _ in 0..6 {
        let p = pool.clone();
        hs.push(tokio::spawn(async move {
            p.submit("a", Policy::default(), JobKind::SelfTest).await
        }));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut full = 0;
    for h in hs {
        if let Err(e) = h.await.unwrap() {
            assert_eq!(e.cause, FailureCause::QueueFull);
            assert_eq!(e.http_status(), 503);
            full += 1;
        }
    }
    assert!(full >= 1);
    // 取り消し: 待ち行列のジョブを捨てても数がずれない。
    let p = pool.clone();
    let _ = tokio::time::timeout(Duration::from_millis(10), async move {
        let _ = p.submit("a", Policy::default(), JobKind::SelfTest).await;
    })
    .await;
    ok_job(&pool).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(pool.stats().queued, 0);
    pool.shutdown().await;
    let e = pool
        .submit("a", Policy::default(), JobKind::SelfTest)
        .await
        .unwrap_err();
    assert_eq!(e.cause, FailureCause::ShuttingDown);
}

#[tokio::test]
async fn parallel_workers() {
    let pool = Arc::new(
        WorkerPool::start(config(4, &["--fault", "slow:200"]))
            .await
            .unwrap(),
    );
    assert_eq!(pool.stats().alive, 4);
    let t0 = std::time::Instant::now();
    let hs: Vec<_> = (0..4)
        .map(|i| {
            let p = pool.clone();
            tokio::spawn(async move {
                p.submit(&format!("t{i}"), Policy::default(), JobKind::SelfTest)
                    .await
            })
        })
        .collect();
    for h in hs {
        h.await.unwrap().unwrap();
    }
    // 4 つが並んで動けば 4 x 200ms より十分短い。
    assert!(
        t0.elapsed() < Duration::from_millis(700),
        "{:?}",
        t0.elapsed()
    );
    pool.shutdown().await;
}

#[test]
fn sandbox_self_test_cli() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_senca-codec-worker"))
        .arg("--self-test-sandbox")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{s}");
    assert!(s.contains("socket_denied=1"), "{s}");
}
