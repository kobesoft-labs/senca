//! MP4（断片化の有無を問わない）を読み、fMP4（`--frag`）または MP4（`--progressive`）に書き直す（相互接続試験用）。
//!
//! 使い方: `mp4_rewrite --frag|--progressive <入力.mp4> <出力.mp4>`

mod common;

use bytes::Bytes;
use senca_fmp4::{Sample, TrackSpec, read_file};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 || !matches!(args[1].as_str(), "--frag" | "--progressive") {
        eprintln!("usage: mp4_rewrite --frag|--progressive <in.mp4> <out.mp4>");
        std::process::exit(2);
    }
    let input = Bytes::from(std::fs::read(&args[2])?);
    let (init, read) = read_file(&input)?;
    let mut specs = Vec::new();
    let mut samples = Vec::new();
    for t in &init.tracks {
        let Some(cfg) = t.config.clone() else { continue };
        let mut spec = TrackSpec::new(t.track_id, cfg);
        spec.timescale = t.timescale;
        let ss: Vec<Sample> = read
            .iter()
            .filter(|s| s.track_id == t.track_id)
            .map(|s| Sample { dts: s.dts, duration: s.duration, cts_offset: s.cts_offset, keyframe: s.keyframe, data: s.data.clone() })
            .collect();
        specs.push(spec);
        samples.push(ss);
    }
    let out = common::write_output(specs, samples, args[1] == "--frag")?;
    std::fs::write(&args[3], out)?;
    Ok(())
}
