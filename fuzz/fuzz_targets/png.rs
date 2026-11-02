//! PNG の復号（zlib / inflate、filter、パレット、インターレース）。
//! 上限は既定値より小さくする（大きな画像の確保そのものは不具合ではないので、fuzz の RSS 上限に当てない）。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_image::Limits;

fuzz_target!(|data: &[u8]| {
    let limits = Limits { max_width: 4096, max_height: 4096, max_pixels: 4 << 20 };
    let _ = senca_image::png::decode_png(data, &limits);
    let _ = senca_image::deflate::zlib_decompress(data, 1 << 20);
    let _ = senca_image::deflate::inflate(data, 1 << 20);
});
