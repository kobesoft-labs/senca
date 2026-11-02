//! 定義（JSON）から部品の設定への変換と、フレームの形の変換。
//!
//! フレームの変換はこのプロセスの中だけで行う（生のフレームはプロセスの外に出さない、REQ-010 R10-8）。

use crate::model::*;
use base64::Engine;
use bytes::Bytes;
use senca_board::{BoardKind, BoardParams, ClaheParams, EnhanceParams, Point};
use senca_codec::frame::{AudioFrame, PixelFormat, SampleFormat, VideoFrame};
use senca_compose::{
    Border, Crop, DestRect, FitMode, ImageId, Layer, LayerContent, OverlayImage, Scene, SourceId,
};
use senca_core::Timebase;
use senca_image::{I420Frame, Limits, Plane, YuvColor, YuvSpec};

/// 板の補正の設定に変換する。`corners` を与えると定義の 4 隅より優先する（自動検出の結果）。
pub fn board_params(p: &BoardParamsSpec, corners: Option<[Point; 4]>) -> BoardParams {
    let mut e = match p.mode {
        BoardMode::Whiteboard => EnhanceParams::whiteboard(),
        BoardMode::Blackboard => EnhanceParams::blackboard(),
    };
    if let Some(g) = p.stroke_gain {
        e.stroke_gain = g;
    }
    if let Some(g) = p.chroma_gain {
        e.chroma_gain = g;
    }
    if let Some(g) = p.glare_suppression {
        e.glare_suppression = g;
    }
    e.invert_to_white = p.invert_to_white;
    if p.clahe {
        e.clahe = Some(ClaheParams::default());
    }
    let corners = corners.or_else(|| p.corners.map(|c| c.map(|[x, y]| Point::new(x, y))));
    BoardParams {
        corners,
        output_width: p.output_width as usize,
        output_height: p.output_height as usize,
        enhance: e,
        enhance_enabled: p.enhance,
    }
}

pub fn board_kind(m: BoardMode) -> BoardKind {
    match m {
        BoardMode::Whiteboard => BoardKind::Whiteboard,
        BoardMode::Blackboard => BoardKind::Blackboard,
    }
}

/// ミキサーの入力の設定。
pub fn mix_input(i: &MixInputSpec) -> senca_compose::InputConfig {
    senca_compose::InputConfig {
        gain_db: i.gain_db,
        mute: i.mute,
        solo: i.solo,
        pan: i.pan,
        delay_ms: i.delay_ms,
    }
}

/// ミキサーの入力の値の範囲。
pub fn check_mix_input(i: &MixInputSpec) -> Result<(), String> {
    if !(i.gain_db.is_finite() && (-120.0..=24.0).contains(&i.gain_db)) {
        return Err("gain_db must be -120..=24".into());
    }
    if !(i.pan.is_finite() && (-1.0..=1.0).contains(&i.pan)) {
        return Err("pan must be -1..=1".into());
    }
    if !(i.delay_ms.is_finite() && (0.0..=5000.0).contains(&i.delay_ms)) {
        return Err("delay_ms must be 0..=5000".into());
    }
    Ok(())
}

/// `#RRGGBB` を YUV にする（形は検証済みとし、壊れていれば黒）。
pub fn color(s: &str, spec: YuvSpec) -> YuvColor {
    match parse_color(s) {
        Some([r, g, b]) => spec.rgb_to_yuv(r, g, b),
        None => spec.black(),
    }
}

/// 静止画を復号する（PNG か JPEG、base64）。
pub fn decode_image(img: &ImageSpec, spec: YuvSpec) -> Result<OverlayImage, String> {
    let limits = Limits {
        max_width: 7680,
        max_height: 4320,
        max_pixels: 7680 * 4320,
    };
    let b64 = base64::engine::general_purpose::STANDARD;
    match (&img.png_base64, &img.jpeg_base64) {
        (Some(p), None) => {
            let data = b64.decode(p).map_err(|e| format!("base64: {e}"))?;
            let d =
                senca_image::png::decode_png(&data, &limits).map_err(|e| format!("png: {e}"))?;
            Ok(OverlayImage::from_rgba(&d.image, spec))
        }
        (None, Some(j)) => {
            let data = b64.decode(j).map_err(|e| format!("base64: {e}"))?;
            let d =
                senca_image::jpeg::decode_jpeg(&data, &limits).map_err(|e| format!("jpeg: {e}"))?;
            Ok(OverlayImage::from_i420(
                d.to_i420().map_err(|e| format!("jpeg: {e}"))?,
            ))
        }
        _ => Err("give exactly one of png_base64 / jpeg_base64".into()),
    }
}

