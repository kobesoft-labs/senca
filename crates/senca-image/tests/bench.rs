//! 性能の計測（`cargo test --release -p senca-image --test bench -- --ignored --nocapture`）。

use std::time::Instant;

use senca_image::color::{YuvSpec, i420_to_rgb, rgb_to_i420};
use senca_image::jpeg::{decode_jpeg, encode_jpeg_i420};
use senca_image::png::{decode_png, encode_png_rgb};
use senca_image::scale::scale_i420;
use senca_image::{I420Frame, Limits, RgbImage, ScaleFilter};

fn frame() -> I420Frame {
    let (w, h) = (1920, 1080);
    let mut img = RgbImage::new(w, h).unwrap();
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let r = 128.0 + 100.0 * (fx * 13.0).sin() * (fy * 7.0).cos();
            let g = 40.0 + 180.0 * fy + ((x ^ y) & 15) as f32;
            let b = 128.0 + 90.0 * ((fx + fy) * 19.0).cos();
            img.pixel_mut(x, y)
                .copy_from_slice(&[r as u8, g as u8, b as u8]);
        }
    }
    rgb_to_i420(&img, YuvSpec::BT709_LIMITED)
}

fn time<T>(name: &str, n: usize, mut f: impl FnMut() -> T) {
    f();
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(f());
    }
    println!(
        "{name}: {:.2} ms",
        t.elapsed().as_secs_f64() * 1000.0 / n as f64
    );
}

#[test]
#[ignore = "性能の計測"]
fn bench_1080p() {
    let f = frame();
    time("JPEG 符号化 1080p 4:2:0 q85", 10, || {
        encode_jpeg_i420(&f, YuvSpec::BT709_LIMITED, 85).unwrap()
    });
    let j = encode_jpeg_i420(&f, YuvSpec::BT709_LIMITED, 85).unwrap();
    println!("  大きさ {} KiB", j.len() / 1024);
    time("JPEG 復号 1080p", 10, || {
        decode_jpeg(&j, &Limits::default()).unwrap()
    });
    time("縮小 1080p→720p Lanczos3", 10, || {
        scale_i420(&f, 1280, 720, ScaleFilter::Lanczos3).unwrap()
    });
    time("縮小 1080p→720p 双線形", 10, || {
        scale_i420(&f, 1280, 720, ScaleFilter::Bilinear).unwrap()
    });
    time("サムネイル 1080p→320x180 面積平均", 10, || {
        scale_i420(&f, 320, 180, ScaleFilter::Area).unwrap()
    });
    let rgb = i420_to_rgb(&f, YuvSpec::BT709_LIMITED);
    time("I420→RGB 1080p", 10, || {
        i420_to_rgb(&f, YuvSpec::BT709_LIMITED)
    });
    time("PNG 符号化 1080p RGB level 6", 3, || {
        encode_png_rgb(&rgb, 6).unwrap()
    });
    let p = encode_png_rgb(&rgb, 6).unwrap();
    println!("  大きさ {} KiB", p.len() / 1024);
    time("PNG 復号 1080p", 3, || {
        decode_png(&p, &Limits::default()).unwrap()
    });
}
