//! 形式・解像度・フレームレートの取り決め（I/O を持たない純粋な関数）。
//!
//! # 映像の選び方
//!
//! 候補（能力の各行）ごとに、要求に一番近い解像度とフレームレートを求め、次の順で比べて最小のものを選ぶ。
//!
//! 1. フレームレートが要求を満たすか（要求の 99% 未満なら不足）。ライブ配信ではコマ落ちを避けるのを優先する。
//!    例: UVC の YUYV 1080p は USB 2.0 の帯域で 5 fps しか出ないことが多い。そのとき MJPEG 1080p 30 fps を選ぶ。
//! 2. 解像度の差（`|w - W| + |h - H|`）。
//! 3. フレームレートの差（要求を超えるものは小さい方）。
//! 4. 形式の優先順（要求の `formats` の順。空なら I420 → NV12 → YUYV → UYVY → MJPEG。
//!    非圧縮を先にするのは画質と CPU（JPEG の復号）のため）。
//!
//! H.264（機器の符号化の素通し）は `formats` に明示したときだけ候補にする。

use crate::error::{CaptureError, Result};
use crate::types::{
    AudioCapability, AudioMode, AudioRequest, Fps, IntervalRange, RawFormat, SizeRange,
    VideoCapability, VideoMode, VideoRequest,
};

/// 1 辺の上限。
pub const MAX_DIMENSION: u32 = senca_codec::frame::MAX_DIMENSION;

/// 自動のときの形式の優先順。
pub const AUTO_FORMATS: [RawFormat; 5] = [
    RawFormat::I420,
    RawFormat::Nv12,
    RawFormat::Yuyv,
    RawFormat::Uyvy,
    RawFormat::Mjpeg,
];

/// 映像の要求を検査する。
pub fn validate_video_request(req: &VideoRequest) -> Result<()> {
    if req.width == 0 || req.height == 0 || req.width > MAX_DIMENSION || req.height > MAX_DIMENSION
    {
        return Err(CaptureError::invalid_config(format!(
            "bad resolution {}x{}",
            req.width, req.height
        )));
    }
    if !req.fps.is_valid() {
        return Err(CaptureError::invalid_config(format!(
            "bad frame rate {}",
            req.fps
        )));
    }
    if req.formats.iter().any(|f| !f.is_video()) {
        return Err(CaptureError::invalid_config(
            "non-video format in video request",
        ));
    }
    if req.formats.len() > 8 {
        return Err(CaptureError::invalid_config("too many formats"));
    }
    Ok(())
}

/// 音声の要求を検査する。
pub fn validate_audio_request(req: &AudioRequest) -> Result<()> {
    if req.channels == 0 || req.channels > senca_codec::frame::MAX_AUDIO_CHANNELS {
        return Err(CaptureError::invalid_config("bad channel count"));
    }
    if !(8000..=192_000).contains(&req.sample_rate) {
        return Err(CaptureError::invalid_config("bad sample rate"));
    }
    Ok(())
}

fn format_rank(req: &VideoRequest, f: RawFormat) -> Option<usize> {
    if req.formats.is_empty() {
        AUTO_FORMATS.iter().position(|x| *x == f)
    } else {
        req.formats.iter().position(|x| *x == f)
    }
}

/// `v` を `min..=max` に収め、`min` からの `step` 刻みの最も近い値に丸める。
fn snap(v: u32, min: u32, max: u32, step: u32) -> u32 {
    let v = v.clamp(min, max.max(min));
    let step = step.max(1);
    let k = (v - min + step / 2) / step;
    let r = min.saturating_add(k.saturating_mul(step));
    if r > max { r - step } else { r }.max(min)
}

/// 能力の 1 行から、要求に最も近い解像度。
pub fn nearest_size(size: &SizeRange, w: u32, h: u32) -> (u32, u32) {
    match *size {
        SizeRange::Discrete { width, height } => (width, height),
        SizeRange::Stepwise {
            min_width,
            max_width,
            step_width,
            min_height,
            max_height,
            step_height,
        } => (
            snap(w, min_width, max_width, step_width),
            snap(h, min_height, max_height, step_height),
        ),
    }
}

