//! MP4 / fragmented MP4 の読み込み（init の moov と moof/mdat）。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_fmp4::{Mp4Reader, read_file};

fuzz_target!(|data: &[u8]| {
    let b = Bytes::copy_from_slice(data);
    if let Ok((init, _samples)) = read_file(&b) {
        // 同じ init で、後半を断片として読み直す
        let mut r = Mp4Reader::new(init);
        let _ = r.read_fragments(&b.slice(data.len() / 2..));
    }
});
