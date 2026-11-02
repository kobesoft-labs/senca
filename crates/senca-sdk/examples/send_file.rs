//! Rust API の使用例: MPEG-TS か FLV のファイルを分離し、AU を SDK に入れて実時間で送る。
//!
//! `send_file <config> <in.ts|in.flv> [--fast]`
//!
//! config は `senca://ingest?...`・`srt://...`・`rtmp://.../KEY`。終わったら統計を 1 行出す。

use senca_core::time::Rescale;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase};
use senca_flv::{FlvIngest, FlvReader, IngestEvent};
use senca_mpegts::{DemuxEvent, DemuxerConfig, TsDemuxer};
use senca_sdk::frame::{
    AudioCodec, AudioConfig, AudioFormat, AudioFrame, VideoCodec, VideoFormat, VideoFrame,
};
use senca_sdk::{Sender, SenderConfig, SenderState};
use std::time::{Duration, Instant};

enum In {
    Config(CodecConfig),
    Au(AccessUnit),
}

fn demux(path: &str) -> Result<Vec<In>, Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    let mut out = Vec::new();
    if path.ends_with(".flv") {
        let mut r = FlvReader::new();
        r.feed(&data);
        let mut ing = FlvIngest::default();
        let mut ev = Vec::new();
        while let Some(t) = r.next_tag()? {
            ing.push(t.tag_type, t.timestamp, &t.body, &mut ev)?;
            for e in ev.drain(..) {
                match e {
                    IngestEvent::Config { config, .. } => out.push(In::Config(config)),
                    IngestEvent::AccessUnit(a) => out.push(In::Au(a)),
                    IngestEvent::EndOfSequence { .. } => {}
                }
            }
        }
    } else {
        let mut d = TsDemuxer::new(DemuxerConfig::default());
        d.push(&data);
        d.flush();
        while let Some(e) = d.poll() {
            match e {
                DemuxEvent::Config { config, .. } => out.push(In::Config(config)),
                DemuxEvent::AccessUnit(a) => out.push(In::Au(a)),
                _ => {}
            }
        }
    }
    Ok(out)
}

fn us(v: i64, tb: Timebase) -> i64 {
    v.rescale(tb, Timebase::MICROS).unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: send_file <config> <in.ts|in.flv> [--fast]");
        std::process::exit(2);
    }
    let realtime = !args.iter().any(|a| a == "--fast");
    let items = demux(&args[2])?;
    let s = Sender::new(SenderConfig::from_uri(&args[1])?)?;
    s.set_event_handler(|e| eprintln!("event {e:?}"))?;
    s.start()?;
    // 比べやすいように、つながってから入れ始める
    let t = Instant::now();
    while s.state() != SenderState::Connected {
        if s.state() == SenderState::Failed || t.elapsed() > Duration::from_secs(15) {
            eprintln!("not connected: {:?}", s.state());
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // 受け側の準備を待つ時間（試験用、既定 0）
    if let Some(ms) = std::env::var("SEND_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
    let (mut vin, mut ain, mut dropped) = (0u64, 0u64, 0u64);
    let start = Instant::now();
    let mut base: Option<i64> = None;
    for it in &items {
        match it {
            In::Config(c) => match c.codec {
                Codec::H264 => s.set_video_config(VideoCodec::H264, &c.record)?,
                Codec::H265 => s.set_video_config(VideoCodec::H265, &c.record)?,
                Codec::Aac | Codec::Opus => {
                    let a = c.audio.unwrap_or_default();
                    s.set_audio_config(&AudioConfig {
                        codec: if c.codec == Codec::Aac {
                            AudioCodec::Aac
                        } else {
                            AudioCodec::Opus
                        },
                        sample_rate: a.sample_rate,
                        channels: a.channels,
                        extra: c.record.to_vec(),
                    })?
                }
                other => return Err(format!("unsupported codec {other}").into()),
            },
            In::Au(a) => {
                let dts = us(a.dts, a.timebase);
                let pts = us(a.pts, a.timebase);
                let b = *base.get_or_insert(dts);
                if realtime {
                    let due = start + Duration::from_micros((dts - b).max(0) as u64);
                    if let Some(d) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(d);
                    }
                }
                let r = match a.codec {
                    Codec::H264 | Codec::H265 => {
                        vin += 1;
                        let codec = if a.codec == Codec::H264 {
                            VideoCodec::H264
                        } else {
                            VideoCodec::H265
                        };
                        s.push_video(&VideoFrame {
                            codec,
                            format: VideoFormat::LengthPrefixed,
                            data: &a.payload,
                            pts_us: pts,
                            dts_us: dts,
                            keyframe: Some(a.keyframe),
                        })?
                    }
                    Codec::Aac | Codec::Opus => {
                        ain += 1;
                        let codec = if a.codec == Codec::Aac {
                            AudioCodec::Aac
                        } else {
                            AudioCodec::Opus
                        };
                        s.push_audio(&AudioFrame {
                            codec,
                            format: AudioFormat::Raw,
                            data: &a.payload,
                            pts_us: pts,
                        })?
                    }
                    _ => continue,
                };
                if r == senca_sdk::PushOutcome::Dropped {
                    dropped += 1;
                }
            }
        }
    }
    s.stop();
    let st = s.stats();
    println!(
        "video_in={vin} audio_in={ain} push_dropped={dropped} video_sent={} audio_sent={} bytes={} reconnects={} rtt_us={} state={:?}",
        st.video_frames_sent,
        st.audio_frames_sent,
        st.bytes_sent,
        st.reconnects,
        st.rtt_us.map_or(-1, |r| r as i64),
        s.state()
    );
    if s.state() != SenderState::Stopped
        || st.video_frames_sent != vin
        || st.audio_frames_sent != ain
    {
        std::process::exit(1);
    }
    Ok(())
}
