//! MPEG-TS の分離（PAT/PMT/PES/PCR）。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_mpegts::{DemuxerConfig, TsDemuxer};

fuzz_target!(|data: &[u8]| {
    let mut d = TsDemuxer::new(DemuxerConfig::default());
    let step = 1 + data.first().copied().unwrap_or(0) as usize * 11;
    for chunk in data.chunks(step) {
        d.push(chunk);
        while d.poll().is_some() {}
    }
    d.flush();
    while d.poll().is_some() {}
    let _ = d.stats();
});
