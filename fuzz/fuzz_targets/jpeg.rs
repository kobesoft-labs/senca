//! JPEG（基本方式）の復号。復号できたものは色空間の変換まで通す。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_image::Limits;

fuzz_target!(|data: &[u8]| {
    let limits = Limits { max_width: 4096, max_height: 4096, max_pixels: 4 << 20 };
    if let Ok(j) = senca_image::jpeg::decode_jpeg(data, &limits) {
        let _ = j.to_gray();
        let _ = j.to_rgb();
        let _ = j.to_i420();
    }
});