/// 合成の名前（入力・画像）と番号の対応。
#[derive(Debug, Clone)]
pub struct ComposeNames {
    pub inputs: Vec<String>,
    pub images: Vec<String>,
    pub spec: YuvSpec,
}

impl ComposeNames {
    pub fn new(c: &ComposeNode) -> Self {
        ComposeNames {
            inputs: c.inputs.keys().cloned().collect(),
            images: c.images.keys().cloned().collect(),
            spec: YuvSpec::BT709_LIMITED,
        }
    }

    pub fn source_id(&self, name: &str) -> Option<SourceId> {
        self.inputs
            .iter()
            .position(|n| n == name)
            .map(|i| SourceId(i as u32))
    }

    pub fn image_id(&self, name: &str) -> Option<ImageId> {
        self.images
            .iter()
            .position(|n| n == name)
            .map(|i| ImageId(i as u32))
    }

    /// レイヤー 1 枚を変換する。
    pub fn layer(&self, l: &LayerSpec) -> Result<Layer, String> {
        let content = match (&l.source, &l.image, &l.color) {
            (Some(s), None, None) => LayerContent::Video(
                self.source_id(s)
                    .ok_or_else(|| format!("layer refers to unknown input '{s}'"))?,
            ),
            (None, Some(i), None) => LayerContent::Image(
                self.image_id(i)
                    .ok_or_else(|| format!("layer refers to unknown image '{i}'"))?,
            ),
            (None, None, Some(c)) => {
                if parse_color(c).is_none() {
                    return Err(format!("bad color '{c}'"));
                }
                LayerContent::Color(color(c, self.spec))
            }
            _ => return Err("a layer needs exactly one of source / image / color".into()),
        };
        if !l.opacity.is_finite() || !(0.0..=1.0).contains(&l.opacity) {
            return Err("opacity must be 0..=1".into());
        }
        let border = match &l.border {
            None => None,
            Some(b) => {
                if parse_color(&b.color).is_none() || b.width > 512 {
                    return Err("bad border".into());
                }
                Some(Border {
                    width: b.width,
                    color: color(&b.color, self.spec),
                })
            }
        };
        if l.corner_radius > 4096 {
            return Err("corner_radius too large".into());
        }
        Ok(Layer {
            content,
            dest: DestRect::new(l.x, l.y, l.width, l.height),
            crop: Crop {
                left: l.crop.left,
                top: l.crop.top,
                right: l.crop.right,
                bottom: l.crop.bottom,
            },
            fit: match l.fit {
                FitSpec::Stretch => FitMode::Stretch,
                FitSpec::Contain => FitMode::Contain,
                FitSpec::Cover => FitMode::Cover,
            },
            z: l.z,
            opacity: l.opacity,
            corner_radius: l.corner_radius,
            border,
            visible: l.visible,
            filter: l.filter.to_image(),
        })
    }