fn cmp_fps(a: Fps, b: Fps) -> std::cmp::Ordering {
    (a.num as u64 * b.den as u64).cmp(&(b.num as u64 * a.den as u64))
}

/// 能力の 1 行から、要求に最も近いフレームレート（要求以上で最小、無ければ最大）。
pub fn nearest_fps(range: &IntervalRange, want: Fps) -> Option<Fps> {
    match range {
        IntervalRange::Discrete { fps } => {
            let valid = fps.iter().copied().filter(|f| f.is_valid());
            let above = valid
                .clone()
                .filter(|f| cmp_fps(*f, want).is_ge())
                .min_by(|a, b| cmp_fps(*a, *b));
            above.or_else(|| valid.max_by(|a, b| cmp_fps(*a, *b)))
        }
        IntervalRange::Range { min, max } => {
            if !min.is_valid() || !max.is_valid() {
                return None;
            }
            Some(if cmp_fps(want, *min).is_lt() {
                *min
            } else if cmp_fps(want, *max).is_gt() {
                *max
            } else {
                want
            })
        }
    }
}

/// 映像の形式を選ぶ。候補が無ければ `Unsupported`。
pub fn choose_video(caps: &[VideoCapability], req: &VideoRequest) -> Result<VideoMode> {
    validate_video_request(req)?;
    let want = req.fps.as_f64();
    let mut best: Option<((bool, u64, u64, usize), VideoMode)> = None;
    for c in caps {
        let Some(rank) = format_rank(req, c.format) else {
            continue;
        };
        let (w, h) = nearest_size(&c.size, req.width, req.height);
        if w == 0 || h == 0 || w > MAX_DIMENSION || h > MAX_DIMENSION {
            continue;
        }
        let Some(fps) = nearest_fps(&c.fps, req.fps) else {
            continue;
        };
        let got = fps.as_f64();
        let short = got < want * 0.99;
        let size_diff = (w.abs_diff(req.width) + h.abs_diff(req.height)) as u64;
        let fps_diff = ((got - want).abs() * 1000.0) as u64;
        let key = (short, size_diff, fps_diff, rank);
        if best.as_ref().is_none_or(|(k, _)| key < *k) {
            best = Some((
                key,
                VideoMode {
                    format: c.format,
                    width: w,
                    height: h,
                    fps,
                    mode: c.mode.clone(),
                },
            ));
        }
    }
    best.map(|(_, m)| m).ok_or_else(|| {
        let offered: Vec<&str> = caps.iter().map(|c| c.format.as_str()).collect();
        CaptureError::unsupported(format!(
            "no usable video format (device offers {:?}, requested {:?})",
            offered,
            req.formats.iter().map(|f| f.as_str()).collect::<Vec<_>>()
        ))
    })
}

