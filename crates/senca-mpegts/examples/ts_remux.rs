//! TS を分離して多重化し直す（相互接続試験用）。
//!
//! 使い方: `ts_remux <入力.ts> <出力.ts>`

use senca_mpegts::{DemuxEvent, DemuxerConfig, MuxerConfig, TsDemuxer, TsMuxer};
use std::collections::HashMap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: ts_remux <in.ts> <out.ts>");
        std::process::exit(2);
    }
    let input = std::fs::read(&args[1])?;
    let mut demux = TsDemuxer::new(DemuxerConfig::default());
    let mut mux = TsMuxer::new(MuxerConfig::default());
    let mut out = Vec::with_capacity(input.len() + input.len() / 10);
    let mut known = HashMap::new();
    let mut aus = 0u64;
    // 任意の区切りで入れても同じ結果になることを確かめるため、半端な大きさで入れる
    for chunk in input.chunks(1000) {
        demux.push(chunk);
        drain(&mut demux, &mut mux, &mut known, &mut out, &mut aus)?;
    }
    demux.flush();
    drain(&mut demux, &mut mux, &mut known, &mut out, &mut aus)?;
    std::fs::write(&args[2], &out)?;
    eprintln!("access units: {aus}, stats: {:?}", demux.stats());
    Ok(())
}

fn drain(
    demux: &mut TsDemuxer,
    mux: &mut TsMuxer,
    known: &mut HashMap<u32, ()>,
    out: &mut Vec<u8>,
    aus: &mut u64,
) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(ev) = demux.poll() {
        match ev {
            DemuxEvent::Config { track, config } => {
                if known.insert(track.0, ()).is_none() {
                    mux.add_track(track, &config)?;
                } else {
                    mux.update_config(track, &config)?;
                }
            }
            DemuxEvent::AccessUnit(au) => {
                mux.push(&au, out)?;
                *aus += 1;
            }
            _ => {}
        }
    }
    Ok(())
}
