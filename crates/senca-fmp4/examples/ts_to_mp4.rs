//! TS を分離して fMP4（`--frag`）または MP4（`--progressive`）にする（相互接続試験用）。
//!
//! 使い方: `ts_to_mp4 --frag|--progressive <入力.ts> <出力.mp4>`

mod common;

use senca_core::{AccessUnit, CodecConfig};
use senca_fmp4::{SampleQueue, TrackSpec};
use senca_mpegts::{DemuxEvent, DemuxerConfig, TsDemuxer};
use std::collections::BTreeMap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 || !matches!(args[1].as_str(), "--frag" | "--progressive") {
        eprintln!("usage: ts_to_mp4 --frag|--progressive <in.ts> <out.mp4>");
        std::process::exit(2);
    }
    let input = std::fs::read(&args[2])?;
    let mut demux = TsDemuxer::new(DemuxerConfig::default());
    let mut tracks: BTreeMap<u32, (Option<CodecConfig>, Vec<AccessUnit>)> = BTreeMap::new();
    let mut take = |demux: &mut TsDemuxer| {
        while let Some(ev) = demux.poll() {
            match ev {
                DemuxEvent::Config { track, config } => {
                    let e = tracks.entry(track.0).or_default();
                    if e.0.is_none() {
                        e.0 = Some(config);
                    }
                }
                DemuxEvent::AccessUnit(au) => tracks.entry(au.track.0).or_default().1.push(au),
                _ => {}
            }
        }
    };
    for chunk in input.chunks(64 * 1024) {
        demux.push(chunk);
        take(&mut demux);
    }
    demux.flush();
    take(&mut demux);
    let mut specs = Vec::new();
    let mut samples = Vec::new();
    for (id, (cfg, aus)) in tracks {
        let Some(cfg) = cfg else { continue };
        let spec = TrackSpec::new(id + 1, cfg);
        let mut q = SampleQueue::new(spec.timescale);
        for au in &aus {
            q.push(au)?;
        }
        q.flush();
        samples.push(q.take_ready());
        specs.push(spec);
    }
    let out = common::write_output(specs, samples, args[1] == "--frag")?;
    std::fs::write(&args[3], out)?;
    Ok(())
}