/// 音声の形式を選ぶ（標本化周波数・チャンネル数は最も近いもの）。
pub fn choose_audio(caps: &[AudioCapability], req: &AudioRequest) -> Result<AudioMode> {
    validate_audio_request(req)?;
    let mut best: Option<((u32, u8), AudioMode)> = None;
    for c in caps {
        if c.format != RawFormat::PcmS16 {
            continue;
        }
        let Some(rate) = c
            .sample_rates
            .iter()
            .copied()
            .min_by_key(|r| (r.abs_diff(req.sample_rate), u32::MAX - r))
        else {
            continue;
        };
        let Some(ch) = c
            .channels
            .iter()
            .copied()
            .filter(|c| *c > 0)
            .min_by_key(|c| (c.abs_diff(req.channels), u8::MAX - c))
        else {
            continue;
        };
        let key = (rate.abs_diff(req.sample_rate), ch.abs_diff(req.channels));
        if best.as_ref().is_none_or(|(k, _)| key < *k) {
            best = Some((
                key,
                AudioMode {
                    format: c.format,
                    sample_rate: rate,
                    channels: ch,
                },
            ));
        }
    }
    best.map(|(_, m)| m)
        .ok_or_else(|| CaptureError::unsupported("device has no usable audio format"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disc(format: RawFormat, w: u32, h: u32, fps: &[u32]) -> VideoCapability {
        VideoCapability {
            format,
            size: SizeRange::Discrete {
                width: w,
                height: h,
            },
            fps: IntervalRange::Discrete {
                fps: fps.iter().map(|f| Fps::integer(*f)).collect(),
            },
            mode: None,
            interlaced: false,
        }
    }

    fn req(w: u32, h: u32, fps: u32, formats: &[RawFormat]) -> VideoRequest {
        VideoRequest {
            width: w,
            height: h,
            fps: Fps::integer(fps),
            formats: formats.to_vec(),
        }
    }

    /// 典型的な UVC の Web カメラ（YUYV は高解像度でコマ数が落ちる、MJPEG は 30 fps、H.264 もある）。
    fn webcam() -> Vec<VideoCapability> {
        vec![
            disc(RawFormat::Yuyv, 640, 480, &[30, 15]),
            disc(RawFormat::Yuyv, 1280, 720, &[10, 5]),
            disc(RawFormat::Yuyv, 1920, 1080, &[5]),
            disc(RawFormat::Mjpeg, 640, 480, &[30]),
            disc(RawFormat::Mjpeg, 1280, 720, &[30, 60]),
            disc(RawFormat::Mjpeg, 1920, 1080, &[30]),
            disc(RawFormat::H264, 1920, 1080, &[30]),
        ]
    }

    #[test]
    fn prefers_raw_when_fps_is_met() {
        let m = choose_video(&webcam(), &req(640, 480, 30, &[])).unwrap();
        assert_eq!(
            (m.format, m.width, m.height, m.fps),
            (RawFormat::Yuyv, 640, 480, Fps::integer(30))
        );
    }

    #[test]
    fn falls_back_to_mjpeg_for_frame_rate() {
        let m = choose_video(&webcam(), &req(1920, 1080, 30, &[])).unwrap();
        assert_eq!(
            (m.format, m.width, m.height),
            (RawFormat::Mjpeg, 1920, 1080)
        );
        let m = choose_video(&webcam(), &req(1280, 720, 30, &[])).unwrap();
        assert_eq!(m.format, RawFormat::Mjpeg);
        assert_eq!(m.fps, Fps::integer(30));
        // 60 fps を求めれば 60 を選ぶ
        let m = choose_video(&webcam(), &req(1280, 720, 60, &[])).unwrap();
        assert_eq!(m.fps, Fps::integer(60));
    }

    #[test]
    fn h264_only_when_explicit() {
        let m = choose_video(&webcam(), &req(1920, 1080, 30, &[])).unwrap();
        assert_ne!(m.format, RawFormat::H264);
        let m = choose_video(&webcam(), &req(1920, 1080, 30, &[RawFormat::H264])).unwrap();
        assert_eq!(m.format, RawFormat::H264);
        // 明示した形式の順が優先
        let m = choose_video(
            &webcam(),
            &req(640, 480, 30, &[RawFormat::Mjpeg, RawFormat::Yuyv]),
        )
        .unwrap();
        assert_eq!(m.format, RawFormat::Mjpeg);
    }

    #[test]
    fn nearest_size_when_not_offered() {
        let m = choose_video(&webcam(), &req(800, 600, 30, &[RawFormat::Yuyv])).unwrap();
        assert_eq!((m.width, m.height), (640, 480));
        // YUYV だけなら、30 fps を満たす小さい解像度を 1080p 5 fps より優先する
        let m = choose_video(&webcam(), &req(1920, 1080, 30, &[RawFormat::Yuyv])).unwrap();
        assert_eq!((m.width, m.fps), (640, Fps::integer(30)));
        // どれも fps が足りないときは解像度の近いもの、その中で最大の fps
        let caps = vec![
            disc(RawFormat::Yuyv, 1920, 1080, &[5]),
            disc(RawFormat::Yuyv, 1280, 720, &[10, 5]),
        ];
        let m = choose_video(&caps, &req(1920, 1080, 30, &[])).unwrap();
        assert_eq!((m.width, m.fps), (1920, Fps::integer(5)));
        let m = choose_video(&caps, &req(1280, 720, 30, &[])).unwrap();
        assert_eq!((m.width, m.fps), (1280, Fps::integer(10)));
    }

    #[test]
    fn unsupported_when_nothing_matches() {
        let e = choose_video(&webcam(), &req(640, 480, 30, &[RawFormat::Nv12])).unwrap_err();
        assert_eq!(e.kind, crate::ErrorKind::Unsupported);
        assert!(choose_video(&[], &req(640, 480, 30, &[])).is_err());
        assert!(choose_video(&webcam(), &req(0, 480, 30, &[])).is_err());
        let mut r = req(640, 480, 30, &[]);
        r.fps = Fps::new(1, 0);
        assert!(choose_video(&webcam(), &r).is_err());
    }

    #[test]
    fn stepwise_and_continuous() {
        let caps = vec![VideoCapability {
            format: RawFormat::Nv12,
            size: SizeRange::Stepwise {
                min_width: 32,
                max_width: 1920,
                step_width: 16,
                min_height: 32,
                max_height: 1080,
                step_height: 8,
            },
            fps: IntervalRange::Range {
                min: Fps::integer(1),
                max: Fps::integer(60),
            },
            mode: None,
            interlaced: false,
        }];
        let m = choose_video(&caps, &req(1000, 563, 25, &[])).unwrap();
        assert_eq!((m.width, m.height, m.fps), (1008, 560, Fps::integer(25)));
        let m = choose_video(&caps, &req(4000, 4000, 120, &[])).unwrap();
        assert_eq!((m.width, m.height, m.fps), (1920, 1080, Fps::integer(60)));
        let m = choose_video(&caps, &req(8, 8, 30, &[])).unwrap();
        assert_eq!((m.width, m.height), (32, 32));
        assert_eq!(snap(1919, 32, 1920, 16), 1920);
        assert_eq!(snap(1920, 0, 1919, 16), 1904);
    }

    #[test]
    fn ntsc_rates() {
        let caps = vec![VideoCapability {
            format: RawFormat::Uyvy,
            size: SizeRange::Discrete {
                width: 1920,
                height: 1080,
            },
            fps: IntervalRange::Discrete {
                fps: vec![Fps::new(30000, 1001)],
            },
            mode: Some("Hp29".into()),
            interlaced: false,
        }];
        // 29.97 は 30 の 99% 以上なので「足りている」
        let m = choose_video(&caps, &req(1920, 1080, 30, &[])).unwrap();
        assert_eq!(m.fps, Fps::new(30000, 1001));
        assert_eq!(m.mode.as_deref(), Some("Hp29"));
    }

    #[test]
    fn audio() {
        let caps = vec![AudioCapability {
            format: RawFormat::PcmS16,
            sample_rates: vec![44100, 48000],
            channels: vec![1, 2],
        }];
        let m = choose_audio(
            &caps,
            &AudioRequest {
                sample_rate: 48000,
                channels: 2,
            },
        )
        .unwrap();
        assert_eq!((m.sample_rate, m.channels), (48000, 2));
        let m = choose_audio(
            &caps,
            &AudioRequest {
                sample_rate: 32000,
                channels: 6,
            },
        )
        .unwrap();
        assert_eq!((m.sample_rate, m.channels), (44100, 2));
        assert!(
            choose_audio(
                &[],
                &AudioRequest {
                    sample_rate: 48000,
                    channels: 2
                }
            )
            .is_err()
        );
        assert!(
            choose_audio(
                &caps,
                &AudioRequest {
                    sample_rate: 48000,
                    channels: 0
                }
            )
            .is_err()
        );
    }
}