    /// シーンを変換して検査する。
    pub fn scene(&self, s: &SceneSpec) -> Result<Scene, String> {
        let layers = s
            .layers
            .iter()
            .enumerate()
            .map(|(i, l)| self.layer(l).map_err(|e| format!("layer {i}: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        let scene = Scene::new(layers);
        scene.validate().map_err(|e| e.to_string())?;
        Ok(scene)
    }
}

/// 復号したフレームを I420（`senca-image`）にする。NV12 は並べ替え、P010 は上位 8 bit を取る。
pub fn video_to_i420(f: &VideoFrame) -> Result<I420Frame, String> {
    let f = match f.format {
        PixelFormat::I420 => f.clone(),
        PixelFormat::Nv12 => f.convert(PixelFormat::I420).map_err(|e| e.to_string())?,
        PixelFormat::P010 => p010_to_i420(f)?,
    };
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let plane = |i: usize, pw: usize, ph: usize| -> Result<Plane, String> {
        let p = &f.planes[i];
        let need = p.stride * (ph - 1) + pw;
        if p.data.len() < need {
            return Err("plane too short".into());
        }
        Plane::from_vec_with_stride(pw, ph, p.stride, p.data[..need].to_vec())
            .map_err(|e| e.to_string())
    };
    I420Frame::from_planes(plane(0, w, h)?, plane(1, cw, ch)?, plane(2, cw, ch)?)
        .map_err(|e| e.to_string())
}

fn p010_to_i420(f: &VideoFrame) -> Result<VideoFrame, String> {
    f.validate().map_err(|e| e.to_string())?;
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = Vec::with_capacity(PixelFormat::I420.packed_size(f.width, f.height));
    for y in 0..h {
        let r = f.row(0, y);
        out.extend((0..w).map(|x| r[2 * x + 1]));
    }
    for c in 0..2 {
        for y in 0..ch {
            let r = f.row(1, y);
            out.extend((0..cw).map(|x| r[4 * x + 2 * c + 1]));
        }
    }
    VideoFrame::from_packed(
        PixelFormat::I420,
        f.width,
        f.height,
        Bytes::from(out),
        f.pts,
        f.timebase,
    )
    .map_err(|e| e.to_string())
}

/// I420 を符号化器の入力にする。
pub fn i420_to_video(f: &I420Frame, pts: i64, timebase: Timebase) -> Result<VideoFrame, String> {
    VideoFrame::from_packed(
        PixelFormat::I420,
        f.width() as u32,
        f.height() as u32,
        f.to_bytes(),
        pts,
        timebase,
    )
    .map_err(|e| e.to_string())
}

/// 音声のフレームを f32 のインターリーブにする。
pub fn audio_to_f32(f: &AudioFrame) -> Vec<f32> {
    match f.format {
        SampleFormat::F32 => f
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_ne_bytes(*c))
            .collect(),
        SampleFormat::I16 => f
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_ne_bytes(*c) as f32 / 32768.0)
            .collect(),
    }
}

/// f32 のインターリーブを符号化器の入力にする。
pub fn f32_to_audio(samples: &[f32], rate: u32, channels: u8, pts: i64) -> AudioFrame {
    AudioFrame {
        format: SampleFormat::F32,
        sample_rate: rate,
        channels,
        data: Bytes::from(
            samples
                .iter()
                .flat_map(|v| v.to_ne_bytes())
                .collect::<Vec<u8>>(),
        ),
        pts,
        timebase: Timebase::new(1, rate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i420_roundtrip_and_nv12() {
        let mut d = Vec::new();
        for i in 0..(6 * 4 + 2 * 3 * 2) {
            d.push(i as u8);
        }
        let v = VideoFrame::from_packed(
            PixelFormat::I420,
            6,
            4,
            Bytes::from(d.clone()),
            7,
            Timebase::MPEG,
        )
        .unwrap();
        let f = video_to_i420(&v).unwrap();
        assert_eq!(f.to_bytes().as_ref(), &d[..]);
        let back = i420_to_video(&f, 7, Timebase::MPEG).unwrap();
        assert_eq!(back, v);
        let nv = v.convert(PixelFormat::Nv12).unwrap();
        assert_eq!(video_to_i420(&nv).unwrap(), f);
    }

    #[test]
    fn p010_takes_high_byte() {
        let (w, h) = (4u32, 2u32);
        let mut d = Vec::new();
        for i in 0..(w * h) {
            d.extend_from_slice(&[0x40, i as u8]);
        }
        for i in 0..2 {
            d.extend_from_slice(&[0, 100 + i, 0, 200 + i]);
        }
        let v = VideoFrame::from_packed(PixelFormat::P010, w, h, Bytes::from(d), 0, Timebase::MPEG)
            .unwrap();
        let f = video_to_i420(&v).unwrap();
        assert_eq!(f.y().row(1), &[4, 5, 6, 7]);
        assert_eq!(f.u().row(0), &[100, 101]);
        assert_eq!(f.v().row(0), &[200, 201]);
    }

    #[test]
    fn audio_conversion() {
        let f = f32_to_audio(&[0.5, -0.25], 48000, 2, 10);
        assert_eq!(audio_to_f32(&f), vec![0.5, -0.25]);
    }
}
