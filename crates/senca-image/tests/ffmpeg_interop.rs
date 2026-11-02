//! FFmpeg（試験の相手としてだけ使う）との相互運用試験。
//!
//! FFmpeg が要るので `#[ignore]`。`tools/interop/image.sh`、または
//! `cargo test -p senca-image --test ffmpeg_interop -- --ignored` で動かす。
//! 環境変数 `FFMPEG` で FFmpeg の場所を変えられる（既定 `~/tools/ffmpeg`）。

use std::path::{Path, PathBuf};
use std::process::Command;

use senca_image::color::{YuvSpec, rgb_to_i420};
use senca_image::jpeg::{
    JpegSubsampling, decode_jpeg, encode_jpeg_i420, encode_jpeg_i420_with_restart, encode_jpeg_rgb,
};
use senca_image::metrics::{psnr, psnr_rgb};
use senca_image::png::{decode_png, encode_png_gray, encode_png_rgb, encode_png_rgba};
use senca_image::{Limits, Plane, RgbImage, RgbaImage};

fn ffmpeg() -> PathBuf {
    std::env::var_os("FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            Path::new(&home).join("tools/ffmpeg")
        })
}

struct TempDir(PathBuf);
impl TempDir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("senca-image-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self, f: &str) -> PathBuf {
        self.0.join(f)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_ffmpeg(args: &[&str]) {
    let out = Command::new(ffmpeg())
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg を起動できない");
    assert!(
        out.status.success(),
        "ffmpeg {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// ffmpeg で画像を rawvideo に復号する。
fn ffmpeg_decode(input: &Path, pix_fmt: &str) -> Vec<u8> {
    let out = Command::new(ffmpeg())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args(["-f", "rawvideo", "-pix_fmt", pix_fmt, "-"])
        .output()
        .expect("ffmpeg を起動できない");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn photo(w: usize, h: usize) -> RgbImage {
    let mut img = RgbImage::new(w, h).unwrap();
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let r = 128.0 + 100.0 * (fx * 6.0).sin() * (fy * 3.0).cos();
            let g = 40.0
                + 180.0 * fy
                + if (x / 16 + y / 16) % 2 == 0 {
                    10.0
                } else {
                    0.0
                };
            let b = 128.0 + 90.0 * ((fx + fy) * 9.0).cos();
            img.pixel_mut(x, y)
                .copy_from_slice(&[r as u8, g as u8, b as u8]);
        }
    }
    img
}

fn plane_from(w: usize, h: usize, d: &[u8]) -> Plane {
    Plane::from_vec(w, h, d.to_vec()).unwrap()
}

#[test]
#[ignore = "FFmpeg が要る"]
fn our_jpeg_444_decoded_by_ffmpeg() {
    let t = TempDir::new("j444");
    let img = photo(321, 179);
    let j = encode_jpeg_rgb(&img, 95, JpegSubsampling::S444).unwrap();
    let p = t.path("a.jpg");
    std::fs::write(&p, &j).unwrap();
    let theirs = RgbImage::from_vec(321, 179, ffmpeg_decode(&p, "rgb24")).unwrap();
    let ours = decode_jpeg(&j, &Limits::default())
        .unwrap()
        .to_rgb()
        .unwrap();
    let vs_src = psnr_rgb(&img, &theirs).unwrap();
    let vs_ours = psnr_rgb(&ours, &theirs).unwrap();
    println!("444 q95: ffmpeg vs 元 {vs_src:.2} dB、ffmpeg vs 自前の復号 {vs_ours:.2} dB");
    assert!(vs_src >= 40.0);
    assert!(vs_ours >= 45.0);
}

#[test]
#[ignore = "FFmpeg が要る"]
fn our_jpeg_420_decoded_by_ffmpeg() {
    let t = TempDir::new("j420");
    let img = photo(320, 180);
    let f = rgb_to_i420(&img, YuvSpec::BT601_FULL);
    let j = encode_jpeg_i420(&f, YuvSpec::BT601_FULL, 95).unwrap();
    let p = t.path("a.jpg");
    std::fs::write(&p, &j).unwrap();
    // yuvj420p は範囲の変換なしで平面をそのまま出す。
    let raw = ffmpeg_decode(&p, "yuvj420p");
    let ours = decode_jpeg(&j, &Limits::default())
        .unwrap()
        .to_i420()
        .unwrap();
    let ty = plane_from(320, 180, &raw[..320 * 180]);
    let tu = plane_from(160, 90, &raw[320 * 180..320 * 180 + 160 * 90]);
    let py = psnr(f.y(), &ty).unwrap();
    let pu = psnr(f.u(), &tu).unwrap();
    let po = psnr(ours.y(), &ty).unwrap();
    println!("420 q95: Y {py:.2} dB、U {pu:.2} dB、自前との差 Y {po:.2} dB");
    assert!(py >= 40.0 && pu >= 40.0);
    assert!(po >= 45.0);
    // リスタートマーカー入りも FFmpeg が同じに復号する
    let jr = encode_jpeg_i420_with_restart(&f, YuvSpec::BT601_FULL, 95, 5).unwrap();
    let pr = t.path("r.jpg");
    std::fs::write(&pr, &jr).unwrap();
    assert_eq!(ffmpeg_decode(&pr, "yuvj420p"), raw, "リスタート入り");
}

#[test]
#[ignore = "FFmpeg が要る"]
fn our_png_decoded_by_ffmpeg_exactly() {
    let t = TempDir::new("png");
    let img = photo(97, 61);
    let p = t.path("rgb.png");
    std::fs::write(&p, encode_png_rgb(&img, 6).unwrap()).unwrap();
    assert_eq!(ffmpeg_decode(&p, "rgb24"), img.data());
    let mut rgba = img.to_rgba();
    for (i, px) in rgba
        .data_mut()
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .enumerate()
    {
        px[3] = (i * 13) as u8;
    }
    let p = t.path("rgba.png");
    std::fs::write(&p, encode_png_rgba(&rgba, 9).unwrap()).unwrap();
    assert_eq!(ffmpeg_decode(&p, "rgba"), rgba.data());
    let g = Plane::from_fn(77, 33, |x, y| (x * 3 + y * 5) as u8).unwrap();
    let p = t.path("g.png");
    std::fs::write(&p, encode_png_gray(&g, 1).unwrap()).unwrap();
    assert_eq!(ffmpeg_decode(&p, "gray"), g.to_packed());
}

#[test]
#[ignore = "FFmpeg が要る"]
fn ffmpeg_png_decoded_by_us_exactly() {
    let t = TempDir::new("ffpng");
    let img = photo(83, 47);
    let src = t.path("src.rgb");
    std::fs::write(&src, img.data()).unwrap();
    let s = src.to_str().unwrap();
    for (pix, pred) in [
        ("rgb24", "paeth"),
        ("rgba", "mixed"),
        ("gray", "avg"),
        ("rgb48be", "up"),
        ("pal8", "none"),
        ("monob", "none"),
        ("ya8", "sub"),
        ("gray16be", "mixed"),
    ] {
        let out = t.path(&format!("{pix}.png"));
        let o = out.to_str().unwrap();
        run_ffmpeg(&[
            "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", "83x47", "-i", s, "-pix_fmt", pix,
            "-pred", pred, o,
        ]);
        let data = std::fs::read(&out).unwrap();
        let ours = decode_png(&data, &Limits::default()).unwrap_or_else(|e| panic!("{pix}: {e}"));
        let theirs = RgbaImage::from_vec(83, 47, ffmpeg_decode(&out, "rgba")).unwrap();
        if ours.bit_depth == 16 {
            // 16 bit → 8 bit の丸め方が違う（自前は上位バイト）ので ±1 を許す。
            let worst = ours
                .image
                .data()
                .iter()
                .zip(theirs.data())
                .map(|(&a, &b)| (a as i32 - b as i32).abs())
                .max();
            assert!(worst <= Some(1), "{pix}: {worst:?}");
        } else {
            assert_eq!(ours.image, theirs, "{pix}");
        }
        println!(
            "{pix}: 一致（色型 {}、{} bit）",
            ours.color_type, ours.bit_depth
        );
    }
}

#[test]
#[ignore = "FFmpeg が要る"]
fn ffmpeg_jpeg_decoded_by_us() {
    let t = TempDir::new("ffjpg");
    let img = photo(203, 117);
    let src = t.path("src.rgb");
    std::fs::write(&src, img.data()).unwrap();
    let s = src.to_str().unwrap();
    for pix in ["yuvj420p", "yuvj422p", "yuvj444p", "gray"] {
        let out = t.path(&format!("{pix}.jpg"));
        let o = out.to_str().unwrap();
        run_ffmpeg(&[
            "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", "203x117", "-i", s, "-pix_fmt", pix,
            "-q:v", "2", o,
        ]);
        let data = std::fs::read(&out).unwrap();
        let d = decode_jpeg(&data, &Limits::default()).unwrap_or_else(|e| panic!("{pix}: {e}"));
        // 輝度の平面を ffmpeg の復号と比べる（IDCT の実装差だけ）。
        let theirs = ffmpeg_decode(&out, if pix == "gray" { "gray" } else { pix });
        let ty = plane_from(203, 117, &theirs[..203 * 117]);
        let p = psnr(&d.components[0].plane, &ty).unwrap();
        // 元画像との比較（RGB）。
        let rgb = d.to_rgb().unwrap();
        let ps = if pix == "gray" {
            f64::INFINITY
        } else {
            psnr_rgb(&img, &rgb).unwrap()
        };
        println!("{pix}: Y の ffmpeg との差 {p:.2} dB、元との差 {ps:.2} dB");
        assert!(p >= 45.0, "{pix}: {p}");
        assert!(ps >= 30.0, "{pix}: {ps}");
    }
}
